use super::*;
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(test)]
thread_local! {
    static PATH_COMPARISONS: std::cell::Cell<(usize, Option<usize>)> = const { std::cell::Cell::new((0, None)) };
}

#[inline]
fn record_path_comparison() {
    #[cfg(test)]
    PATH_COMPARISONS.with(|stats| {
        let (count, limit) = stats.get();
        let count = count + 1;
        stats.set((count, limit));
        assert!(
            limit.is_none_or(|limit| count <= limit),
            "watcher delta comparison budget exceeded: {count}"
        );
    });
}

fn compare_paths(left: &[u8], right: &[u8]) -> std::cmp::Ordering {
    record_path_comparison();
    left.cmp(right)
}

#[derive(Default)]
/// Byte-sorted sets shared with the HEAD listing. Each changed path needs at
/// most two logarithmic lookups, even for a checkout-wide watcher batch.
pub(super) struct HeadPaths {
    members: Vec<Vec<u8>>,
    directories: Vec<Vec<u8>>,
}

impl HeadPaths {
    fn new(head: &[crate::alias::TrackedPath]) -> Self {
        let mut paths = Self::default();
        for entry in head {
            paths.members.push(entry.rel_path.clone());
            for (index, byte) in entry.rel_path.iter().enumerate() {
                if *byte == b'/' {
                    paths.directories.push(entry.rel_path[..index].to_vec());
                }
            }
        }
        for values in [&mut paths.members, &mut paths.directories] {
            values.sort_unstable_by(|left, right| compare_paths(left, right));
            values.dedup_by(|left, right| compare_paths(left, right).is_eq());
            values.shrink_to_fit();
        }
        paths
    }

    fn contains(values: &[Vec<u8>], path: &[u8]) -> bool {
        values
            .binary_search_by(|value| compare_paths(value, path))
            .is_ok()
    }
}

pub(super) fn delta_requires_full_read(
    head: &[crate::alias::TrackedPath],
    paths: &OnceLock<HeadPaths>,
    changed: &BTreeSet<Vec<u8>>,
) -> bool {
    let paths = paths.get_or_init(|| HeadPaths::new(head));
    changed.iter().any(|path| {
        HeadPaths::contains(&paths.directories, path) || !HeadPaths::contains(&paths.members, path)
    })
}

type RemovalStamp = (Vec<u8>, i64);

#[derive(Clone, PartialEq, Eq)]
pub(super) struct StoreStamps(RemovalStamp, RemovalStamp);

impl StoreStamps {
    pub(super) fn read(callgraph: &BlobStore, semantic: &BlobStore) -> Result<Self> {
        Ok(Self(
            callgraph.removal_stamp().map_err(store_error)?,
            semantic.removal_stamp().map_err(store_error)?,
        ))
    }
}

fn store_error(error: crate::blob_store::BlobStoreError) -> ViewError {
    ViewError::InvalidManifest(error.to_string())
}

#[derive(Clone)]
pub(super) struct CachedManifest {
    pub generation: String,
    pub manifest: Manifest,
    pub stamps: StoreStamps,
    pub tree: String,
    pub metadata: Arc<BTreeMap<Vec<u8>, fs::Metadata>>,
    pub admission: crate::db::file_identity::AdmissionSnapshot,
    pub head: Option<HeadSnapshot>,
}

#[derive(Clone)]
pub(super) struct HeadSnapshot {
    tree: String,
    index: IndexStamp,
    entries: Arc<Vec<crate::alias::TrackedPath>>,
    paths: Arc<OnceLock<HeadPaths>>,
}

pub(super) struct HeadRead {
    pub tree: String,
    pub entries: Arc<Vec<crate::alias::TrackedPath>>,
    pub cached: bool,
    pub snapshot: Option<HeadSnapshot>,
    pub paths: Arc<OnceLock<HeadPaths>>,
}

#[derive(Default)]
struct RootCache {
    previous: Option<CachedManifest>,
    head: Option<HeadSnapshot>,
    publications: u64,
    last_full: Option<Instant>,
    mismatch_pending: bool,
    active_preparations: usize,
}

#[derive(Clone, PartialEq, Eq)]
struct IndexStamp {
    path: PathBuf,
    len: u64,
    modified: std::time::SystemTime,
}

