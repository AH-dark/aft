use super::*;
use std::io::{BufRead, BufReader, Read};
use std::process::Stdio;

pub(super) struct DerivedSeed {
    pub path: PathBuf,
    pub manifest: Manifest,
    _pin: crate::pins::QueryPin,
    _owner_pin: Option<crate::pins::QueryPin>,
}

pub(super) fn sibling_checkout_seed(
    request: &AssemblyRequest,
    deadline: Instant,
) -> Result<Option<DerivedSeed>> {
    let Some(roots) = worktree_roots(&request.project_root) else {
        return Ok(None);
    };
    let mut candidates = Vec::new();
    for root in roots {
        let scope = crate::path_identity::project_scope_key(&root);
        if scope == request.scope {
            continue;
        }
        let Some(view) = ViewStore::existing_dir(request.storage.join("views").join(&scope)) else {
            continue;
        };
        let Ok(Some(generation)) = view.current_generation_read_only() else {
            continue;
        };
        // The immutable manifest is written after derived materialization and
        // closure validation. Its modification time ranks completed generations,
        // rather than the name's timestamp, which precedes a potentially long build.
        let Some(published_at) = view
            .manifest_path(&generation)
            .ok()
            .and_then(|path| fs::metadata(path).ok())
            .and_then(|metadata| metadata.modified().ok())
        else {
            continue;
        };
        candidates.push((published_at, scope, root, view, generation));
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    for (_, scope, root, view, generation) in candidates {
        if crate::search_index::artifact_cache_key_read_only(&root, &request.storage)
            .ok()
            .as_deref()
            != Some(request.family.as_str())
        {
            continue;
        }
        let seed = (|| {
            let pin = crate::pins::QueryPin::acquire(view.view_dir(), &generation)
                .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
            match crate::callgraph_store::manifest_view_database_ready(
                view.view_dir(),
                &generation,
                deadline,
            ) {
                Ok(true) => {}
                Ok(false) => return Ok(None),
                Err(crate::callgraph_store::CallGraphStoreError::Sqlite(error)) => {
                    return Err(ViewError::Sqlite(error))
                }
                Err(error) => return Err(ViewError::InvalidManifest(error.to_string())),
            }
            let owner = view.derived_owner(&generation)?;
            let owner_pin = (owner != generation)
                .then(|| crate::pins::QueryPin::acquire(view.view_dir(), &owner))
                .transpose()
                .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
            let manifest = view.load_manifest(&owner)?;
            let path = view.derived_path(&generation)?;
            log::info!(
                "view derived seed: scope={} donor_scope={} generation={generation}",
                request.scope,
                scope
            );
            Ok(Some(DerivedSeed {
                path,
                manifest,
                _pin: pin,
                _owner_pin: owner_pin,
            }))
        })();
        match seed {
            Ok(Some(seed)) => return Ok(Some(seed)),
            Ok(None) => {}
            // An optional unreadable or busy sibling does not hide an older,
            // ready candidate. The checkout's own base admission still retries
            // contention rather than incorrectly declaring that base unusable.
            Err(error) => {
                log::debug!("view derived seed unavailable: donor_scope={scope} error={error}")
            }
        }
    }
    Ok(None)
}

/// Every worktree of the repository `root` belongs to, main worktree first.
pub(in crate::views) fn worktree_roots(root: &Path) -> Option<Vec<PathBuf>> {
    let child = crate::effective_path::new_command("git")
        .build()
        .arg("-C")
        .arg(root)
        .args(["worktree", "list", "--porcelain", "-z"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    read_worktree_child(child)
}

fn read_worktree_child(mut child: std::process::Child) -> Option<Vec<PathBuf>> {
    let roots = read_worktree_roots(child.stdout.take()?);
    if roots.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    // A valid prefix followed by a Git error is not a complete sibling census.
    child.wait().ok()?.success().then_some(roots?)
}

fn read_worktree_roots(reader: impl Read) -> Option<Vec<PathBuf>> {
    const MAX_BYTES: u64 = 4 * 1024 * 1024;
    const MAX_WORKTREES: usize = 4096;
    // Bound bytes before buffering and each record before allocating. An
    // incomplete topology is not enough evidence to choose the freshest donor.
    let mut reader = BufReader::new(reader.take(MAX_BYTES + 1));
    let mut roots = Vec::new();
    let mut consumed = 0;
    loop {
        let mut record = Vec::new();
        let count = reader
            .by_ref()
            .take(65_536)
            .read_until(0, &mut record)
            .ok()?;
        if count == 0 {
            return Some(roots);
        }
        consumed += count as u64;
        if consumed > MAX_BYTES || record.last() != Some(&0) {
            return None;
        }
        let Some(path) = record
            .strip_prefix(b"worktree ")
            .and_then(|record| record.strip_suffix(&[0]))
        else {
            continue;
        };
        if path.is_empty() || roots.len() == MAX_WORKTREES {
            return None;
        }
        #[cfg(unix)]
        let path = {
            use std::os::unix::ffi::OsStringExt;
            PathBuf::from(std::ffi::OsString::from_vec(path.to_vec()))
        };
        #[cfg(not(unix))]
        let path = PathBuf::from(std::str::from_utf8(path).ok()?);
        roots.push(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn donor_topology_rejects_a_valid_prefix_from_a_failed_command() {
        for (exit, accepted) in [(0, true), (1, false)] {
            let child = std::process::Command::new("sh")
                .args(["-c", &format!("printf 'worktree /main\\0'; exit {exit}")])
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            assert_eq!(
                read_worktree_child(child).is_some(),
                accepted,
                "exit={exit}"
            );
        }
    }

    #[test]
    fn donor_topology_reader_is_bounded_and_accepts_nul_delimited_paths() {
        assert_eq!(
            read_worktree_roots(
                &b"worktree /main checkout\nwith newline\0HEAD abc\0\0worktree /worker\0"[..]
            ),
            Some(vec![
                PathBuf::from("/main checkout\nwith newline"),
                PathBuf::from("/worker")
            ])
        );
        let mut overlong = std::io::Cursor::new(vec![b'x'; 100_000]);
        assert!(read_worktree_roots(&mut overlong).is_none());
        assert!(
            overlong.position() <= 65_536,
            "record overflow must stop at the iterator"
        );
        assert!(read_worktree_roots(&b"worktree \0"[..]).is_none());
        let excess = "worktree /root\0".repeat(4097);
        assert!(read_worktree_roots(excess.as_bytes()).is_none());
    }
}
