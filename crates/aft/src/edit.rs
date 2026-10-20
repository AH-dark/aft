//! Shared edit engine: byte-offset conversion, content replacement,
//! syntax validation, and auto-backup orchestration.
//!
//! Used by `write`, `edit_symbol`, `edit_match`, and `batch` commands.

#![cfg_attr(test, allow(clippy::items_after_test_module))]

use std::path::Path;

use crate::config::Config;
use crate::context::AppContext;
use crate::error::AftError;
use crate::format;
use crate::parser::{detect_language, parse_source_with_cached_parser, FileParser};

/// How [`atomic_write_source`] stored the new bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceWriteMode {
    /// A synced sibling file replaced the target by rename, or created it.
    Replaced,
    /// The existing file was truncated, rewritten and synced in place, because
    /// a replacement file could not keep its hard links, owner, group, mode,
    /// ACLs or extended attributes exactly.
    InPlace,
}

/// Write a source file so that a failed write leaves the original bytes
/// untouched whenever that is possible without changing who can read or reach
/// the file.
///
/// Replacing by rename gives the path a new inode, which keeps nothing of the
/// old file's links, owner, group, ACLs or extended attributes unless they are
/// copied. The rename is used only when the replacement matches the original:
/// a regular file with a single link, owned by the current user, whose owner,
/// group, mode and extended attributes were copied and read back equal. On
/// Linux the extended attributes include the POSIX ACL. On macOS the ACL is
/// copied by the system's `fcopyfile` and is not read back separately. Any
/// other existing file, and any existing file on Windows or another Unix, is
/// rewritten in place and synced, as editors do when a backup-and-rename would
/// alter the file, so access is never silently widened.
/// Every source write path, including syntax rollback and hashline
/// transactions, goes through this one decision.
pub(crate) fn atomic_write_source(path: &Path, bytes: &[u8]) -> std::io::Result<SourceWriteMode> {
    atomic_write_source_with(path, bytes, |file, bytes| {
        use std::io::Write;
        file.write_all(bytes)
    })
}

fn atomic_write_source_with(
    path: &Path,
    bytes: &[u8],
    writer: impl FnOnce(&mut std::fs::File, &[u8]) -> std::io::Result<()>,
) -> std::io::Result<SourceWriteMode> {
    use std::fs::{self, OpenOptions};
    use std::io;
    // Follow an existing symlink just as an in-place write would, rather than
    // replacing the link itself with a regular file.
    let destination = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::canonicalize(path)?,
        Ok(_) => path.to_path_buf(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => path.to_path_buf(),
        Err(error) => return Err(error),
    };
    let original = match fs::metadata(&destination) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if let Some(metadata) = original.as_ref() {
        // Preserve the existing file's write-access checks even when its parent
        // directory would permit replacing a read-only file by rename.
        OpenOptions::new().write(true).open(&destination)?;
        if !source_replacement::may_keep_access(metadata) {
            return write_source_in_place(&destination, bytes, writer);
        }
    }
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let temporary = parent.join(format!(".aft-source-{suffix}.tmp"));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if let Some(metadata) = original.as_ref() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(metadata.permissions().mode() & 0o777);
    }
    let file = match options.open(&temporary) {
        Ok(file) => file,
        // A writable file in a directory that refuses new entries can still be
        // rewritten in place, as it could before replacement writes existed.
        Err(error) if original.is_some() && error.kind() == io::ErrorKind::PermissionDenied => {
            return write_source_in_place(&destination, bytes, writer);
        }
        Err(error) => return Err(error),
    };
    struct RemoveTemporary {
        path: std::path::PathBuf,
        file: Option<fs::File>,
    }
    impl Drop for RemoveTemporary {
        fn drop(&mut self) {
            self.file.take();
            let _ = fs::remove_file(&self.path);
        }
    }
    let mut cleanup = RemoveTemporary {
        path: temporary.clone(),
        file: Some(file),
    };
    let file = cleanup.file.as_mut().expect("temporary file is open");
    if let Some(metadata) = original.as_ref() {
        // Copy and verify access metadata before any content is written, so a
        // replacement that cannot match the original costs nothing to abandon.
        if !source_replacement::copy_access(&destination, file, metadata) {
            drop(cleanup);
            return write_source_in_place(&destination, bytes, writer);
        }
    }
    writer(file, bytes)?;
    if let Some(metadata) = original.as_ref() {
        // Writing can clear set-id bits; restore the exact original mode.
        file.set_permissions(metadata.permissions())?;
    }
    file.sync_all()
        .map_err(|error| io::Error::new(error.kind(), format!("durability: {error}")))?;
    cleanup.file.take();
    crate::fs_lock::rename_over(&temporary, &destination)?;
    // The replacement has already committed; a directory sync failure cannot
    // be reported as a failed edit without implying the original still exists.
    if let Ok(directory) = fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(SourceWriteMode::Replaced)
}

/// Rewrite an existing file through its own inode, keeping every link, owner,
/// ACL and extended attribute, then sync it so the torn-write window ends as
/// soon as the full content is written.
fn write_source_in_place(
    destination: &Path,
    bytes: &[u8],
    writer: impl FnOnce(&mut std::fs::File, &[u8]) -> std::io::Result<()>,
) -> std::io::Result<SourceWriteMode> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(destination)?;
    writer(&mut file, bytes)?;
    file.sync_all()
        .map_err(|error| std::io::Error::new(error.kind(), format!("durability: {error}")))?;
    Ok(SourceWriteMode::InPlace)
}

/// Decides whether a renamed replacement can carry an existing file's access
/// metadata, and copies that metadata onto the replacement.
mod source_replacement {
    use std::fs::{File, Metadata};
    use std::path::Path;

    /// Cheap checks that rule out a replacement before a temporary exists: a
    /// rename would split a hard link, and only the owner can give a new file
    /// the original's owner.
    #[cfg(unix)]
    pub(super) fn may_keep_access(metadata: &Metadata) -> bool {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions and cannot fail.
        let euid = unsafe { libc::geteuid() };
        metadata.is_file() && metadata.nlink() == 1 && metadata.uid() == euid
    }

    /// Windows replacements take the directory's inherited ACL instead of the
    /// file's own, and there is no cheap way to prove they match, so existing
    /// files are always rewritten in place.
    #[cfg(not(unix))]
    pub(super) fn may_keep_access(_metadata: &Metadata) -> bool {
        false
    }

