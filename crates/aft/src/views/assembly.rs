use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[cfg(test)]
use rusqlite::Connection;
use rusqlite::OptionalExtension;

use crate::alias::{head_tree_entries, AliasStore, GitMode};
use crate::blob_store::{BlobPlane, BlobStore, CallgraphKey, FullKey, PutOutcome};
use crate::callgraph_store::join::CallgraphBlob;
use crate::path_status::{PathStatusStore, SkipReason};
use crate::pins::AssemblyPin;

use super::{
    ArtifactPlane, ByteString, ClosureRequirements, Manifest, ManifestEntry, PublicationArtifacts,
    PublicationClosure, PublicationRequest, PublishOutcome, RegularPlanes, RelPath, Result,
    ViewError, ViewStore,
};

#[derive(Clone, Debug)]
pub struct AssemblyRequest {
    pub storage: PathBuf,
    pub project_root: PathBuf,
    pub family: String,
    pub scope: String,
    pub desired_head: String,
    pub changed_paths: BTreeSet<Vec<u8>>,
    pub semantic_keys: BTreeMap<Vec<u8>, String>,
    pub require_semantic: bool,
    pub allow_blob_put: bool,
    /// False when the root's call graph is off: no callgraph payload is
    /// extracted and regular entries carry no callgraph key. Extraction parses
    /// every changed source file, which dominates a first publication. An
    /// entry published without a key is re-extracted once the call graph is
    /// turned on.
    pub callgraph: bool,
}

#[derive(Clone, Debug)]
pub struct AssemblyReport {
    pub generation: Option<String>,
    pub manifest: Option<Manifest>,
    pub blob_puts: usize,
    pub pending_paths: BTreeSet<Vec<u8>>,
    pub pending_inputs: BTreeMap<Vec<u8>, PendingInput>,
    pub published: bool,
}

/// A missing blob or unsupported tracked-file mode found while assembling a
/// checkout generation. Retain this evidence so queries can explain what must
/// arrive or change without reading the blob stores or checkout files again.
#[derive(Clone, Debug, serde::Serialize)]
pub struct PendingInput {
    pub plane: &'static str,
    pub reason: &'static str,
    pub blob_key: Option<String>,
}

struct Candidate {
    path: RelPath,
    entry: ManifestEntry,
    key: Option<FullKey>,
    payload: Option<Vec<u8>>,
    tracked: Option<crate::alias::TrackedPath>,
    source: Option<Vec<u8>>,
}

