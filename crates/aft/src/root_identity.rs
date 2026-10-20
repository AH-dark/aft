//! A root's first bound identity survives missing Git and process restarts.
//! Path identities never become repository identities implicitly.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Identity {
    Path,
    Git {
        root_commit: String,
        toplevel: PathBuf,
        relative: Option<String>,
    },
}

/// Only a successful Git answer is fresh verification. Memo or saved-record
/// fallback may select an identity, but cannot certify new repository metadata.
pub(crate) struct IdentityChoice {
    pub(crate) identity: Identity,
    verified: bool,
}

impl IdentityChoice {
    pub(crate) fn verified(identity: Identity) -> Self {
        Self {
            identity,
            verified: true,
        }
    }

    pub(crate) fn retained(identity: Identity) -> Self {
        Self {
            identity,
            verified: false,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Record {
    version: u32,
    root: PathBuf,
    identity: Identity,
    // A background-verification hint, not evidence for changing the identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    probe_stamp: Option<String>,
}

pub(crate) fn canonical_root(root: &Path) -> PathBuf {
    crate::root_cache::canonical_root(root)
}

fn record_path(root: &Path, storage: &Path) -> PathBuf {
    let hash = blake3::hash(root.as_os_str().as_encoded_bytes());
    storage.join("root-identities").join(format!("{hash}.json"))
}

pub(crate) fn read(root: &Path, storage: &Path) -> io::Result<Option<Identity>> {
    Ok(read_record(root, storage)?.map(|record| record.identity))
}

fn read_record(root: &Path, storage: &Path) -> io::Result<Option<Record>> {
    let root = canonical_root(root);
    let path = record_path(&root, storage);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let record: Record = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if record.version != 1 || record.root != root {
        return Err(io::Error::other(
            "root identity record has an unsupported version or target",
        ));
    }
    record.identity.validate(&root)?;
    Ok(Some(record))
}

/// Serialize the first choice across processes and aliases. A corrupt record
/// is an error, not permission to choose a replacement identity.
pub(crate) fn bind(
    root: &Path,
    storage: &Path,
    choose: impl FnOnce(bool, Option<&Identity>) -> io::Result<IdentityChoice>,
) -> io::Result<Identity> {
    bind_with_recheck(root, storage, true, choose)
}

/// Real binds force verification; background passes may reuse an observed stamp.
pub(crate) fn bind_with_recheck(
    root: &Path,
    storage: &Path,
    force: bool,
    choose: impl FnOnce(bool, Option<&Identity>) -> io::Result<IdentityChoice>,
) -> io::Result<Identity> {
    let root = canonical_root(root);
    if let Some(record) = read_record(&root, storage)? {
        if reusable(&record, &root, force) {
            return Ok(record.identity);
        }
    }
    let path = record_path(&root, storage);
    let directory = path.parent().unwrap();
    crate::backup::create_private_durable_dir(directory)?;
    let lock_path = path.with_extension("lock");
    crate::production_storage::refuse_write(&lock_path)?;
    let mut options = crate::private_storage::options();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let lock = options.open(lock_path)?;
    lock.lock()?;
    let previous = read_record(&root, storage)?;
    if let Some(record) = &previous {
        if reusable(record, &root, force) {
            return Ok(record.identity.clone());
        }
    }
    // With Git available, resolve again under the record lock: a different
    // repository can replace a checkout without changing its canonical path.
    let before = crate::developer_tools::git_usable()
        .then(|| crate::search_index::repository_identity_stamp(&root))
        .flatten();
    let choice = select(&root, storage, |pinned| {
        choose(pinned, previous.as_ref().map(|record| &record.identity))
    })?;
    let identity = choice.identity;
    identity.validate(&root)?;
    if !choice.verified
        && previous
            .as_ref()
            .is_some_and(|record| record.identity != identity)
    {
        return Err(io::Error::other(
            "an unverified choice cannot replace a saved root identity",
        ));
    }
    let probe_stamp = if choice.verified {
        if matches!(&identity, Identity::Git { .. }) {
            before.filter(|stamp| {
                Some(stamp) == crate::search_index::repository_identity_stamp(&root).as_ref()
            })
        } else {
            None
        }
    } else {
        // Retain an older certificate, never the metadata from this failed check.
        previous
            .as_ref()
            .filter(|record| record.identity == identity)
            .and_then(|record| record.probe_stamp.clone())
    };
    if previous
        .as_ref()
        .is_some_and(|record| record.identity == identity && record.probe_stamp == probe_stamp)
    {
        return Ok(identity);
    }
    let bytes = serde_json::to_vec(&Record {
        version: 1,
        root,
        identity: identity.clone(),
        probe_stamp,
    })
    .map_err(io::Error::other)?;
    crate::production_storage::refuse_write(&path)?;
    crate::durability::atomic_write(
        &path,
        &bytes,
        crate::durability::AtomicWriteDurability::FileAndDirectory,
    )?;
    Ok(identity)
}

fn reusable(record: &Record, root: &Path, force: bool) -> bool {
    matches!(&record.identity, Identity::Path)
        || !crate::developer_tools::git_usable()
        || (!force
            && record.probe_stamp.as_ref().is_some_and(|stamp| {
                crate::search_index::repository_identity_stamp(root).as_ref() == Some(stamp)
            }))
}

/// Older standing entries already pin an identity even before this record
/// format existed. Import path pins; refuse a repository downgrade or mismatch.
pub(crate) fn select(
    root: &Path,
    storage: &Path,
    choose: impl FnOnce(bool) -> io::Result<IdentityChoice>,
) -> io::Result<IdentityChoice> {
    let canonical = canonical_root(root);
    let root = canonical.as_path();
    let database = storage.join("aft.db");
    let mut pins = Vec::new();
    if database.try_exists()? {
        let connection = crate::db::open_readonly(&database).map_err(io::Error::other)?;
        let exists: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='standing_roots')", [], |row| row.get(0)).map_err(io::Error::other)?;
        if exists {
            let mut statement = connection.prepare("SELECT resolved_git_toplevel, scoped_relative_path FROM standing_roots WHERE resolved_target = ?1").map_err(io::Error::other)?;
            pins = statement
                .query_map([root.to_string_lossy().as_ref()], |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                })
                .map_err(io::Error::other)?
                .collect::<Result<_, _>>()
                .map_err(io::Error::other)?;
        }
    }
    let choice = choose(pins.iter().any(|(toplevel, _)| toplevel.is_none()))?;
    let (toplevel, relative) = choice.identity.topology();
    let toplevel = toplevel.map(|path| path.to_string_lossy().into_owned());
    if pins
        .iter()
        .any(|pin| pin != &(toplevel.clone(), relative.clone()))
    {
        return Err(io::Error::other("the existing standing repository identity cannot be recovered; Git unavailability does not authorize a path identity"));
    }
    Ok(choice)
}

impl Identity {
    pub(crate) fn git(root: &Path, root_commit: String) -> io::Result<Self> {
        let root = canonical_root(root);
        let toplevel = nearest_git_marker(&root)?.unwrap_or_else(|| root.clone());
        let relative = if root == toplevel {
            None
        } else {
            Some(
                crate::scoped_key::scoped_relative_path(&root, &toplevel)
                    .map_err(io::Error::other)?,
            )
        };
        Ok(Self::Git {
            root_commit,
            toplevel,
            relative,
        })
    }