    /// Give `replacement` the original's group, mode and extended attributes,
    /// then read them back. Returns false when any of them cannot be matched.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(super) fn copy_access(destination: &Path, replacement: &File, original: &Metadata) -> bool {
        use std::os::unix::fs::MetadataExt;
        let Ok(source) = File::open(destination) else {
            return false;
        };
        // The original may have changed since it was first inspected; decide
        // on the open handle so the copied metadata belongs to one file.
        let Ok(current) = source.metadata() else {
            return false;
        };
        if current.dev() != original.dev() || current.ino() != original.ino() {
            return false;
        }
        let Ok(created) = replacement.metadata() else {
            return false;
        };
        if created.gid() != original.gid()
            && std::os::unix::fs::fchown(replacement, None, Some(original.gid())).is_err()
        {
            return false;
        }
        if replacement.set_permissions(original.permissions()).is_err() {
            return false;
        }
        let Ok(wanted) = xattrs::snapshot(&source) else {
            return false;
        };
        if xattrs::copy_platform_security(&source, replacement).is_err()
            || xattrs::replace_all(replacement, &wanted).is_err()
        {
            return false;
        }
        let Ok(copied) = replacement.metadata() else {
            return false;
        };
        copied.uid() == original.uid()
            && copied.gid() == original.gid()
            && copied.mode() == original.mode()
            && xattrs::snapshot(replacement).is_ok_and(|actual| actual == wanted)
            && xattrs::platform_acls_match(&source, replacement)
    }

    /// Other platforms offer no checked way to copy ACLs, so a replacement
    /// could widen access; their existing files are rewritten in place.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(super) fn copy_access(
        _destination: &Path,
        _replacement: &File,
        _original: &Metadata,
    ) -> bool {
        false
    }

    /// Extended attributes read and written through open file descriptors.
    /// On Linux this includes `system.posix_acl_access`, the stored POSIX ACL.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(super) mod xattrs {
        use std::collections::BTreeMap;
        use std::ffi::{CStr, CString};
        use std::fs::File;
        use std::io;
        use std::os::fd::AsRawFd;

        pub(crate) type Snapshot = BTreeMap<CString, Vec<u8>>;

        /// Sizes can grow between the length query and the read; retry a few
        /// times rather than looping forever on a file that keeps changing.
        const SIZE_RETRIES: usize = 8;

        fn unsupported(error: &io::Error) -> bool {
            error.raw_os_error() == Some(libc::ENOTSUP)
                || error.raw_os_error() == Some(libc::EOPNOTSUPP)
        }

        #[cfg(target_os = "linux")]
        unsafe fn list_raw(fd: i32, buffer: *mut libc::c_char, size: usize) -> isize {
            libc::flistxattr(fd, buffer, size)
        }
        #[cfg(target_os = "macos")]
        unsafe fn list_raw(fd: i32, buffer: *mut libc::c_char, size: usize) -> isize {
            libc::flistxattr(fd, buffer, size, 0)
        }
        #[cfg(target_os = "linux")]
        unsafe fn get_raw(fd: i32, name: &CStr, value: *mut libc::c_void, size: usize) -> isize {
            libc::fgetxattr(fd, name.as_ptr(), value, size)
        }
        #[cfg(target_os = "macos")]
        unsafe fn get_raw(fd: i32, name: &CStr, value: *mut libc::c_void, size: usize) -> isize {
            libc::fgetxattr(fd, name.as_ptr(), value, size, 0, 0)
        }
        #[cfg(target_os = "linux")]
        unsafe fn set_raw(fd: i32, name: &CStr, value: &[u8]) -> i32 {
            libc::fsetxattr(fd, name.as_ptr(), value.as_ptr().cast(), value.len(), 0)
        }
        #[cfg(target_os = "macos")]
        unsafe fn set_raw(fd: i32, name: &CStr, value: &[u8]) -> i32 {
            libc::fsetxattr(fd, name.as_ptr(), value.as_ptr().cast(), value.len(), 0, 0)
        }
        #[cfg(target_os = "linux")]
        unsafe fn remove_raw(fd: i32, name: &CStr) -> i32 {
            libc::fremovexattr(fd, name.as_ptr())
        }
        #[cfg(target_os = "macos")]
        unsafe fn remove_raw(fd: i32, name: &CStr) -> i32 {
            libc::fremovexattr(fd, name.as_ptr(), 0)
        }

        /// Read a variable-length attribute buffer with the usual
        /// query-size-then-read protocol.
        fn read_sized(mut read: impl FnMut(*mut u8, usize) -> isize) -> io::Result<Vec<u8>> {
            for _ in 0..SIZE_RETRIES {
                let size = read(std::ptr::null_mut(), 0);
                if size < 0 {
                    return Err(io::Error::last_os_error());
                }
                let mut buffer = vec![0u8; size as usize];
                if buffer.is_empty() {
                    return Ok(buffer);
                }
                let read_size = read(buffer.as_mut_ptr(), buffer.len());
                if read_size >= 0 {
                    buffer.truncate(read_size as usize);
                    return Ok(buffer);
                }
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ERANGE) {
                    return Err(error);
                }
            }
            Err(io::Error::other("extended attributes kept changing size"))
        }

        /// Every extended attribute visible to this process, by name. A file
        /// system without extended attributes has an empty snapshot.
        pub(crate) fn snapshot(file: &File) -> io::Result<Snapshot> {
            let fd = file.as_raw_fd();
            // SAFETY: the buffer pointer and size describe a live allocation
            // (or are null and zero for the size query).
            let names =
                match read_sized(|buffer, size| unsafe { list_raw(fd, buffer.cast(), size) }) {
                    Ok(names) => names,
                    Err(error) if unsupported(&error) => return Ok(Snapshot::new()),
                    Err(error) => return Err(error),
                };
            let mut snapshot = Snapshot::new();
            for name in names
                .split(|byte| *byte == 0)
                .filter(|name| !name.is_empty())
            {
                let name = CString::new(name).map_err(io::Error::other)?;
                // SAFETY: as above; `name` is a NUL-terminated C string.
                let value =
                    read_sized(|buffer, size| unsafe { get_raw(fd, &name, buffer.cast(), size) })?;
                snapshot.insert(name, value);
            }
            Ok(snapshot)
        }

        /// Make `file`'s extended attributes exactly `wanted`, removing any it
        /// received on creation (for example an ACL inherited from the parent
        /// directory's default ACL).
        pub(crate) fn replace_all(file: &File, wanted: &Snapshot) -> io::Result<()> {
            let fd = file.as_raw_fd();
            for name in snapshot(file)?
                .keys()
                .filter(|name| !wanted.contains_key(*name))
            {
                // SAFETY: `name` is a NUL-terminated C string.
                if unsafe { remove_raw(fd, name) } != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            for (name, value) in wanted {
                // SAFETY: `name` is a NUL-terminated C string and `value` a
                // live byte slice.
                if unsafe { set_raw(fd, name, value) } != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        }

        /// macOS ACLs are not extended attributes; copy them (and the
        /// attributes) with the system's own metadata copier.
        #[cfg(target_os = "macos")]
        pub(crate) fn copy_platform_security(source: &File, replacement: &File) -> io::Result<()> {
            // SAFETY: both descriptors are open for the duration of the call
            // and a null state is documented as allowed.
            let result = unsafe {
                libc::fcopyfile(
                    source.as_raw_fd(),
                    replacement.as_raw_fd(),
                    std::ptr::null_mut(),
                    libc::COPYFILE_ACL | libc::COPYFILE_XATTR,
                )
            };
            if result == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }

        /// Linux stores POSIX ACLs as extended attributes, which
        /// [`replace_all`] already copies.
        #[cfg(target_os = "linux")]
        pub(crate) fn copy_platform_security(
            _source: &File,
            _replacement: &File,
        ) -> io::Result<()> {
            Ok(())
        }

        /// Linux ACLs, including an access ACL a new file inherits from its
        /// directory's default ACL, are the `system.posix_acl_access`
        /// attribute, so the attribute comparison already covers them.
        #[cfg(target_os = "linux")]
        pub(crate) fn platform_acls_match(_source: &File, _replacement: &File) -> bool {
            true
        }

        /// macOS ACLs are not extended attributes. A new file can inherit
        /// entries from its directory's inheritable ACEs, and `fcopyfile` is
        /// not relied on to remove them, so both ACLs are read back and must
        /// be equal (or both absent).
        #[cfg(target_os = "macos")]
        pub(crate) fn platform_acls_match(source: &File, replacement: &File) -> bool {
            match (extended_acl_text(source), extended_acl_text(replacement)) {
                (Ok(original), Ok(copied)) => original == copied,
                _ => false,
            }
        }

        #[cfg(target_os = "macos")]
        mod acl_sys {
            use std::ffi::{c_char, c_int, c_void};
            /// `ACL_TYPE_EXTENDED` from `<sys/acl.h>`.
            pub(super) const ACL_TYPE_EXTENDED: c_int = 0x0000_0100;
            extern "C" {
                pub(super) fn acl_get_fd_np(fd: c_int, acl_type: c_int) -> *mut c_void;
                pub(super) fn acl_to_text(acl: *mut c_void, len: *mut libc::ssize_t)
                    -> *mut c_char;
                pub(super) fn acl_free(object: *mut c_void) -> c_int;
            }
        }

        /// The file's extended ACL as text, or `None` when it has no ACL.
        #[cfg(target_os = "macos")]
        pub(crate) fn extended_acl_text(file: &File) -> io::Result<Option<String>> {
            // SAFETY: the descriptor is open for the duration of the call.
            let acl =
                unsafe { acl_sys::acl_get_fd_np(file.as_raw_fd(), acl_sys::ACL_TYPE_EXTENDED) };
            if acl.is_null() {
                let error = io::Error::last_os_error();
                return if error.raw_os_error() == Some(libc::ENOENT) {
                    Ok(None)
                } else {
                    Err(error)
                };
            }
            // SAFETY: `acl` is a live ACL returned above; a null length
            // pointer is allowed.
            let text = unsafe { acl_sys::acl_to_text(acl, std::ptr::null_mut()) };
            let result = if text.is_null() {
                Err(io::Error::last_os_error())
            } else {
                // SAFETY: acl_to_text returns a NUL-terminated string.
                Ok(Some(
                    unsafe { CStr::from_ptr(text) }
                        .to_string_lossy()
                        .into_owned(),
                ))
            };
            // SAFETY: both objects were allocated by the ACL library and are
            // freed exactly once.
            unsafe {
                if !text.is_null() {
                    acl_sys::acl_free(text.cast());
                }
                acl_sys::acl_free(acl);
            }
            result
        }
    }
}
/// Convert 0-indexed line/col to a byte offset within `source`.
///
/// Tree-sitter columns are byte-indexed within the line, so `col` is a byte
/// offset from the start of the line (not a character offset).
///
/// Walks LF-delimited segments so a lone carriage return remains line content.
/// CRLF line endings still advance past both bytes. Returns `source.len()` if
/// `line` is beyond the end of the file.
pub fn line_col_to_byte(source: &str, line: u32, col: u32) -> usize {
    let target_line = line as usize;
    let mut line_start = 0usize;

    for (index, segment) in source.split_inclusive('\n').enumerate() {
        let line_with_possible_cr = segment.strip_suffix('\n').unwrap_or(segment);
        let line_text = if segment.ends_with("\r\n") {
            line_with_possible_cr
                .strip_suffix('\r')
                .unwrap_or(line_with_possible_cr)
        } else {
            line_with_possible_cr
        };

        if index == target_line {
            return line_start + (col as usize).min(line_text.len());
        }
        line_start += segment.len();
        #[cfg(test)]
        LINE_OFFSET_BYTES_SCANNED.with(|count| count.set(count.get() + segment.len()));
    }

    source.len()
}

/// Byte starts shared by all line-range edits against one captured source.
pub(crate) struct LineOffsets {
    starts: Vec<usize>,
    source_len: usize,
}

impl LineOffsets {
    pub(crate) fn new(source: &str) -> Self {
        let mut start = 0;
        let starts = source
            .split_inclusive('\n')
            .map(|segment| {
                let offset = start;
                start += segment.len();
                #[cfg(test)]
                LINE_OFFSET_BYTES_SCANNED.with(|count| count.set(count.get() + segment.len()));
                offset
            })
            .collect();
        Self {
            starts,
            source_len: source.len(),
        }
    }
    pub(crate) fn line_count(&self) -> usize {
        self.starts.len()
    }
    pub(crate) fn offset(&self, line: usize) -> usize {
        self.starts.get(line).copied().unwrap_or(self.source_len)
    }
}

pub(crate) fn validate_byte_range(source: &str, start: usize, end: usize) -> Result<(), AftError> {
    if start > end {
        return Err(AftError::InvalidRequest {
            message: format!(
                "invalid byte range [{}..{}): start must be <= end",
                start, end
            ),
        });
    }
    if end > source.len() {
        return Err(AftError::InvalidRequest {
            message: format!(
                "invalid byte range [{}..{}): end exceeds source length {}",
                start,
                end,
                source.len()
            ),
        });
    }
    if !source.is_char_boundary(start) {
        return Err(AftError::InvalidRequest {
            message: format!(
                "invalid byte range [{}..{}): start is not a char boundary",
                start, end
            ),
        });
    }
    if !source.is_char_boundary(end) {
        return Err(AftError::InvalidRequest {
            message: format!(
                "invalid byte range [{}..{}): end is not a char boundary",
                start, end
            ),
        });
    }
    Ok(())
}

/// Replace bytes in `[start..end)` with `replacement`.
///
/// Returns an error if the range is invalid or does not align to UTF-8 char boundaries.
pub fn replace_byte_range(
    source: &str,
    start: usize,
    end: usize,
    replacement: &str,
) -> Result<String, AftError> {
    validate_byte_range(source, start, end)?;

    let mut result = String::with_capacity(
        source.len().saturating_sub(end.saturating_sub(start)) + replacement.len(),
    );
    result.push_str(&source[..start]);
    result.push_str(replacement);
    result.push_str(&source[end..]);
    Ok(result)
}

/// Validate syntax of a file using a fresh FileParser (D023).
///
/// Returns `Ok(Some(true))` if syntax is valid, `Ok(Some(false))` if there are
/// parse errors, and `Ok(None)` if the language is unsupported.
pub fn validate_syntax(path: &Path) -> Result<Option<bool>, AftError> {
    validate_syntax_with_parser(&mut FileParser::new(), path)
}