#[cfg(test)]
thread_local! {
    static RETAINED_CANDIDATE_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub fn head_tree_fingerprint(entries: &[crate::alias::TrackedPath]) -> String {
    let mut hasher = blake3::Hasher::new();
    for entry in entries {
        hasher.update(&(entry.rel_path.len() as u64).to_le_bytes());
        hasher.update(&entry.rel_path);
        hasher.update(entry.mode.as_bytes());
        hasher.update(entry.git_oid.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

pub fn publish_checkout(request: &AssemblyRequest) -> Result<AssemblyReport> {
    let mut prepared = prepare_checkout(request, &mut |_| Ok(()))?;
    let report = prepared.commit()?;
    prepared.settle_status();
    Ok(report)
}

/// Owns the unpublished files and assembly pin until CAS succeeds or the build
/// is dropped. Cancellation and errors follow the same cleanup path.
pub struct PreparedAssembly {
    report: AssemblyReport,
    publication: Option<super::PreparedPublication>,
    files: Option<(ViewStore, String)>,
    derived_checkpoint: Option<(PathBuf, crate::db::file_identity::IdentityConnection)>,
    pin: Option<AssemblyPin>,
    _base_pin: Option<crate::pins::QueryPin>,
    profile: PublicationProfile,
    /// Status changes of a complete assembly, until the commit decides them.
    settlement: Option<StatusSettlement>,
    /// Status changes the commit accepted, waiting for `settle_status`.
    committed_settlement: Option<StatusSettlement>,
}

/// The path-status changes a complete assembly computed: pending rows to
/// clear and call-graph skips to record or drop. They describe the generation
/// being prepared, so they are applied only once that generation is the one
/// served: after its publication succeeds, or when the publication is a no-op
/// because the current generation already has the same manifest. A build that
/// fails, is cancelled, is dropped uncommitted or loses the publication race
/// drops them, so status keeps describing the generation still served.
///
/// They are applied after the publisher leaves its commit barrier (the root
/// actor's write gate and the publication job table), so reads and other
/// projects' publications never wait for status writes.
struct StatusSettlement {
    view: ViewStore,
    /// The generation the changes describe: the one the publication swaps
    /// in, or for a no-op the one already served.
    generation: String,
    root: PathBuf,
    intent: crate::path_status::StatusIntent,
}

/// What became of a committed settlement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettleOutcome {
    /// No committed settlement was waiting, or applying it failed (the
    /// failure is logged).
    Nothing,
    /// The ledger was brought in line, writing this many rows.
    Applied(usize),
    /// A newer generation is served now; its own settlement describes it, so
    /// this older one is dropped rather than overwrite it.
    Superseded,
}

/// Per-view bookkeeping that orders settlements applied off the barrier.
#[derive(Default)]
struct SettleGate {
    /// Held while a settlement checks the served generation and writes, so a
    /// newer settlement cannot be overtaken between the check and the write.
    serial: std::sync::Mutex<()>,
    /// Settlements committed and not yet applied. While any is outstanding,
    /// the ledger is about to change, so a preparation cannot conclude from
    /// what it reads that its own settlement would write nothing.
    outstanding: std::sync::atomic::AtomicUsize,
}

fn settle_gate(view_dir: &Path) -> std::sync::Arc<SettleGate> {
    static GATES: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, std::sync::Arc<SettleGate>>>,
    > = std::sync::LazyLock::new(Default::default);
    GATES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(view_dir.to_path_buf())
        .or_default()
        .clone()
}

impl StatusSettlement {
    fn apply(self) -> std::result::Result<SettleOutcome, String> {
        let gate = settle_gate(self.view.view_dir());
        let _serial = gate
            .serial
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let served = self
            .view
            .current_generation()
            .map_err(|error| error.to_string())?;
        if served.as_deref() != Some(self.generation.as_str()) {
            return Ok(SettleOutcome::Superseded);
        }
        #[cfg(test)]
        run_before_settle_write(self.view.view_dir());
        let written = PathStatusStore::open(self.view.view_dir())
            .and_then(|mut status| status.settle(&self.intent))
            .map_err(|error| error.to_string())?;
        Ok(SettleOutcome::Applied(written))
    }
}

#[cfg(test)]
type SettleHook = std::sync::Arc<dyn Fn() + Send + Sync>;

#[cfg(test)]
static BEFORE_SETTLE_WRITE: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, SettleHook>>,
> = std::sync::LazyLock::new(Default::default);

/// Runs `hook` each time a settlement for `view_dir` is about to open the
/// status ledger to write.
#[cfg(test)]
pub(crate) fn before_settle_write_for_test(
    view_dir: &Path,
    hook: impl Fn() + Send + Sync + 'static,
) {
    BEFORE_SETTLE_WRITE
        .lock()
        .unwrap()
        .insert(view_dir.to_path_buf(), std::sync::Arc::new(hook));
}

#[cfg(test)]
fn run_before_settle_write(view_dir: &Path) {
    let hook = BEFORE_SETTLE_WRITE.lock().unwrap().get(view_dir).cloned();
    if let Some(hook) = hook {
        hook();
    }
}

impl PreparedAssembly {
    pub fn report(&self) -> &AssemblyReport {
        &self.report
    }

    pub fn commit(&mut self) -> Result<AssemblyReport> {
        if let Some(publication) = self.publication.take() {
            let retires_legacy_plane = publication.base_generation.is_none();
            let pointer_started = Instant::now();
            let outcome = publication.commit();
            self.profile.pointer_ms = pointer_started.elapsed().as_millis();
            match outcome? {
                PublishOutcome::Published => {
                    self.report.published = true;
                    self.files = None;
                    if retires_legacy_plane {
                        log::info!(
                            "views: root={} legacy plane retired at generation={}",
                            self.profile.root.display(),
                            self.report
                                .generation
                                .as_deref()
                                .expect("prepared publication has a generation")
                        );
                    }
                    if let Some((path, connection)) = self.derived_checkpoint.take() {
                        self.profile.finish();
                        super::generation::schedule_derived_checkpoint(
                            path,
                            connection,
                            self.profile.root.clone(),
                        );
                    }
                    self.profile.outcome = "published";
                    self.accept_settlement();
                }
                PublishOutcome::Conflict { current_generation } => {
                    self.profile.outcome = "conflict";
                    // Another generation won; this one's status changes do
                    // not describe it.
                    self.settlement = None;
                    return Err(ViewError::InvalidManifest(format!(
                        "publication base changed to {current_generation:?}"
                    )));
                }
            }
        } else {
            // Nothing to publish. A settlement here belongs to a no-op whose
            // manifest equals the current generation's, so it describes the
            // generation being served and applies as is.
            self.accept_settlement();
        }
        self.profile.finish();
        Ok(AssemblyReport {
            generation: self.report.generation.take(),
            manifest: self.report.manifest.take(),
            blob_puts: self.report.blob_puts,
            pending_paths: std::mem::take(&mut self.report.pending_paths),
            pending_inputs: std::mem::take(&mut self.report.pending_inputs),
            published: self.report.published,
        })
    }

    /// Marks the prepared status changes as describing the served generation.
    /// Runs inside the publisher's commit barrier, so it only moves them
    /// aside; `settle_status` writes them once the barrier is released.
    fn accept_settlement(&mut self) {
        if let Some(settlement) = self.settlement.take() {
            settle_gate(settlement.view.view_dir())
                .outstanding
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            self.committed_settlement = Some(settlement);
        }
    }

    /// Applies the status changes a successful commit accepted. Call it after
    /// releasing any lock the commit ran under: it opens the view's status
    /// ledger and writes in one transaction. Only the rows that differ are
    /// written, and a settlement whose generation is no longer served is
    /// dropped, so a slow settlement never overwrites a newer one. The
    /// generation is already current, so a write that fails does not fail
    /// the publication: it is logged, the transaction rolls back, and the next
    /// publication of the view (a no-op included) computes the changes again.
    pub fn settle_status(&mut self) -> SettleOutcome {
        let Some(settlement) = self.committed_settlement.take() else {
            return SettleOutcome::Nothing;
        };
        let gate = settle_gate(settlement.view.view_dir());
        let root = settlement.root.clone();
        let outcome = settlement.apply().unwrap_or_else(|error| {
            log::warn!(
                "views: root={} path status not updated after publication: {error}",
                root.display()
            );
            SettleOutcome::Nothing
        });
        gate.outstanding
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        if let SettleOutcome::Applied(written) = outcome {
            if written > 0 {
                crate::slog_info!(
                    "content-addressed view publication settled {} path status row(s) root={}",
                    written,
                    root.display()
                );
            }
        }
        outcome
    }
}

impl Drop for PreparedAssembly {
    fn drop(&mut self) {
        // A committed settlement describes the served generation. Callers
        // settle it after leaving their commit barrier; one that did not still
        // gets its status here rather than losing it.
        self.settle_status();
        // An abandoned build still owns the checkpoint keeper. Close it before
        // cleanup, otherwise the file-identity guard correctly refuses unlink
        // and leaves an entire unpublished database behind on every retry.
        self.derived_checkpoint.take();
        if let Some((view, generation)) = &self.files {
            if view
                .current_generation()
                .is_ok_and(|current| current.as_deref() != Some(generation))
            {
                // Even an unpublished name can have a query pin. Serialize the
                // final protection check with pin admission and fail closed if
                // admission cannot be locked. A later sweep can finish cleanup.
                if let Ok(_barrier) = crate::storage_retention::pin_barrier(view.view_dir()) {
                    if !crate::root_cache::protected_read_marker_exists(view.view_dir(), generation)
                    {
                        view.remove_generation_files(generation);
                    }
                }
            }
        }
        // Keep the pin alive until generation cleanup has finished.
        self.pin.take();
    }
}

pub fn prepare_checkout(
    request: &AssemblyRequest,
    phase: &mut impl FnMut(&str) -> Result<()>,
) -> Result<PreparedAssembly> {
    prepare_checkout_deferring(request, false, phase)
}

/// Like [`prepare_checkout`], but with `defer_derived` the checkout publishes
/// its manifest without building a derived call-graph database. Callgraph keys
/// and blobs are still recorded, so the manifest can be compared with another
/// checkout's and the graph can be built later from it. The caller decides
/// whether to defer from whether a query has needed the graph, not from
/// whether a derived database already exists: one an earlier process built
/// is not a query of this one, and patching it can take minutes.
/// An unchanged manifest whose current derived database is ready stays a
/// no-op; any other change publishes a generation without one.
pub fn prepare_checkout_deferring(
    request: &AssemblyRequest,
    defer_derived: bool,
    phase: &mut impl FnMut(&str) -> Result<()>,
) -> Result<PreparedAssembly> {
    publication_delay_for_test();
    let readiness_deadline = Instant::now() + crate::db::STEADY_BUSY_TIMEOUT;
    let mut timing = super::profile::PublicationTiming::new(&request.project_root);
    // Every publisher (scheduler, migration import, tests) reaches this point
    // with the HEAD fingerprint it observed, so the read-path cache that
    // navigation and Tier-2 compare generations against is populated here
    // rather than only where the scheduler happened to compute it.
    super::cache_head_fingerprint(request.project_root.clone(), request.desired_head.clone());
    let mut profile = PublicationProfile::new(&request.project_root);
    profile.enter(0, phase)?;
    let view = ViewStore::open(&request.storage, &request.scope)?;
    let current_generation = view.current_generation()?;
    let base_pin = current_generation
        .as_deref()
        .map(|generation| crate::pins::QueryPin::acquire(view.view_dir(), generation))
        .transpose()
        .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
    let previous = current_generation
        .as_deref()
        .map(|generation| view.load_manifest(generation))
        .transpose()?;
    // A generation published while its derived graph was deferred has no
    // derived file. That is no base at all, not a base that failed a check.
    let current_derived_built = current_generation
        .as_deref()
        .is_some_and(|generation| super::read::derived_built(view.view_dir(), generation));
    // A byte-identical manifest can still name incompatible build output after
    // an upgrade. Never no-op, reuse, or patch a graph its reader would refuse.
    let unusable_base = match current_generation
        .as_deref()
        .filter(|_| current_derived_built)
        .map(|generation| {
            crate::callgraph_store::manifest_view_database_ready(
                view.view_dir(),
                generation,
                readiness_deadline,
            )
        })
        .transpose()
    {
        Ok(Some(false)) => Some((
            ColdBuildReason::BaseNotReady,
            "derived graph fails the current build-output readiness check".to_string(),
        )),
        Ok(_) => None,
        // A checkpoint or live writer can temporarily prevent the probe. Keep
        // SQLite's typed contention error so the publication scheduler retries
        // with its existing backoff instead of treating the manifest as invalid.
        Err(crate::callgraph_store::CallGraphStoreError::Sqlite(error))
            if crate::db::is_busy_error(&error) =>
        {
            return Err(ViewError::Sqlite(error));
        }
        Err(crate::callgraph_store::CallGraphStoreError::Sqlite(error))
            if sqlite_error_is_unreadable_database(&error) =>
        {
            Some((ColdBuildReason::BaseUnreadable, error.to_string()))
        }
        Err(error) => return Err(ViewError::InvalidManifest(error.to_string())),
    };
    let base_ready = current_derived_built && unusable_base.is_none();
    let defer_derived = defer_derived && request.callgraph;
    timing.phase("previous_manifest");
    let head_started = Instant::now();
    super::io::WORK
        .full_walks
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let head = head_tree_entries(&request.project_root)
        .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
    profile.head_ms = head_started.elapsed().as_millis();
    timing.phase("head_tree");
    let mut callgraph = BlobStore::open(
        &request.storage,
        request.family.clone(),
        BlobPlane::Callgraph,
    )
    .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
    let semantic = BlobStore::open(
        &request.storage,
        request.family.clone(),
        BlobPlane::Semantic,
    )
    .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
    let mut aliases = AliasStore::open(&request.storage, &request.family)
        .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;

    timing.phase("store_opens");
    let previous_entries = previous
        .as_ref()
        .map(|manifest| {
            manifest
                .entries()
                .map(|(path, entry)| (path.as_bytes().to_vec(), entry.clone()))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let rebuild_all = request.changed_paths.is_empty();
    let assembly_started = Instant::now();
    let mut blocking_paths = BTreeSet::new();
    let mut semantic_pending_paths = BTreeSet::new();
    let mut pending_inputs = BTreeMap::new();
    let mut fresh_skips = Vec::new();
    let mut candidates = Vec::with_capacity(head.len());

    for tracked in head {
        let rel_path = RelPath::new(tracked.rel_path.clone())?;
        let absolute = request
            .project_root
            .join(path_from_bytes(&tracked.rel_path));
        // Git membership includes database fixtures and internal storage. Omit
        // them before either opening bytes or reusing an older manifest entry.
        if crate::project_content::is_excluded(&absolute) {
            continue;
        }
        // HEAD describes committed membership, not the checkout. Check absence
        // before reusing an unchanged entry as well as when reading changed bytes.
        // Do not follow symlinks: a dangling link is still a checkout member.
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
        if !rebuild_all && !request.changed_paths.contains(&tracked.rel_path) {
            if let Some(entry) = previous_entries.get(&tracked.rel_path).filter(|entry| {
                !(request.callgraph && lacks_callgraph_key(entry, &tracked.rel_path))
            }) {
                candidates.push(Candidate {
                    path: rel_path,
                    entry: entry.clone(),
                    key: None,
                    payload: None,
                    tracked: None,
                    source: None,
                });
                continue;
            }
        }

        match tracked.mode {
            GitMode::Gitlink => candidates.push(Candidate {
                path: rel_path,
                entry: ManifestEntry::Gitlink {
                    oid: tracked.git_oid.to_hex(),
                },
                key: None,
                payload: None,
                tracked: None,
                source: None,
            }),
            GitMode::Symlink => {
                phase("working_tree_read")?;
                let Some(target) = read_symlink_bytes(&absolute)? else {
                    continue;
                };
                candidates.push(Candidate {
                    path: rel_path,
                    entry: ManifestEntry::Symlink {
                        target_bytes: ByteString::new(target),
                    },
                    key: None,
                    payload: None,
                    tracked: None,
                    source: None,
                });
            }
            GitMode::Regular { executable } => {
                phase("working_tree_read")?;
                let Some(source) = read_working_tree_file(&absolute)? else {
                    continue;
                };
                let resolution_input = is_resolution_input(&tracked.rel_path);
                let language = if resolution_input {
                    Some("config".to_string())
                } else {
                    super::callgraph::source_language(&absolute)
                        .map(|language| format!("{language:?}").to_lowercase())
                };
                let (key, payload) = language
                    .as_deref()
                    .filter(|_| request.callgraph)
                    .map(|language| {
                        let key = CallgraphKey::from_bytes(
                            &source,
                            language,
                            crate::views::callgraph::PRODUCER,
                        )
                        .full_key();
                        let key_hex = key.to_hex();
                        let callgraph_is_current = previous_entries
                            .get(&tracked.rel_path)
                            .is_some_and(|entry| {
                                manifest_entry_callgraph_key(entry) == Some(key_hex.as_str())
                            });
                        let payload = if callgraph_is_current {
                            None
                        } else {
                            missing_callgraph_payload(
                                &callgraph,
                                &key,
                                &source,
                                language,
                                resolution_input,
                            )?
                        };
                        Ok::<_, ViewError>((key, payload))
                    })
                    .transpose()?
                    .map_or((None, None), |(key, payload)| (Some(key), payload));
                let callgraph_key = key.as_ref().map(FullKey::to_hex);
                if language.is_some() {
                    // Freshly read content decides whether the call graph
                    // skips this member; reused members keep their record.
                    let non_utf8 = request.callgraph
                        && !resolution_input
                        && std::str::from_utf8(&source).is_err();
                    fresh_skips.push((
                        tracked.rel_path.clone(),
                        non_utf8.then_some(SkipReason::NonUtf8Source),
                    ));
                }
                // Only a new payload can seed an alias. Cached or read-only
                // candidates need neither their source bytes nor tracked row.
                let needs_source = payload.is_some() && request.allow_blob_put;
                candidates.push(Candidate {
                    path: rel_path,
                    entry: ManifestEntry::Regular {
                        mode: if executable { 0o100755 } else { 0o100644 },
                        planes: RegularPlanes {
                            semantic: request.semantic_keys.get(&tracked.rel_path).cloned(),
                            callgraph: callgraph_key,
                        },
                        resolution_input,
                    },
                    key,
                    payload,
                    tracked: needs_source.then_some(tracked),
                    source: needs_source.then_some(source),
                });
            }
            GitMode::Other(_) => {
                pending_inputs.insert(
                    tracked.rel_path.clone(),
                    PendingInput {
                        plane: "membership",
                        reason: "unsupported Git mode",
                        blob_key: None,
                    },
                );
                blocking_paths.insert(tracked.rel_path);
            }
        }
    }

    timing.phase("candidate_assembly");
    profile.candidates = candidates.len();
    profile.assembly_ms = assembly_started.elapsed().as_millis();

    if request.require_semantic {
        for candidate in &candidates {
            let missing = matches!(
                &candidate.entry,
                ManifestEntry::Regular { planes, .. }
                    if planes.semantic.is_none()
                        && crate::semantic_index::is_semantic_indexed_extension(
                            &request
                                .project_root
                                .join(path_from_bytes(candidate.path.as_bytes()))
                        )
            );
            if missing {
                pending_inputs.insert(
                    candidate.path.as_bytes().to_vec(),
                    PendingInput {
                        plane: "semantic",
                        reason: "shared semantic blob unavailable",
                        blob_key: None,
                    },
                );
                semantic_pending_paths.insert(candidate.path.as_bytes().to_vec());
            }
        }
    }

    let keys = candidates
        .iter()
        .filter_map(|candidate| candidate.key.clone())
        .collect::<Vec<_>>();
    let next_generation = next_generation(current_generation.as_deref(), &request.desired_head);
    profile.generation = next_generation.clone();
    let pin = AssemblyPin::create(
        view.view_dir(),
        request.family.clone(),
        request.scope.clone(),
        next_generation.clone(),
        &keys,
    )
    .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
    let mut prepared = PreparedAssembly {
        report: AssemblyReport {
            generation: current_generation.clone(),
            manifest: previous.clone(),
            blob_puts: 0,
            pending_paths: BTreeSet::new(),
            pending_inputs: BTreeMap::new(),
            published: false,
        },
        publication: None,
        files: Some((view.clone(), next_generation.clone())),
        derived_checkpoint: None,
        pin: Some(pin),
        _base_pin: base_pin,
        profile,
        settlement: None,
        committed_settlement: None,
    };
    prepared.profile.enter(1, phase)?;
    timing.phase("assembly_pin");
    let mut blob_puts = 0;
    for candidate in &mut candidates {
        let payload = candidate.payload.take();
        let source = candidate.source.take();
        let tracked = candidate.tracked.take();
        let (Some(key), Some(payload)) = (&candidate.key, payload) else {
            continue;
        };
        if request.allow_blob_put {
            prepared
                .pin
                .as_mut()
                .expect("assembly pin")
                .renew_if_due()
                .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
            let put = callgraph
                .put(key, &payload)
                .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
            blob_puts += usize::from(matches!(put.outcome, PutOutcome::Inserted));
            if let (Some(tracked), Some(source)) = (tracked, source) {
                aliases
                    .seed_proven_alias(&tracked, &source)
                    .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
            }
        } else if !callgraph
            .contains(key)
            .map_err(|error| ViewError::InvalidManifest(error.to_string()))?
        {
            pending_inputs.insert(
                candidate.path.as_bytes().to_vec(),
                PendingInput {
                    plane: "callgraph",
                    reason: "shared callgraph blob unavailable",
                    blob_key: Some(key.to_hex()),
                },
            );
            blocking_paths.insert(candidate.path.as_bytes().to_vec());
        }
    }

    #[cfg(test)]
    RETAINED_CANDIDATE_BYTES.with(|count| {
        count.set(
            candidates
                .iter()
                .map(|candidate| {
                    candidate.source.as_ref().map_or(0, Vec::len)
                        + candidate.payload.as_ref().map_or(0, Vec::len)
                })
                .sum(),
        )
    });

    timing.phase("blob_and_alias_puts");
    prepared.profile.blob_puts = blob_puts;
    prepared.profile.pending_paths = blocking_paths.len() + semantic_pending_paths.len();
    let mut status = PathStatusStore::open(view.view_dir())
        .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
    status
        .mark_pending_many(
            blocking_paths
                .iter()
                .chain(&semantic_pending_paths)
                .map(|path| (path.as_slice(), pending_inputs[path].reason)),
            generation_number(&next_generation),
        )
        .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
    prepared.report.pending_inputs = pending_inputs;
    if !blocking_paths.is_empty() {
        prepared.profile.outcome = "pending";
        prepared.report.blob_puts = blob_puts;
        prepared.report.pending_paths = blocking_paths
            .union(&semantic_pending_paths)
            .cloned()
            .collect();
        return Ok(prepared);
    }
    // A complete generation is assembled. Its status changes wait in the
    // prepared assembly until the generation is the one served; see
    // `StatusSettlement`.
    let intent = crate::path_status::StatusIntent {
        generation: generation_number(&next_generation),
        members: candidates
            .iter()
            .map(|candidate| candidate.path.as_bytes().to_vec())
            .collect(),
        keep_pending: semantic_pending_paths.clone(),
        fresh_skips: fresh_skips.into_iter().collect(),
    };
    // Read the ledger now, off the commit barrier: when it already says what
    // this publication would say, there is nothing to settle, and the commit
    // will not open the ledger at all. A settlement committed earlier and not
    // yet applied is about to change the ledger, so then the reading is not
    // conclusive and the changes are worked out again when they are applied.
    let settled_already = settle_gate(view.view_dir())
        .outstanding
        .load(std::sync::atomic::Ordering::Acquire)
        == 0
        && status
            .plan_settlement(&intent)
            .map_err(|error| ViewError::InvalidManifest(error.to_string()))?
            .is_empty();
    drop(status);
    if !settled_already {
        prepared.settlement = Some(StatusSettlement {
            view: view.clone(),
            generation: next_generation.clone(),
            root: request.project_root.clone(),
            intent,
        });
    }
    timing.phase("path_status");
    prepared.report.pending_paths = semantic_pending_paths;

    let manifest = Manifest::new(
        candidates
            .into_iter()
            .map(|candidate| (candidate.path, candidate.entry)),
    )?;
    prepared.profile.semantic_fill = previous.as_ref().is_some_and(|base| {
        base.entries
            .iter()
            .map(|(path, entry)| (path, manifest_entry_callgraph_key(entry)))
            .eq(manifest
                .entries
                .iter()
                .map(|(path, entry)| (path, manifest_entry_callgraph_key(entry))))
            && base != &manifest
    });
    // A publication that reproduces the current manifest byte for byte is a
    // no-op: callers fire on triggers that often leave HEAD untouched (a
    // semantic refresh completing, a watcher batch of ignored edits), and each
    // redundant generation costs a full derived-database materialization and
    // a pointer swap. Keep the current generation and report nothing published.
    if current_generation
        .as_deref()
        .is_some_and(|generation| super::generation_matches_head(generation, &request.desired_head))
        && previous.as_ref() == Some(&manifest)
        // Deferring keeps a current generation that has no derived file, but
        // still replaces one whose derived file fails the readiness check.
        && (base_ready || (defer_derived && !current_derived_built))
    {
        prepared.profile.outcome = "no_op";
        prepared.profile.seed_source = if base_ready { "own" } else { "deferred" };
        prepared.profile.cold_build_reason = "none";
        prepared.report.manifest = None;
        prepared.report.blob_puts = blob_puts;
        // A no-op's status changes describe the generation already served.
        if let (Some(settlement), Some(current)) =
            (prepared.settlement.as_mut(), current_generation.as_ref())
        {
            settlement.generation = current.clone();
        }
        return Ok(prepared);
    }
    prepared.profile.enter(2, phase)?;
    if defer_derived {
        prepared.profile.seed_source = "deferred";
        prepared.profile.cold_build_reason = "none";
    }
    let reused_derived = !defer_derived
        && base_ready
        && previous.as_ref().is_some_and(|base| {
            super::materialization::manifest_callgraph_equivalent(base, &manifest)
        })
        && current_generation
            .as_deref()
            .is_some_and(|base| view.derived_path(base).is_ok_and(|path| path.is_file()));
    if reused_derived {
        prepared.profile.seed_source = "own";
        prepared.profile.cold_build_reason = "none";
        view.reuse_derived(
            &next_generation,
            current_generation.as_deref().expect("reused base"),
        )?;
    }
    prepared.profile.io.enter(super::io::Phase::Clone);
    let derived = view.derived_path(&next_generation)?;
    if !reused_derived && !defer_derived {
        let clone_started = Instant::now();
        let mut cold_build = unusable_base;
        let base = match current_generation.as_deref() {
            Some(base) if base_ready => {
                let base_path = view.derived_path(base)?;
                if base_path.is_file() {
                    let owner = view.derived_owner(base)?;
                    match view.load_manifest(&owner) {
                        Ok(base_manifest) => Some((base_path, base_manifest)),
                        Err(ViewError::IoAt { source, .. })
                            if source.kind() == std::io::ErrorKind::NotFound =>
                        {
                            cold_build = Some((ColdBuildReason::BaseNotReady,
                                format!("derived owner {owner} manifest is missing; forcing a cold rebuild")));
                            None
                        }
                        Err(error) => return Err(error),
                    }
                } else {
                    None
                }
            }
            _ => None,
        };
        // A missing or unusable local base can seed from a ready sibling:
        // derived rows use manifest-relative paths,
        // and the ordinary cutoff/fingerprint checks still gate the patch.
        let sibling = if base.is_none() && request.callgraph {
            worker_seed::sibling_checkout_seed(
                request,
                Instant::now() + crate::db::STEADY_BUSY_TIMEOUT,
            )?
        } else {
            None
        };
        timing.phase("sibling_seed");
        let base = base.or_else(|| {
            sibling
                .as_ref()
                .map(|seed| (seed.path.clone(), seed.manifest.clone()))
        });
        let mut incremental_base = None;
        if let Some((base_path, base_manifest)) = base {
            let size = super::materialization::manifest_diff_size(&base_manifest, &manifest);
            if diff_exceeds_incremental_cutoff(size) {
                // Decided before cloning, so an oversized diff also skips the copy.
                cold_build = Some((
                    ColdBuildReason::LargeDiff,
                    format!(
                        "changed={} entries={} may_force_full_resolution={}",
                        size.changed, size.entries, size.may_force_full_resolution
                    ),
                ));
            } else {
                match super::generation::clone_derived(&base_path, &derived) {
                    Ok(_) => {
                        prepared.profile.seed_source =
                            if sibling.is_some() { "sibling" } else { "own" };
                        prepared.profile.cold_build_reason = "none";
                        #[cfg(test)]
                        {
                            prepared.profile.derived_cloned = true;
                        }
                        incremental_base = Some(base_manifest);
                    }
                    Err(error) if view_error_is_unreadable_database(&error) => {
                        cold_build = Some((ColdBuildReason::BaseUnreadable, error.to_string()));
                        super::generation::remove_unpublished_derived(&derived)?;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        prepared.profile.derived_clone_ms = clone_started.elapsed().as_millis();
        timing.phase("derived_clone");
        prepared.profile.io.enter(super::io::Phase::Materialize);
        let materialization_started = Instant::now();
        let mut patched = false;
        if let Some(base_manifest) = incremental_base.as_ref() {
            // Both the keeper (which reads the clone's schema) and the patch
            // can be the first to find that the copied base is unusable.
            let outcome = match open_derived_keeper(&derived) {
                Ok(keeper) => {
                    prepared.derived_checkpoint = Some((derived.clone(), keeper));
                    super::materialization::apply_manifest_diff_profiled(
                        &derived,
                        base_manifest,
                        &manifest,
                        callgraph.path(),
                    )
                    .map_err(|error| {
                        match cold_build_reason_for_diff_error(&error) {
                            Some(reason) => PatchFailure::Fallback(reason, error.to_string()),
                            None => {
                                PatchFailure::Fatal(ViewError::InvalidManifest(error.to_string()))
                            }
                        }
                    })
                }
                Err(error) if view_error_is_unreadable_database(&error) => Err(
                    PatchFailure::Fallback(ColdBuildReason::BaseUnreadable, error.to_string()),
                ),
                Err(error) => Err(PatchFailure::Fatal(error)),
            };
            match outcome {
                Ok((stats, timings)) => {
                    prepared.profile.materialization = timings;
                    patched = true;
                    log::info!(
                        "view manifest diff: generation={} path=incremental stats={stats:?}",
                        next_generation
                    );
                }
                Err(PatchFailure::Fatal(error)) => return Err(error),
                Err(PatchFailure::Fallback(reason, detail)) => {
                    // The clone cannot seed this generation. Close the keeper so
                    // no connection remains on the file set, delete it, and build
                    // the target from scratch at the same unpublished path.
                    prepared.derived_checkpoint = None;
                    super::generation::remove_unpublished_derived(&derived)?;
                    cold_build = Some((reason, detail));
                }
            }
        }
        if !patched {
            prepared.profile.seed_source = "cold";
            prepared.profile.cold_build_reason = match cold_build.as_ref().map(|(reason, _)| reason)
            {
                Some(reason) => reason.as_str(),
                None => "no_base",
            };
            prepared.derived_checkpoint = Some((derived.clone(), open_derived_keeper(&derived)?));
            if let Some((reason, detail)) = &cold_build {
                record_cold_build(view.view_dir(), *reason);
                if *reason == ColdBuildReason::LargeDiff {
                    log::info!(
                        "view manifest diff: generation={} path=cold reason={} {detail}",
                        next_generation,
                        reason.as_str()
                    );
                } else {
                    log::warn!(
                        "view manifest diff: generation={} path=cold reason={} error={detail}",
                        next_generation,
                        reason.as_str()
                    );
                }
            }
            crate::callgraph_store::materialize_manifest_view_database(
                &derived,
                callgraph.path(),
                &manifest,
            )
            .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
        }
        prepared.profile.materialization_call_ms = materialization_started.elapsed().as_millis();
        timing.phase("derived_materialization");
    }
    prepared.profile.io.enter(super::io::Phase::DerivedOther);
    let trigram = view.trigram_path(&next_generation)?;
    crate::private_storage::write(&trigram, [])
        .map_err(|error| ViewError::io_at("writing", &trigram, error))?;
    let artifacts = PublicationArtifacts {
        blob_databases: if reused_derived {
            vec![semantic.path().to_path_buf()]
        } else {
            vec![
                semantic.path().to_path_buf(),
                callgraph.path().to_path_buf(),
            ]
        },
        derived_database: derived.clone(),
        trigram_artifact: trigram.clone(),
        alias_database: aliases.path().to_path_buf(),
    };
    let closure = SqliteClosure {
        semantic: semantic.path().to_path_buf(),
        callgraph: callgraph.path().to_path_buf(),
        trigram,
        connections: Default::default(),
    };
    prepared.profile.io.enter(super::io::Phase::Closure);
    let closure_started = Instant::now();
    let publication_request = PublicationRequest {
        generation: &next_generation,
        base_generation: current_generation.as_deref(),
        manifest: &manifest,
        artifacts,
        closure_requirements: ClosureRequirements::default(),
    };
    let publication = if defer_derived {
        view.prepare_with_deferred_derived(&publication_request, &closure)?
    } else {
        view.prepare_with_reused_derived(
            &publication_request,
            &closure,
            if reused_derived {
                previous.as_ref()
            } else {
                None
            },
        )?
    };
    prepared.profile.closure_ms = closure_started.elapsed().as_millis();
    timing.phase("publication_closure");
    prepared.profile.derived_bytes = fs::metadata(&derived)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    prepared.profile.enter(3, phase)?;
    prepared.publication = Some(publication);
    prepared.report = AssemblyReport {
        generation: Some(next_generation),
        manifest: Some(manifest),
        blob_puts,
        pending_paths: prepared.report.pending_paths.clone(),
        pending_inputs: prepared.report.pending_inputs.clone(),
        published: false,
    };
    Ok(prepared)
}

/// One profile follows the same boundaries as cancellation and health reporting.
/// It survives preparation so CAS time is included, and logs only after the
/// prepared generation leaves the actor barrier (including failed attempts).
struct PublicationProfile {
    root: PathBuf,
    io: super::io::PublicationIo,
    overlap: super::io::Overlap,
    concurrent_publications: u64,
    semantic_fill: bool,
    generation: String,
    outcome: &'static str,
    candidates: usize,
    blob_puts: usize,
    pending_paths: usize,
    head_ms: u128,
    assembly_ms: u128,
    pointer_ms: u128,
    phase_ms: [u128; 4],
    active_phase: Option<usize>,
    phase_started: Instant,
    started: Instant,
    total_ms: u128,
    derived_bytes: u64,
    derived_clone_ms: u128,
    seed_source: &'static str,
    cold_build_reason: &'static str,
    #[cfg(test)]
    derived_cloned: bool,
    materialization_call_ms: u128,
    closure_ms: u128,
    materialization: super::materialization::profile::PhaseTimings,
}

impl PublicationProfile {
    fn new(root: &Path) -> Self {
        let now = Instant::now();
        Self {
            root: root.to_owned(),
            io: super::io::PublicationIo::new(root),
            overlap: super::io::Overlap::new(true),
            concurrent_publications: 0,
            semantic_fill: false,
            generation: "none".into(),
            outcome: "cancelled_or_failed",
            candidates: 0,
            blob_puts: 0,
            pending_paths: 0,
            head_ms: 0,
            assembly_ms: 0,
            pointer_ms: 0,
            phase_ms: [0; 4],
            active_phase: Some(0),
            phase_started: now,
            started: now,
            total_ms: 0,
            derived_bytes: 0,
            derived_clone_ms: 0,
            seed_source: "cold",
            cold_build_reason: "no_base",
            #[cfg(test)]
            derived_cloned: false,
            materialization_call_ms: 0,
            closure_ms: 0,
            materialization: super::materialization::profile::PhaseTimings::default(),
        }
    }

    fn enter(&mut self, phase: usize, callback: &mut impl FnMut(&str) -> Result<()>) -> Result<()> {
        self.checkpoint();
        self.io.enter(
            [
                super::io::Phase::Manifest,
                super::io::Phase::Blobs,
                super::io::Phase::DerivedOther,
                super::io::Phase::Cas,
            ][phase],
        );
        self.active_phase = Some(phase);
        self.phase_started = Instant::now();
        callback(["manifest", "blobs", "derived", "cas"][phase])
    }

    fn checkpoint(&mut self) {
        if let Some(phase) = self.active_phase.take() {
            self.phase_ms[phase] += self.phase_started.elapsed().as_millis();
        }
    }

    fn finish(&mut self) {
        if self.active_phase.is_none() {
            return;
        }
        self.io.finish();
        self.concurrent_publications = self.overlap.finish();
        self.checkpoint();
        self.total_ms = self.started.elapsed().as_millis();
    }
}

impl Drop for PublicationProfile {
    fn drop(&mut self) {
        if self.active_phase.is_some() {
            self.finish();
        }
        log_publication_profile(self);
    }
}

fn log_publication_profile(profile: &PublicationProfile) {
    crate::slog_info!("{}", publication_profile_line(profile));
}

fn publication_profile_line(profile: &PublicationProfile) -> String {
    // Retain the existing drill fields alongside the shared phase names. The
    // pointer transaction is a subset of the cas phase, which also includes
    // waiting to acquire the actor barrier.
    format!(
        "index_event kind=view_publication plane=views root={} outcome={} candidates={} blob_puts={} pending_paths={} manifest_ms={} blobs_ms={} derived_ms={} cas_ms={} head_ms={} assembly_ms={} blob_ms={} materialize_ms={} derived_clone_ms={} materialization_call_ms={} closure_ms={} materialize_load_bindings_select_ms={} materialize_delete_rows_ms={} materialize_owned_blob_decode_insert_ms={} materialize_join_load_payloads_ms={} materialize_join_decode_bind_index_entries_ms={} materialize_join_index_surface_replay_ms={} materialize_join_decode_resolved_callers_ms={} materialize_join_resolve_record_ms={} materialize_join_dependency_union_ms={} materialize_selected_join_ms={} materialize_write_bindings_ms={} materialize_emit_refs_edges_ms={} materialize_dispatch_ms={} materialize_commit_ms={} materialize_cleanup_memory_ms={} materialize_cleanup_connections_ms={} pointer_ms={} total_ms={} derived_bytes={} semantic_fill={} generation={} concurrent_publications={} seed_source={} cold_build_reason={} {}",
        profile.root.display(), profile.outcome, profile.candidates, profile.blob_puts,
        profile.pending_paths, profile.phase_ms[0], profile.phase_ms[1],
        profile.phase_ms[2], profile.phase_ms[3], profile.head_ms, profile.assembly_ms,
        profile.phase_ms[1], profile.materialization_call_ms,
        profile.derived_clone_ms,
        profile.materialization_call_ms,
        profile.closure_ms,
        profile.materialization.load_bindings_select_ms,
        profile.materialization.delete_rows_ms,
        profile.materialization.owned_blob_decode_and_insert_ms,
        profile.materialization.join_load_payloads_ms,
        profile.materialization.join_decode_bind_index_entries_ms,
        profile.materialization.join_index_and_surface_replay_ms,
        profile.materialization.join_decode_resolved_callers_ms,
        profile.materialization.join_resolve_and_record_ms,
        profile.materialization.join_dependency_union_ms,
        profile.materialization.selected_join_ms,
        profile.materialization.write_bindings_ms,
        profile.materialization.emit_refs_edges_ms,
        profile.materialization.dispatch_ms,
        profile.materialization.commit_ms,
        profile.materialization.cleanup_memory_ms,
        profile.materialization.cleanup_connections_ms,
        profile.pointer_ms, profile.total_ms, profile.derived_bytes,
        profile.semantic_fill, profile.generation, profile.concurrent_publications, profile.seed_source, profile.cold_build_reason, profile.io.fields(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_profile_line_attributes_the_canonical_root() {
        let mut profile = PublicationProfile::new(Path::new("/checkout"));
        profile.outcome = "published";
        profile.phase_ms = [5, 6, 7, 8];
        profile.derived_clone_ms = 9;
        profile.materialization_call_ms = 10;
        profile.closure_ms = 11;
        profile.materialization = crate::views::materialization::profile::PhaseTimings {
            load_bindings_select_ms: 11,
            delete_rows_ms: 12,
            owned_blob_decode_and_insert_ms: 13,
            join_load_payloads_ms: 14,
            join_decode_bind_index_entries_ms: 15,
            join_index_and_surface_replay_ms: 16,
            join_decode_resolved_callers_ms: 17,
            join_resolve_and_record_ms: 18,
            join_dependency_union_ms: 19,
            selected_join_ms: 20,
            write_bindings_ms: 21,
            emit_refs_edges_ms: 22,
            dispatch_ms: 26,
            commit_ms: 23,
            cleanup_memory_ms: 24,
            cleanup_connections_ms: 25,
            writes: Default::default(),
        };
        let line = publication_profile_line(&profile);
        assert!(line.contains("plane=views root=/checkout outcome=published"));
        assert!(line.contains("manifest_ms=5 blobs_ms=6 derived_ms=7 cas_ms=8"));
        assert!(line.contains("derived_clone_ms=9 materialization_call_ms=10 closure_ms=11"));
        assert!(line.contains("blob_ms=6 materialize_ms=10"));
        assert!(line.contains(
            "materialize_load_bindings_select_ms=11 materialize_delete_rows_ms=12 \
             materialize_owned_blob_decode_insert_ms=13 materialize_join_load_payloads_ms=14 \
             materialize_join_decode_bind_index_entries_ms=15 \
             materialize_join_index_surface_replay_ms=16 \
             materialize_join_decode_resolved_callers_ms=17 \
             materialize_join_resolve_record_ms=18 materialize_join_dependency_union_ms=19 \
             materialize_selected_join_ms=20 materialize_write_bindings_ms=21 \
             materialize_emit_refs_edges_ms=22 materialize_dispatch_ms=26 materialize_commit_ms=23 \
             materialize_cleanup_memory_ms=24 materialize_cleanup_connections_ms=25"
        ));
        assert!(line.contains("io_scope=process io_available="));
        assert!(line.contains("manifest_physical_bytes_written="));
        assert!(line.contains("closure_logical_bytes_written="));
        assert!(line.contains("total_bytes_read="));
        assert!(line.contains("semantic_fill=false generation=none concurrent_publications=0"));
        assert!(line.contains("seed_source=cold cold_build_reason=no_base"));
        assert_eq!(line.matches("index_event kind=view_publication").count(), 1);
    }
}

/// Why a graph-changing publication built its derived database from scratch
/// even though a current generation existed to patch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ColdBuildReason {
    /// The base was built with incompatible output, is unfinished, or is missing.
    BaseNotReady,
    /// The current generation's database records a different manifest than
    /// the one its generation names, so its rows cannot seed the diff.
    BaseFingerprintMismatch,
    /// SQLite reported the current generation's database as corrupt or not a
    /// database while copying or patching it.
    BaseUnreadable,
    /// The manifest diff is large enough that patching costs more than a
    /// cold build.
    LargeDiff,
}

impl ColdBuildReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::BaseNotReady => "base_not_ready",
            Self::BaseFingerprintMismatch => "base_fingerprint_mismatch",
            Self::BaseUnreadable => "base_unreadable",
            Self::LargeDiff => "large_diff",
        }
    }
}

/// Why patching a cloned base did not produce the next generation.
enum PatchFailure {
    /// The base is unusable; build the target cold instead.
    Fallback(ColdBuildReason, String),
    /// A cold build would fail the same way; fail the publication.
    Fatal(ViewError),
}

/// Per-view counts of cold derived builds that replaced an incremental patch,
/// by reason, since this process started. Reported in view health.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct ColdBuildCounts {
    pub base_not_ready: u64,
    pub base_fingerprint_mismatch: u64,
    pub base_unreadable: u64,
    pub large_diff: u64,
}

static COLD_BUILDS: std::sync::OnceLock<std::sync::Mutex<BTreeMap<PathBuf, ColdBuildCounts>>> =
    std::sync::OnceLock::new();

fn record_cold_build(view_dir: &Path, reason: ColdBuildReason) {
    let mut counts = COLD_BUILDS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = counts.entry(view_dir.to_path_buf()).or_default();
    match reason {
        ColdBuildReason::BaseNotReady => entry.base_not_ready += 1,
        ColdBuildReason::BaseFingerprintMismatch => entry.base_fingerprint_mismatch += 1,
        ColdBuildReason::BaseUnreadable => entry.base_unreadable += 1,
        ColdBuildReason::LargeDiff => entry.large_diff += 1,
    }
}

pub fn cold_build_counts(view_dir: &Path) -> ColdBuildCounts {
    COLD_BUILDS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(view_dir)
        .copied()
        .unwrap_or_default()
}

// Calibrated on opencode (about 6,300 manifest entries); the measurements are
// in docs/design/views-incremental-materialization.md, section 4.3.

/// Diffs with fewer changed entries than this always patch. Patching 62
/// changed entries took about 1 s on opencode, so small diffs never need the
/// cutoff, and small repositories keep exercising the incremental path.
const INCREMENTAL_CUTOFF_MIN_CHANGED: usize = 100;
/// Share of the target manifest's entries (in percent) above which a cold
/// build replaces patching when no resolver input changed. Patching used
/// about 10% less CPU than a cold build at 40% changed entries and the same
/// at 45%.
const INCREMENTAL_CUTOFF_PERCENT: usize = 40;
/// The same share when resolver configuration or a symlink, gitlink or
/// synthetic entry changed. Such a diff can make the patch re-resolve every
/// caller, and then it cost as much as a cold build at about 10%.
const INCREMENTAL_CUTOFF_PERCENT_FULL_RESOLUTION: usize = 10;

pub(crate) fn diff_exceeds_incremental_cutoff(
    size: super::materialization::ManifestDiffSize,
) -> bool {
    let percent = if size.may_force_full_resolution {
        INCREMENTAL_CUTOFF_PERCENT_FULL_RESOLUTION
    } else {
        INCREMENTAL_CUTOFF_PERCENT
    };
    size.changed >= INCREMENTAL_CUTOFF_MIN_CHANGED && size.changed * 100 > size.entries * percent
}

fn sqlite_error_is_unreadable_database(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if matches!(
                failure.code,
                rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
            )
    )
}

fn view_error_is_unreadable_database(error: &ViewError) -> bool {
    matches!(error, ViewError::Sqlite(error) if sqlite_error_is_unreadable_database(error))
}

/// Map an `apply_manifest_diff` failure to a cold-build fallback, or `None`
/// when a cold build would not help. A missing callgraph blob, for example,
/// fails a cold build the same way, so it stays an error.
fn cold_build_reason_for_diff_error(
    error: &crate::callgraph_store::CallGraphStoreError,
) -> Option<ColdBuildReason> {
    if super::materialization::is_base_fingerprint_mismatch(error) {
        return Some(ColdBuildReason::BaseFingerprintMismatch);
    }
    match error {
        crate::callgraph_store::CallGraphStoreError::Sqlite(error)
            if sqlite_error_is_unreadable_database(error) =>
        {
            Some(ColdBuildReason::BaseUnreadable)
        }
        _ => None,
    }
}

/// Keep one connection alive so SQLite does not checkpoint the committed WAL
/// when the materializer closes its writer before pointer publication.
fn open_derived_keeper(derived: &Path) -> Result<crate::db::file_identity::IdentityConnection> {
    let keeper =
        crate::db::file_identity::IdentityConnection::open(derived, "views::assembly::assemble")?;
    keeper.busy_timeout(std::time::Duration::from_secs(5))?;
    // Opening a handle or setting journal_mode alone does not attach its
    // pager to the WAL. Read the schema so closing the materializer is not
    // the last WAL connection and cannot checkpoint before publication.
    keeper.pragma_update(None, "journal_mode", "WAL")?;
    keeper.query_row("SELECT COUNT(*) FROM sqlite_schema", [], |row| {
        row.get::<_, i64>(0)
    })?;
    Ok(keeper)
}

fn manifest_entry_callgraph_key(entry: &ManifestEntry) -> Option<&str> {
    match entry {
        ManifestEntry::Regular { planes, .. } => planes.callgraph.as_deref(),
        ManifestEntry::Synthetic { planes, .. } => Some(&planes.callgraph),
        ManifestEntry::Symlink { .. } | ManifestEntry::Gitlink { .. } => None,
    }
}

pub(super) fn is_resolution_input(path: &[u8]) -> bool {
    let name = path.rsplit(|byte| *byte == b'/').next().unwrap_or(path);
    name == b"package.json"
        || name == b"Cargo.toml"
        || name == b"go.mod"
        || name == b".gitignore"
        || name.starts_with(b"tsconfig") && name.ends_with(b".json")
}

fn next_generation(current: Option<&str>, desired_head: &str) -> String {
    let generation = current
        .map(generation_number)
        .unwrap_or(0)
        .saturating_add(1);
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{generation}-{}-{nanos}-{serial}-{desired_head}",
        std::process::id()
    )
}

fn generation_number(generation: &str) -> u64 {
    generation
        .split_once('-')
        .and_then(|(number, _)| number.parse().ok())
        .unwrap_or(0)
}

struct SqliteClosure {
    semantic: PathBuf,
    callgraph: PathBuf,
    trigram: PathBuf,
    // Lazy handles preserve probes of empty planes and malformed keys without
    // opening their databases. Valid keys share a handle for this closure only.
    connections: std::cell::RefCell<BTreeMap<bool, crate::db::lifecycle::TrackedConnection>>,
}

impl PublicationClosure for SqliteClosure {
    fn contains_blob(&self, plane: ArtifactPlane, full_key: &str) -> Result<bool> {
        let Some(key) = decode_hex(full_key) else {
            return Ok(false);
        };
        let path = match plane {
            ArtifactPlane::Semantic => &self.semantic,
            ArtifactPlane::Callgraph => &self.callgraph,
        };
        let mut connections = self.connections.borrow_mut();
        let semantic = plane == ArtifactPlane::Semantic;
        if let std::collections::btree_map::Entry::Vacant(entry) = connections.entry(semantic) {
            entry.insert(crate::db::lifecycle::TrackedConnection::open(
                path,
                crate::db::lifecycle::SqliteStore::BlobStore,
            )?);
        }
        let present = connections[&semantic]
            .prepare_cached(
                "SELECT 1 FROM blob_payloads INDEXED BY blob_membership WHERE full_key = ?1",
            )?
            .query_row([key], |_| Ok(()))
            .optional()?
            .is_some();
        Ok(present)
    }

    fn probe_blobs(&self, keys: &[(ArtifactPlane, &str)]) -> Result<()> {
        let mut present = BTreeSet::new();
        for (plane_id, plane) in [ArtifactPlane::Semantic, ArtifactPlane::Callgraph]
            .into_iter()
            .enumerate()
        {
            let wanted = keys
                .iter()
                .filter(|(p, _)| *p == plane)
                .map(|(_, key)| *key)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            for chunk in wanted.chunks(500) {
                // Sorted key batches follow the blob index rather than manifest
                // path order. Keep the tracked plane handle across all batches.
                let Some(first_valid) = chunk.iter().find(|key| decode_hex(key).is_some()) else {
                    continue;
                };
                self.contains_blob(plane, first_valid)?;
                let connections = self.connections.borrow();
                let Some(connection) = connections.get(&(plane == ArtifactPlane::Semantic)) else {
                    continue;
                };
                let decoded = chunk
                    .iter()
                    .filter_map(|key| decode_hex(key))
                    .collect::<Vec<_>>();
                if decoded.is_empty() {
                    continue;
                }
                let sql = membership_query(decoded.len());
                let mut statement = connection.prepare_cached(&sql)?;
                for key in statement.query_map(rusqlite::params_from_iter(&decoded), |row| {
                    row.get::<_, Vec<u8>>(0)
                })? {
                    present.insert((plane_id, key?));
                }
            }
        }
        // Return the first missing key in manifest order, even though membership
        // reads are grouped by plane and sorted for index locality.
        for &(plane, key) in keys {
            let plane_id = usize::from(plane == ArtifactPlane::Callgraph);
            if decode_hex(key).is_none_or(|key| !present.contains(&(plane_id, key))) {
                return Err(ViewError::MissingBlob {
                    plane,
                    key: key.to_owned(),
                });
            }
        }
        Ok(())
    }

    fn trigram_is_present(&self) -> Result<bool> {
        Ok(self.trigram.is_file())
    }

    fn contains_alias(&self, _git_oid: &str) -> Result<bool> {
        Ok(true)
    }
}

/// Integration tests make a publication slow on purpose, without a large
/// checkout, by setting `AFT_TEST_VIEW_PUBLICATION_DELAY_MS`: requests must
/// keep being answered while a publication runs.
///
/// Setting `AFT_TEST_VIEW_PUBLICATION_GATE_FILE` to a path holds every
/// publication until that file exists. A test that must observe the state
/// before a publication lands creates the file when it is done, so the window
/// it observes stays open however slow the machine is, instead of closing
/// after a fixed delay.
fn publication_delay_for_test() {
    if let Some(gate) = std::env::var_os("AFT_TEST_VIEW_PUBLICATION_GATE_FILE") {
        let gate = std::path::PathBuf::from(gate);
        while !gate.exists() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    if let Some(millis) = std::env::var("AFT_TEST_VIEW_PUBLICATION_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        std::thread::sleep(std::time::Duration::from_millis(millis));
    }
}

/// True when `manifest` was published with the call graph off: some source
/// file has a callgraph language but no callgraph key. Such a generation's
/// derived database holds no call graph at all, so a reader must report the
/// call graph unavailable rather than answer from it.
pub fn manifest_lacks_callgraph(manifest: &super::Manifest) -> bool {
    manifest
        .entries()
        .any(|(path, entry)| lacks_callgraph_key(entry, path.as_bytes()))
}

/// True when `entry` is a source file published while the call graph was off:
/// it has a callgraph language but no callgraph key.
fn lacks_callgraph_key(entry: &ManifestEntry, rel_path: &[u8]) -> bool {
    let ManifestEntry::Regular {
        planes,
        resolution_input,
        ..
    } = entry
    else {
        return false;
    };
    planes.callgraph.is_none()
        && (*resolution_input
            || super::callgraph::source_language(&path_from_bytes(rel_path)).is_some())
}

fn missing_callgraph_payload(
    store: &BlobStore,
    key: &FullKey,
    source: &[u8],
    language: &str,
    resolution_input: bool,
) -> Result<Option<Vec<u8>>> {
    // A branch return can name content absent from the previous manifest but
    // already stored by an earlier generation. Probe membership before paying
    // for extraction, serialization, and an immutable no-op put. The joiner
    // verifies the digest when it actually consumes the payload.
    if store
        .contains(key)
        .map_err(|error| ViewError::InvalidManifest(error.to_string()))?
    {
        return Ok(None);
    }
    let blob = if resolution_input {
        CallgraphBlob::config(source.to_vec(), crate::views::callgraph::PRODUCER)
    } else {
        // A source that is not valid UTF-8 cannot be parsed. Failing here
        // would fail the whole publication, so it gets the call graph of an
        // empty file instead: no symbols, as the call graph built without
        // views leaves unreadable files out. Assembly records the path as
        // skipped.
        CallgraphBlob::extract(
            std::str::from_utf8(source).unwrap_or(""),
            language,
            crate::views::callgraph::PRODUCER,
        )
        .map_err(|error| ViewError::InvalidManifest(error.to_string()))?
    };
    Ok(Some(blob.to_bytes().map_err(|error| {
        ViewError::InvalidManifest(error.to_string())
    })?))
}

#[cfg(test)]
mod reuse_tests {
    use super::*;
    use crate::blob_store::{PayloadReadCounts, PAYLOAD_READ_COUNTS};

    fn seed_callgraph_payloads(store: &BlobStore, project: &Path, files: usize) -> usize {
        let mut connection = Connection::open(store.path()).unwrap();
        // These fixtures measure reads, not write durability. Commit the blob
        // database once so Windows does not flush each payload to disk.
        let tx = connection.transaction().unwrap();
        let mut payload_bytes = 0;
        {
            let mut insert = tx.prepare("INSERT INTO blob_payloads (full_key, payload, payload_digest, payload_schema) VALUES (?1, ?2, ?3, ?4)").unwrap();
            for index in 0..files {
                let source = fs::read(project.join(format!("file_{index}.ts"))).unwrap();
                let key = CallgraphKey::from_bytes(
                    &source,
                    "typescript",
                    crate::views::callgraph::PRODUCER,
                )
                .full_key();
                let payload = missing_callgraph_payload(store, &key, &source, "typescript", false)
                    .unwrap()
                    .unwrap();
                payload_bytes += payload.len();
                insert
                    .execute(rusqlite::params![
                        key.as_bytes().as_slice(),
                        &payload,
                        blake3::hash(&payload).as_bytes().as_slice(),
                        crate::blob_store::CALLGRAPH_PAYLOAD_SCHEMA
                    ])
                    .unwrap();
            }
        }
        assert!(
            !tx.is_autocommit(),
            "fixture payloads must share one transaction"
        );
        tx.commit().unwrap();
        payload_bytes
    }

    #[test]
    fn views_shared_checkout_presence_reads_no_payloads() {
        const FILES: usize = 2048;
        let project = super::cold_fallback_tests::repository(FILES);
        let storage = tempfile::tempdir().unwrap();
        let store = BlobStore::open(storage.path(), "shared", BlobPlane::Callgraph).unwrap();
        let payload_bytes = seed_callgraph_payloads(&store, project.path(), FILES);
        let request = AssemblyRequest {
            storage: storage.path().to_path_buf(),
            project_root: project.path().to_path_buf(),
            family: "shared".into(),
            scope: "new-checkout".into(),
            desired_head: "unchanged".into(),
            changed_paths: Default::default(),
            semantic_keys: Default::default(),
            require_semantic: false,
            allow_blob_put: true,
            callgraph: true,
        };
        PAYLOAD_READ_COUNTS.with(|counts| counts.set(PayloadReadCounts::default()));
        let mut measured = false;
        let mut prepared = prepare_checkout(&request, &mut |phase| {
            if phase == "derived" {
                let counts = PAYLOAD_READ_COUNTS.with(|counts| counts.get());
                eprintln!("checkout_presence files={FILES} stored_payload_bytes={payload_bytes} reads={counts:?}");
                assert_eq!(counts, PayloadReadCounts::default());
                measured = true;
            }
            Ok(())
        }).unwrap();
        assert!(measured, "must reach the boundary before materialization");
        let report = prepared.commit().unwrap();
        assert!(report.published);
        assert_eq!(report.blob_puts, 0);
        let materialization = PAYLOAD_READ_COUNTS.with(|counts| counts.get());
        eprintln!("checkout_materialization files={FILES} reads={materialization:?}");
        PAYLOAD_READ_COUNTS.with(|counts| counts.set(PayloadReadCounts::default()));
        let report = publish_checkout(&request).unwrap();
        assert!(!report.published);
        assert_eq!(report.blob_puts, 0);
        assert_eq!(
            PAYLOAD_READ_COUNTS.with(|counts| counts.get()),
            PayloadReadCounts::default()
        );
    }

    #[test]
    fn views_corrupt_cached_payload_is_not_reused() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = BlobStore::open(dir.path(), "corrupt", BlobPlane::Callgraph).unwrap();
        let source = b"export function target() {}";
        let key = CallgraphKey::from_bytes(source, "typescript", crate::views::callgraph::PRODUCER)
            .full_key();
        let payload = missing_callgraph_payload(&store, &key, source, "typescript", false)
            .unwrap()
            .unwrap();
        store.put(&key, &payload).unwrap();
        Connection::open(store.path())
            .unwrap()
            .execute("UPDATE blob_payloads SET payload_digest = zeroblob(32)", [])
            .unwrap();
        assert!(store.contains(&key).unwrap());
        assert!(
            missing_callgraph_payload(&store, &key, source, "typescript", false)
                .unwrap()
                .is_none()
        );
        assert!(
            store.get(&key).unwrap().is_none(),
            "membership must not let corrupt bytes reach a consumer"
        );
    }

    #[test]
    fn views_pending_first_publication_has_no_manifest_to_reuse() {
        const FILES: usize = 8;
        let project = super::cold_fallback_tests::repository(FILES);
        let storage = tempfile::tempdir().unwrap();
        let mut store = BlobStore::open(storage.path(), "pending", BlobPlane::Callgraph).unwrap();
        seed_callgraph_payloads(&store, project.path(), FILES - 1);
        let mut request = AssemblyRequest {
            storage: storage.path().to_path_buf(),
            project_root: project.path().to_path_buf(),
            family: "pending".into(),
            scope: "pending-checkout".into(),
            desired_head: "unchanged".into(),
            changed_paths: Default::default(),
            semantic_keys: Default::default(),
            require_semantic: false,
            allow_blob_put: false,
            callgraph: true,
        };
        let mut source_reads = 0;
        let mut prepared = prepare_checkout(&request, &mut |phase| {
            source_reads += usize::from(phase == "working_tree_read");
            Ok(())
        })
        .unwrap();
        let report = prepared.commit().unwrap();
        assert!(!report.published);
        assert_eq!(report.blob_puts, 0);
        assert_eq!(
            report.pending_paths,
            BTreeSet::from([b"file_7.ts".to_vec()])
        );
        assert!(report.manifest.is_none());
        assert_eq!(source_reads, FILES);
        drop(prepared);
        let view = ViewStore::open(storage.path(), &request.scope).unwrap();
        assert!(view.current_generation().unwrap().is_none());

        // Filling the missing blob cannot reconstruct the other seven entries:
        // the failed publication did not persist its candidate manifest.
        let source = fs::read(project.path().join("file_7.ts")).unwrap();
        let key =
            CallgraphKey::from_bytes(&source, "typescript", crate::views::callgraph::PRODUCER)
                .full_key();
        let payload = missing_callgraph_payload(&store, &key, &source, "typescript", false)
            .unwrap()
            .unwrap();
        store.put(&key, &payload).unwrap();
        request.changed_paths = report.pending_paths;
        source_reads = 0;
        let mut prepared = prepare_checkout(&request, &mut |phase| {
            source_reads += usize::from(phase == "working_tree_read");
            Ok(())
        })
        .unwrap();
        let report = prepared.commit().unwrap();
        assert!(report.published);
        assert_eq!(report.blob_puts, 0);
        assert_eq!(report.manifest.unwrap().entries().count(), FILES);
        assert_eq!(source_reads, FILES);
    }

    #[test]
    fn views_cached_callgraph_payload_does_not_extract_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = BlobStore::open(dir.path(), "reuse", BlobPlane::Callgraph).unwrap();
        let source = b"export function target() {}";
        let key = CallgraphKey::from_bytes(source, "typescript", crate::views::callgraph::PRODUCER)
            .full_key();
        let payload = missing_callgraph_payload(&store, &key, source, "typescript", false)
            .unwrap()
            .unwrap();
        store.put(&key, &payload).unwrap();
        assert!(
            missing_callgraph_payload(&store, &key, source, "typescript", false)
                .unwrap()
                .is_none(),
            "cached content must not be extracted or put again"
        );
    }

    #[test]
    fn views_read_only_assembly_late_fill_probe_reads_no_payloads() {
        let project = super::cold_fallback_tests::repository(1);
        let storage = tempfile::tempdir().unwrap();
        let mut store = BlobStore::open(storage.path(), "late-fill", BlobPlane::Callgraph).unwrap();
        let source = fs::read(project.path().join("file_0.ts")).unwrap();
        let key =
            CallgraphKey::from_bytes(&source, "typescript", crate::views::callgraph::PRODUCER)
                .full_key();
        let payload = missing_callgraph_payload(&store, &key, &source, "typescript", false)
            .unwrap()
            .unwrap();
        let request = AssemblyRequest {
            storage: storage.path().to_path_buf(),
            project_root: project.path().to_path_buf(),
            family: "late-fill".into(),
            scope: "late-fill-checkout".into(),
            desired_head: "unchanged".into(),
            changed_paths: Default::default(),
            semantic_keys: Default::default(),
            require_semantic: false,
            allow_blob_put: false,
            callgraph: true,
        };
        PAYLOAD_READ_COUNTS.with(|counts| counts.set(PayloadReadCounts::default()));
        let mut measured = false;
        let mut prepared = prepare_checkout(&request, &mut |phase| {
            if phase == "blobs" {
                // Another writer may fill a key between candidate assembly and
                // the read-only publisher's availability check.
                store.put(&key, &payload).unwrap();
            }
            if phase == "derived" {
                assert_eq!(
                    PAYLOAD_READ_COUNTS.with(|counts| counts.get()),
                    PayloadReadCounts::default()
                );
                measured = true;
            }
            Ok(())
        })
        .unwrap();
        assert!(measured);
        let report = prepared.commit().unwrap();
        assert!(report.published);
        assert_eq!(report.blob_puts, 0);
        assert!(report.pending_paths.is_empty());
    }
}

fn membership_query(count: usize) -> String {
    format!(
        "SELECT full_key FROM blob_payloads INDEXED BY blob_membership WHERE full_key IN ({})",
        vec!["?"; count].join(",")
    )
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    crate::blob_store::v2::parse_hex32(value).map(|bytes| bytes.to_vec())
}

#[cfg(test)]
mod key_decode_tests {
    #[test]
    fn non_ascii_hex_is_rejected_without_panicking() {
        let key = format!("aé{}", "0".repeat(61));
        assert_eq!(key.len(), 64);
        assert!(super::decode_hex(&key).is_none());
    }
}

fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;
        PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec()))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// A missing checkout path is a deletion, even if it disappeared after the
/// membership walk. Other failures must stay errors rather than hide members.
fn working_tree_io<T>(
    result: std::io::Result<T>,
    operation: &'static str,
    path: &Path,
) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ViewError::io_at(operation, path, error)),
    }
}

