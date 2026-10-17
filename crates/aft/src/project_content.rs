//! Files that must not be opened as project source by indexers and walkers.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

fn storage_roots() -> &'static RwLock<BTreeSet<PathBuf>> {
    static ROOTS: OnceLock<RwLock<BTreeSet<PathBuf>>> = OnceLock::new();
    ROOTS.get_or_init(RwLock::default)
}

/// Retain resolved storage roots even after reconfiguration: old readers and
/// databases can remain alive while a new route starts using another root.
pub(crate) fn register_storage_root(path: &Path) {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(path)
    } else {
        return;
    };
    let canonical =
        crate::production_storage::canonicalized_with(&absolute, &|p| p.canonicalize().ok());
    let mut roots = storage_roots().write().unwrap_or_else(|p| p.into_inner());
    roots.insert(absolute);
    roots.insert(canonical);
}

/// SQLite uses `-shm` files as shared-memory indexes and protects them with
/// process-wide file locks on macOS and Linux. Closing any descriptor to one of
/// these files, even a read-only indexing probe, releases SQLite's locks. A
/// second connection can then shrink the shared file and crash the first with
/// SIGBUS when it reads the old mapping. Reject databases and their sidecars
/// before binary detection, which would already open and close them. Internal
/// storage is also excluded wholesale so its generated source and cache files
/// never feed back into project indexing.
pub(crate) fn is_excluded(path: &Path) -> bool {
    is_excluded_file(path) || is_storage_path(path)
}

fn is_storage_path(path: &Path) -> bool {
    storage_roots()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .any(|root| path.starts_with(root))
}

fn is_excluded_file(path: &Path) -> bool {
    crate::os_metadata::is_os_metadata_path(path)
        || has_sqlite_suffix(path) && !path.is_dir()
        || crate::db::file_identity::is_registered_file_path(path)
}

fn has_sqlite_suffix(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        let name = name.as_encoded_bytes();
        [
            b"-wal".as_slice(),
            b"-shm",
            b"-journal",
            b".db",
            b".sqlite",
            b".sqlite3",
        ]
        .iter()
        .any(|suffix| {
            name.len() >= suffix.len()
                && name[name.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
        })
    })
}

fn is_excluded_file_with_metadata(path: &Path, metadata: Option<&std::fs::Metadata>) -> bool {
    crate::os_metadata::is_os_metadata_path(path)
        || has_sqlite_suffix(path) && !metadata.is_some_and(std::fs::Metadata::is_dir)
        || crate::db::file_identity::is_registered_file_path(path)
}

/// Authorized task content is not project source, but must still be checked
/// before a pinned-directory permission probe opens and closes its descriptor.
pub(crate) fn is_sqlite_or_metadata(path: &Path) -> bool {
    is_excluded_file(path) || crate::db::file_identity::is_live_sqlite_file(path)
}

/// Only explicit, ownership-checked registered task artifacts may bypass the
/// storage root exclusion. SQLite names and aliases remain forbidden.
pub(crate) fn read_bounded(
    path: &Path,
    deadline: Option<std::time::Instant>,
    authorized_task_artifact: bool,
) -> std::io::Result<Vec<u8>> {
    if authorized_task_artifact {
        if is_sqlite_or_metadata(path) || std::fs::symlink_metadata(path)?.file_type().is_symlink()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "SQLite files and aliases are not readable task content",
            ));
        }
    } else {
        admit(path)?;
    }
    #[cfg(test)]
    record_open(path);
    crate::bounded_io::read(path, deadline)
}

/// Guard direct source reads as well as enumeration. Watcher deltas and queued
/// view work can hold paths admitted before storage was configured.
pub(crate) fn read(path: impl AsRef<Path>) -> std::io::Result<Vec<u8>> {
    let path = path.as_ref();
    admit(path)?;
    #[cfg(test)]
    record_open(path);
    std::fs::read(path)
}