/// [`validate_syntax`] through a caller-owned parser. The parsed tree stays in
/// that parser's tree cache, so a caller that extracts symbols from the same
/// file next reuses it instead of parsing the file a second time.
pub fn validate_syntax_with_parser(
    parser: &mut FileParser,
    path: &Path,
) -> Result<Option<bool>, AftError> {
    match parser.parse(path) {
        Ok((tree, _lang)) => Ok(Some(!tree.root_node().has_error())),
        Err(AftError::InvalidRequest { .. }) => {
            // Unsupported language — not an error, just can't validate
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// Validate syntax of an in-memory string without touching disk.
///
/// Uses a worker-local reusable parser to validate the proposed content.
/// Returns `None` for
/// unsupported languages, `Some(true)` for valid, `Some(false)` for invalid.
pub fn validate_syntax_str(content: &str, path: &Path) -> Option<bool> {
    let lang = detect_language(path)?;
    let tree = parse_source_with_cached_parser(path, content, lang).ok()?;
    Some(!tree.root_node().has_error())
}

/// Check if the caller requested diff info in the response.
///
/// `include_diff` yields a compact counts-only diff (`additions`/`deletions`),
/// which is what agent-facing/raw consumers should use — the payload does not
/// scale with file size. Full before/after content requires the separate
/// `include_diff_content` flag (UI metadata only); see [`wants_diff_content`].
pub fn wants_diff(params: &serde_json::Value) -> bool {
    params
        .get("include_diff")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || wants_diff_content(params)
}

/// Check if the caller requested the full before/after file contents in the
/// diff. This is for UI rendering only (e.g. the OpenCode/Pi plugins building a
/// diff view in tool metadata) and is deliberately NOT the default: full
/// content makes the response scale with file size, not edit size, which floods
/// agent context on large files. Agent-facing/raw consumers should pass
/// `include_diff` (counts only) instead.
pub fn wants_diff_content(params: &serde_json::Value) -> bool {
    params
        .get("include_diff_content")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Check whether the caller requested an internal, side-effect-free preview.
///
/// This is deliberately a wire-only flag used by host integrations before they
/// ask for edit approval. It is not exposed in any agent-facing tool schema.
pub fn wants_preview(params: &serde_json::Value) -> bool {
    params
        .get("preview")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// A shared byte-to-line map and borrowed context lines for ambiguity payloads.
/// Line endings follow `str::lines`, while offsets count every LF byte.
pub(crate) struct SourceLineIndex<'a> {
    starts: Vec<usize>,
    lines: Vec<&'a str>,
}

impl<'a> SourceLineIndex<'a> {
    pub(crate) fn new(source: &'a str) -> Self {
        #[cfg(test)]
        A03_LINE_INDEX_PASSES.with(|count| count.set(count.get() + 1));
        let mut starts = vec![0];
        let mut lines = Vec::new();
        let mut offset = 0;
        for chunk in source.split_inclusive('\n') {
            let line = if let Some(line) = chunk.strip_suffix('\n') {
                line.strip_suffix('\r').unwrap_or(line)
            } else {
                chunk
            };
            lines.push(line);
            offset += chunk.len();
            if chunk.ends_with('\n') {
                starts.push(offset);
            }
        }
        Self { starts, lines }
    }

    pub(crate) fn line_at(&self, byte: usize) -> usize {
        self.starts
            .partition_point(|start| *start <= byte)
            .saturating_sub(1)
    }

    pub(crate) fn context(&self, line: usize, margin: usize) -> String {
        let start = line.saturating_sub(margin).min(self.lines.len());
        let end = line
            .saturating_add(margin)
            .saturating_add(1)
            .min(self.lines.len());
        self.lines[start..end].join("\n")
    }
}

/// The regular-file capture and identity associated with a handler's source.
/// Reuse is conditional on live identity; symlinks and platforms without strong
/// version evidence retain the path-reading pipeline.
pub(crate) struct EditObservation {
    capture: Option<crate::backup::CapturedRegularFile>,
    version: Option<crate::backup::DiskFileVersion>,
}

pub(crate) fn read_edit_source(path: &Path) -> std::io::Result<(String, EditObservation)> {
    let (bytes, observation) = read_edit_bytes(path)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "stream did not contain valid UTF-8",
            )
        })?
        .to_owned();
    Ok((text, observation))
}

pub(crate) fn read_edit_bytes(
    path: &Path,
) -> std::io::Result<(std::sync::Arc<[u8]>, EditObservation)> {
    let before = crate::backup::DiskFileVersion::of_path(path);
    let (bytes, capture) = match crate::backup::CapturedRegularFile::read(path)? {
        Some(capture) => (capture.shared_bytes(), Some(capture)),
        None => {
            #[cfg(test)]
            A03_HANDLER_READS.with(|count| count.set(count.get() + 1));
            (std::fs::read(path)?.into(), None)
        }
    };
    let version =
        before.filter(|version| Some(*version) == crate::backup::DiskFileVersion::of_path(path));
    Ok((bytes, EditObservation { capture, version }))
}

pub(crate) fn auto_backup_observed(
    ctx: &AppContext,
    session: &str,
    path: &Path,
    description: &str,
    op_id: Option<&str>,
    observation: &EditObservation,
) -> Result<Option<String>, AftError> {
    if let (Some(capture), Some(version)) = (observation.capture.as_ref(), observation.version) {
        if std::fs::symlink_metadata(path).is_err() {
            return Ok(None);
        }
        let mut store = ctx.backup().lock();
        // Validate under the backup mutex immediately before storing. A capture
        // predating a replacement or an external save must not become undo data.
        if crate::backup::DiskFileVersion::of_path(path) == Some(version) {
            let mut capture = capture.clone();
            if capture.refresh_if_stale(path).is_ok() {
                return store.snapshot_with_op_from_capture(
                    session,
                    path,
                    description,
                    op_id,
                    &capture,
                );
            }
        }
        store.snapshot_with_op(session, path, description, op_id)
    } else {
        auto_backup(ctx, session, path, description, op_id)
    }
}

pub(crate) struct ObservedFinalText {
    text: String,
    version: Option<crate::backup::DiskFileVersion>,
}

impl ObservedFinalText {
    fn read(path: &Path) -> Option<Self> {
        let before = crate::backup::DiskFileVersion::of_path(path);
        let text = read_edit_text(path).ok()?;
        let version = before
            .filter(|version| Some(*version) == crate::backup::DiskFileVersion::of_path(path));
        Some(Self { text, version })
    }
}

pub(crate) fn read_edit_text(path: &Path) -> std::io::Result<String> {
    #[cfg(test)]
    A03_HANDLER_READS.with(|count| count.set(count.get() + 1));
    std::fs::read_to_string(path)
}

/// Build the unified-diff string used by internal permission previews.
///
/// The OpenCode approval prompt expects a plain unified-diff string in
/// `metadata.diff`. Host wrappers may alternatively consume the `diff.before` /
/// `diff.after` response fields and render their own patch, but returning the
/// ready-to-display string keeps the bridge contract self-contained.
pub fn build_unified_diff(file: &str, before: &str, after: &str) -> String {
    if before == after {
        return format!(
            "Index: {file}
===================================================================
--- {file}
+++ {file}
"
        );
    }

    let text_diff = construct_diff(before, after);
    let patch = text_diff.unified_diff().header(file, file).to_string();
    format!(
        "Index: {file}
===================================================================
{patch}"
    )
}

/// Reject an identity edit before writing or recording undo state.
pub fn no_change_response(id: &str) -> crate::protocol::Response {
    crate::protocol::Response::error(
        id,
        "no_change",
        "No change: the replacement is identical to the existing text.",
    )
}

/// Supply the unified patch consumed by host diff views, independently of diff counts.
pub fn attach_mutation_diff(result: &mut serde_json::Value, file: &str, before: &str, after: &str) {
    result["metadata"] = serde_json::json!({ "diff": build_unified_diff(file, before, after) });
}

/// Attach the standard preview diff fields to a command response payload.
pub fn attach_preview_diff(
    result: &mut serde_json::Value,
    params: &serde_json::Value,
    file: &str,
    before: &str,
    after: &str,
) {
    let (patch, info) = response_diff(params, file, before, after);
    result["metadata"] = serde_json::json!({ "diff": patch });
    result["preview"] = serde_json::json!(true);
    result["diff"] = info;
    result["preview_diff"] = serde_json::json!(patch);
}

fn construct_diff<'a>(before: &'a str, after: &'a str) -> similar::TextDiff<'a, 'a, 'a, str> {
    #[cfg(test)]
    A03_DIFF_CONSTRUCTIONS.with(|count| count.set(count.get() + 1));
    similar::TextDiff::from_lines(before, after)
}

fn counts_from_diff<'a>(diff: &similar::TextDiff<'a, 'a, 'a, str>) -> (usize, usize) {
    use similar::ChangeTag;
    let mut additions = 0usize;
    let mut deletions = 0usize;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => additions += 1,
            ChangeTag::Delete => deletions += 1,
            ChangeTag::Equal => {}
        }
    }
    (additions, deletions)
}

fn diff_counts(before: &str, after: &str) -> (usize, usize) {
    counts_from_diff(&construct_diff(before, after))
}

pub(crate) fn response_diff(
    params: &serde_json::Value,
    file: &str,
    before: &str,
    after: &str,
) -> (String, serde_json::Value) {
    let diff = construct_diff(before, after);
    let (additions, deletions) = counts_from_diff(&diff);
    let patch = if before == after {
        build_unified_diff(file, before, after)
    } else {
        format!("Index: {file}\n===================================================================\n{}", diff.unified_diff().header(file, file))
    };
    let info = diff_info_from_counts(
        before,
        after,
        additions,
        deletions,
        wants_diff_content(params),
    );
    (patch, info)
}

/// Render the optional counts and the host patch from the same line diff.
/// Both representations describe one observed before/after pair.
pub(crate) fn attach_edit_diff(
    result: &mut serde_json::Value,
    params: &serde_json::Value,
    file: &str,
    before: &str,
    after: &str,
) {
    if wants_diff(params) {
        let (patch, info) = response_diff(params, file, before, after);
        result["metadata"] = serde_json::json!({ "diff": patch });
        result["diff"] = info;
    } else {
        attach_mutation_diff(result, file, before, after);
    }
}

/// Compute compact diff counts (additions/deletions) without echoing any file
/// content. This is the agent-facing default — the payload is constant-size
/// regardless of how large the edited file is.
pub fn compute_diff_counts(before: &str, after: &str) -> serde_json::Value {
    let (additions, deletions) = diff_counts(before, after);
    serde_json::json!({
        "additions": additions,
        "deletions": deletions,
    })
}

/// Pick the right diff shape for a response based on request flags.
///
/// Default (`include_diff`): compact counts only — constant-size payload that
/// never floods agent context. Full before/after content is returned only when
/// the caller explicitly opts in with `include_diff_content` (UI metadata path).
pub fn compute_diff_for_response(
    params: &serde_json::Value,
    before: &str,
    after: &str,
) -> serde_json::Value {
    if wants_diff_content(params) {
        compute_diff_info(before, after)
    } else {
        compute_diff_counts(before, after)
    }
}

/// Compute diff info between before/after content for UI metadata.
/// Returns a JSON value with before, after, additions, deletions.
/// For files >512KB, omits full content and returns only counts.
pub fn compute_diff_info(before: &str, after: &str) -> serde_json::Value {
    let (additions, deletions) = diff_counts(before, after);
    diff_info_from_counts(before, after, additions, deletions, true)
}

fn diff_info_from_counts(
    before: &str,
    after: &str,
    additions: usize,
    deletions: usize,
    full: bool,
) -> serde_json::Value {
    if !full {
        return serde_json::json!({"additions": additions, "deletions": deletions});
    }
    // For large files, skip sending full content to avoid bloating JSON
    let size_limit = 512 * 1024; // 512KB
    if before.len() > size_limit || after.len() > size_limit {
        serde_json::json!({
            "additions": additions,
            "deletions": deletions,
            "truncated": true,
        })
    } else {
        serde_json::json!({
            "before": before,
            "after": after,
            "additions": additions,
            "deletions": deletions,
        })
    }
}
/// Snapshot the file into the backup store before mutation, scoped to a session.
///
/// Returns `Ok(Some(backup_id))` if the file existed and was backed up,
/// `Ok(None)` if the file doesn't exist (new file creation).
///
/// The `session` argument is the request-level session namespace (see
/// [`crate::protocol::RawRequest::session`]). Snapshots created by one session
/// are not visible from another, which is what keeps undo state isolated in
/// a shared-bridge setup (issue #14).
///
/// Drops the RefCell borrow before returning (D029).
pub fn auto_backup(
    ctx: &AppContext,
    session: &str,
    path: &Path,
    description: &str,
    op_id: Option<&str>,
) -> Result<Option<String>, AftError> {
    if std::fs::symlink_metadata(path).is_err() {
        return Ok(None);
    }
    let backup_id = {
        let mut store = ctx.backup().lock();
        store.snapshot_with_op(session, path, description, op_id)?
    }; // borrow dropped here
    Ok(backup_id)
}