fn index_stamp(request: &AssemblyRequest) -> Option<IndexStamp> {
    let output = std::process::Command::new("git")
        .current_dir(&request.project_root)
        .args(["rev-parse", "--git-path", "index"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let bytes = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    let path = request.project_root.join(path_from_bytes(bytes));
    let metadata = fs::metadata(&path).ok()?;
    Some(IndexStamp {
        path,
        len: metadata.len(),
        modified: metadata.modified().ok()?,
    })
}

fn caches() -> &'static Mutex<BTreeMap<PathBuf, RootCache>> {
    static CACHES: OnceLock<Mutex<BTreeMap<PathBuf, RootCache>>> = OnceLock::new();
    CACHES.get_or_init(Default::default)
}

pub(super) fn previous(view: &ViewStore, generation: Option<&str>) -> Option<CachedManifest> {
    caches()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(view.view_dir())?
        .previous
        .as_ref()
        .filter(|cached| Some(cached.generation.as_str()) == generation)
        .cloned()
}

pub(super) fn remember(view: &ViewStore, cached: CachedManifest, full_verified: bool) {
    let mut caches = caches().lock().unwrap_or_else(|e| e.into_inner());
    let cache = caches.entry(view.view_dir().to_path_buf()).or_default();
    cache.head = cached.head.clone();
    cache.previous = Some(cached);
    if full_verified {
        cache.publications = 0;
        cache.last_full = Some(Instant::now());
        cache.mismatch_pending = false;
    }
}

/// Rescans also refresh attributes: check-attr reads the index, whose attributes
/// can change without a new HEAD tree. Attribute edits and index replacements
/// must not reuse that cache, even when the watcher reports only a source edit.
pub(super) fn head(request: &AssemblyRequest, view: &ViewStore) -> Result<HeadRead> {
    let output = std::process::Command::new("git")
        .current_dir(&request.project_root)
        .args(["rev-parse", "HEAD^{tree}"])
        .output()?;
    if !output.status.success() {
        return Err(ViewError::InvalidManifest(
            "cannot resolve HEAD tree".into(),
        ));
    }
    let tree = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let index = index_stamp(request);
    let eligible = !request.changed_paths.is_empty()
        && index.is_some()
        && !request.changed_paths.iter().any(|path| {
            path.rsplit(|byte| *byte == b'/').next() == Some(b".gitattributes".as_slice())
        });
    if eligible {
        if let Some(snapshot) = caches()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(view.view_dir())
            .and_then(|cache| cache.head.clone())
        {
            if snapshot.tree == tree && Some(&snapshot.index) == index.as_ref() {
                return Ok(HeadRead {
                    tree,
                    entries: snapshot.entries.clone(),
                    paths: snapshot.paths.clone(),
                    cached: true,
                    snapshot: Some(snapshot),
                });
            }
        }
    }
    let entries = Arc::new(
        head_tree_entries(&request.project_root)
            .map_err(|error| ViewError::InvalidManifest(error.to_string()))?,
    );
    let paths = Arc::new(OnceLock::new());
    // This listing is only a candidate. A failed or cancelled preparation must
    // leave the served manifest's index invalidation pending for the retry.
    let snapshot = index.map(|index| HeadSnapshot {
        tree: tree.clone(),
        index,
        entries: entries.clone(),
        paths: paths.clone(),
    });
    Ok(HeadRead {
        tree,
        entries,
        cached: false,
        snapshot,
        paths,
    })
}

pub(super) struct PreparationVerification {
    view_dir: PathBuf,
    pub due: bool,
}

impl Drop for PreparationVerification {
    fn drop(&mut self) {
        if let Some(cache) = caches()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&self.view_dir)
        {
            cache.active_preparations = cache.active_preparations.saturating_sub(1);
        }
    }
}

pub(super) fn begin_preparation(view: &ViewStore) -> PreparationVerification {
    let mut caches = caches().lock().unwrap_or_else(|e| e.into_inner());
    let cache = caches.entry(view.view_dir().to_path_buf()).or_default();
    cache.publications = cache.publications.saturating_add(1);
    cache.active_preparations += 1;
    PreparationVerification {
        view_dir: view.view_dir().to_path_buf(),
        due: full_verification_required(cache, Instant::now()),
    }
}

const FULL_VERIFICATION_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10 * 60);

// Failed preparations leave both the count and deadline due. Only remembering
// a successfully served full result (including an identical no-op) renews them.
fn full_verification_required(cache: &RootCache, now: Instant) -> bool {
    cache.publications >= 64
        || cache
            .last_full
            .is_none_or(|last| now.saturating_duration_since(last) >= FULL_VERIFICATION_INTERVAL)
}

pub(super) fn full_verification_due(view_dir: &Path) -> bool {
    let caches = match caches().try_lock() {
        Ok(caches) => caches,
        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return true,
    };
    caches.get(view_dir).is_none_or(|cache| {
        cache.active_preparations == 0 && full_verification_required(cache, Instant::now())
    })
}

pub(super) fn note_mismatch(view: &ViewStore) -> Option<u64> {
    let mut caches = caches().lock().unwrap_or_else(|e| e.into_inner());
    let cache = caches.entry(view.view_dir().to_path_buf()).or_default();
    if cache.mismatch_pending {
        return None;
    }
    cache.mismatch_pending = true;
    Some(VERIFICATION_MISMATCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1)
}

#[cfg(test)]
pub(super) fn expire_verification_for_test(view_dir: &Path) {
    let mut caches = caches().lock().unwrap();
    let cache = caches.entry(view_dir.to_path_buf()).or_default();
    cache.last_full = Some(Instant::now() - FULL_VERIFICATION_INTERVAL);
}

pub(super) static VERIFICATION_MISMATCHES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(super) fn full_candidates(
    reader: &CandidateReader<'_>,
    head: &[crate::alias::TrackedPath],
    skips: &mut Vec<(Vec<u8>, Option<SkipReason>)>,
    reads: &mut std::collections::HashSet<Vec<u8>>,
    phase: &mut impl FnMut(&str) -> Result<()>,
) -> Result<Vec<Candidate>> {
    let mut candidates = Vec::new();
    for tracked in head.iter().cloned() {
        let path = RelPath::new(tracked.rel_path.clone())?;
        let absolute = reader
            .request
            .project_root
            .join(path_from_bytes(&tracked.rel_path));
        if crate::project_content::is_excluded(&absolute) {
            continue;
        }
        if matches!(tracked.mode, GitMode::Regular { .. } | GitMode::Symlink)
            && working_tree_io(
                fs::symlink_metadata(&absolute),
                "reading metadata for",
                &absolute,
            )?
            .is_none()
        {
            continue;
        }
        let entry = match tracked.mode {
            GitMode::Regular { executable } => {
                phase("working_tree_read")?;
                let Some(source) = read_working_tree_file(&absolute)? else {
                    continue;
                };
                reads.insert(tracked.rel_path.clone());
                let (candidate, skip) =
                    reader.regular(path, &absolute, executable, source, Some(tracked), true)?;
                skips.extend(skip);
                candidates.push(candidate);
                continue;
            }
            GitMode::Symlink => {
                phase("working_tree_read")?;
                let Some(target) = read_symlink_bytes(&absolute)? else {
                    continue;
                };
                ManifestEntry::Symlink {
                    target_bytes: ByteString::new(target),
                }
            }
            GitMode::Gitlink => ManifestEntry::Gitlink {
                oid: tracked.git_oid.to_hex(),
            },
            GitMode::Other(_) => {
                skips.push((tracked.rel_path, Some(SkipReason::UnsupportedGitMode)));
                continue;
            }
        };
        candidates.push(Candidate {
            path,
            entry,
            key: None,
            payload: None,
            tracked: None,
            source: None,
        });
    }
    Ok(candidates)
}