pub(crate) fn read_working_tree_file(path: &Path) -> Result<Option<Vec<u8>>> {
    working_tree_io(crate::project_content::read(path), "reading", path)
}

fn read_symlink_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    let Some(target) = working_tree_io(fs::read_link(path), "reading symlink", path)? else {
        return Ok(None);
    };
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        Ok(Some(target.as_os_str().as_bytes().to_vec()))
    }
    #[cfg(not(unix))]
    {
        Ok(Some(target.to_string_lossy().as_bytes().to_vec()))
    }
}

#[cfg(test)]
#[path = "closure_connection_tests.rs"]
mod closure_connection_tests;

#[cfg(test)]
#[path = "semantic_fill_tests.rs"]
mod semantic_fill_tests;

#[cfg(test)]
#[path = "cold_fallback_tests.rs"]
mod cold_fallback_tests;

#[path = "assembly_seed.rs"]
pub(super) mod worker_seed;

#[cfg(test)]
#[path = "worker_seed_tests.rs"]
mod worker_seed_tests;

/// Path status describes the generation being served, so a publication's
/// status changes land only with the generation they describe.
#[cfg(test)]
mod status_settlement_tests {
    use super::*;

    const FAMILY: &str = "settle-family";
    const SCOPE: &str = "settle-scope";