/// Attach the stable reason when an automatic snapshot for this mutation was skipped.
pub fn attach_backup_skipped_reason(
    result: &mut serde_json::Value,
    ctx: &AppContext,
    session: &str,
    op_id: &str,
    path: Option<&Path>,
) {
    let reason = ctx
        .backup()
        .lock()
        .skipped_reason_for_operation(session, op_id, path);
    if let (Some(object), Some(reason)) = (result.as_object_mut(), reason) {
        object.insert(
            "backup_skipped_reason".to_string(),
            serde_json::Value::String(reason.as_str().to_string()),
        );
    }
}

/// Persist a regular-file capture that was already freshness-checked while
/// creating the operation's rollback checkpoint.
pub(crate) fn auto_backup_from_capture(
    ctx: &AppContext,
    session: &str,
    path: &Path,
    description: &str,
    op_id: Option<&str>,
    capture: &crate::backup::CapturedRegularFile,
) -> Result<Option<String>, AftError> {
    let backup_id = {
        let mut store = ctx.backup().lock();
        store.snapshot_with_op_from_capture(session, path, description, op_id, capture)?
    };
    Ok(backup_id)
}

/// Post-format excerpt of the region(s) the formatter reflowed, so the agent
/// can re-anchor its next edit on the real on-disk text instead of the text it
/// submitted. `None` on WriteResult when the formatter did not change the
/// applied edit (self-suppressing).
pub struct ReformattedExcerpt {
    /// Post-format text of the changed region(s), with ~2 lines of context,
    /// capped. Empty when `extensive` is true.
    pub text: String,
    /// True when the reflow exceeded the cap (whole-file reformat etc.) — too
    /// large to inline; the agent should re-read the file before re-anchoring.
    pub extensive: bool,
}

const REFORMATTED_EXCERPT_MAX_LINES: usize = 60;
const REFORMATTED_EXCERPT_MAX_BYTES: usize = 4096;

/// Compute a bounded post-format excerpt when `pre_format` (agent-applied edit)
/// differs from `post_format` (on-disk after formatting).
pub fn compute_reformatted_excerpt(
    pre_format: &str,
    post_format: &str,
) -> Option<ReformattedExcerpt> {
    if pre_format == post_format {
        return None;
    }

    use similar::DiffTag;

    let diff = construct_diff(pre_format, post_format);
    let post_lines: Vec<&str> = post_format.lines().collect();
    let mut collected: Vec<String> = Vec::new();
    let mut last_post_idx: Option<usize> = None;
    let mut byte_count = 0usize;
    let mut append_line = |collected: &mut Vec<String>, line: &str| {
        let next_bytes = byte_count.saturating_add(line.len()).saturating_add(1);
        if collected.len() >= REFORMATTED_EXCERPT_MAX_LINES
            || next_bytes > REFORMATTED_EXCERPT_MAX_BYTES
        {
            return false;
        }
        byte_count = next_bytes;
        #[cfg(test)]
        EXCERPT_COPIED_LINES.with(|count| count.set(count.get() + 1));
        collected.push(line.to_string());
        true
    };

    for group in diff.grouped_ops(2) {
        let mut group_start: Option<usize> = None;
        let mut group_end: Option<usize> = None;

        for op in group {
            let tag = op.tag();
            if tag == DiffTag::Delete {
                continue;
            }
            let new_range = op.new_range();
            if new_range.is_empty() {
                continue;
            }
            let start = new_range.start;
            let end = new_range.end.saturating_sub(1);
            group_start = Some(group_start.map_or(start, |s| s.min(start)));
            group_end = Some(group_end.map_or(end, |e| e.max(end)));
        }

        let (Some(start), Some(end)) = (group_start, group_end) else {
            continue;
        };

        if let Some(prev) = last_post_idx {
            if start > prev + 1 {
                if !append_line(&mut collected, "…") {
                    return Some(ReformattedExcerpt {
                        text: String::new(),
                        extensive: true,
                    });
                }
            }
        }

        for idx in start..=end {
            if idx < post_lines.len() {
                if !append_line(&mut collected, post_lines[idx]) {
                    return Some(ReformattedExcerpt {
                        text: String::new(),
                        extensive: true,
                    });
                }
            }
        }
        last_post_idx = Some(end);
    }

    Some(ReformattedExcerpt {
        text: collected.join("\n"),
        extensive: false,
    })
}

/// Result of the write → format → validate pipeline.
///
/// Returned by `write_format_validate` to give callers a single struct
/// with all post-write signals for the response JSON.
pub struct WriteResult {
    /// Whether tree-sitter syntax validation passed. `None` if unsupported language.
    pub syntax_valid: Option<bool>,
    /// 1-based line of the first parse error, set only when this call created
    /// the file and its confirmed syntax check failed. Such a file is kept on
    /// disk as written (there is nothing to roll back to), and the reply tells
    /// the agent where the error is.
    pub syntax_error_line: Option<usize>,
    /// Whether the file was auto-formatted.
    pub formatted: bool,
    /// Why formatting was skipped, if it was. Values: "unsupported_language",
    /// "no_formatter_configured", "formatter_not_installed", "formatter_excluded_path",
    /// "timeout", "error".
    pub format_skipped_reason: Option<String>,
    /// Whether full validation was requested (controls whether validation_errors is included in response).
    pub validate_requested: bool,
    /// Structured type-checker errors (only populated when validate:"full" is requested).
    pub validation_errors: Vec<format::ValidationError>,
    /// Why validation was skipped, if it was. Values: "unsupported_language",
    /// "no_checker_configured", "checker_not_installed", "timeout", "error".
    pub validate_skipped_reason: Option<String>,
    /// True when the write+format+validate pipeline detected post-write
    /// invalid syntax against a previously-valid file and restored the
    /// pre-write content. The on-disk file is the original; `syntax_valid`
    /// reports the would-have-been-written status (Some(false)).
    pub rolled_back: bool,
    /// Per-edit LSP diagnostics outcome (v0.17.3). Carries the verified-fresh
    /// diagnostics PLUS per-server status (pending/exited) so the response
    /// can report `complete: bool` honestly.
    ///
    /// `None` means the caller didn't request diagnostics OR the request
    /// was a fire-and-forget notify (no wait). `Some(outcome)` always
    /// reports diagnostics from servers that proved freshness against the
    /// post-edit document version.
    pub lsp_outcome: Option<crate::lsp::manager::PostEditWaitOutcome>,
    /// Post-format excerpt when the formatter reflowed the applied edit.
    pub reformatted_excerpt: Option<ReformattedExcerpt>,
    pub(crate) final_text: Option<ObservedFinalText>,
}

/// Render structured validation errors as a compact `line N: message` list for
/// an error message. Used by the refactor handlers when a write was rolled back.
pub fn format_validation_errors(errors: &[format::ValidationError]) -> String {
    errors
        .iter()
        .map(|e| format!("line {}: {}", e.line, e.message))
        .collect::<Vec<_>>()
        .join("; ")
}

impl WriteResult {
    /// Return the validated final text only while the live path still has the
    /// same identity. LSP notifications may wait, so response rendering checks
    /// again rather than assuming the file stayed unchanged during that wait.
    pub(crate) fn final_content(&mut self, path: &Path) -> Option<&str> {
        let current = crate::backup::DiskFileVersion::of_path(path);
        if !self
            .final_text
            .as_ref()
            .is_some_and(|observed| observed.version.is_some() && observed.version == current)
        {
            self.final_text = ObservedFinalText::read(path);
        }
        self.final_text
            .as_ref()
            .map(|observed| observed.text.as_str())
    }
    /// Append LSP diagnostics + per-server status to a response JSON
    /// object.
    ///
    /// v0.17.3 honest-reporting contract: when diagnostics were requested
    /// (`lsp_outcome.is_some()`), this ALWAYS emits `lsp_diagnostics: [...]`
    /// (even if empty) plus `lsp_complete: bool`, `lsp_pending_servers`,
    /// and `lsp_exited_servers`. Empty `lsp_diagnostics` no longer means
    /// "the field disappeared" — it means "we waited and got an explicit
    /// fresh-but-clean result, OR every expected server is in the pending/
    /// exited list (check `lsp_complete`)."
    ///
    /// When diagnostics were NOT requested (`lsp_outcome.is_none()`),
    /// nothing is added — keeps the no-LSP edit path's response shape
    /// unchanged.
    pub fn append_lsp_diagnostics_to(&self, result: &mut serde_json::Value) {
        result["rolled_back"] = serde_json::json!(self.rolled_back);

        let Some(outcome) = self.lsp_outcome.as_ref() else {
            return;
        };

        result["lsp_diagnostics"] = serde_json::json!(outcome
            .diagnostics
            .iter()
            .map(|d| {
                serde_json::json!({
                    "file": d.file.display().to_string(),
                    "line": d.line,
                    "column": d.column,
                    "end_line": d.end_line,
                    "end_column": d.end_column,
                    "severity": d.severity.as_str(),
                    "message": d.message,
                    "code": d.code,
                    "source": d.source,
                })
            })
            .collect::<Vec<_>>());

        result["lsp_complete"] = serde_json::Value::Bool(outcome.complete());

        if !outcome.pending_servers.is_empty() {
            result["lsp_status"] = serde_json::json!(if outcome.unresponsive_servers.is_empty() {
                "diagnostics unknown (no authoritative response within the diagnostics budget)"
            } else {
                "diagnostics unknown (server not responding)"
            });
            result["lsp_pending_servers"] = serde_json::json!(outcome
                .pending_servers
                .iter()
                .map(|key| key.kind.id_str().to_string())
                .collect::<Vec<_>>());
        }
        if !outcome.exited_servers.is_empty() {
            result["lsp_exited_servers"] = serde_json::json!(outcome
                .exited_servers
                .iter()
                .map(|key| key.kind.id_str().to_string())
                .collect::<Vec<_>>());
        }
    }

    /// Append post-format reflow excerpt when the formatter changed the applied edit.
    pub fn append_reformatted_excerpt_to(&self, result: &mut serde_json::Value) {
        if let Some(excerpt) = &self.reformatted_excerpt {
            if excerpt.extensive {
                result["reformatted"] = serde_json::json!({ "extensive": true });
            } else {
                result["reformatted"] = serde_json::json!({ "text": excerpt.text });
            }
        }
    }

    /// Record where a file created by this call fails to parse. Reply
    /// renderers read `syntax_error_line` to tell the agent the new file was
    /// written even though it does not parse.
    pub fn append_created_syntax_error_to(&self, result: &mut serde_json::Value) {
        if let Some(line) = self.syntax_error_line {
            result[CREATED_SYNTAX_ERROR_LINE_FIELD] = serde_json::json!(line);
        }
    }
}

/// Response field carrying the 1-based first error line of a created file that
/// does not parse. Present only for files the call created.
pub(crate) const CREATED_SYNTAX_ERROR_LINE_FIELD: &str = "syntax_error_line";

/// The reply line for a file this call created that does not parse. Every
/// reply path renders it through this function so the wording stays the same.
pub(crate) fn created_file_syntax_error_notice(first_error_line: u64) -> String {
    format!(
        "Syntax check: the new file does not parse (first error near line {first_error_line}); it was written as given."
    )
}

