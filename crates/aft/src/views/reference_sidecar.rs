//! Binary sidecars holding the blob keys each published manifest references.
//!
//! Storage-wide blob GC must know every key named by every manifest in every
//! view, and a manifest lists every file of its checkout. Decoding all of that
//! JSON on each retention pass dominated the pass and never finished on large
//! storage. Instead, `manifest-<generation>.keys` beside each manifest holds the
//! generation's keys as sorted, unique 32-byte records, so GC can search them
//! with positioned reads and keep only its candidate batch in memory.
//!
//! File layout (little endian):
//!
//! | bytes   | field                                                      |
//! |---------|------------------------------------------------------------|
//! | 0..8    | magic `AFTREFK1`                                           |
//! | 8..12   | format version                                             |
//! | 12..16  | origin: 1 written by the publisher, 2 derived from the JSON |
//! | 16..24  | key count                                                  |
//! | 24..32  | time the keys were derived, nanoseconds since 1970         |
//! | 32..96  | [`FileStamp`] of the manifest the keys came from           |
//! | 96..128 | BLAKE3 of bytes 0..96 followed by every key                |
//! | 128..   | keys, strictly ascending                                   |
//!
//! Safety rule: a sidecar is used only when its header, size, checksum and
//! ordering are valid and the manifest's current stamp equals the recorded
//! one. Anything else — no sidecar, a damaged one, a replaced manifest — means
//! the JSON is decoded again and the sidecar rewritten. A sidecar problem can
//! cost time; it can never make GC treat a referenced blob as unreferenced.
use std::{
    collections::{BTreeSet, HashMap},
    fs::{self, File},
    io::Read as _,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, SystemTime},
};

const MAGIC: [u8; 8] = *b"AFTREFK1";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 128;
const CHECKSUM_AT: usize = 96;
const KEY_LEN: usize = 32;
const ORIGIN_PUBLISHER: u32 = 1;
const ORIGIN_DERIVED: u32 = 2;
/// Bytes read at a time while verifying a sidecar.
const VERIFY_CHUNK: usize = 64 * 1024;
/// Sidecars already verified by this process, keyed by path. Bounded so a
/// huge storage tree cannot grow it without limit; it is simply cleared when
/// full and sidecars are re-verified on their next use.
const MAX_VERIFIED: usize = 65_536;
/// A sidecar derived from a manifest whose timestamps were younger than this is
/// not trusted. On a filesystem with coarse timestamps (one or two seconds), a
/// file replaced in the same tick, reusing the same inode and size, could
/// otherwise present an identical stamp. Once a file is older than the
/// coarsest tick, any later replacement necessarily carries a newer timestamp.
pub(crate) const SETTLE: Duration = Duration::from_secs(5);

/// The name of the sidecar for `generation`'s manifest.
pub(crate) fn sidecar_name(generation: &str) -> String {
    format!("manifest-{generation}.keys")
}

/// Identity of a manifest file on disk: which file it is and the last time its
/// contents or metadata changed. Stored as plain numbers so it can be written
/// into a sidecar header and compared exactly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileStamp {
    len: u64,
    modified: (i64, u32),
    /// Device and inode on unix; zero elsewhere, where std exposes neither.
    file: (u64, u64),
    /// Status-change time on unix, which no ordinary API can set back: renames,
    /// rewrites, link changes and timestamp edits all advance it. Creation time
    /// elsewhere, when the platform reports one.
    changed: Option<(i64, u32)>,
}

fn epoch_parts(time: SystemTime) -> Option<(i64, u32)> {
    let since = time.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    Some((i64::try_from(since.as_secs()).ok()?, since.subsec_nanos()))
}

fn parts_nanos((seconds, nanos): (i64, u32)) -> i128 {
    i128::from(seconds) * 1_000_000_000 + i128::from(nanos)
}

fn now_nanos() -> Option<i128> {
    epoch_parts(SystemTime::now()).map(parts_nanos)
}