    fn git(project: &Path, args: &[&str]) {
        assert!(std::process::Command::new("git")
            .current_dir(project)
            .args(args)
            .status()
            .unwrap()
            .success());
    }

    fn request(project: &Path, storage: &Path, head: &str) -> AssemblyRequest {
        AssemblyRequest {
            storage: storage.to_path_buf(),
            project_root: project.to_path_buf(),
            family: FAMILY.into(),
            scope: SCOPE.into(),
            desired_head: head.into(),
            changed_paths: Default::default(),
            semantic_keys: Default::default(),
            require_semantic: false,
            allow_blob_put: true,
            callgraph: true,
        }
    }

    fn status(storage: &Path) -> PathStatusStore {
        let view = ViewStore::open(storage, SCOPE).unwrap();
        PathStatusStore::open(view.view_dir()).unwrap()
    }

    fn skips(storage: &Path) -> BTreeMap<String, usize> {
        status(storage).summary().unwrap().skipped_by_reason
    }

    fn pending(storage: &Path) -> usize {
        status(storage).summary().unwrap().pending_count
    }

    /// A skip recorded for the served generation stays while a publication
    /// that would clear it is cancelled or dropped before it publishes, and
    /// so does a pending row it would settle. The first publication that
    /// succeeds applies both; a later no-op publication settles too, because
    /// its manifest is the served one.
    #[test]
    fn views_status_changes_wait_for_their_generation_to_publish() {
        let project = super::cold_fallback_tests::repository(1);
        let root = project.path().canonicalize().unwrap();
        // 0xe9 is "é" in Latin-1 and an invalid byte in UTF-8.
        fs::write(root.join("latin1.py"), b"def caf\xe9():\n    pass\n").unwrap();
        git(&root, &["add", "."]);
        git(
            &root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--quiet",
                "-m",
                "latin1",
            ],
        );
        let storage = tempfile::tempdir().unwrap();
        assert!(
            publish_checkout(&request(&root, storage.path(), "head-1"))
                .unwrap()
                .published
        );
        let skipped = BTreeMap::from([("non_utf8_source".to_string(), 1)]);
        assert_eq!(skips(storage.path()), skipped);