pub(crate) fn read_to_string(path: impl AsRef<Path>) -> std::io::Result<String> {
    String::from_utf8(read(path)?)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

pub(crate) fn open(path: impl AsRef<Path>) -> std::io::Result<std::fs::File> {
    let path = path.as_ref();
    admit(path)?;
    #[cfg(test)]
    record_open(path);
    std::fs::File::open(path)
}

fn admit(path: &Path) -> std::io::Result<()> {
    let metadata = crate::db::file_identity::candidate_metadata(path).ok();
    if is_excluded_file_with_metadata(path, metadata.as_ref()) || is_storage_path(path) {
        return Err(excluded_error());
    }
    let excluded_alias = metadata
        .as_ref()
        .is_some_and(|metadata| metadata.file_type().is_symlink())
        && path.canonicalize().is_ok_and(|target| is_excluded(&target));
    if excluded_alias
        || crate::db::file_identity::is_live_sqlite_file_with_metadata(path, metadata.as_ref())
    {
        Err(excluded_error())
    } else {
        Ok(())
    }
}

fn excluded_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "internal storage and SQLite files are not project source",
    )
}

#[cfg(test)]
mod open_probe {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    type Opens = Arc<Mutex<Vec<PathBuf>>>;
    fn probes() -> &'static Mutex<BTreeMap<PathBuf, Opens>> {
        static PROBES: OnceLock<Mutex<BTreeMap<PathBuf, Opens>>> = OnceLock::new();
        PROBES.get_or_init(Mutex::default)
    }

    pub(crate) struct Probe {
        root: PathBuf,
        opens: Opens,
    }

    impl Probe {
        pub(crate) fn new(root: &Path) -> Self {
            let opens = Arc::new(Mutex::new(Vec::new()));
            probes()
                .lock()
                .unwrap()
                .insert(root.to_path_buf(), opens.clone());
            Self {
                root: root.to_path_buf(),
                opens,
            }
        }

        pub(crate) fn paths(&self) -> Vec<PathBuf> {
            self.opens.lock().unwrap().clone()
        }
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            probes().lock().unwrap().remove(&self.root);
        }
    }

    pub(crate) fn record_open(path: &Path) {
        for (root, opens) in probes().lock().unwrap().iter() {
            if path.starts_with(root) {
                opens.lock().unwrap().push(path.to_path_buf());
            }
        }
    }
}