impl FileStamp {
    /// `None` when the platform cannot report a usable modification time (or
    /// reports one before 1970); such a file never gets a trusted sidecar.
    pub(crate) fn of(metadata: &fs::Metadata) -> Option<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;
        #[cfg(unix)]
        let changed = Some((metadata.ctime(), u32::try_from(metadata.ctime_nsec()).ok()?))
            .filter(|(seconds, _)| *seconds >= 0);
        #[cfg(not(unix))]
        let changed = metadata.created().ok().and_then(epoch_parts);
        #[cfg(unix)]
        if changed.is_none() {
            return None;
        }
        Some(Self {
            len: metadata.len(),
            modified: epoch_parts(metadata.modified().ok()?)?,
            #[cfg(unix)]
            file: (metadata.dev(), metadata.ino()),
            #[cfg(not(unix))]
            file: (0, 0),
            changed,
        })
    }

    /// Whether the file's timestamps were at least `settle` older than `at`.
    fn settled_by(&self, at: i128, settle: Duration) -> bool {
        let settle = settle.as_nanos() as i128;
        parts_nanos(self.modified) + settle <= at
            && self
                .changed
                .is_none_or(|changed| parts_nanos(changed) + settle <= at)
    }

    fn encode(&self) -> [u8; 64] {
        let mut out = [0_u8; 64];
        out[0..8].copy_from_slice(&self.len.to_le_bytes());
        out[8..16].copy_from_slice(&self.modified.0.to_le_bytes());
        out[16..20].copy_from_slice(&self.modified.1.to_le_bytes());
        out[20..28].copy_from_slice(&self.file.0.to_le_bytes());
        out[28..36].copy_from_slice(&self.file.1.to_le_bytes());
        if let Some((seconds, nanos)) = self.changed {
            out[36..44].copy_from_slice(&seconds.to_le_bytes());
            out[44..48].copy_from_slice(&nanos.to_le_bytes());
            out[48] = 1;
        }
        out
    }
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("four bytes"))
}

fn le_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().expect("eight bytes"))
}

/// Keys in strictly ascending order that can be read by position, either from
/// memory or from a sidecar file.
pub(crate) trait SortedKeys {
    fn len(&self) -> usize;
    /// Fill `out` with the keys starting at index `start`.
    fn read(&self, start: usize, out: &mut [[u8; 32]]) -> std::io::Result<()>;
}

impl SortedKeys for Vec<[u8; 32]> {
    fn len(&self) -> usize {
        Vec::len(self)
    }
    fn read(&self, start: usize, out: &mut [[u8; 32]]) -> std::io::Result<()> {
        let end = start
            .checked_add(out.len())
            .filter(|end| *end <= Vec::len(self))
            .ok_or_else(|| std::io::Error::other("sorted key read out of range"))?;
        out.copy_from_slice(&self[start..end]);
        Ok(())
    }
}

/// A verified sidecar, read with positioned reads through the handle that was
/// verified, so a concurrent rewrite (which replaces the name, not the file)
/// cannot change what this pass sees.
pub(crate) struct SidecarKeys {
    file: File,
    count: usize,
}

fn read_exact_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_exact_at(file, buffer, offset)
    }
    #[cfg(windows)]
    {
        let mut done = 0;
        while done < buffer.len() {
            let read = std::os::windows::fs::FileExt::seek_read(
                file,
                &mut buffer[done..],
                offset + done as u64,
            )?;
            if read == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            done += read;
        }
        Ok(())
    }
}

impl SortedKeys for SidecarKeys {
    fn len(&self) -> usize {
        self.count
    }
    fn read(&self, start: usize, out: &mut [[u8; 32]]) -> std::io::Result<()> {
        if start
            .checked_add(out.len())
            .is_none_or(|end| end > self.count)
        {
            return Err(std::io::Error::other("sidecar read out of range"));
        }
        let offset = (HEADER_LEN + start * KEY_LEN) as u64;
        read_exact_at(&self.file, out.as_flattened_mut(), offset)
    }
}

/// Where a generation's keys came from on one visit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeySource {
    Sidecar,
    /// The JSON was decoded; `written` says whether a sidecar was saved.
    Decoded {
        written: bool,
    },
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct Verified {
    sidecar: FileStamp,
    manifest: FileStamp,
    count: usize,
}