thread_local! {
    static FULL_VALIDATION_START_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `f` with a one-shot hook that fires on this thread when
/// [`write_format_validate`] has finished writing, formatting and
/// syntax-checking a file and is about to run the project type checker.
///
/// The standalone request loop runs edits on a worker thread under this hook
/// and waits for either the hook or the finished response. Everything before
/// the hook therefore keeps its inline ordering relative to other requests;
/// only the external checker wait (and the response assembly after it) runs
/// while the loop answers cheap requests such as `status`. The hook is
/// cleared when `f` returns, even by unwinding.
pub fn with_full_validation_start_hook<R>(hook: Box<dyn FnOnce()>, f: impl FnOnce() -> R) -> R {
    struct ClearHook;
    impl Drop for ClearHook {
        fn drop(&mut self) {
            let _ = FULL_VALIDATION_START_HOOK.try_with(|slot| slot.borrow_mut().take());
        }
    }

    FULL_VALIDATION_START_HOOK.with(|slot| *slot.borrow_mut() = Some(hook));
    let _clear = ClearHook;
    f()
}

/// Fire the hook installed by [`with_full_validation_start_hook`], if any.
/// Only the first checker run of a request fires it; later runs (multi-file
/// commands) find the slot empty.
fn notify_full_validation_start() {
    let hook = FULL_VALIDATION_START_HOOK
        .try_with(|slot| slot.borrow_mut().take())
        .ok()
        .flatten();
    if let Some(hook) = hook {
        hook();
    }
}

pub(crate) fn full_validation_requested(config: &Config, params: &serde_json::Value) -> bool {
    params
        .get("validate")
        .and_then(|value| value.as_str())
        .or(config.validate_on_edit.as_deref())
        == Some("full")
}

/// Validate the completed multi-file operation, never its intermediate writes.
/// Call only after all writes succeeded and rollback is no longer needed.
pub(crate) fn validate_written_files(
    paths: &[std::path::PathBuf],
    config: &Config,
    params: &serde_json::Value,
) -> Option<format::BatchValidation> {
    if paths.is_empty() || !full_validation_requested(config, params) {
        return None;
    }
    notify_full_validation_start();
    Some(format::validate_full_batch(paths, config))
}

/// Write content to disk, auto-format, then validate syntax.
///
/// This is the shared tail for all mutation commands. The pipeline order is:
/// 1. `fs::write` — persist content
/// 2. `auto_format` — run the project formatter (reads the written file, writes back)
/// 3. `validate_syntax` — parse the (potentially formatted) file
/// 4. `validate_full` — run type checker if requested by params or config
///
/// The `params` argument carries the original request parameters. When it
/// contains `"validate": "full"`, or config sets `validate_on_edit: "full"`,
/// the project's type checker is invoked after syntax validation and the
/// results are included in `WriteResult`.
pub fn write_format_validate(
    path: &Path,
    content: &str,
    config: &Config,
    params: &serde_json::Value,
) -> Result<WriteResult, AftError> {
    write_format_validate_impl(path, content, config, params, false, None, false)
}

/// Reuse the captured original only while its strong file identity still matches.
pub(crate) fn write_format_validate_observed(
    path: &Path,
    content: &str,
    config: &Config,
    params: &serde_json::Value,
    original: &str,
    observation: &EditObservation,
) -> Result<WriteResult, AftError> {
    let pre = observation
        .capture
        .as_ref()
        .and(observation.version)
        .filter(|version| Some(*version) == crate::backup::DiskFileVersion::of_path(path))
        .map(|_| original);
    write_format_validate_impl(path, content, config, params, false, pre, false)
}

/// Write, format and syntax-check now; the caller checks the completed project
/// with `validate_written_files` after every file has been written successfully.
pub(crate) fn write_format_validate_deferred(
    path: &Path,
    content: &str,
    config: &Config,
    params: &serde_json::Value,
) -> Result<WriteResult, AftError> {
    write_format_validate_impl(path, content, config, params, true, None, false)
}

/// Write and format a file that did not exist before the caller's
/// multi-step operation started, even if an earlier step of that operation
/// already wrote it. There is no earlier content to protect, so an invalid
/// result is never rolled back, and no syntax check runs here: the caller
/// checks the file's final content once with [`check_created_file_syntax`]
/// after its last step. `syntax_valid` and `syntax_error_line` are `None`.
pub(crate) fn write_format_deferred_created(
    path: &Path,
    content: &str,
    config: &Config,
    params: &serde_json::Value,
) -> Result<WriteResult, AftError> {
    write_format_validate_impl(path, content, config, params, true, None, true)
}

mod swift;

fn write_format_validate_impl(
    path: &Path,
    content: &str,
    config: &Config,
    params: &serde_json::Value,
    defer_full_validation: bool,
    captured_original: Option<&str>,
    syntax_checked_by_caller: bool,
) -> Result<WriteResult, AftError> {
    // A file this call creates has no prior content: it cannot be rolled back
    // to anything, so an invalid result is kept and reported through
    // `syntax_error_line` instead. With `syntax_checked_by_caller`, the file
    // was created earlier in the caller's own multi-step operation: it also
    // has no prior content to protect, and the caller checks its syntax.
    let existed = !syntax_checked_by_caller && (captured_original.is_some() || path.exists());
    let pre_write_content = if let Some(source) = captured_original {
        Some(std::borrow::Cow::Borrowed(source))
    } else if existed {
        #[cfg(test)]
        WRITE_PIPELINE_READS.with(|count| count.set(count.get() + 1));
        read_edit_text(path).ok().map(std::borrow::Cow::Owned)
    } else {
        None
    };
    // Existing clean files are protected from invalid mutations. New files have
    // no safe prior content to restore, so their pre-write validity remains None
    // and invalid syntax is reported without rollback.
    let mut was_syntax_valid = pre_write_content
        .as_deref()
        .and_then(|source| validate_captured_syntax(path, source));

    let _view_intent = crate::views::intent::record_paths([path]);
    // Step 1: Write
    atomic_write_source(path, content.as_bytes()).map_err(|e| AftError::InvalidRequest {
        message: format!("failed to write file: {}", e),
    })?;

    // Step 2: Format (before validate so we validate the formatted content)
    let (formatted, format_skipped_reason) = format::auto_format(path, config);

    // Step 3: Validate syntax
    #[cfg(test)]
    WRITE_PIPELINE_READS.with(|count| count.set(count.get() + 1));
    let mut final_text = ObservedFinalText::read(path);
    let mut syntax_error_line = None;
    let syntax_valid = final_text
        .as_ref()
        .map(|observed| observed.text.as_str())
        .filter(|_| !syntax_checked_by_caller)
        .and_then(|source| {
            if !existed {
                let check = check_created_file_syntax(path, source);
                syntax_error_line = check.first_error_line;
                return check.valid;
            }
            let syntax_valid = validate_captured_syntax(path, source);
            swift::confirm_if_needed(
                path,
                source,
                pre_write_content.as_deref(),
                &mut was_syntax_valid,
                syntax_valid,
            )
        });
    let rolled_back = if was_syntax_valid == Some(true) && syntax_valid == Some(false) {
        if let Some(original) = pre_write_content.as_ref() {
            atomic_write_source(path, original.as_bytes()).map_err(|e| {
                AftError::InvalidRequest {
                    message: format!("failed to roll back invalid edit: {}", e),
                }
            })?;
            true
        } else {
            false
        }
    } else {
        false
    };

    // Step 4: Full validation (type checker) — only when requested
    // Explicit param overrides config. Valid values: "syntax" | "full" | "off".
    let validate_requested = full_validation_requested(config, params) && !defer_full_validation;
    let (validation_errors, validate_skipped_reason) = if validate_requested {
        notify_full_validation_start();
        format::validate_full(path, config)
    } else {
        (Vec::new(), None)
    };

    if validate_requested {
        // Checkers can modify files, including a just-rolled-back file. Observe
        // their post-state before sharing text with response and LSP consumers.
        #[cfg(test)]
        WRITE_PIPELINE_READS.with(|count| count.set(count.get() + 1));
        final_text = ObservedFinalText::read(path);
    } else if rolled_back {
        final_text = ObservedFinalText::read(path);
    }
    let reformatted_excerpt = if rolled_back {
        None
    } else {
        final_text
            .as_ref()
            .and_then(|observed| compute_reformatted_excerpt(content, &observed.text))
    };

    Ok(WriteResult {
        syntax_valid,
        syntax_error_line,
        formatted,
        format_skipped_reason,
        validate_requested,
        validation_errors,
        validate_skipped_reason,
        rolled_back,
        lsp_outcome: None,
        reformatted_excerpt,
        final_text,
    })
}

fn validate_captured_syntax(path: &Path, source: &str) -> Option<bool> {
    parse_captured_syntax(path, source).map(|tree| !tree.root_node().has_error())
}

fn parse_captured_syntax(path: &Path, source: &str) -> Option<tree_sitter::Tree> {
    let lang = detect_language(path)?;
    #[cfg(test)]
    A03_SYNTAX_PARSES.with(|count| count.set(count.get() + 1));
    crate::parser::parse_source_with_cached_parser(path, source, lang).ok()
}

/// Syntax verdict for a file the current call created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CreatedFileSyntax {
    /// `None` when the language has no parser.
    pub valid: Option<bool>,
    /// 1-based line of the first parse error. Set only when `valid` is
    /// `Some(false)` after any compiler confirmation.
    pub first_error_line: Option<usize>,
}

/// Check the syntax of a file that did not exist before this call.
///
/// A new file cannot be rolled back to earlier content, so an invalid result
/// is kept on disk and reported to the agent instead. The error line comes
/// from the same tree-sitter parse that produced the verdict. Swift files that
/// tree-sitter rejects are confirmed with `swiftc -parse` first, so a file the
/// compiler accepts counts as valid and carries no error line.
pub(crate) fn check_created_file_syntax(path: &Path, source: &str) -> CreatedFileSyntax {
    let Some(tree) = parse_captured_syntax(path, source) else {
        return CreatedFileSyntax {
            valid: None,
            first_error_line: None,
        };
    };
    let root = tree.root_node();
    let valid = swift::confirm_created(path, source, Some(!root.has_error()));
    let first_error_line = if valid == Some(false) {
        first_syntax_error_line(root)
    } else {
        None
    };
    CreatedFileSyntax {
        valid,
        first_error_line,
    }
}

/// 1-based start line of the first ERROR or MISSING node in document order.
///
/// tree-sitter sets `has_error` on a node exactly when its subtree contains an
/// ERROR or MISSING node, so the walk only descends into subtrees that carry
/// one and returns `None` only for a tree without errors. It is iterative
/// because deeply nested syntax trees could overflow a recursive walk.
pub(crate) fn first_syntax_error_line(root: tree_sitter::Node<'_>) -> Option<usize> {
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        if node.is_error() || node.is_missing() {
            return Some(node.start_position().row + 1);
        }
        if node.has_error() && cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return None;
            }
        }
    }
}