        // The file becomes readable, and a scheduled path that is not a
        // member waits for a complete publication to settle it.
        fs::write(root.join("latin1.py"), "def cafe():\n    pass\n").unwrap();
        status(storage.path())
            .mark_pending(b"gone.ts", "view publication scheduled", 1)
            .unwrap();

        // Cancelled at the last phase before the pointer swap.
        let cancelled = prepare_checkout(&request(&root, storage.path(), "head-2"), &mut |phase| {
            if phase == "cas" {
                Err(ViewError::InvalidManifest("cancelled".into()))
            } else {
                Ok(())
            }
        });
        assert!(cancelled.is_err());
        assert_eq!(
            skips(storage.path()),
            skipped,
            "cancelled build changed status"
        );
        assert_eq!(
            pending(storage.path()),
            1,
            "cancelled build settled a pending row"
        );

        // Fully prepared, then dropped without publishing (for example when
        // the configuration changes before the prepared build is committed).
        let prepared =
            prepare_checkout(&request(&root, storage.path(), "head-2"), &mut |_| Ok(())).unwrap();
        drop(prepared);
        assert_eq!(
            skips(storage.path()),
            skipped,
            "unpublished build changed status"
        );
        assert_eq!(
            pending(storage.path()),
            1,
            "unpublished build settled a pending row"
        );

