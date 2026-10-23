//! Copy-on-first-use: copy one row of a v1 or v2 store into the v3 family of
//! the same name. No production path calls it; tests and integration children
//! do.
//!
//! The only writes outside `blobs/v3/` are one assembly pin's two files (and
//! the `pins/` directory if it is missing). The pin lists the source key, is
//! created before the source is read, and is removed after the v3 publication
//! has passed `ClosureFlushed`. While it lives, both the v1 and the v2 sweep
//! keep the source row:
//!
//! - v1: the pin goes in `<storage>/views/<scope>/pins/` through
//!   `AssemblyPin::create`, which takes `storage_retention::barrier`, the lock
//!   the v1 sweep holds from marking through deletion.
//! - v2: the pin goes in the holding member's `pins/` inside the family
//!   registry's write barrier, and only when no live process holds
//!   `sweep_lease`. A v2 sweep reads pins only while it holds that lease, and
//!   the copier writes no v2 row (no touch), so a pin created while a sweep
//!   is past marking would not protect the source from that sweep. While a
//!   lease is live the copier retries, up to [`SWEEP_LEASE_WAIT`].
//!
//! Sources are read without changing them: a v2 source through the store's
//! own read-only handle, and a v1 source through a read-only SQLite connection
//! that runs no schema batch and no `journal_mode` change.

