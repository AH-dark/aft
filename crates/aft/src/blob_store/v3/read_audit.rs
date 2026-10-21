//! Test-only instrumentation of SQLite's own VFS calls. This never opens a
//! file itself or reads database/sidecar bytes; all I/O stays inside SQLite.

use rusqlite::{ffi, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex, OnceLock,
};

const NAME: &std::ffi::CStr = c"aft-v3-read-audit";
static ENABLED: AtomicBool = AtomicBool::new(false);
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Calls {
    pub opens: u64,
    pub writes: u64,
    pub truncates: u64,
    pub syncs: u64,
    pub closes: u64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Counts {
    pub main: Calls,
    pub wal: Calls,
    pub other: Calls,
}
#[derive(Clone, Copy)]
enum Class {
    Main,
    Wal,
    Other,
}
impl Counts {
    fn class(&mut self, class: Class) -> &mut Calls {
        match class {
            Class::Main => &mut self.main,
            Class::Wal => &mut self.wal,
            Class::Other => &mut self.other,
        }
    }
}
fn totals() -> &'static Mutex<Counts> {
    static COUNTS: OnceLock<Mutex<Counts>> = OnceLock::new();
    COUNTS.get_or_init(|| Mutex::new(Counts::default()))
}
fn files() -> &'static Mutex<HashMap<usize, (usize, Class)>> {
    static FILES: OnceLock<Mutex<HashMap<usize, (usize, Class)>>> = OnceLock::new();
    FILES.get_or_init(|| Mutex::new(HashMap::new()))
}
fn tables() -> &'static Mutex<HashMap<usize, usize>> {
    static TABLES: OnceLock<Mutex<HashMap<usize, usize>>> = OnceLock::new();
    TABLES.get_or_init(|| Mutex::new(HashMap::new()))
}
fn platform() -> *mut ffi::sqlite3_vfs {
    static PLATFORM: OnceLock<usize> = OnceLock::new();
    *PLATFORM.get_or_init(|| {
        // SAFETY: SQLite returns a process-lifetime VFS registration.
        let ptr = unsafe { ffi::sqlite3_vfs_find(std::ptr::null()) };
        assert!(!ptr.is_null());
        ptr as usize
    }) as *mut ffi::sqlite3_vfs
}
pub(super) fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}
pub(super) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}
pub(super) fn counts() -> Counts {
    *totals().lock().unwrap()
}
pub(super) fn reset() {
    *totals().lock().unwrap() = Counts::default();
}
pub(super) fn open(path: &Path, flags: OpenFlags) -> rusqlite::Result<Connection> {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    REGISTERED.get_or_init(|| {
        // SAFETY: the copied VFS has process lifetime. Unchanged callbacks use
        // the original file object; xOpen explicitly forwards to its real VFS.
        unsafe {
            let mut vfs = *platform();
            vfs.zName = NAME.as_ptr();
            vfs.pNext = std::ptr::null_mut();
            vfs.xOpen = Some(open_file);
            assert_eq!(
                ffi::sqlite3_vfs_register(Box::into_raw(Box::new(vfs)), 0),
                ffi::SQLITE_OK
            );
        }
    });
    Connection::open_with_flags_and_vfs(path, flags, NAME.to_str().unwrap())
}
unsafe extern "C" fn open_file(
    _vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    // SAFETY: forward SQLite's name, file, flags and out unchanged, using
    // the original platform VFS pointer in place of the wrapper's pointer.
    let rc = unsafe { ((*platform()).xOpen.unwrap())(platform(), name, file, flags, out) };
    if rc != ffi::SQLITE_OK {
        return rc;
    }
    let class = if flags & ffi::SQLITE_OPEN_MAIN_DB != 0 {
        Class::Main
    } else if flags & ffi::SQLITE_OPEN_WAL != 0 {
        Class::Wal
    } else {
        Class::Other
    };
    // SAFETY: successful xOpen initializes a valid SQLite I/O method table.
    let original = unsafe { (*file).pMethods };
    if original.is_null() {
        return rc;
    }
    files()
        .lock()
        .unwrap()
        .insert(file as usize, (original as usize, class));
    totals().lock().unwrap().class(class).opens += 1;
    let mut table = tables().lock().unwrap();
    let patched = *table.entry(original as usize).or_insert_with(|| {
        // SAFETY: copy the platform table, preserving every unmodified callback.
        let mut copy = unsafe { *original };
        copy.xWrite = Some(write);
        copy.xTruncate = Some(truncate);
        copy.xSync = Some(sync);
        copy.xClose = Some(close);
        Box::into_raw(Box::new(copy)) as usize
    });
    // SAFETY: leaked method tables outlive every audited connection.
    unsafe { (*file).pMethods = patched as *const ffi::sqlite3_io_methods };
    rc
}
fn original(
    file: *mut ffi::sqlite3_file,
    update: impl FnOnce(&mut Calls),
) -> *const ffi::sqlite3_io_methods {
    let (methods, class) = files().lock().unwrap()[&(file as usize)];
    update(totals().lock().unwrap().class(class));
    methods as *const ffi::sqlite3_io_methods
}
unsafe extern "C" fn write(
    file: *mut ffi::sqlite3_file,
    buffer: *const c_void,
    amount: c_int,
    offset: i64,
) -> c_int {
    let methods = original(file, |calls| calls.writes += 1);
    // SAFETY: forward SQLite's buffer and offsets to the original callback.
    unsafe { ((*methods).xWrite.unwrap())(file, buffer, amount, offset) }
}
unsafe extern "C" fn truncate(file: *mut ffi::sqlite3_file, size: i64) -> c_int {
    let methods = original(file, |calls| calls.truncates += 1);
    // SAFETY: forward the original SQLite-owned file and requested length.
    unsafe { ((*methods).xTruncate.unwrap())(file, size) }
}
unsafe extern "C" fn sync(file: *mut ffi::sqlite3_file, flags: c_int) -> c_int {
    let methods = original(file, |calls| calls.syncs += 1);
    // SAFETY: forward the original SQLite-owned file and synchronization flags.
    unsafe { ((*methods).xSync.unwrap())(file, flags) }
}
unsafe extern "C" fn close(file: *mut ffi::sqlite3_file) -> c_int {
    let methods = original(file, |calls| calls.closes += 1);
    files().lock().unwrap().remove(&(file as usize));
    // SAFETY: restore the original table before the platform closes its file.
    unsafe {
        (*file).pMethods = methods;
        ((*methods).xClose.unwrap())(file)
    }
}