#[cfg(test)]
thread_local! {
    pub(crate) static LINE_OFFSET_BYTES_SCANNED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static EXCERPT_COPIED_LINES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static WRITE_PIPELINE_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static A03_DIFF_CONSTRUCTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static A03_LINE_INDEX_PASSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static A03_HANDLER_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static A03_SYNTAX_PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    #[test]
    fn swift_parser_accepts_tuple_metatype() {
        let Some(()) = swift::with_native_swiftc(|| {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("tuple.swift");
            let original = "let value = 1\n";
            let updated = "_ = (Int, Result<String, Error>).self\n";
            std::fs::write(&path, original).unwrap();
            assert_eq!(validate_syntax_str(updated, &path), Some(false));
            let config = Config {
                format_on_edit: false,
                ..Config::default()
            };
            let mut result =
                write_format_validate(&path, updated, &config, &serde_json::json!({})).unwrap();
            assert!(!result.rolled_back);
            assert_eq!(result.syntax_valid, Some(true));
            assert_eq!(result.final_content(&path), Some(updated));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), updated);
        }) else {
            eprintln!("skipping native Swift parser test: swiftc is unavailable");
            return;
        };
    }

    use super::*;

    #[test]
    fn created_file_syntax_reports_first_error_line() {
        let path = Path::new("new.ts");
        // ERROR node on line 3 (later lines are broken too).
        let check = check_created_file_syntax(
            path,
            "const a = 1;\nconst b = 2;\nfunction broken( {{\nconst c = ;\n",
        );
        assert_eq!(check.valid, Some(false));
        assert_eq!(check.first_error_line, Some(3));
        // A MISSING node (unclosed call) also counts.
        let missing = check_created_file_syntax(path, "const a = 1;\nfoo(1\n");
        assert_eq!(missing.valid, Some(false));
        assert!(missing.first_error_line.is_some_and(|line| line >= 2));
        let valid = check_created_file_syntax(path, "const a = 1;\n");
        assert_eq!(valid.valid, Some(true));
        assert_eq!(valid.first_error_line, None);
        let unsupported = check_created_file_syntax(Path::new("notes.unknownext"), "{{");
        assert_eq!(unsupported.valid, None);
        assert_eq!(unsupported.first_error_line, None);
    }

    #[test]
    fn created_file_pipeline_keeps_invalid_new_file_and_reports_line() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            format_on_edit: false,
            ..Config::default()
        };
        let path = temp.path().join("new.ts");
        let broken = "const a = 1;\n\nfunction {\n";
        let result = write_format_validate(&path, broken, &config, &serde_json::json!({})).unwrap();
        assert!(!result.rolled_back);
        assert_eq!(result.syntax_valid, Some(false));
        assert_eq!(result.syntax_error_line, Some(3));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);

        // The same content written over an existing clean file rolls back and
        // carries no created-file line.
        let existing = temp.path().join("existing.ts");
        std::fs::write(&existing, "const a = 1;\n").unwrap();
        let result =
            write_format_validate(&existing, broken, &config, &serde_json::json!({})).unwrap();
        assert!(result.rolled_back);
        assert_eq!(result.syntax_error_line, None);
    }
    #[test]
    fn a03_diff_construction_bound_and_output_goldens() {
        let cases = [
            (String::new(), "é\r\n".to_owned()),
            ("same\n".to_owned(), "same\n".to_owned()),
            ("one\r\ntwo".to_owned(), "one\r\n三\n".to_owned()),
            (
                (0..2_000).map(|i| format!("before-{i:05}\n")).collect(),
                (0..2_000).map(|i| format!("after--{i:05}\n")).collect(),
            ),
            (
                (0..2_000).map(|i| format!("{i:05}\n")).collect(),
                (0..2_000).rev().map(|i| format!("{i:05}\n")).collect(),
            ),
            ("a\n".repeat(300_000), "a\n".repeat(300_000) + "new\n"),
        ];
        for (before, after) in cases {
            let baseline = similar::TextDiff::from_lines(&before, &after);
            let patch = if before == after {
                "Index: golden.ts\n===================================================================\n--- golden.ts\n+++ golden.ts\n".to_owned()
            } else {
                format!("Index: golden.ts\n===================================================================\n{}", baseline.unified_diff().header("golden.ts", "golden.ts"))
            };
            let additions = baseline
                .iter_all_changes()
                .filter(|c| c.tag() == similar::ChangeTag::Insert)
                .count();
            let deletions = baseline
                .iter_all_changes()
                .filter(|c| c.tag() == similar::ChangeTag::Delete)
                .count();
            for full in [false, true] {
                A03_DIFF_CONSTRUCTIONS.with(|counter| counter.set(0));
                let params = serde_json::json!({"include_diff_content": full});
                let mut actual = serde_json::json!({});
                attach_preview_diff(&mut actual, &params, "golden.ts", &before, &after);
                let info = if !full {
                    serde_json::json!({"additions": additions, "deletions": deletions})
                } else if before.len() > 512 * 1024 || after.len() > 512 * 1024 {
                    serde_json::json!({"additions": additions, "deletions": deletions, "truncated": true})
                } else {
                    serde_json::json!({"additions": additions, "deletions": deletions, "before": before, "after": after})
                };
                assert_eq!(
                    actual,
                    serde_json::json!({"metadata": {"diff": patch}, "preview": true, "diff": info, "preview_diff": patch})
                );
                let constructions = A03_DIFF_CONSTRUCTIONS.with(|counter| counter.get());
                eprintln!(
                    "A03 preview diff bytes={}/{}: constructions={constructions}",
                    before.len(),
                    after.len()
                );
                assert_eq!(
                    constructions, 1,
                    "one construction per preview before/after pair"
                );
                A03_DIFF_CONSTRUCTIONS.with(|counter| counter.set(0));
                let mut applied = serde_json::json!({});
                let apply_params =
                    serde_json::json!({"include_diff": true, "include_diff_content": full});
                attach_edit_diff(&mut applied, &apply_params, "golden.ts", &before, &after);
                assert_eq!(
                    applied,
                    serde_json::json!({"metadata": {"diff": patch}, "diff": info})
                );
                assert_eq!(
                    A03_DIFF_CONSTRUCTIONS.with(|counter| counter.get()),
                    1,
                    "one construction per apply pair"
                );
            }
        }
    }

    #[test]
    fn a03_stale_capture_backup_preserves_live_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("capture.txt");
        let ctx = AppContext::new(Box::new(crate::language::StubProvider), Config::default());
        std::fs::write(&path, "original\n").unwrap();
        let (_, observation) = read_edit_source(&path).unwrap();
        let replacement = temp.path().join("replacement.txt");
        std::fs::write(&replacement, "external\n").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        auto_backup_observed(
            &ctx,
            "freshness",
            &path,
            "before edit",
            Some("freshness-op"),
            &observation,
        )
        .unwrap();
        assert_eq!(
            ctx.backup().lock().history("freshness", &path)[0]
                .content_bytes
                .as_ref(),
            b"external\n"
        );
    }

    #[test]
    fn a03_buffer_identity_rollback_and_utf8_parity() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("edge.ts");
        std::fs::write(&path, "const value = 1;\n").unwrap();
        let (source, observation) = read_edit_source(&path).unwrap();
        let mut result = write_format_validate_observed(
            &path,
            "function {",
            &Config::default(),
            &serde_json::json!({}),
            &source,
            &observation,
        )
        .unwrap();
        assert!(result.rolled_back);
        assert_eq!(result.syntax_valid, Some(false));
        assert_eq!(result.final_content(&path), Some("const value = 1;\n"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        std::fs::write(&path, "const external = 8;\n").unwrap();
        assert_eq!(result.final_content(&path), Some("const external = 8;\n"));
        // A stale valid source must not cause rollback against a now-invalid file.
        std::fs::write(&path, "function { external-invalid").unwrap();
        let mut result = write_format_validate_observed(
            &path,
            "function { new-invalid",
            &Config::default(),
            &serde_json::json!({}),
            &source,
            &observation,
        )
        .unwrap();
        assert!(!result.rolled_back);
        assert_eq!(result.final_content(&path), Some("function { new-invalid"));
        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        assert_eq!(
            read_edit_source(&path).err().unwrap().to_string(),
            std::fs::read_to_string(&path).err().unwrap().to_string()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a03_formatter_checker_final_buffer_parity() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("edge.ts");
        let bin = root.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(root.path().join(".oxfmtrc.json"), "{}").unwrap();
        std::fs::write(root.path().join("tsconfig.json"), "{}").unwrap();
        for (tool, value) in [("oxfmt", 3), ("tsc", 4)] {
            let tool_path = bin.join(tool);
            std::fs::write(&tool_path, format!("#!/bin/sh\nif [ \"$1\" = '--version' ]; then echo '{tool} 1.0'; exit 0; fi\nprintf 'const value = {value};\\n' > '{}'\n", path.display())).unwrap();
            std::fs::set_permissions(&tool_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let config = Config {
            project_root: Some(root.path().to_path_buf()),
            format_on_edit: true,
            formatter: [("typescript".into(), "oxfmt".into())].into(),
            checker: [("typescript".into(), "tsc".into())].into(),
            ..Config::default()
        };
        for full in [false, true] {
            std::fs::write(&path, "const value = 1;\n").unwrap();
            let (original, observation) = read_edit_source(&path).unwrap();
            A03_HANDLER_READS.with(|count| count.set(0));
            A03_SYNTAX_PARSES.with(|count| count.set(0));
            let params = serde_json::json!({"validate": if full {"full"} else {"syntax"}, "include_diff_content": true});
            let mut result = write_format_validate_observed(
                &path,
                "const value = 2;\n",
                &config,
                &params,
                &original,
                &observation,
            )
            .unwrap();
            let final_text = if full {
                "const value = 4;\n"
            } else {
                "const value = 3;\n"
            };
            assert!(result.formatted, "{:?}", result.format_skipped_reason);
            assert!(result.validation_errors.is_empty());
            assert_eq!(result.syntax_valid, Some(true));
            assert_eq!(
                result.reformatted_excerpt.as_ref().unwrap().text,
                final_text.trim_end()
            );
            assert_eq!(result.final_content(&path), Some(final_text));
            let mut response = serde_json::json!({});
            attach_edit_diff(
                &mut response,
                &params,
                "edge.ts",
                &original,
                result.final_content(&path).unwrap(),
            );
            assert_eq!(
                response["diff"],
                serde_json::json!({"before": original, "after": final_text, "additions": 1, "deletions": 1})
            );
            assert_eq!(response["metadata"]["diff"], format!("Index: edge.ts\n===================================================================\n--- edge.ts\n+++ edge.ts\n@@ -1 +1 @@\n-const value = 1;\n+{}", final_text));
            assert_eq!(
                A03_HANDLER_READS.with(|count| count.get()),
                if full { 2 } else { 1 }
            );
            assert_eq!(A03_SYNTAX_PARSES.with(|count| count.get()), 2);

            std::fs::write(&path, "const value = 1;\n").unwrap();
            crate::backup::reset_capture_read_count(&path);
            A03_HANDLER_READS.with(|count| count.set(0));
            A03_SYNTAX_PARSES.with(|count| count.set(0));
            let ctx = AppContext::new(Box::new(crate::language::StubProvider), config.clone());
            let req: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
                "id": "formatter-checker", "command": "batch", "file": path,
                "edits": [{"match": "1", "replacement": "2"}],
                "validate": if full {"full"} else {"syntax"}, "include_diff_content": true
            }))
            .unwrap();
            let response = crate::commands::batch::handle_batch(&req, &ctx);
            assert!(response.success, "{:?}", response);
            assert_eq!(response.data["diff"]["after"], final_text);
            assert_eq!(
                response.data["metadata"]["diff"],
                build_unified_diff(path.to_str().unwrap(), "const value = 1;\n", final_text)
            );
            let reads = A03_HANDLER_READS.with(|count| count.get())
                + crate::backup::capture_read_count(&path);
            assert_eq!(
                reads,
                if full { 3 } else { 2 },
                "handler-wide formatter/checker content reads"
            );
            assert_eq!(A03_SYNTAX_PARSES.with(|count| count.get()), 2);
        }
    }

    // --- line_col_to_byte ---

    #[test]
    fn line_col_to_byte_empty_string() {
        assert_eq!(line_col_to_byte("", 0, 0), 0);
    }

    #[test]
    fn write_pipeline_reads_each_syntax_buffer_once() {
        let files = tempfile::tempdir().unwrap();
        let path = files.path().join("large.ts");
        let original = "const value = 1;\n".repeat(20_000);
        std::fs::write(&path, &original).unwrap();
        let updated = "const value = 2;\n".repeat(20_000);
        let config = Config {
            format_on_edit: false,
            ..Config::default()
        };
        WRITE_PIPELINE_READS.with(|count| count.set(0));
        crate::parser::work_counters::TREE_SOURCE_READS.with(|count| count.set(0));
        let result =
            write_format_validate(&path, &updated, &config, &serde_json::json!({})).unwrap();
        assert_eq!(result.syntax_valid, Some(true));
        assert!(!result.rolled_back);
        assert!(result.reformatted_excerpt.is_none());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), updated);
        let reads = WRITE_PIPELINE_READS.with(|count| count.get())
            + crate::parser::work_counters::TREE_SOURCE_READS.with(|count| count.get());
        assert_eq!(reads, 2, "shared write pipeline content reads");
        let invalid =
            write_format_validate(&path, "function {", &config, &serde_json::json!({})).unwrap();
        assert_eq!(invalid.syntax_valid, Some(false));
        assert!(invalid.rolled_back);
        assert!(invalid.reformatted_excerpt.is_none());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), updated);
    }

    #[test]
    fn swift_write_pipeline_accepts_try_cast_with_dictionary_default() {
        let files = tempfile::tempdir().unwrap();
        let path = files.path().join("Transcript.swift");
        std::fs::write(
            &path,
            "import Foundation\nfunc decode(data: Data) throws {\n\
             let page = AgentTranscriptDecoder.decode(data)\n_ = page\n}\n",
        )
        .unwrap();
        let updated = "import Foundation
func decode(data: Data) throws {
    let json = try JSONSerialization.jsonObject(with: data) as? [String: Any] ?? [:]
    let page = AgentTranscriptDecoder.decode(json)
    _ = page
}
";
        let config = Config {
            format_on_edit: false,
            ..Config::default()
        };

        let result =
            write_format_validate(&path, updated, &config, &serde_json::json!({})).unwrap();

        assert_eq!(result.syntax_valid, Some(true));
        assert!(!result.rolled_back);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), updated);
    }

    #[test]
    fn swift_write_pipeline_accepts_try_await_optional_binding() {
        let files = tempfile::tempdir().unwrap();
        let path = files.path().join("ViewModel.swift");
        std::fs::write(&path, "class ViewModel {\n}\n").unwrap();
        let updated = "class ViewModel {
    func probe() async {
        if let fresh = try? await fleet.fetch() {
            rows = fresh.rows
        }
    }
}
";
        let config = Config {
            format_on_edit: false,
            ..Config::default()
        };

        let result =
            write_format_validate(&path, updated, &config, &serde_json::json!({})).unwrap();

        assert_eq!(result.syntax_valid, Some(true));
        assert!(!result.rolled_back);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), updated);
    }

    #[test]
    fn line_col_to_byte_single_line() {
        let source = "hello";
        assert_eq!(line_col_to_byte(source, 0, 0), 0);
        assert_eq!(line_col_to_byte(source, 0, 3), 3);
        assert_eq!(line_col_to_byte(source, 0, 5), 5); // end of line
    }

    #[test]
    fn line_col_to_byte_multi_line() {
        let source = "abc\ndef\nghi\n";
        // line 0: "abc" at bytes 0..3, newline at 3
        assert_eq!(line_col_to_byte(source, 0, 0), 0);
        assert_eq!(line_col_to_byte(source, 0, 2), 2);
        // line 1: "def" at bytes 4..7, newline at 7
        assert_eq!(line_col_to_byte(source, 1, 0), 4);
        assert_eq!(line_col_to_byte(source, 1, 3), 7);
        // line 2: "ghi" at bytes 8..11, newline at 11
        assert_eq!(line_col_to_byte(source, 2, 0), 8);
        assert_eq!(line_col_to_byte(source, 2, 2), 10);
    }

    #[test]
    fn line_col_to_byte_last_line_no_trailing_newline() {
        let source = "abc\ndef";
        // line 1: "def" at bytes 4..7, no trailing newline
        assert_eq!(line_col_to_byte(source, 1, 0), 4);
        assert_eq!(line_col_to_byte(source, 1, 3), 7); // end
    }

    #[test]
    fn line_col_to_byte_multi_byte_utf8() {
        // "é" is 2 bytes in UTF-8
        let source = "café\nbar";
        // line 0: "café" is 5 bytes (c=1, a=1, f=1, é=2)
        assert_eq!(line_col_to_byte(source, 0, 0), 0);
        assert_eq!(line_col_to_byte(source, 0, 5), 5); // end of "café"
                                                       // line 1: "bar" starts at byte 6
        assert_eq!(line_col_to_byte(source, 1, 0), 6);
        assert_eq!(line_col_to_byte(source, 1, 2), 8);
    }

    #[test]
    fn line_col_to_byte_beyond_end() {
        let source = "abc";
        // Line beyond file returns source.len()
        assert_eq!(line_col_to_byte(source, 5, 0), source.len());
    }

    #[test]
    fn line_col_to_byte_col_clamped_to_line_length() {
        let source = "ab\ncd";
        // col=10 on a 2-char line should clamp to 2
        assert_eq!(line_col_to_byte(source, 0, 10), 2);
    }

    #[test]
    fn line_col_to_byte_treats_bare_cr_as_content() {
        let source = "alpha\rbeta\ngamma\n";
        let offsets = [
            line_col_to_byte(source, 0, 0),
            line_col_to_byte(source, 1, 0),
            line_col_to_byte(source, 2, 0),
        ];
        assert_eq!(offsets, [0, 11, 17]);
    }

    #[test]
    fn line_col_to_byte_crlf() {
        let source = "abc\r\ndef\r\nghi\r\n";
        assert_eq!(line_col_to_byte(source, 0, 0), 0);
        assert_eq!(line_col_to_byte(source, 0, 10), 3);
        assert_eq!(line_col_to_byte(source, 1, 0), 5);
        assert_eq!(line_col_to_byte(source, 1, 3), 8);
        assert_eq!(line_col_to_byte(source, 2, 0), 10);
    }

    // --- replace_byte_range ---

    #[test]
    fn replace_byte_range_basic() {
        let source = "hello world";
        let result = replace_byte_range(source, 6, 11, "rust").unwrap();
        assert_eq!(result, "hello rust");
    }

    #[test]
    fn replace_byte_range_delete() {
        let source = "hello world";
        let result = replace_byte_range(source, 5, 11, "").unwrap();
        assert_eq!(result, "hello");
    }

    #[test]
    fn replace_byte_range_insert_at_same_position() {
        let source = "helloworld";
        let result = replace_byte_range(source, 5, 5, " ").unwrap();
        assert_eq!(result, "hello world");
    }

    #[test]
    fn replace_byte_range_replace_entire_string() {
        let source = "old content";
        let result = replace_byte_range(source, 0, source.len(), "new content").unwrap();
        assert_eq!(result, "new content");
    }

    #[test]
    fn compute_reformatted_excerpt_self_suppresses_when_unchanged() {
        let s = "fn main() {\n    let x = 1;\n}\n";
        assert!(compute_reformatted_excerpt(s, s).is_none());
    }

    #[test]
    fn compute_reformatted_excerpt_includes_post_format_text() {
        let before = "fn  main( ){  let   x=1;  }";
        let after = "fn main() {\n    let x = 1;\n}\n";
        let excerpt = compute_reformatted_excerpt(before, after).expect("should diff");
        assert!(!excerpt.extensive);
        assert!(excerpt.text.contains("fn main()"));
        assert!(excerpt.text.contains("let x = 1"));
    }

    #[test]
    fn compute_reformatted_excerpt_extensive_when_over_line_cap() {
        let before: String = (0..80).map(|i| format!("line{i} ugly\n")).collect();
        let after: String = (0..80).map(|i| format!("line{i} neat\n")).collect();
        let excerpt = compute_reformatted_excerpt(&before, &after).expect("should diff");
        assert!(excerpt.extensive);
        assert!(excerpt.text.is_empty());
    }

    // --- validate_syntax_str: `&raw` must not be a false syntax error ---

    /// `&raw` where `raw` is an ordinary variable is valid Rust (a reference to
    /// the binding `raw`). `raw` is only a contextual keyword in the raw-borrow
    /// operators `&raw const` / `&raw mut`. tree-sitter-rust before 0.24.2
    /// mis-parsed a bare `&raw` as the start of a raw-borrow and emitted an
    /// ERROR node, so `validate_syntax_str` returned `Some(false)` and the edit
    /// pipeline rolled back a correct edit. This pins the fixed grammar
    /// behavior: a grammar downgrade that reintroduces the false positive fails
    /// here instead of silently discarding users' edits.
    #[test]
    fn validate_syntax_str_accepts_reference_to_variable_named_raw() {
        let path = Path::new("lib.rs");
        let src = "fn handle_hash(x: &u32) -> u32 { *x }\n\
                   fn main() {\n    let raw = 5u32;\n    let _ = handle_hash(&raw);\n}\n";
        assert_eq!(validate_syntax_str(src, path), Some(true));
    }

    /// The genuine raw-borrow operators must still parse cleanly (guard against
    /// a "fix" that loosens the grammar the wrong way).
    #[test]
    fn validate_syntax_str_accepts_raw_borrow_operators() {
        let path = Path::new("lib.rs");
        let const_borrow = "fn main() {\n    let x = 5u32;\n    let _p = &raw const x;\n}\n";
        let mut_borrow = "fn main() {\n    let mut x = 5u32;\n    let _p = &raw mut x;\n}\n";
        assert_eq!(validate_syntax_str(const_borrow, path), Some(true));
        assert_eq!(validate_syntax_str(mut_borrow, path), Some(true));
    }
}