use super::*;
use crate::blob_store::v2::FamilyStoreReader;
use crate::blob_store::{BlobPlane, FullKey};
use crate::pins::AssemblyPin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// The longest a copier waits for a live v2 sweep lease to clear.
pub const SWEEP_LEASE_WAIT: Duration = Duration::from_secs(120);
/// A rowid-layout touch changes only a small liveness row. Reserve room for
/// its WAL frame and the registry transaction, independent of payload size.
pub(super) const TOUCH_RESERVATION: u64 = 64 * 1024;
// The older v3 layout still rewrites the payload record on an epoch touch.
const SHARED_TOUCH_MARGIN: u64 = 1 << 20;
const LEASE_RETRY: Duration = Duration::from_millis(20);
static PIN_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The row to copy. The v3 family is the source family's name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopySource<'a> {
    /// A row of `<storage>/blobs/<artifact_key>/<plane>.sqlite`.
    V1 {
        artifact_key: &'a str,
        plane: BlobPlane,
        key: [u8; 32],
    },
    /// A row of the v2 family store `<storage>/blobs/v2/<family>/`.
    V2 { family: &'a str, key: FamilyKey },
}

impl CopySource<'_> {
    fn family(&self) -> &str {
        match self {
            Self::V1 { artifact_key, .. } => artifact_key,
            Self::V2 { family, .. } => family,
        }
    }
    fn v3_key(&self) -> FamilyKey {
        match *self {
            Self::V1 { plane, key, .. } => FamilyKey::new(plane.into(), key),
            Self::V2 { key, .. } => key,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyOutcome {
    /// The row was inserted into the v3 plane store.
    Copied,
    /// An equal row was already in v3; its `ref_epoch` was touched.
    Reused,
    /// The source has no such row (it may have been collected).
    SourceMissing,
}

/// Copies `source` into v3 under the protection of the registered member
/// `holder`: a v1 scope with a directory under `<storage>/views/`, or a member
/// of the source's v2 family registry. `observer` sees the publication's
/// `V3FloorRaised` and `ClosureFlushed`; production would pass `None`.
#[doc(hidden)]
pub fn copy_on_first_use(
    storage: &Path,
    source: CopySource<'_>,
    holder: &str,
    observer: Option<&dyn DurabilityObserver>,
) -> Result<CopyOutcome> {
    let family = source.family();
    family_dir(storage, family)?;
    let key = source.v3_key();
    // Protection first: nothing below reads the source before the pin exists.
    let pin = protect(storage, &source, holder)?;
    let Some((length, digest)) = source_identity(storage, &source)? else {
        return Ok(CopyOutcome::SourceMissing);
    };
    // An equal row already in v3 (for example, from a copy that committed
    // before its caller saw the result) needs only a ref_epoch touch, so the
    // attempt reserves for that alone. The touch never inserts: if the row
    // is gone by the time the closure runs, that attempt writes nothing and
    // the full copy below runs under its own full-size reservation.
    if let Some(layout) = destination_holds(storage, family, &key, length, &digest)? {
        let request = if layout == 1 {
            length.saturating_add(SHARED_TOUCH_MARGIN)
        } else {
            TOUCH_RESERVATION
        };
        if publish(storage, family, request, observer, |publication| {
            publication.touch_row(&key, &digest)
        })? {
            drop(pin);
            return Ok(CopyOutcome::Reused);
        }
    }
    // Room for the row, its index entry and WAL frames; the commit charges
    // the measured growth instead.
    let request = length.saturating_mul(2).saturating_add(1 << 20);
    let outcome = publish(storage, family, request, observer, |publication| {
        let Some((payload, payload_schema)) = read_source(storage, &source)? else {
            return Ok(CopyOutcome::SourceMissing);
        };
        Ok(if publication.put_row(&key, &payload, payload_schema)? {
            CopyOutcome::Copied
        } else {
            CopyOutcome::Reused
        })
    });
    // The publication returns after its ledger commit, which follows
    // ClosureFlushed, so the source stays protected until the copy is durable.
    drop(pin);
    outcome
}

#[cfg(test)]
thread_local! {
    /// Test-only fault injection: skip the copier's pin, or treat a live sweep
    /// lease as absent, so tests can show what each guard prevents. Each is
    /// set on the copier's own thread and never exists outside test builds.
    pub(super) static SKIP_PIN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    pub(super) static SKIP_LEASE_CHECK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn pin_error(error: crate::pins::PinError) -> Error {
    Error::Io(std::io::Error::other(error.to_string()))
}

enum Attempt {
    NotRegistered,
    LeaseLive,
    Pinned(std::result::Result<AssemblyPin, crate::pins::PinError>),
}

fn protect(storage: &Path, source: &CopySource<'_>, holder: &str) -> Result<Option<AssemblyPin>> {
    #[cfg(test)]
    if SKIP_PIN.with(std::cell::Cell::get) {
        return Ok(None);
    }
    let generation = format!(
        "v3-copy-{}-{}",
        std::process::id(),
        PIN_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    // The key file holds plane-free 64-digit hex, so a FullKey carries the
    // bytes of any plane's key, trigram included.
    let carrier = |bytes: [u8; 32]| FullKey {
        bytes,
        plane: BlobPlane::Semantic,
    };
    match *source {
        CopySource::V1 {
            artifact_key, key, ..
        } => {
            if holder.is_empty()
                || holder.contains(['/', '\\', '\0'])
                || holder == "."
                || holder == ".."
            {
                return Err(Error::InvalidIdentity);
            }
            let view_dir = storage.join("views").join(holder);
            if holder == super::super::v2::V2_DIR || !view_dir.is_dir() {
                return Err(Error::Refused("copy_holder_not_registered"));
            }
            AssemblyPin::create(&view_dir, artifact_key, holder, generation, &[carrier(key)])
                .map(Some)
                .map_err(pin_error)
        }
        CopySource::V2 { family, key } => {
            let registry = crate::views::registry::FamilyRegistry::open_existing(storage, family)
                .map_err(|_| Error::Refused("copy_holder_not_registered"))?
                .ok_or(Error::Refused("copy_holder_not_registered"))?;
            let deadline = Instant::now() + SWEEP_LEASE_WAIT;
            loop {
                let attempt = registry
                    .with_barrier(|tx| {
                        let view_dir: Option<String> = tx
                            .query_row(
                                "SELECT view_dir FROM members WHERE scope = ?1",
                                [holder],
                                |row| row.get(0),
                            )
                            .optional()?;
                        let Some(view_dir) = view_dir else {
                            return Ok(Attempt::NotRegistered);
                        };
                        if sweep_lease_is_live(tx)? {
                            return Ok(Attempt::LeaseLive);
                        }
                        Ok(Attempt::Pinned(AssemblyPin::create(
                            Path::new(&view_dir),
                            family,
                            holder,
                            generation.clone(),
                            &[carrier(*key.as_bytes())],
                        )))
                    })
                    .map_err(|error| Error::Io(std::io::Error::other(error.to_string())))?;
                match attempt {
                    Attempt::Pinned(pin) => return pin.map(Some).map_err(pin_error),
                    Attempt::NotRegistered => {
                        return Err(Error::Refused("copy_holder_not_registered"))
                    }
                    Attempt::LeaseLive if Instant::now() >= deadline => {
                        return Err(Error::Refused("v2_sweep_lease_wait"))
                    }
                    Attempt::LeaseLive => std::thread::sleep(LEASE_RETRY),
                }
            }
        }
    }
}

/// Whether a live process holds the family's sweep lease, read inside the
/// registry's write barrier. A dead holder's lease is taken over by the next
/// sweep, which then marks pins created now.
fn sweep_lease_is_live(tx: &rusqlite::Transaction<'_>) -> rusqlite::Result<bool> {
    #[cfg(test)]
    if SKIP_LEASE_CHECK.with(std::cell::Cell::get) {
        return Ok(false);
    }
    let holder: Option<(i64, i64)> = tx
        .query_row(
            "SELECT owner_pid, owner_start FROM sweep_lease WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(holder.is_some_and(|(pid, start)| {
        crate::pins::owner_is_live(&PinOwner {
            pid: pid as u32,
            start_time: start.max(0) as u64,
        })
    }))
}

fn v1_path(storage: &Path, artifact_key: &str, plane: BlobPlane) -> Result<PathBuf> {
    super::super::validate_artifact_key(artifact_key).map_err(|_| Error::InvalidIdentity)?;
    Ok(storage
        .join("blobs")
        .join(artifact_key)
        .join(format!("{}.sqlite", plane.as_str())))
}

/// Opens an existing v1 store for reading only: no create, no schema batch,
/// no `journal_mode` switch and no other persistent pragma.
pub(super) fn open_v1(path: &Path) -> Result<Option<IdentityConnection>> {
    if !path.is_file() {
        return Ok(None);
    }
    let connection = IdentityConnection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        "views-v3",
    )?;
    connection.busy_timeout(Duration::from_millis(TIMEOUT_MS as u64))?;
    Ok(Some(connection))
}

pub(super) fn v2_reader(
    storage: &Path,
    family: &str,
    key: &FamilyKey,
) -> Result<Option<FamilyStoreReader>> {
    FamilyStoreReader::open_existing(storage, family, key.plane())
        .map_err(|error| Error::Io(std::io::Error::other(error.to_string())))
}

/// The source row's payload length and stored digest, read without reading
/// the payload.
fn source_identity(storage: &Path, source: &CopySource<'_>) -> Result<Option<(u64, Vec<u8>)>> {
    match *source {
        CopySource::V1 {
            artifact_key,
            plane,
            key,
        } => {
            let Some(connection) = open_v1(&v1_path(storage, artifact_key, plane)?)? else {
                return Ok(None);
            };
            Ok(connection
                .query_row(
                    "SELECT length(payload), payload_digest FROM blob_payloads WHERE full_key = ?1",
                    [key.as_slice()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?)
        }
        CopySource::V2 { family, key } => match v2_reader(storage, family, &key)? {
            Some(reader) => reader
                .copy_source_identity(&key)
                .map_err(|error| Error::Io(std::io::Error::other(error.to_string()))),
            None => Ok(None),
        },
    }
}

/// Whether v3 already holds `key` with the same stored digest and payload
/// length, read without reading the payload.
fn destination_holds(
    storage: &Path,
    family: &str,
    key: &FamilyKey,
    length: u64,
    digest: &[u8],
) -> Result<Option<u32>> {
    if !plane_path(storage, family, key.plane())?.try_exists()? {
        return Ok(None);
    }
    let reader = PlaneReader::open_existing(storage, family, key.plane())?;
    let row: Option<(u64, Vec<u8>)> = reader
        .database
        .connection()?
        .query_row(
            "SELECT length(payload), payload_digest FROM blob_payloads WHERE full_key = ?1",
            [key.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if row.is_some_and(|(stored_length, stored_digest)| {
        stored_length == length && stored_digest == digest
    }) {
        Ok(Some(plane_layout(&**reader.database.connection()?)?))
    } else {
        Ok(None)
    }
}

/// The source payload and its stored `payload_schema`, after checking the
/// stored digest against the bytes.
fn read_source(storage: &Path, source: &CopySource<'_>) -> Result<Option<(Vec<u8>, u32)>> {
    let row = match *source {
        CopySource::V1 {
            artifact_key,
            plane,
            key,
        } => {
            let Some(connection) = open_v1(&v1_path(storage, artifact_key, plane)?)? else {
                return Ok(None);
            };
            connection
                .query_row(
                    "SELECT payload, payload_digest, payload_schema FROM blob_payloads WHERE full_key = ?1",
                    [key.as_slice()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?
        }
        CopySource::V2 { family, key } => match v2_reader(storage, family, &key)? {
            Some(reader) => reader
                .copy_source_row(&key)
                .map_err(|error| Error::Io(std::io::Error::other(error.to_string())))?,
            None => None,
        },
    };
    let Some((payload, digest, schema)): Option<(Vec<u8>, Vec<u8>, i64)> = row else {
        return Ok(None);
    };
    if blake3::hash(&payload).as_bytes().as_slice() != digest.as_slice() {
        return Err(Error::Refused("payload_digest"));
    }
    let schema = u32::try_from(schema).map_err(|_| Error::Refused("payload_schema"))?;
    Ok(Some((payload, schema)))
}