    fn validate(&self, root: &Path) -> io::Result<()> {
        if let Self::Git {
            root_commit,
            toplevel,
            relative,
        } = self
        {
            if root_commit.trim().is_empty() {
                return Err(io::Error::other("root identity has no repository identity"));
            }
            let expected = if root == toplevel {
                None
            } else {
                Some(
                    crate::scoped_key::scoped_relative_path(root, toplevel)
                        .map_err(io::Error::other)?,
                )
            };
            if relative != &expected {
                return Err(io::Error::other(
                    "root identity has inconsistent repository scope",
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn key(&self, root: &Path, standing: bool) -> String {
        match self {
            Self::Path => crate::search_index::artifact_path_identity_key(root),
            Self::Git {
                root_commit,
                relative: None,
                ..
            } => crate::search_index::artifact_key_from_git_identity(root_commit),
            Self::Git {
                root_commit,
                relative: Some(relative),
                ..
            } => {
                let key = crate::scoped_key::scoped_v1_key(root_commit, relative.as_bytes())
                    .expect("validated root identity scope");
                if standing {
                    key
                } else {
                    key[..16].to_string()
                }
            }
        }
    }

    pub(crate) fn topology(&self) -> (Option<PathBuf>, Option<String>) {
        match self {
            Self::Path => (None, None),
            Self::Git {
                toplevel, relative, ..
            } => (Some(toplevel.clone()), relative.clone()),
        }
    }
}

/// Inspect ancestors without invoking Git, including linked-worktree markers.
/// An unreadable marker cannot prove that a folder has no repository identity.
pub(crate) fn nearest_git_marker(root: &Path) -> io::Result<Option<PathBuf>> {
    for directory in root.ancestors() {
        match std::fs::symlink_metadata(directory.join(".git")) {
            Ok(_) => return Ok(Some(directory.to_path_buf())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_choice_cannot_replace_a_saved_identity() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        crate::developer_tools::with_git_usable_for_test(true, || {
            let first = bind(root.path(), storage.path(), |_, _| {
                Identity::git(root.path(), "a".repeat(40)).map(IdentityChoice::verified)
            })
            .unwrap();
            let result = bind(root.path(), storage.path(), |_, _| {
                Identity::git(root.path(), "b".repeat(40)).map(IdentityChoice::retained)
            });
            assert!(result.is_err());
            assert_eq!(
                read(root.path(), storage.path())
                    .unwrap()
                    .unwrap()
                    .key(root.path(), false),
                first.key(root.path(), false)
            );
        });
    }

    #[test]
    fn legacy_standing_repository_pin_is_not_downgraded() {
        let root = tempfile::tempdir().unwrap();
        let root = canonical_root(root.path());
        std::fs::create_dir(root.join(".git")).unwrap();
        let storage = tempfile::tempdir().unwrap();
        let record = crate::db::standing_roots::StandingRootRecord {
            literal_path: root.display().to_string(),
            resolved_target: root.display().to_string(),
            resolved_git_toplevel: Some(root.display().to_string()),
            scoped_relative_path: None,
        };
        let mut database = crate::db::open(&storage.path().join("aft.db")).unwrap();
        crate::db::standing_roots::ensure_standing_root(
            &mut database,
            &record,
            &[crate::config::IndexKind::Search],
        )
        .unwrap();
        crate::developer_tools::with_git_usable_for_test(false, || {
            let error = crate::search_index::artifact_cache_key_with_memo(
                &root,
                &root,
                storage.path(),
                None,
            )
            .unwrap_err();
            assert!(error
                .detail()
                .contains("existing standing repository identity cannot be recovered"));
            assert!(read(&root, storage.path()).unwrap().is_none());
        });
        assert_eq!(
            crate::db::standing_roots::get_standing_root(&database, &record.literal_path).unwrap(),
            Some(record)
        );
    }

    #[test]
    fn competing_first_binds_share_one_durable_choice() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let start = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let handles: Vec<_> = [
                Identity::Path,
                Identity::git(root.path(), "a".repeat(40)).unwrap(),
            ]
            .into_iter()
            .map(|choice| {
                let (calls, start) = (&calls, &start);
                let (root, storage) = (root.path(), storage.path());
                scope.spawn(move || {
                    start.wait();
                    // Without Git, both contenders must reuse the first choice.
                    crate::developer_tools::with_git_usable_for_test(false, || {
                        bind(root, storage, |_, _| {
                            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            Ok(IdentityChoice::retained(choice))
                        })
                    })
                    .unwrap()
                    .key(root, false)
                })
            })
            .collect();
            let keys: Vec<_> = handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect();
            assert_eq!(keys[0], keys[1]);
        });
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn corrupt_identity_is_refused_without_replacement() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        bind(root.path(), storage.path(), |_, _| {
            Identity::git(root.path(), "a".repeat(40)).map(IdentityChoice::retained)
        })
        .unwrap();
        let path = record_path(&canonical_root(root.path()), storage.path());
        std::fs::write(&path, b"broken identity").unwrap();
        assert!(bind(root.path(), storage.path(), |_, _| panic!(
            "a corrupt repository record must not select a new identity"
        ))
        .is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"broken identity");
    }

    #[test]
    fn observing_identity_does_not_create_storage() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("not-created");
        crate::developer_tools::with_git_usable_for_test(false, || {
            assert!(crate::scoped_key::resolve_standing_root_in_storage(
                root.path().to_str().unwrap(),
                &storage,
                false
            )
            .is_ok());
        });
        assert!(!storage.exists());
    }

    #[cfg(unix)]
    #[test]
    fn canonical_alias_reuses_the_record() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let storage = directory.path().join("storage");
        let first = bind(&root, &storage, |_, _| {
            Ok(IdentityChoice::retained(Identity::Path))
        })
        .unwrap();
        let second = bind(&alias, &storage, |_, _| {
            panic!("alias must not choose another identity")
        })
        .unwrap();
        assert_eq!(first.key(&root, false), second.key(&alias, false));
    }
}