#[cfg(test)]
mod audit11_regressions {
    use super::*;

    #[test]
    fn excerpt_caps_bound_owned_line_construction() {
        EXCERPT_COPIED_LINES.with(|count| count.set(0));
        let before = "old\n".repeat(80);
        let after = "new\n".repeat(80);
        let excerpt = compute_reformatted_excerpt(&before, &after).unwrap();
        assert!(excerpt.extensive);
        assert!(excerpt.text.is_empty());
        assert!(EXCERPT_COPIED_LINES.with(|count| count.get()) <= REFORMATTED_EXCERPT_MAX_LINES);
    }

    #[test]
    fn atomic_source_write_failure_preserves_every_original_byte() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        let original = b"\xef\xbb\xbfexisting\r\n\xfflast";
        std::fs::write(&path, original).unwrap();
        let result = atomic_write_source_with(&path, b"new\r\n", |file, _| {
            use std::io::Write;
            file.write_all(b"partial")?;
            Err(std::io::Error::other("injected write failure"))
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn syntax_validation_reuses_one_loaded_grammar() {
        crate::parser::work_counters::GRAMMAR_LOADS.with(|count| count.set(0));
        assert_eq!(
            validate_syntax_str("let x = 1;", Path::new("a.ts")),
            Some(true)
        );
        assert_eq!(
            validate_syntax_str("function {", Path::new("a.ts")),
            Some(false)
        );
        assert_eq!(crate::parser::work_counters::grammar_loads(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn edit_and_syntax_rollback_replace_instead_of_truncating() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.ts");
        let original = b"\xef\xbb\xbflet x = 1;\r\n";
        std::fs::write(&path, original).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o751)).unwrap();
        let held_original = std::fs::File::open(&path).unwrap();
        let inode = held_original.metadata().unwrap().ino();
        let config = Config {
            format_on_edit: false,
            ..Default::default()
        };
        let updated = "\u{feff}let x = 2;\r\n";
        let result =
            write_format_validate(&path, updated, &config, &serde_json::json!({})).unwrap();
        assert!(!result.rolled_back);
        assert_eq!(std::fs::read(&path).unwrap(), updated.as_bytes());
        assert_ne!(std::fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o751
        );
        let held_updated = std::fs::File::open(&path).unwrap();
        let inode = held_updated.metadata().unwrap().ino();
        let result =
            write_format_validate(&path, "function {", &config, &serde_json::json!({})).unwrap();
        assert!(result.rolled_back);
        assert_ne!(std::fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(std::fs::read(&path).unwrap(), updated.as_bytes());
    }
}

/// Source writes must never change who can read or reach a file: ACLs,
/// extended attributes, hard links, owner and group survive every edit.
#[cfg(all(test, unix))]
mod source_write_access_tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn plain_config() -> Config {
        Config {
            format_on_edit: false,
            ..Default::default()
        }
    }

    /// A POSIX ACL in the kernel's extended-attribute encoding: owner rw-,
    /// one named user r--, owning group ---, mask r--, other ---.
    #[cfg(target_os = "linux")]
    fn restrictive_acl(named_user: u32) -> Vec<u8> {
        const UNDEFINED_ID: u32 = u32::MAX;
        let mut blob = 2u32.to_le_bytes().to_vec();
        for (tag, perm, id) in [
            (0x01u16, 6u16, UNDEFINED_ID),
            (0x02, 4, named_user),
            (0x04, 0, UNDEFINED_ID),
            (0x10, 4, UNDEFINED_ID),
            (0x20, 0, UNDEFINED_ID),
        ] {
            blob.extend_from_slice(&tag.to_le_bytes());
            blob.extend_from_slice(&perm.to_le_bytes());
            blob.extend_from_slice(&id.to_le_bytes());
        }
        blob
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn edit_keeps_posix_acl_and_does_not_widen_group_access() {
        use std::ffi::CString;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.txt");
        std::fs::write(&path, "old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let name = CString::new("system.posix_acl_access").unwrap();
        let c_path = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: valid NUL-terminated strings and a live byte slice.
        let blob = restrictive_acl(unsafe { libc::geteuid() } + 1);
        let set = unsafe {
            libc::setxattr(
                c_path.as_ptr(),
                name.as_ptr(),
                blob.as_ptr().cast(),
                blob.len(),
                0,
            )
        };
        assert_eq!(
            set,
            0,
            "the test file system must support POSIX ACLs: {}",
            std::io::Error::last_os_error()
        );
        let acl_of = |path: &Path| {
            let file = std::fs::File::open(path).unwrap();
            source_replacement::xattrs::snapshot(&file)
                .unwrap()
                .get(name.as_c_str())
                .cloned()
        };
        let before = acl_of(&path).expect("ACL stored");

        write_format_validate(&path, "new\n", &plain_config(), &serde_json::json!({})).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new\n");
        assert_eq!(acl_of(&path).as_ref(), Some(&before));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );

        // The copied ACL lets the atomic replacement proceed.
        let inode = std::fs::metadata(&path).unwrap().ino();
        assert_eq!(
            atomic_write_source(&path, b"third\n").unwrap(),
            SourceWriteMode::Replaced
        );
        assert_ne!(std::fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(acl_of(&path).as_ref(), Some(&before));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn edit_keeps_macos_acl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.txt");
        std::fs::write(&path, "old\n").unwrap();
        let status = std::process::Command::new("/bin/chmod")
            .args(["+a", "user:nobody deny read"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let acl_of = |path: &Path| {
            let output = std::process::Command::new("/bin/ls")
                .arg("-le")
                .arg(path)
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .skip(1)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        let before = acl_of(&path);
        assert!(
            before.iter().any(|line| line.contains("nobody deny read")),
            "{before:?}"
        );
        write_format_validate(&path, "new\n", &plain_config(), &serde_json::json!({})).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new\n");
        assert_eq!(acl_of(&path), before);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn edit_does_not_add_an_acl_inherited_from_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("inheriting");
        std::fs::create_dir(&folder).unwrap();
        let path = folder.join("plain.txt");
        // Created before the directory gains an inheritable entry, so the
        // original has no ACL of its own.
        std::fs::write(&path, "old\n").unwrap();
        let status = std::process::Command::new("/bin/chmod")
            .args(["+a", "group:everyone allow read,file_inherit"])
            .arg(&folder)
            .status()
            .unwrap();
        assert!(status.success());
        let acl_of = |path: &Path| {
            source_replacement::xattrs::extended_acl_text(&std::fs::File::open(path).unwrap())
                .unwrap()
        };
        let probe = folder.join("probe.txt");
        std::fs::write(&probe, "").unwrap();
        assert!(
            acl_of(&probe).is_some(),
            "a new file in the folder must inherit an ACL for this test to mean anything"
        );
        assert_eq!(acl_of(&path), None);

        write_format_validate(&path, "new\n", &plain_config(), &serde_json::json!({})).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new\n");
        assert_eq!(acl_of(&path), None);
        let mode = atomic_write_source(&path, b"third\n").unwrap();
        eprintln!("inherited-ACL directory write took {mode:?}");
        assert_eq!(std::fs::read(&path).unwrap(), b"third\n");
        assert_eq!(acl_of(&path), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn edit_does_not_add_an_acl_inherited_from_the_default_acl() {
        use std::ffi::CString;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("inheriting");
        std::fs::create_dir(&folder).unwrap();
        let path = folder.join("plain.txt");
        // Created before the directory gains a default ACL, so the original
        // has no access ACL of its own.
        std::fs::write(&path, "old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        // Default ACL u::rwx, u:<other>:r, g::rwx, m::rwx, o::r. The named
        // entry makes new files store a real access ACL instead of folding
        // a minimal one into their mode bits.
        const UNDEFINED_ID: u32 = u32::MAX;
        // SAFETY: geteuid has no preconditions.
        let other_user = unsafe { libc::geteuid() } + 1;
        let mut blob = 2u32.to_le_bytes().to_vec();
        for (tag, perm, id) in [
            (0x01u16, 7u16, UNDEFINED_ID),
            (0x02, 4, other_user),
            (0x04, 7, UNDEFINED_ID),
            (0x10, 7, UNDEFINED_ID),
            (0x20, 4, UNDEFINED_ID),
        ] {
            blob.extend_from_slice(&tag.to_le_bytes());
            blob.extend_from_slice(&perm.to_le_bytes());
            blob.extend_from_slice(&id.to_le_bytes());
        }
        let default_name = CString::new("system.posix_acl_default").unwrap();
        let c_folder = CString::new(folder.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: valid NUL-terminated strings and a live byte slice.
        let set = unsafe {
            libc::setxattr(
                c_folder.as_ptr(),
                default_name.as_ptr(),
                blob.as_ptr().cast(),
                blob.len(),
                0,
            )
        };
        assert_eq!(
            set,
            0,
            "the test file system must support default ACLs: {}",
            std::io::Error::last_os_error()
        );
        let access_name = CString::new("system.posix_acl_access").unwrap();
        let acl_of = |path: &Path| {
            let file = std::fs::File::open(path).unwrap();
            source_replacement::xattrs::snapshot(&file)
                .unwrap()
                .get(access_name.as_c_str())
                .cloned()
        };
        let probe = folder.join("probe.txt");
        std::fs::write(&probe, "").unwrap();
        assert!(
            acl_of(&probe).is_some(),
            "a new file in the folder must inherit an access ACL for this test to mean anything"
        );
        assert_eq!(acl_of(&path), None);

        write_format_validate(&path, "new\n", &plain_config(), &serde_json::json!({})).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new\n");
        assert_eq!(acl_of(&path), None);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let mode = atomic_write_source(&path, b"third\n").unwrap();
        eprintln!("inherited default-ACL write took {mode:?}");
        assert_eq!(std::fs::read(&path).unwrap(), b"third\n");
        assert_eq!(acl_of(&path), None);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn edit_keeps_hard_links_joined() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        let other_name = dir.path().join("b.txt");
        std::fs::write(&path, "old\n").unwrap();
        std::fs::hard_link(&path, &other_name).unwrap();
        let inode = std::fs::metadata(&path).unwrap().ino();
        write_format_validate(&path, "new\n", &plain_config(), &serde_json::json!({})).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new\n");
        assert_eq!(std::fs::read(&other_name).unwrap(), b"new\n");
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(std::fs::metadata(&other_name).unwrap().nlink(), 2);
    }

    /// A group other than the one new files get: any group as root, otherwise
    /// a supplementary group of the current user.
    fn foreign_group() -> Option<u32> {
        // SAFETY: these calls have no preconditions; the buffer is sized by
        // the first getgroups call.
        let (euid, egid) = unsafe { (libc::geteuid(), libc::getegid()) };
        if euid == 0 {
            return Some(egid + 4242);
        }
        let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        let mut groups = vec![0; count.max(0) as usize];
        let count = unsafe { libc::getgroups(groups.len() as libc::c_int, groups.as_mut_ptr()) };
        groups.truncate(count.max(0) as usize);
        groups.into_iter().find(|group| *group != egid)
    }

    #[test]
    fn edit_keeps_a_foreign_group_and_owner() {
        let Some(group) = foreign_group() else {
            // Without root or a supplementary group no file can be given a
            // foreign group, so there is nothing to observe on this host.
            eprintln!("skipped: the test user has no root access or supplementary group");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.txt");
        std::fs::write(&path, "old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        std::os::unix::fs::chown(&path, None, Some(group)).unwrap();
        write_format_validate(&path, "new\n", &plain_config(), &serde_json::json!({})).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new\n");
        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(metadata.gid(), group);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o640);

        // As root, a file owned by another user must keep that owner too.
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            std::os::unix::fs::chown(&path, Some(4242), None).unwrap();
            assert_eq!(
                atomic_write_source(&path, b"third\n").unwrap(),
                SourceWriteMode::InPlace
            );
            let metadata = std::fs::metadata(&path).unwrap();
            assert_eq!((metadata.uid(), metadata.gid()), (4242, group));
            assert_eq!(std::fs::read(&path).unwrap(), b"third\n");
        }
    }
}