        assert!(
            publish_checkout(&request(&root, storage.path(), "head-2"))
                .unwrap()
                .published
        );
        assert!(skips(storage.path()).is_empty());
        assert_eq!(pending(storage.path()), 0);

        // A no-op publication describes the served generation: it settles.
        status(storage.path())
            .mark_pending(b"also_gone.ts", "view publication scheduled", 2)
            .unwrap();
        let report = publish_checkout(&request(&root, storage.path(), "head-2")).unwrap();
        assert!(!report.published, "expected a no-op publication");
        assert_eq!(pending(storage.path()), 0);
    }

    /// A committed repository with one Python file that is not UTF-8, so
    /// its first publication records a call-graph skip.
    fn latin1_repository() -> (tempfile::TempDir, PathBuf) {
        let project = super::cold_fallback_tests::repository(1);
        let root = project.path().canonicalize().unwrap();
        fs::write(root.join("latin1.py"), LATIN1).unwrap();
        git(&root, &["add", "."]);
        git(
            &root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--quiet",
                "-m",
                "latin1",
            ],
        );
        (project, root)
    }

    // 0xe9 is "é" in Latin-1 and an invalid byte in UTF-8.
    const LATIN1: &[u8] = b"def caf\xe9():\n    pass\n";

    /// Settlements are written after the commit barrier, so an older one can
    /// finish after a newer one. It must not overwrite the newer status: here
    /// the older generation would clear a skip the newer one records again.
    #[test]
    fn views_an_older_settlement_finishing_last_never_overwrites_a_newer_one() {
        let (_project, root) = latin1_repository();
        let storage = tempfile::tempdir().unwrap();
        publish_checkout(&request(&root, storage.path(), "head-1")).unwrap();
        let skipped = BTreeMap::from([("non_utf8_source".to_string(), 1)]);
        assert_eq!(skips(storage.path()), skipped);

        // The older publication sees the file decode, and commits; its
        // settlement is held back.
        fs::write(root.join("latin1.py"), "def cafe():\n    pass\n").unwrap();
        let mut older =
            prepare_checkout(&request(&root, storage.path(), "head-2"), &mut |_| Ok(())).unwrap();
        assert!(older.commit().unwrap().published);
        assert_eq!(skips(storage.path()), skipped, "settled inside commit");

        // The newer publication sees it undecodable again and settles first.
        fs::write(root.join("latin1.py"), LATIN1).unwrap();
        let mut newer =
            prepare_checkout(&request(&root, storage.path(), "head-3"), &mut |_| Ok(())).unwrap();
        assert!(newer.commit().unwrap().published);
        assert!(matches!(newer.settle_status(), SettleOutcome::Applied(_)));
        assert_eq!(skips(storage.path()), skipped);

        assert_eq!(older.settle_status(), SettleOutcome::Superseded);
        assert_eq!(skips(storage.path()), skipped, "the older settlement won");
    }

    /// A no-op publication whose status is already settled has nothing to
    /// write, and its commit does not open the status ledger.
    #[test]
    fn views_settled_no_op_publication_writes_no_status() {
        let (_project, root) = latin1_repository();
        let storage = tempfile::tempdir().unwrap();
        publish_checkout(&request(&root, storage.path(), "head-1")).unwrap();
        let view_dir = ViewStore::open(storage.path(), SCOPE)
            .unwrap()
            .view_dir()
            .to_path_buf();
        let writes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = std::sync::Arc::clone(&writes);
        before_settle_write_for_test(&view_dir, move || {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });

        let mut prepared =
            prepare_checkout(&request(&root, storage.path(), "head-1"), &mut |_| Ok(())).unwrap();
        assert!(!prepared.commit().unwrap().published, "expected a no-op");
        assert_eq!(prepared.settle_status(), SettleOutcome::Nothing);
        assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 0);

        // With a row to settle, the same no-op writes exactly that row.
        status(storage.path())
            .mark_pending(b"gone.ts", "view publication scheduled", 1)
            .unwrap();
        let mut prepared =
            prepare_checkout(&request(&root, storage.path(), "head-1"), &mut |_| Ok(())).unwrap();
        assert!(!prepared.commit().unwrap().published);
        assert_eq!(prepared.settle_status(), SettleOutcome::Applied(1));
        assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(pending(storage.path()), 0);
    }
}