pub(crate) struct ReferenceSidecars {
    settle: Duration,
    /// Re-hashing every sidecar on every pass would read all of them in full;
    /// a sidecar whose own stamp is unchanged since it was verified is not.
    verified: Mutex<HashMap<PathBuf, Verified>>,
}

pub(crate) fn global() -> &'static ReferenceSidecars {
    static SIDECARS: OnceLock<ReferenceSidecars> = OnceLock::new();
    SIDECARS.get_or_init(|| ReferenceSidecars::new(SETTLE))
}

fn encode(stamp: &FileStamp, origin: u32, derived_at: i128, keys: &[[u8; 32]]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER_LEN + keys.len() * KEY_LEN);
    bytes.extend_from_slice(&MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&origin.to_le_bytes());
    bytes.extend_from_slice(&(keys.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&(u64::try_from(derived_at).unwrap_or(0)).to_le_bytes());
    bytes.extend_from_slice(&stamp.encode());
    let mut hasher = blake3::Hasher::new();
    hasher.update(&bytes);
    hasher.update(keys.as_flattened());
    bytes.extend_from_slice(hasher.finalize().as_bytes());
    bytes.extend_from_slice(keys.as_flattened());
    bytes
}

/// Write `keys` (sorted, unique) as the sidecar of the manifest at
/// `view/manifest-<generation>.json` whose stamp is `stamp`.
fn write_sidecar(
    view: &Path,
    generation: &str,
    stamp: &FileStamp,
    origin: u32,
    derived_at: i128,
    keys: &[[u8; 32]],
) -> std::io::Result<()> {
    let path = view.join(sidecar_name(generation));
    crate::production_storage::refuse_write(&path)?;
    // The sidecar is rebuildable from the manifest, so it needs a synced file
    // before the rename but not a directory sync of its own.
    crate::durability::atomic_write(
        &path,
        &encode(stamp, origin, derived_at, keys),
        crate::durability::AtomicWriteDurability::File,
    )
}

/// Record the keys of a manifest this process has just published. The keys
/// come from the in-memory manifest that was written, so no settle window is
/// needed: generation names are unique per publication, and nothing else
/// writes a file under that name.
pub(crate) fn write_published(
    view: &Path,
    generation: &str,
    keys: &BTreeSet<[u8; 32]>,
) -> std::io::Result<()> {
    let manifest = view.join(format!("manifest-{generation}.json"));
    let stamp = FileStamp::of(&fs::metadata(&manifest)?)
        .ok_or_else(|| std::io::Error::other("manifest has no usable timestamp"))?;
    let derived_at = now_nanos().ok_or_else(|| std::io::Error::other("clock before 1970"))?;
    let keys: Vec<[u8; 32]> = keys.iter().copied().collect();
    write_sidecar(
        view,
        generation,
        &stamp,
        ORIGIN_PUBLISHER,
        derived_at,
        &keys,
    )
}

impl ReferenceSidecars {
    pub(crate) fn new(settle: Duration) -> Self {
        Self {
            settle,
            verified: Mutex::new(HashMap::new()),
        }
    }

