//! Native Windows commit experiment, compiled only into tests. Production keeps
//! ReplaceFileW until the experiment explains what readers observe during a
//! replacement and which file metadata may change.

use std::cell::{Cell, RefCell};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use windows_sys::Win32::Storage::FileSystem::{
    FileRenameInfoEx, SetFileInformationByHandle, FILE_RENAME_INFO,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Primitive {
    ReplaceFile,
    RenameEx,
    InjectError(i32),
}

#[derive(Debug, PartialEq, Eq)]
enum Event {
    Renamed,
    Fallback {
        code: Option<i32>,
        reason: &'static str,
    },
}

type Checkpoint = fn(&Path, &Path);
thread_local! {
    static PRIMITIVE: Cell<Primitive> = const { Cell::new(Primitive::ReplaceFile) };
    static EVENTS: RefCell<Vec<Event>> = const { RefCell::new(Vec::new()) };
    static BEFORE_COMMIT: Cell<Option<Checkpoint>> = const { Cell::new(None) };
    static OWNER_COMPARISON: Cell<Option<bool>> = const { Cell::new(None) };
}

pub(super) fn enabled() -> bool {
    PRIMITIVE.get() != Primitive::ReplaceFile
}

pub(super) fn checkpoint(destination: &Path, replacement: &Path) {
    // atomic_write_source reaches this point only after syncing and closing the
    // prepared temporary. The hook models a crash before it replaces the target.
    if let Some(checkpoint) = BEFORE_COMMIT.take() {
        checkpoint(destination, replacement);
    }
}

fn using<T>(primitive: Primitive, f: impl FnOnce() -> T) -> (T, Vec<Event>) {
    struct Restore(Primitive);
    impl Drop for Restore {
        fn drop(&mut self) {
            PRIMITIVE.set(self.0);
            BEFORE_COMMIT.set(None);
            OWNER_COMPARISON.set(None);
        }
    }
    let restore = Restore(PRIMITIVE.replace(primitive));
    EVENTS.with_borrow_mut(Vec::clear);
    let result = f();
    let events = EVENTS.with_borrow_mut(std::mem::take);
    drop(restore);
    (result, events)
}

fn fallback_reason(code: Option<i32>) -> &'static str {
    match code {
        // These codes can indicate an unsupported information class, flag or
        // filesystem. They do not identify the filesystem or Windows version.
        Some(1 | 50 | 87 | 120 | 124) => "rename API/flags/filesystem unsupported or invalid",
        Some(5) => "rename access denied",
        Some(32) => "rename sharing violation",
        _ => "rename failed",
    }
}

pub(super) fn replace_with_fallback(
    destination: &Path,
    replacement: &Path,
    replace_file: impl FnOnce() -> io::Result<bool>,
) -> io::Result<bool> {
    let result = match PRIMITIVE.get() {
        Primitive::RenameEx => rename_ex(destination, replacement),
        Primitive::InjectError(code) => Err(io::Error::from_raw_os_error(code)),
        Primitive::ReplaceFile => unreachable!("the experiment must be explicitly enabled"),
    };
    let (code, reason, detail) = match result {
        Ok(true) => {
            EVENTS.with_borrow_mut(|events| events.push(Event::Renamed));
            return Ok(true);
        }
        Ok(false) => (
            None,
            "owner_differs",
            "destination and temporary owner SIDs differ".to_owned(),
        ),
        Err(error) => {
            let code = error.raw_os_error();
            (code, fallback_reason(code), error.to_string())
        }
    };
    EVENTS.with_borrow_mut(|events| events.push(Event::Fallback { code, reason }));
    // Preserve the established replacement primitive on every experimental
    // failure. A failed fallback must not silently become an in-place write.
    match replace_file()? {
        true => Ok(true),
        false => Err(io::Error::other(format!(
            "FileRenameInfoEx not used ({reason}: {detail}); ReplaceFileW fallback failed; in-place fallback disabled for the experiment"
        ))),
    }
}

fn owner_sid(file: &std::fs::File) -> io::Result<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{GetLengthSid, OWNER_SECURITY_INFORMATION};
    let mut owner = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: the live handle has READ_CONTROL; the API allocates a security
    // descriptor and points owner into it. Both output pointers are writable.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle() as _,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    struct Descriptor(*mut std::ffi::c_void);
    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: GetSecurityInfo allocated this descriptor with LocalAlloc.
            unsafe { LocalFree(self.0) };
        }
    }
    let _descriptor = Descriptor(descriptor);
    if owner.is_null() {
        return Err(io::Error::other(
            "file security descriptor has no owner SID",
        ));
    }
    // SAFETY: Windows returned a valid SID owned by the live descriptor.
    Ok(unsafe {
        std::slice::from_raw_parts(owner.cast::<u8>(), GetLengthSid(owner) as usize).to_vec()
    })
}

fn rename_ex(destination: &Path, replacement: &Path) -> io::Result<bool> {
    // FILE_RENAME_INFO's Flags union uses these SDK bit values. Do not add
    // IGNORE_READONLY_ATTRIBUTE: edits must still respect read-only targets.
    const FILE_RENAME_FLAG_REPLACE_IF_EXISTS: u32 = 0x1;
    const FILE_RENAME_FLAG_POSIX_SEMANTICS: u32 = 0x2;
    const DELETE: u32 = 0x0001_0000;
    const READ_CONTROL: u32 = 0x0002_0000;
    let source = std::fs::OpenOptions::new()
        .access_mode(DELETE | READ_CONTROL)
        .open(replacement)?;
    let target = std::fs::OpenOptions::new()
        .access_mode(READ_CONTROL)
        .open(destination)?;
    let owners_match = owner_sid(&source)? == owner_sid(&target)?;
    let owners_match = OWNER_COMPARISON.get().unwrap_or(owners_match);
    // Renaming keeps the temporary's owner. If it differs from the destination's,
    // use ReplaceFileW rather than changing the destination's ownership.
    if !owners_match {
        return Ok(false);
    }
    let destination = std::fs::canonicalize(destination)?;
    let name: Vec<u16> = destination.as_os_str().encode_wide().collect();
    let name_bytes = name
        .len()
        .checked_mul(2)
        .ok_or_else(|| io::Error::other("rename path too long"))?;
    let size = std::mem::size_of::<FILE_RENAME_INFO>()
        .checked_add(name_bytes)
        .ok_or_else(|| io::Error::other("rename buffer too long"))?;
    let size_u32 = u32::try_from(size).map_err(|_| io::Error::other("rename buffer too long"))?;
    let name_bytes =
        u32::try_from(name_bytes).map_err(|_| io::Error::other("rename path too long"))?;
    // u64 storage gives FILE_RENAME_INFO the alignment required on Win64.
    let mut buffer = vec![0u64; size.div_ceil(8)];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: the aligned allocation contains the fixed header plus all UTF-16
    // path bytes. FileNameLength excludes the terminating NUL (not required).
    unsafe {
        (*info).Anonymous.Flags =
            FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS;
        (*info).RootDirectory = std::ptr::null_mut();
        (*info).FileNameLength = name_bytes;
        std::ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
    }
    // SAFETY: the handle and buffer remain live, and size_u32 describes the
    // whole allocation including the variable-length filename.
    let ok = unsafe {
        SetFileInformationByHandle(
            source.as_raw_handle() as _,
            FileRenameInfoEx,
            info.cast(),
            size_u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(true)
}

#[cfg(test)]
mod tests;
