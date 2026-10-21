//! Read-only SQLite snapshots without opening writable WAL sidecars. A normal
//! SQLite read-only open can create a WAL/SHM pair in a writable directory.
//! Registry ledgers must not do that, including when the writer is closed.

use super::*;

fn wal(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut name = path.as_os_str().to_os_string();
    name.push("-wal");
    match fs::read(PathBuf::from(name)) {
        Ok(bytes) => Ok(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}
fn u32be(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[..4].try_into().expect("four bytes"))
}
fn checksum(bytes: &[u8], big: bool, mut state: (u32, u32)) -> (u32, u32) {
    for chunk in bytes.chunks_exact(8) {
        let a: [u8; 4] = chunk[..4].try_into().unwrap();
        let b: [u8; 4] = chunk[4..].try_into().unwrap();
        let (a, b) = if big {
            (u32::from_be_bytes(a), u32::from_be_bytes(b))
        } else {
            (u32::from_le_bytes(a), u32::from_le_bytes(b))
        };
        state.0 = state.0.wrapping_add(a).wrapping_add(state.1);
        state.1 = state.1.wrapping_add(b).wrapping_add(state.0);
    }
    state
}
fn invalid() -> Error {
    Error::Refused("reservation_ledger_unreadable")
}
fn replay(mut database: Vec<u8>, wal: &[u8]) -> Result<Vec<u8>> {
    if database.len() < 100 || &database[..16] != b"SQLite format 3\0" {
        return Err(invalid());
    }
    if wal.len() >= 32 {
        let magic = u32be(wal);
        if magic != 0x377f0682 && magic != 0x377f0683 {
            return Err(invalid());
        }
        let big = magic == 0x377f0683;
        let page = u32be(&wal[8..]) as usize;
        if !(512..=65536).contains(&page) || !page.is_power_of_two() {
            return Err(invalid());
        }
        let mut sum = checksum(&wal[..24], big, (0, 0));
        if sum != (u32be(&wal[24..]), u32be(&wal[28..])) {
            return Err(invalid());
        }
        let mut last_commit = None;
        for (index, frame) in wal[32..].chunks_exact(page + 24).enumerate() {
            if frame[8..16] != wal[16..24] {
                break;
            }
            sum = checksum(&frame[..8], big, sum);
            sum = checksum(&frame[24..], big, sum);
            if sum != (u32be(&frame[16..]), u32be(&frame[20..])) {
                break;
            }
            if u32be(frame) == 0 {
                return Err(invalid());
            }
            if u32be(&frame[4..]) != 0 {
                last_commit = Some((index, u32be(&frame[4..]) as usize));
            }
        }
        if let Some((last, pages)) = last_commit {
            let size = pages.checked_mul(page).ok_or_else(invalid)?;
            if size > database.len().saturating_add(wal.len()) {
                return Err(invalid());
            }
            database.resize(size, 0);
            for (index, frame) in wal[32..].chunks_exact(page + 24).enumerate() {
                if index > last {
                    break;
                }
                let number = u32be(frame) as usize;
                if number <= pages {
                    database[(number - 1) * page..number * page].copy_from_slice(&frame[24..]);
                }
            }
        }
    } else if !wal.is_empty() {
        return Err(invalid());
    }
    // Deserialized databases cannot attach a WAL. The snapshot has already
    // replayed every committed frame, so use rollback-mode headers in memory.
    database[18] = 1;
    database[19] = 1;
    Ok(database)
}
pub(super) fn open(path: &Path) -> Result<Connection> {
    // A checkpoint can rewrite the main database and recycle the WAL between
    // reads. Accept only a stable pair; checksums reject partially written WAL
    // frames, and uncommitted frames never enter the snapshot.
    for _ in 0..8 {
        let database = fs::read(path)?;
        let log = wal(path)?;
        if database != fs::read(path)? || log != wal(path)? {
            continue;
        }
        let bytes = replay(database, &log)?;
        let c = Connection::open_in_memory()?;
        let len = i64::try_from(bytes.len()).map_err(|_| invalid())?;
        // SAFETY: SQLite owns this allocation after a successful deserialize.
        // FREEONCLOSE also frees it when the connection is dropped.
        unsafe {
            let data = rusqlite::ffi::sqlite3_malloc64(bytes.len() as u64).cast::<u8>();
            if data.is_null() {
                return Err(invalid());
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
            let code = rusqlite::ffi::sqlite3_deserialize(
                c.handle(),
                c"main".as_ptr(),
                data,
                len,
                len,
                rusqlite::ffi::SQLITE_DESERIALIZE_FREEONCLOSE
                    | rusqlite::ffi::SQLITE_DESERIALIZE_READONLY,
            );
            if code != rusqlite::ffi::SQLITE_OK {
                return Err(invalid());
            }
        }
        return Ok(c);
    }
    Err(invalid())
}