/// The closure borrows the stores already opened by assembly. In particular,
/// reading removal counters never opens another descriptor on a live database.
pub(super) struct StoreClosure<'a> {
    pub callgraph: &'a BlobStore,
    pub semantic: &'a BlobStore,
    pub trigram: PathBuf,
    pub trusted: Option<(&'a Manifest, StoreStamps)>,
}

impl PublicationClosure for StoreClosure<'_> {
    fn contains_blob(&self, plane: ArtifactPlane, key: &str) -> Result<bool> {
        let store = if plane == ArtifactPlane::Semantic {
            self.semantic
        } else {
            self.callgraph
        };
        Ok(store
            .missing_hex_keys([key], None)
            .map_err(store_error)?
            .missing
            .is_empty())
    }

    fn probe_blobs(&self, keys: &[(ArtifactPlane, &str)]) -> Result<()> {
        #[cfg(test)]
        let started = Instant::now();
        #[cfg(test)]
        let before = crate::blob_store::MEMBERSHIP_PROBE_STATS.with(|stats| stats.get().queries);
        // This runs under the publication durability barrier, after assembly's
        // puts. A removal in the meantime revokes the entire base's proof.
        let stamps = StoreStamps::read(self.callgraph, self.semantic)?;
        let old = self
            .trusted
            .as_ref()
            .filter(|(_, before)| *before == stamps)
            .map(|(manifest, _)| {
                manifest
                    .plane_keys()
                    .map(|(plane, key)| (plane == ArtifactPlane::Semantic, key))
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        for plane in [ArtifactPlane::Semantic, ArtifactPlane::Callgraph] {
            let store = if plane == ArtifactPlane::Semantic {
                self.semantic
            } else {
                self.callgraph
            };
            let missing = store
                .missing_hex_keys(
                    keys.iter()
                        .filter(|(p, key)| {
                            *p == plane && !old.contains(&(*p == ArtifactPlane::Semantic, *key))
                        })
                        .map(|(_, key)| *key),
                    None,
                )
                .map_err(store_error)?
                .missing;
            if let Some((_, key)) = keys
                .iter()
                .find(|(p, key)| *p == plane && missing.contains(*key))
            {
                return Err(ViewError::MissingBlob {
                    plane,
                    key: (*key).to_owned(),
                });
            }
        }
        #[cfg(test)]
        CLOSURE_PROBE_STATS.with(|stats| {
            let after = crate::blob_store::MEMBERSHIP_PROBE_STATS.with(|stats| stats.get().queries);
            crate::blob_store::ProbeStats::add(stats, after - before, started);
        });
        Ok(())
    }

    fn trigram_is_present(&self) -> Result<bool> {
        Ok(self.trigram.is_file())
    }
    fn contains_alias(&self, _git_oid: &str) -> Result<bool> {
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn views_large_watcher_delta_has_bounded_path_comparisons() {
        let oid = crate::alias::GitOid::from_bytes(&[0; 20]).unwrap();
        let head = (0..60_000)
            .map(|index| crate::alias::TrackedPath {
                rel_path: format!("src/p{:04}/f{index:05}.rs", index / 100).into_bytes(),
                mode: GitMode::Regular { executable: false },
                git_oid: oid,
                filtered: false,
                present_in_previous_generation: true,
                tracked: true,
            })
            .collect::<Vec<_>>();
        let changed = head
            .iter()
            .skip(head.len() - 4096)
            .map(|entry| entry.rel_path.clone())
            .collect::<BTreeSet<_>>();
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                PATH_COMPARISONS.with(|stats| stats.set((0, None)));
            }
        }
        let _reset = Reset;
        PATH_COMPARISONS.with(|stats| stats.set((0, Some(4_000_000))));
        let paths = OnceLock::new();
        assert!(!delta_requires_full_read(&head, &paths, &changed));
        let comparisons = PATH_COMPARISONS.with(|stats| stats.get().0);
        assert!(
            comparisons > changed.len(),
            "the counter must observe actual path comparisons"
        );
        println!(
            "watcher_delta head={} changed={} comparisons={comparisons}",
            head.len(),
            changed.len()
        );
        PATH_COMPARISONS.with(|stats| stats.set((0, Some(200_000))));
        assert!(!delta_requires_full_read(&head, &paths, &changed));
        println!(
            "watcher_delta warm_comparisons={}",
            PATH_COMPARISONS.with(|stats| stats.get().0)
        );
    }

    #[test]
    fn views_full_rebuild_skips_delta_comparison_index() {
        let (_project, storage, request) = fixture();
        let view = ViewStore::open(storage.path(), &request.scope).unwrap();
        assert!(caches()
            .lock()
            .unwrap()
            .get(view.view_dir())
            .unwrap()
            .head
            .as_ref()
            .unwrap()
            .paths
            .get()
            .is_none());
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                PATH_COMPARISONS.with(|stats| stats.set((0, None)));
            }
        }
        let _reset = Reset;
        PATH_COMPARISONS.with(|stats| stats.set((0, Some(0))));
        publish(&request);
        assert_eq!(PATH_COMPARISONS.with(|stats| stats.get().0), 0);
    }

    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn fixture() -> (tempfile::TempDir, tempfile::TempDir, AssemblyRequest) {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        git(project.path(), &["init", "--quiet"]);
        for i in 0..8 {
            fs::write(
                project.path().join(format!("f{i}.rs")),
                format!("pub fn f{i}() {{}}\n"),
            )
            .unwrap();
        }
        git(project.path(), &["add", "."]);
        git(project.path(), &["commit", "--quiet", "-m", "base"]);
        let request = AssemblyRequest {
            project_root: project.path().to_path_buf(),
            storage: storage.path().to_path_buf(),
            family: "incremental".into(),
            scope: "incremental".into(),
            desired_head: "head".into(),
            changed_paths: BTreeSet::new(),
            semantic_keys: BTreeMap::new(),
            require_semantic: false,
            allow_blob_put: true,
            callgraph: true,
        };
        publish(&request);
        (project, storage, request)
    }

    fn publish(request: &AssemblyRequest) -> Manifest {
        let mut prepared = prepare_checkout_deferring(request, true, &mut |_| Ok(())).unwrap();
        let manifest = prepared.report().manifest.clone().unwrap_or_else(|| {
            ViewStore::open(&request.storage, &request.scope)
                .unwrap()
                .load_manifest(prepared.report().generation.as_deref().unwrap())
                .unwrap()
        });
        prepared.commit().unwrap();
        prepared.settle_status();
        manifest
    }

    fn rebuild_manifest(request: &AssemblyRequest) -> Manifest {
        let callgraph =
            BlobStore::open(&request.storage, &request.family, BlobPlane::Callgraph).unwrap();
        let semantic =
            BlobStore::open(&request.storage, &request.family, BlobPlane::Semantic).unwrap();
        let reader = CandidateReader {
            request,
            previous: None,
            callgraph: &callgraph,
        };
        let mut skips = Vec::new();
        let mut reads = std::collections::HashSet::new();
        let head = head_tree_entries(&request.project_root).unwrap();
        let mut candidates =
            full_candidates(&reader, &head, &mut skips, &mut reads, &mut |_| Ok(())).unwrap();
        heal_missing_payloads(
            &reader,
            &semantic,
            &mut candidates,
            &mut skips,
            &mut reads,
            None,
            &mut |_| Ok(()),
        )
        .unwrap();
        Manifest::new(
            candidates
                .into_iter()
                .map(|candidate| (candidate.path, candidate.entry)),
        )
        .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn views_incremental_reuses_manifest_and_head_without_decoding() {
        let (project, _storage, mut request) = fixture();
        crate::views::take_manifest_decodes();
        CLOSURE_PROBE_STATS.with(|stats| stats.set(Default::default()));
        let heads = crate::alias::head_tree_entry_calls_for_test();
        fs::write(project.path().join("f0.rs"), "pub fn edited() {}\n").unwrap();
        request.changed_paths.insert(b"f0.rs".to_vec());
        let mut prepared = None;
        let stats = crate::db::file_identity::stat_probe::count_candidates(|| {
            prepared = Some(prepare_checkout_deferring(&request, true, &mut |_| Ok(())).unwrap());
        });
        let mut prepared = prepared.unwrap();
        assert_eq!(crate::views::take_manifest_decodes(), 0);
        assert_eq!(crate::alias::head_tree_entry_calls_for_test(), heads);
        assert_eq!(CLOSURE_PROBE_STATS.with(|stats| stats.get().queries), 1);
        assert!(
            !stats.contains_key(&project.path().join("f1.rs")),
            "unchanged regular file was statted: {stats:?}"
        );
        assert!(prepared.commit().unwrap().published);
    }

    #[test]
    fn views_incremental_index_attribute_change_refreshes_head_cache() {
        let (project, storage, mut request) = fixture();
        let view = ViewStore::open(storage.path(), &request.scope).unwrap();
        request.changed_paths.insert(b"f0.rs".to_vec());
        assert!(head(&request, &view).unwrap().cached);
        fs::write(project.path().join(".gitattributes"), "*.rs filter=lfs\n").unwrap();
        git(project.path(), &["add", ".gitattributes"]);
        let head = head(&request, &view).unwrap();
        assert!(
            !head.cached,
            "staged attributes changed without a HEAD move"
        );
        assert!(head.entries.iter().all(|entry| entry.filtered));
    }

    #[test]
    fn views_failed_preparation_does_not_consume_index_invalidation() {
        let (project, _storage, mut request) = fixture();
        fs::write(project.path().join("f0.rs"), "pub fn dirty() {}\n").unwrap();
        request.changed_paths.insert(b"f0.rs".to_vec());
        publish(&request);
        git(project.path(), &["reset", "--quiet", "--hard", "HEAD"]);
        // A same-tree index rewrite must remain unvalidated after failure.
        fs::write(project.path().join(".gitattributes"), "*.rs text\n").unwrap();
        git(project.path(), &["add", ".gitattributes"]);
        fs::write(project.path().join("f1.rs"), "pub fn reported() {}\n").unwrap();
        request.changed_paths = BTreeSet::from([b"f1.rs".to_vec()]);
        let failure = prepare_checkout_deferring(&request, true, &mut |phase| {
            if phase == "working_tree_read" {
                Err(ViewError::InvalidManifest(
                    "injected post-HEAD failure".into(),
                ))
            } else {
                Ok(())
            }
        });
        assert!(failure.is_err());
        assert_eq!(
            publish(&request).to_json_bytes().unwrap(),
            rebuild_manifest(&request).to_json_bytes().unwrap(),
            "the retry must read the reset file, not reuse its pre-reset dirty key"
        );
    }

    #[test]
    fn views_head_snapshot_waits_for_commit_even_for_a_no_op() {
        let (project, storage, mut request) = fixture();
        let view = ViewStore::open(storage.path(), &request.scope).unwrap();
        let before = caches()
            .lock()
            .unwrap()
            .get(view.view_dir())
            .unwrap()
            .head
            .as_ref()
            .unwrap()
            .index
            .clone();
        fs::write(project.path().join(".gitattributes"), "*.rs text\n").unwrap();
        git(project.path(), &["add", ".gitattributes"]);
        request.changed_paths.insert(b"f0.rs".to_vec());
        let prepared = prepare_checkout_deferring(&request, true, &mut |_| Ok(())).unwrap();
        assert!(
            caches()
                .lock()
                .unwrap()
                .get(view.view_dir())
                .unwrap()
                .head
                .as_ref()
                .unwrap()
                .index
                == before
        );
        drop(prepared);
        assert!(!head(&request, &view).unwrap().cached);
        let mut prepared = prepare_checkout_deferring(&request, true, &mut |_| Ok(())).unwrap();
        assert!(!prepared.commit().unwrap().published);
        assert!(head(&request, &view).unwrap().cached);
    }

    #[cfg(unix)]
    #[test]
    fn views_incremental_cached_identity_rejects_new_live_database_alias() {
        let (project, storage, mut request) = fixture();
        let database = storage.path().join("inactive.sqlite");
        Connection::open(&database)
            .unwrap()
            .execute_batch("CREATE TABLE fixture(value)")
            .unwrap();
        fs::copy(&database, project.path().join("f7.rs")).unwrap();
        publish(&request);
        let alias = storage.path().join("live.sqlite");
        fs::hard_link(project.path().join("f7.rs"), &alias).unwrap();
        let _live =
            crate::db::file_identity::IdentityConnection::open(&alias, "manifest identity test")
                .unwrap();
        fs::write(project.path().join("f0.rs"), "pub fn edited() {}\n").unwrap();
        request.changed_paths.insert(b"f0.rs".to_vec());
        let manifest = publish(&request);
        assert!(manifest
            .get(&RelPath::new(b"f7.rs".to_vec()).unwrap())
            .is_none());
    }

    #[test]
    fn views_incremental_periodic_verification_repairs_missed_change() {
        let (project, storage, mut request) = fixture();
        let view = ViewStore::open(storage.path(), &request.scope).unwrap();
        caches()
            .lock()
            .unwrap()
            .get_mut(view.view_dir())
            .unwrap()
            .publications = 63;
        let before = VERIFICATION_MISMATCHES.load(std::sync::atomic::Ordering::Relaxed);
        fs::write(project.path().join("f1.rs"), "pub fn missed() {}\n").unwrap();
        fs::write(project.path().join("f0.rs"), "pub fn notified() {}\n").unwrap();
        request.changed_paths.insert(b"f0.rs".to_vec());
        let incremental = publish(&request).to_json_bytes().unwrap();
        assert!(VERIFICATION_MISMATCHES.load(std::sync::atomic::Ordering::Relaxed) > before);
        request.changed_paths.clear();
        assert_eq!(incremental, publish(&request).to_json_bytes().unwrap());
    }

    #[test]
    fn views_periodic_verification_survives_nonincremental_and_failed_preparations() {
        for fail_after_compare in [false, true] {
            let (project, storage, mut request) = fixture();
            let view = ViewStore::open(storage.path(), &request.scope).unwrap();
            caches()
                .lock()
                .unwrap()
                .get_mut(view.view_dir())
                .unwrap()
                .publications = 63;
            let before = VERIFICATION_MISMATCHES.load(std::sync::atomic::Ordering::Relaxed);
            fs::write(project.path().join("f1.rs"), "pub fn missed() {}\n").unwrap();
            fs::write(project.path().join("f0.rs"), "pub fn notified() {}\n").unwrap();
            request.changed_paths.insert(b"f0.rs".to_vec());
            if fail_after_compare {
                let failure = prepare_checkout_deferring(&request, true, &mut |phase| {
                    if phase == "cas" {
                        Err(ViewError::InvalidManifest(
                            "injected pre-commit failure".into(),
                        ))
                    } else {
                        Ok(())
                    }
                });
                assert!(failure.is_err());
            } else {
                let generation = view.current_generation().unwrap().unwrap();
                let manifest = view.load_manifest(&generation).unwrap();
                let key = manifest_entry_callgraph_key(
                    manifest
                        .get(&RelPath::new(b"f7.rs".to_vec()).unwrap())
                        .unwrap(),
                )
                .unwrap();
                let store =
                    BlobStore::open(storage.path(), &request.family, BlobPlane::Callgraph).unwrap();
                let path = store.path().to_path_buf();
                drop(store);
                Connection::open(path)
                    .unwrap()
                    .execute(
                        "DELETE FROM blob_payloads WHERE full_key = ?1",
                        [decode_hex(key).unwrap()],
                    )
                    .unwrap();
            }
            let actual = publish(&request).to_json_bytes().unwrap();
            assert_eq!(
                actual,
                rebuild_manifest(&request).to_json_bytes().unwrap(),
                "fail_after_compare={fail_after_compare}"
            );
            assert_eq!(
                VERIFICATION_MISMATCHES.load(std::sync::atomic::Ordering::Relaxed) - before,
                1,
                "one mismatch incident must log once, including across a failed preparation"
            );
        }
    }

    #[test]
    fn views_full_verification_deadline_and_restart_are_bounded() {
        let (project, storage, mut request) = fixture();
        let view = ViewStore::open(storage.path(), &request.scope).unwrap();
        {
            let mut caches = caches().lock().unwrap();
            let cache = caches.get_mut(view.view_dir()).unwrap();
            let verified = cache.last_full.unwrap();
            assert!(!full_verification_required(
                cache,
                verified + std::time::Duration::from_secs(599)
            ));
            assert!(full_verification_required(
                cache,
                verified + std::time::Duration::from_secs(600)
            ));
        }
        expire_verification_for_test(view.view_dir());
        assert!(full_verification_due(view.view_dir()));
        fs::write(
            project.path().join("f1.rs"),
            "pub fn missed_at_deadline() {}\n",
        )
        .unwrap();
        fs::write(project.path().join("f0.rs"), "pub fn notified() {}\n").unwrap();
        request.changed_paths.insert(b"f0.rs".to_vec());
        assert_eq!(
            publish(&request).to_json_bytes().unwrap(),
            rebuild_manifest(&request).to_json_bytes().unwrap()
        );
        assert!(!full_verification_due(view.view_dir()));
        // A new process has no remembered deadline or manifest proof.
        caches().lock().unwrap().remove(view.view_dir());
        assert!(full_verification_due(view.view_dir()));
        fs::write(
            project.path().join("f1.rs"),
            "pub fn missed_while_stopped() {}\n",
        )
        .unwrap();
        assert_eq!(
            publish(&request).to_json_bytes().unwrap(),
            rebuild_manifest(&request).to_json_bytes().unwrap()
        );
        assert!(!full_verification_due(view.view_dir()));
    }

    #[test]
    fn views_incremental_counter_forces_heal_after_external_delete() {
        let (project, storage, mut request) = fixture();
        let store = BlobStore::open(storage.path(), &request.family, BlobPlane::Callgraph).unwrap();
        let before = store.removal_stamp().unwrap();
        let path = store.path().to_path_buf();
        drop(store);
        // Simulate a maintenance writer bypassing BlobStore, with no second
        // descriptor kept open while the publisher owns the database.
        Connection::open(&path)
            .unwrap()
            .execute_batch("DELETE FROM blob_payloads")
            .unwrap();
        let store = BlobStore::open(storage.path(), &request.family, BlobPlane::Callgraph).unwrap();
        assert_ne!(before, store.removal_stamp().unwrap());
        drop(store);
        fs::write(project.path().join("f0.rs"), "pub fn edited() {}\n").unwrap();
        request.changed_paths.insert(b"f0.rs".to_vec());
        let manifest = publish(&request);
        let store = BlobStore::open(storage.path(), &request.family, BlobPlane::Callgraph).unwrap();
        assert!(store
            .missing_hex_keys(manifest.plane_keys().map(|(_, key)| key), None)
            .unwrap()
            .missing
            .is_empty());
        assert_eq!(manifest.entries().count(), 8);
    }

    #[test]
    fn views_incremental_store_recreation_revokes_stamp() {
        let (project, storage, mut request) = fixture();
        let store = BlobStore::open(storage.path(), &request.family, BlobPlane::Callgraph).unwrap();
        let before = store.removal_stamp().unwrap();
        let path = store.path().to_path_buf();
        drop(store);
        fs::write(&path, b"not a SQLite database").unwrap();
        let store = BlobStore::open(storage.path(), &request.family, BlobPlane::Callgraph).unwrap();
        assert_ne!(before, store.removal_stamp().unwrap());
        drop(store);
        fs::write(project.path().join("f0.rs"), "pub fn edited() {}\n").unwrap();
        request.changed_paths.insert(b"f0.rs".to_vec());
        assert_eq!(publish(&request).entries().count(), 8);
    }

    #[test]
    fn views_directory_events_remove_deleted_and_renamed_subtrees() {
        for rename in [false, true] {
            let (project, _storage, mut request) = fixture();
            fs::create_dir_all(project.path().join("group/nested")).unwrap();
            fs::write(project.path().join("group/a.rs"), "pub fn a() {}\n").unwrap();
            fs::write(project.path().join("group/nested/b.rs"), "pub fn b() {}\n").unwrap();
            git(project.path(), &["add", "-A"]);
            git(project.path(), &["commit", "--quiet", "-m", "subtree"]);
            publish(&request);
            if rename {
                fs::rename(project.path().join("group"), project.path().join("moved")).unwrap();
            } else {
                fs::remove_dir_all(project.path().join("group")).unwrap();
            }
            fs::write(project.path().join("f0.rs"), "pub fn notified() {}\n").unwrap();
            // No child path is reported for the removed/renamed directory.
            request.changed_paths = BTreeSet::from([b"group".to_vec(), b"f0.rs".to_vec()]);
            if rename {
                request.changed_paths.insert(b"moved".to_vec());
            }
            let manifest = publish(&request);
            assert_eq!(
                manifest.to_json_bytes().unwrap(),
                rebuild_manifest(&request).to_json_bytes().unwrap(),
                "rename={rename}"
            );
            assert!(manifest
                .entries()
                .all(|(path, _)| !path.as_bytes().starts_with(b"group/")));
        }
    }

    #[test]
    fn views_ambiguous_watcher_spellings_force_full_read() {
        for variant in 0..3 {
            let (project, _storage, mut request) = fixture();
            let (actual, reported) = if variant == 1 {
                use unicode_normalization::UnicodeNormalization;
                let name = "caf\u{e9}.rs";
                fs::write(project.path().join(name), "pub fn before() {}\n").unwrap();
                git(project.path(), &["add", "-A"]);
                git(project.path(), &["commit", "--quiet", "-m", "unicode"]);
                publish(&request);
                let tracked = head_tree_entries(project.path())
                    .unwrap()
                    .into_iter()
                    .find(|entry| entry.rel_path.starts_with(b"caf"))
                    .unwrap();
                let spelling = String::from_utf8(tracked.rel_path).unwrap();
                let nfc: String = spelling.nfc().collect();
                let nfd: String = spelling.nfd().collect();
                (name.to_owned(), if spelling == nfc { nfd } else { nfc })
            } else {
                ("f1.rs".to_owned(), "F1.rs".to_owned())
            };
            if variant == 2 {
                fs::rename(
                    project.path().join(&actual),
                    project.path().join("case-stage.rs"),
                )
                .unwrap();
                fs::rename(
                    project.path().join("case-stage.rs"),
                    project.path().join(&reported),
                )
                .unwrap();
                fs::write(project.path().join(&reported), "pub fn case_renamed() {}\n").unwrap();
            } else {
                fs::write(project.path().join(&actual), "pub fn changed() {}\n").unwrap();
            }
            fs::write(project.path().join("f0.rs"), "pub fn notified() {}\n").unwrap();
            request.changed_paths = BTreeSet::from([b"f0.rs".to_vec(), reported.into_bytes()]);
            assert_eq!(
                publish(&request).to_json_bytes().unwrap(),
                rebuild_manifest(&request).to_json_bytes().unwrap(),
                "variant={variant}"
            );
        }
    }

    #[test]
    fn views_incremental_closure_uses_owning_connections_and_revokes_stale_proof() {
        let (_project, storage, request) = fixture();
        let view = ViewStore::open(storage.path(), &request.scope).unwrap();
        let generation = view.current_generation().unwrap().unwrap();
        let cached = previous(&view, Some(&generation)).unwrap();
        let callgraph =
            BlobStore::open(storage.path(), &request.family, BlobPlane::Callgraph).unwrap();
        let semantic =
            BlobStore::open(storage.path(), &request.family, BlobPlane::Semantic).unwrap();
        let closure = StoreClosure {
            callgraph: &callgraph,
            semantic: &semantic,
            trigram: view.trigram_path(&generation).unwrap(),
            trusted: Some((&cached.manifest, cached.stamps.clone())),
        };
        let opens = crate::db::lifecycle::thread_counts::total_opens_on_this_thread(
            crate::db::lifecycle::SqliteStore::BlobStore,
        );
        closure
            .probe_blobs(&cached.manifest.plane_keys().collect::<Vec<_>>())
            .unwrap();
        assert_eq!(
            crate::db::lifecycle::thread_counts::total_opens_on_this_thread(
                crate::db::lifecycle::SqliteStore::BlobStore
            ),
            opens
        );
        drop(closure);
        let path = callgraph.path().to_path_buf();
        drop(callgraph);
        Connection::open(path)
            .unwrap()
            .execute_batch("DELETE FROM blob_payloads")
            .unwrap();
        let callgraph =
            BlobStore::open(storage.path(), &request.family, BlobPlane::Callgraph).unwrap();
        // A base proof from before the removal must not authorize old keys.
        let stale = StoreClosure {
            callgraph: &callgraph,
            semantic: &semantic,
            trigram: view.trigram_path(&generation).unwrap(),
            trusted: Some((&cached.manifest, cached.stamps)),
        };
        assert!(matches!(
            stale.probe_blobs(&cached.manifest.plane_keys().collect::<Vec<_>>()),
            Err(ViewError::MissingBlob { .. })
        ));
    }

    #[test]
    fn views_incremental_random_sequences_equal_full_rebuild() {
        // Deterministic seeds keep a failing sequence reproducible. Each cycle
        // includes every operation, with random members, bytes and ordering.
        for seed in (1..=16u64).map(|value| value.wrapping_mul(0x9e3779b97f4a7c15)) {
            let (project, _storage, mut request) = fixture();
            let mut random = seed;
            for step in 0..72 {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                let index = random as usize % 8;
                let name = format!("f{index}.rs");
                let path = project.path().join(&name);
                let mut changed = BTreeSet::from([name.as_bytes().to_vec()]);
                match (step + seed as usize) % 11 {
                    op @ (0 | 1) => {
                        let was_link = fs::symlink_metadata(&path)
                            .is_ok_and(|meta| meta.file_type().is_symlink());
                        if was_link {
                            fs::remove_file(&path).unwrap();
                        }
                        fs::write(
                            &path,
                            format!("pub fn f{index}() {{ let _x = {random}u64; }}\n"),
                        )
                        .unwrap();
                        if op == 0 || was_link {
                            git(project.path(), &["add", "-A"]);
                            git(
                                project.path(),
                                &["commit", "--quiet", "--allow-empty", "-m", "add"],
                            );
                        }
                    }
                    2 => {
                        if fs::symlink_metadata(&path).is_ok() {
                            fs::remove_file(&path).unwrap();
                        }
                    }
                    3 => {
                        let to = format!("renamed{index}.rs");
                        if fs::symlink_metadata(&path).is_ok() {
                            fs::rename(&path, project.path().join(&to)).unwrap();
                        }
                        changed.insert(to.into_bytes());
                        git(project.path(), &["add", "-A"]);
                        git(
                            project.path(),
                            &["commit", "--quiet", "--allow-empty", "-m", "rename"],
                        );
                    }
                    4 => {
                        git(project.path(), &["add", "-A"]);
                        if path.is_file() {
                            let mode = if random & 1 == 0 {
                                "--chmod=+x"
                            } else {
                                "--chmod=-x"
                            };
                            git(project.path(), &["update-index", mode, &name]);
                        }
                        git(
                            project.path(),
                            &["commit", "--quiet", "--allow-empty", "-m", "mode"],
                        );
                    }
                    5 => {
                        #[cfg(unix)]
                        {
                            if fs::symlink_metadata(&path).is_ok() {
                                fs::remove_file(&path).unwrap();
                            }
                            std::os::unix::fs::symlink(format!("target-{random}"), &path).unwrap();
                            git(project.path(), &["add", "-A"]);
                            git(
                                project.path(),
                                &["commit", "--quiet", "--allow-empty", "-m", "symlink"],
                            );
                        }
                    }
                    6 => {
                        git(project.path(), &["add", "-A"]);
                        git(
                            project.path(),
                            &["commit", "--quiet", "--allow-empty", "-m", "head"],
                        );
                        if random & 1 == 0 {
                            git(project.path(), &["reset", "--quiet", "--hard", "HEAD~1"]);
                        }
                    }
                    8 | 9 => {
                        let directory = format!("group-{step}");
                        let nested = project.path().join(&directory).join("nested");
                        fs::create_dir_all(&nested).unwrap();
                        fs::write(
                            nested.join("child.rs"),
                            format!("pub fn child_{step}() {{}}\n"),
                        )
                        .unwrap();
                        fs::write(project.path().join("notify.rs"), "pub fn before() {}\n")
                            .unwrap();
                        git(project.path(), &["add", "-A"]);
                        git(
                            project.path(),
                            &["commit", "--quiet", "--allow-empty", "-m", "directory"],
                        );
                        request.changed_paths.clear();
                        publish(&request);
                        if (step + seed as usize) % 11 == 8 {
                            fs::remove_dir_all(project.path().join(&directory)).unwrap();
                        } else {
                            let destination = format!("moved-{step}");
                            fs::rename(
                                project.path().join(&directory),
                                project.path().join(&destination),
                            )
                            .unwrap();
                            changed.insert(destination.into_bytes());
                        }
                        fs::write(
                            project.path().join("notify.rs"),
                            format!("pub fn after_{step}() {{}}\n"),
                        )
                        .unwrap();
                        changed.insert(directory.into_bytes());
                        changed.insert(b"notify.rs".to_vec());
                    }
                    10 => {
                        let lower = format!("case{step}.rs");
                        let upper = format!("CASE{step}.rs");
                        fs::write(project.path().join(&lower), "pub fn before_case() {}\n")
                            .unwrap();
                        fs::write(project.path().join("notify.rs"), "pub fn before() {}\n")
                            .unwrap();
                        git(project.path(), &["add", "-A"]);
                        git(
                            project.path(),
                            &["commit", "--quiet", "--allow-empty", "-m", "case member"],
                        );
                        request.changed_paths.clear();
                        publish(&request);
                        fs::rename(
                            project.path().join(&lower),
                            project.path().join("case-stage.rs"),
                        )
                        .unwrap();
                        fs::rename(
                            project.path().join("case-stage.rs"),
                            project.path().join(&upper),
                        )
                        .unwrap();
                        fs::write(
                            project.path().join(&upper),
                            format!("pub fn case_{step}() {{}}\n"),
                        )
                        .unwrap();
                        fs::write(
                            project.path().join("notify.rs"),
                            format!("pub fn notified_{step}() {{}}\n"),
                        )
                        .unwrap();
                        changed = BTreeSet::from([upper.into_bytes(), b"notify.rs".to_vec()]);
                    }
                    _ => {
                        // A rescan must find an edit the watcher never named.
                        let rescan = project.path().join("rescan.rs");
                        fs::write(&rescan, "pub fn original() {}\n").unwrap();
                        git(project.path(), &["add", "-A"]);
                        git(
                            project.path(),
                            &["commit", "--quiet", "--allow-empty", "-m", "rescan member"],
                        );
                        request.changed_paths.clear();
                        publish(&request);
                        fs::write(&rescan, format!("pub fn rescan_{step}() {{}}\n")).unwrap();
                        changed.clear();
                    }
                }
                request.changed_paths = changed;
                let incremental = publish(&request).to_json_bytes().unwrap();
                let rebuilt = rebuild_manifest(&request).to_json_bytes().unwrap();
                assert_eq!(incremental, rebuilt, "seed={seed} step={step}");
            }
        }
    }
}