    fn verified(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Verified>> {
        self.verified
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Forget which sidecars were verified, so each is fully re-checked on its
    /// next use.
    pub(crate) fn forget_verified(&self) {
        self.verified().clear();
    }

    /// Show `generation`'s keys to `visit` exactly once, from its sidecar when
    /// that is valid for the manifest on disk, and otherwise by decoding the
    /// manifest JSON with `decode` (and saving a sidecar for next time).
    pub(crate) fn visit(
        &self,
        view: &Path,
        generation: &str,
        decode: impl FnOnce(&[u8]) -> super::Result<BTreeSet<[u8; 32]>>,
        visit: &mut dyn FnMut(&dyn SortedKeys) -> std::io::Result<()>,
    ) -> super::Result<KeySource> {
        let manifest = view.join(format!("manifest-{generation}.json"));
        let stamp = FileStamp::of(&fs::metadata(&manifest)?);
        if let Some(stamp) = &stamp {
            if let Some(keys) = self.open_trusted(&view.join(sidecar_name(generation)), stamp) {
                visit(&keys)?;
                return Ok(KeySource::Sidecar);
            }
        }
        let bytes = fs::read(&manifest)?;
        let keys: Vec<[u8; 32]> = decode(&bytes)?.into_iter().collect();
        drop(bytes);
        visit(&keys)?;
        // Save the keys only if the file kept one identity across the read and
        // is past the settle window; otherwise a later pass decodes it again.
        let Some(stamp) = stamp else {
            return Ok(KeySource::Decoded { written: false });
        };
        let unchanged = fs::metadata(&manifest)
            .ok()
            .and_then(|metadata| FileStamp::of(&metadata))
            == Some(stamp);
        let Some(now) = now_nanos() else {
            return Ok(KeySource::Decoded { written: false });
        };
        if !unchanged || !stamp.settled_by(now, self.settle) {
            return Ok(KeySource::Decoded { written: false });
        }
        let written = match write_sidecar(view, generation, &stamp, ORIGIN_DERIVED, now, &keys) {
            Ok(()) => true,
            Err(error) => {
                log::debug!(
                    "views: reference sidecar not written view={} generation={generation} error={error}",
                    view.display()
                );
                false
            }
        };
        Ok(KeySource::Decoded { written })
    }

    /// The sidecar at `path`, if it is intact and describes the manifest whose
    /// current stamp is `manifest`. Every failure means "decode the JSON".
    fn open_trusted(&self, path: &Path, manifest: &FileStamp) -> Option<SidecarKeys> {
        let mut file = File::open(path).ok()?;
        let sidecar = FileStamp::of(&file.metadata().ok()?)?;
        if let Some(verified) = self.verified().get(path) {
            if verified.sidecar == sidecar && verified.manifest == *manifest {
                return Some(SidecarKeys {
                    file,
                    count: verified.count,
                });
            }
        }
        let mut header = [0_u8; HEADER_LEN];
        file.read_exact(&mut header).ok()?;
        if header[0..8] != MAGIC || le_u32(&header[8..12]) != VERSION {
            return None;
        }
        let origin = le_u32(&header[12..16]);
        let count = usize::try_from(le_u64(&header[16..24])).ok()?;
        let derived_at = i128::from(le_u64(&header[24..32]));
        if header[32..96] != manifest.encode() {
            return None;
        }
        let trusted = match origin {
            ORIGIN_PUBLISHER => true,
            ORIGIN_DERIVED => manifest.settled_by(derived_at, self.settle),
            _ => false,
        };
        let expected_len = count.checked_mul(KEY_LEN)?.checked_add(HEADER_LEN)? as u64;
        if !trusted || sidecar.len != expected_len {
            return None;
        }
        // Stream the keys once: the checksum catches damage, and the order
        // check guarantees the searches GC runs over this file are sound.
        let mut hasher = blake3::Hasher::new();
        hasher.update(&header[..CHECKSUM_AT]);
        let mut buffer = vec![0_u8; VERIFY_CHUNK];
        let mut previous: Option<[u8; 32]> = None;
        let mut remaining = count * KEY_LEN;
        while remaining > 0 {
            let take = remaining.min(VERIFY_CHUNK);
            file.read_exact(&mut buffer[..take]).ok()?;
            hasher.update(&buffer[..take]);
            for key in buffer[..take].chunks_exact(KEY_LEN) {
                let key: [u8; 32] = key.try_into().ok()?;
                if previous.is_some_and(|previous| previous >= key) {
                    return None;
                }
                previous = Some(key);
            }
            remaining -= take;
        }
        if hasher.finalize().as_bytes()[..] != header[CHECKSUM_AT..HEADER_LEN] {
            return None;
        }
        let mut verified = self.verified();
        if verified.len() >= MAX_VERIFIED {
            verified.clear();
        }
        verified.insert(
            path.to_path_buf(),
            Verified {
                sidecar,
                manifest: *manifest,
                count,
            },
        );
        Some(SidecarKeys { file, count })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test manifests are raw concatenated keys; decoding them is trivial so
    /// these tests exercise only the sidecar rules.
    fn decode(bytes: &[u8]) -> super::super::Result<BTreeSet<[u8; 32]>> {
        Ok(bytes
            .chunks_exact(KEY_LEN)
            .map(|key| key.try_into().unwrap())
            .collect())
    }

    fn keys_of(bytes: &[u8]) -> Vec<[u8; 32]> {
        decode(bytes).unwrap().into_iter().collect()
    }

    /// Replace through a rename: the new file gets a new inode while the old
    /// one still exists.
    fn write_manifest(view: &Path, generation: &str, bytes: &[u8]) {
        let staging = view.join("staging.tmp");
        fs::write(&staging, bytes).unwrap();
        fs::rename(&staging, view.join(format!("manifest-{generation}.json"))).unwrap();
    }

    fn visit_keys(
        sidecars: &ReferenceSidecars,
        view: &Path,
        generation: &str,
    ) -> (KeySource, Vec<[u8; 32]>) {
        let mut seen = Vec::new();
        let source = sidecars
            .visit(view, generation, decode, &mut |keys| {
                seen = vec![[0_u8; 32]; keys.len()];
                keys.read(0, &mut seen)
            })
            .unwrap();
        (source, seen)
    }

    fn manifest_bytes() -> Vec<u8> {
        // Deliberately unsorted and with a duplicate.
        [[9_u8; 32], [1; 32], [5; 32], [1; 32]].concat()
    }

    #[test]
    fn reference_sidecar_round_trips_and_is_reused() {
        let view = tempfile::tempdir().unwrap();
        let sidecars = ReferenceSidecars::new(Duration::ZERO);
        write_manifest(view.path(), "g", &manifest_bytes());
        let expected = keys_of(&manifest_bytes());
        assert_eq!(
            visit_keys(&sidecars, view.path(), "g"),
            (KeySource::Decoded { written: true }, expected.clone())
        );
        let sidecar = view.path().join(sidecar_name("g"));
        assert_eq!(
            fs::metadata(&sidecar).unwrap().len() as usize,
            HEADER_LEN + expected.len() * KEY_LEN
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(&sidecar).unwrap().permissions().mode() & 0o777,
                crate::private_storage::FILE_MODE
            );
        }
        assert_eq!(
            visit_keys(&sidecars, view.path(), "g"),
            (KeySource::Sidecar, expected.clone())
        );
        // A fresh process (no verification memo) re-verifies and reuses it.
        sidecars.forget_verified();
        assert_eq!(
            visit_keys(&sidecars, view.path(), "g"),
            (KeySource::Sidecar, expected)
        );
    }

    #[test]
    fn reference_sidecar_is_never_written_or_trusted_inside_the_settle_window() {
        let view = tempfile::tempdir().unwrap();
        let strict = ReferenceSidecars::new(Duration::from_secs(3600));
        write_manifest(view.path(), "g", &manifest_bytes());
        let expected = keys_of(&manifest_bytes());
        assert_eq!(
            visit_keys(&strict, view.path(), "g"),
            (KeySource::Decoded { written: false }, expected.clone())
        );
        assert!(!view.path().join(sidecar_name("g")).exists());
        // A sidecar derived moments after the manifest changed is not trusted
        // by a policy whose window has not passed between the two.
        let eager = ReferenceSidecars::new(Duration::ZERO);
        assert_eq!(
            visit_keys(&eager, view.path(), "g").0,
            KeySource::Decoded { written: true }
        );
        assert_eq!(
            visit_keys(&strict, view.path(), "g"),
            (KeySource::Decoded { written: false }, expected)
        );
    }

    #[test]
    fn reference_sidecar_from_the_publisher_is_trusted_at_once() {
        let view = tempfile::tempdir().unwrap();
        let strict = ReferenceSidecars::new(Duration::from_secs(3600));
        write_manifest(view.path(), "g", &manifest_bytes());
        write_published(view.path(), "g", &decode(&manifest_bytes()).unwrap()).unwrap();
        assert_eq!(
            visit_keys(&strict, view.path(), "g"),
            (KeySource::Sidecar, keys_of(&manifest_bytes()))
        );
    }

    #[test]
    fn reference_sidecar_from_the_publisher_is_dropped_when_the_manifest_is_replaced() {
        let view = tempfile::tempdir().unwrap();
        let strict = ReferenceSidecars::new(Duration::from_secs(3600));
        write_manifest(view.path(), "g", &manifest_bytes());
        write_published(view.path(), "g", &decode(&manifest_bytes()).unwrap()).unwrap();
        assert_eq!(visit_keys(&strict, view.path(), "g").0, KeySource::Sidecar);
        // Same name and length, different keys, new file: a publisher sidecar
        // is trusted without a settle window, so only the recorded manifest
        // stamp can reveal that it now describes another file.
        let replacement = [[2_u8; 32], [3; 32], [4; 32], [6; 32]].concat();
        assert_eq!(replacement.len(), manifest_bytes().len());
        write_manifest(view.path(), "g", &replacement);
        assert_eq!(
            visit_keys(&strict, view.path(), "g"),
            (KeySource::Decoded { written: false }, keys_of(&replacement))
        );
    }

    #[test]
    fn reference_sidecar_damage_is_re_derived_never_read_as_empty() {
        let view = tempfile::tempdir().unwrap();
        let sidecars = ReferenceSidecars::new(Duration::ZERO);
        write_manifest(view.path(), "g", &manifest_bytes());
        let expected = keys_of(&manifest_bytes());
        let sidecar = view.path().join(sidecar_name("g"));
        let stamp =
            FileStamp::of(&fs::metadata(view.path().join("manifest-g.json")).unwrap()).unwrap();
        let now = now_nanos().unwrap();
        let valid = encode(&stamp, ORIGIN_DERIVED, now, &expected);
        let mut unsorted_keys = expected.clone();
        unsorted_keys.reverse();
        let damaged: Vec<(&str, Vec<u8>)> = vec![
            ("flipped key byte", {
                let mut bytes = valid.clone();
                *bytes.last_mut().unwrap() ^= 1;
                bytes
            }),
            ("flipped derived-time byte", {
                let mut bytes = valid.clone();
                bytes[25] ^= 1;
                bytes
            }),
            ("truncated", valid[..valid.len() - KEY_LEN].to_vec()),
            ("count rewritten to zero", {
                let mut bytes = valid.clone();
                bytes[16..24].copy_from_slice(&0_u64.to_le_bytes());
                bytes
            }),
            ("bad magic", {
                let mut bytes = valid.clone();
                bytes[0] = b'X';
                bytes
            }),
            ("future version", {
                let mut bytes = valid.clone();
                bytes[8..12].copy_from_slice(&(VERSION + 1).to_le_bytes());
                bytes
            }),
            ("unknown origin", encode(&stamp, 7, now, &expected)),
            (
                "unsorted with a valid checksum",
                encode(&stamp, ORIGIN_DERIVED, now, &unsorted_keys),
            ),
            ("empty file", Vec::new()),
        ];
        for (label, bytes) in damaged {
            fs::write(&sidecar, &bytes).unwrap();
            assert_eq!(
                visit_keys(&sidecars, view.path(), "g"),
                (KeySource::Decoded { written: true }, expected.clone()),
                "{label}"
            );
            assert_eq!(
                visit_keys(&sidecars, view.path(), "g"),
                (KeySource::Sidecar, expected.clone()),
                "{label}: rewritten"
            );
        }
    }

    #[test]
    fn sidecar_positioned_reads_return_the_stored_keys() {
        let view = tempfile::tempdir().unwrap();
        let sidecars = ReferenceSidecars::new(Duration::ZERO);
        let keys: Vec<[u8; 32]> = (0..1000_u32)
            .map(|index| {
                let mut key = [0_u8; 32];
                key[..4].copy_from_slice(&index.to_be_bytes());
                key
            })
            .collect();
        write_manifest(view.path(), "g", &keys.concat());
        visit_keys(&sidecars, view.path(), "g");
        sidecars
            .visit(view.path(), "g", decode, &mut |run| {
                assert_eq!(run.len(), keys.len());
                let mut window = [[0_u8; 32]; 7];
                run.read(500, &mut window)?;
                assert_eq!(window, keys[500..507]);
                assert!(run.read(995, &mut window).is_err());
                Ok(())
            })
            .unwrap();
    }
}