#[cfg(test)]
pub(crate) use open_probe::{record_open, Probe};

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn ordinary_project_reads_do_not_stat_registered_databases() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let databases = (0..20)
            .map(|index| {
                crate::db::file_identity::IdentityConnection::open(
                    root.join(format!("database_{index}.cache")),
                    "source_read_stat_count",
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let source = root.join("source.rs");
        std::fs::write(&source, "fn source() {}\n").unwrap();
        let counts = crate::db::file_identity::stat_probe::count(|| {
            for _ in 0..1_000 {
                assert_eq!(read_to_string(&source).unwrap(), "fn source() {}\n");
            }
        });
        let candidate_stats = counts.get(&source).copied().unwrap_or(0);
        let registry_stats = counts
            .iter()
            .filter(|(path, _)| **path != source)
            .map(|(_, count)| count)
            .sum::<usize>();
        println!("1,000 source reads / 20 databases: {candidate_stats} candidate identity stats, {registry_stats} registry identity stats");
        assert_eq!(
            registry_stats, 0,
            "source reads stat registered database files"
        );
        assert_eq!(candidate_stats, 1_000);
        drop(databases);
    }

    #[test]
    fn sqlite_named_directories_remain_project_source() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut expected = Vec::new();
        for name in [
            "models.db",
            "models.SQLITE",
            "models.sqlite3",
            "event-wal",
            "event-shm",
            "event-journal",
        ] {
            let directory = root.join(name);
            std::fs::create_dir(&directory).unwrap();
            let source = directory.join("source.rs");
            std::fs::write(&source, "fn user_source() {}\n").unwrap();
            assert!(!is_excluded(&directory), "excluded directory {directory:?}");
            assert!(!is_excluded(&source));
            expected.push(source);
        }
        let storage = root.join("internal.db");
        std::fs::create_dir(&storage).unwrap();
        register_storage_root(&storage);
        std::fs::write(storage.join("internal.rs"), "fn internal() {}\n").unwrap();
        assert!(is_excluded(&storage));
        let mut walked = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
        walked.sort();
        expected.sort();
        assert_eq!(walked, expected);
        for name in [
            "fixture.db",
            "fixture.sqlite",
            "fixture.sqlite3",
            "fixture-wal",
            "fixture-shm",
            "fixture-journal",
        ] {
            let file = root.join(name);
            std::fs::write(&file, "not source").unwrap();
            assert!(is_excluded(&file), "admitted file {file:?}");
        }
    }

    #[test]
    fn direct_project_reads_reject_storage_and_sqlite_before_open() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let storage = root.join("storage");
        std::fs::create_dir_all(&storage).unwrap();
        register_storage_root(&storage);
        let probe = Probe::new(&root);
        for path in [
            storage.join("internal.rs"),
            root.join("live.db-shm"),
            root.join("LIVE.SQLITE-WAL"),
        ] {
            std::fs::write(&path, "valid UTF-8 must still not be opened").unwrap();
            assert!(read(&path).is_err(), "admitted {path:?}");
            assert!(open(&path).is_err(), "opened {path:?}");
            assert!(read_to_string(&path).is_err(), "decoded {path:?}");
            assert!(crate::inspect::scanners::source_text::read_source_text(&path).is_ok());
        }
        assert!(
            probe.paths().is_empty(),
            "opened excluded paths: {:?}",
            probe.paths()
        );
        let ordinary = root.join("storage-sibling.rs");
        std::fs::write(&ordinary, "fn user_source() {}\n").unwrap();
        assert!(!read(&ordinary).unwrap().is_empty());
        assert_eq!(probe.paths(), vec![ordinary]);
    }

    #[test]
    fn project_walks_and_watcher_skip_nested_storage() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let storage = root.join("state");
        std::fs::create_dir_all(storage.join("generated")).unwrap();
        register_storage_root(&storage);
        let internal = storage.join("generated/internal.rs");
        std::fs::write(&internal, "fn internal() {}\n").unwrap();
        let source = root.join("source.rs");
        std::fs::write(&source, "fn user_source() {}\n").unwrap();
        assert_eq!(
            crate::callgraph::walk_project_files(&root).collect::<Vec<_>>(),
            vec![source.clone()]
        );
        assert!(crate::watcher_filter::watcher_path_is_infra_skip_under(
            &root, &internal
        ));
        let probe = Probe::new(&root);
        let policy = crate::blob_store::v2::TrigramPolicy {
            max_file_size: 1024,
        };
        let walk = crate::views::live_delta::strict_walk(&root, &policy, 1);
        assert!(walk.gaps.is_empty(), "{:?}", walk.gaps);
        assert_eq!(probe.paths(), vec![source]);
    }

    #[cfg(unix)]
    #[test]
    fn late_created_shm_hard_link_is_refused_before_open() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let database = root.join("unconventional.cache");
        let connection =
            crate::db::file_identity::IdentityConnection::open(&database, "late_sidecar_read")
                .unwrap();
        let sidecar = root.join("unconventional.cache-shm");
        assert!(!sidecar.exists());
        connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE probe(value TEXT); INSERT INTO probe VALUES ('alive');").unwrap();
        assert!(sidecar.exists());
        let alias = root.join("innocent.rs");
        std::fs::hard_link(&sidecar, &alias).unwrap();
        let probe = Probe::new(&root);
        assert_eq!(
            read(&alias).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        let symlink = root.join("symlink.rs");
        std::os::unix::fs::symlink(&alias, &symlink).unwrap();
        assert_eq!(
            open(&symlink).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(probe.paths().is_empty());
        assert_eq!(
            connection
                .query_row("SELECT value FROM probe", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "alive"
        );
    }

    #[cfg(unix)]
    #[test]
    fn nonstandard_live_sqlite_names_are_excluded_by_identity() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let database = root.join("unconventional.cache");
        let connection = crate::db::lifecycle::TrackedConnection::open(
            &database,
            crate::db::lifecycle::SqliteStore::AftDb,
        )
        .unwrap();
        connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE probe(value TEXT); INSERT INTO probe VALUES ('alive');").unwrap();
        let probe = Probe::new(&root);
        assert!(is_excluded(&database));
        for (index, original) in [
            database.clone(),
            root.join("unconventional.cache-wal"),
            root.join("unconventional.cache-shm"),
        ]
        .iter()
        .enumerate()
        {
            let alias = root.join(format!("alias_{index}.rs"));
            std::fs::hard_link(original, &alias).unwrap();
            assert!(
                read(&alias).is_err(),
                "opened a live SQLite inode through {alias:?}"
            );
        }
        assert!(
            probe.paths().is_empty(),
            "opened live SQLite files: {:?}",
            probe.paths()
        );
        assert_eq!(
            connection
                .query_row("SELECT value FROM probe", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "alive"
        );
    }
}
