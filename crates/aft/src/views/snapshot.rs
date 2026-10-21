//! Pinned query snapshots, the live delta, and the generation switch.
//!
//! A query reads one `(generation, delta version, intent version)` snapshot,
//! taken before its first index read, and never re-reads the pointer
//! mid-query. Taking a snapshot is an `Arc` clone: the generation is kept
//! resident by one read marker per open generation, not one per query.
//!
//! The live delta holds the difference between the checkout's disk and its
//! generation. Its invariant: **an entry exists for a path exactly when the
//! last observed disk state differs from the generation's entry**. Every
//! mutation is journaled, including one that removes an entry, because the
//! generation switch needs it.
//!
//! When a new generation `G+1` is published (by this daemon or by another
//! one), the successor delta is derived over
//! `K = keys(delta@v) ∪ paths(diff(G, G+1))`: for each path in `K` the last
//! observed disk state is the live entry's if there is one, else `G`'s entry
//! (exact, by the invariant), and the successor has an entry exactly when that
//! state differs from `G+1`. The rule never reads what the fold itself read,
//! so it is correct for a revert to `G`'s bytes or an add-then-delete anywhere
//! between the fold's read and the swap, and for a foreign CAS winner.
//! Mutations after `v` are replayed from the journal under the swap lock, and
//! a successor built against an older epoch is discarded.

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use crate::blob_store::v2::{ContentHash, FamilyPlane};

use super::manifest_v2::{EntryV2, ManifestV2};
use super::readiness::LiveEntries;
use super::registry::ProtectedGeneration;
use super::RelPath;

/// The last observed disk state of one path.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DiskState {
    Present { content: ContentHash, size: u64 },
    Absent,
}

impl DiskState {
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self::Present {
            content: ContentHash::of(bytes),
            size: bytes.len() as u64,
        }
    }

    /// What a generation says about a path's disk bytes. Only regular files
    /// have content; symlinks, gitlinks and synthetic entries read as absent
    /// here and are reconciled through membership instead.
    pub fn in_generation(manifest: &ManifestV2, rel_path: &RelPath) -> Self {
        match manifest.get(rel_path) {
            Some(EntryV2::Regular { content, size, .. }) => Self::Present {
                content: *content,
                size: *size,
            },
            _ => Self::Absent,
        }
    }
}

/// How a generation is kept resident while snapshots of it exist.
#[derive(Debug)]
pub enum Residency {
    /// This process's own view: a read marker in its view directory.
    Marker(crate::root_cache::ReadMarker),
    /// A registry reader's pin, taken with pin-then-verify.
    Protected(ProtectedGeneration),
}

/// One opened, immutable generation. Dropping the last `Arc` releases its
/// residency pin.
#[derive(Debug)]
pub struct OpenGeneration {
    name: String,
    manifest: Arc<ManifestV2>,
    _residency: Option<Residency>,
}

impl OpenGeneration {
    pub fn new(
        name: impl Into<String>,
        manifest: ManifestV2,
        residency: Option<Residency>,
    ) -> Self {
        Self {
            name: name.into(),
            manifest: Arc::new(manifest),
            _residency: residency,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn manifest(&self) -> &ManifestV2 {
        &self.manifest
    }

    pub fn disk_state(&self, rel_path: &RelPath) -> DiskState {
        DiskState::in_generation(&self.manifest, rel_path)
    }
}

/// Plane data attached to a live entry, such as the trigram postings computed
/// when the entry was applied. Planes downcast their own attachment.
pub type PlaneAttachment = Arc<dyn Any + Send + Sync>;

#[derive(Clone)]
pub struct LiveEntry {
    pub disk: DiskState,
    /// The watcher or intent sequence that produced this state.
    pub seq: u64,
    pub attachments: BTreeMap<FamilyPlane, PlaneAttachment>,
}

impl fmt::Debug for LiveEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveEntry")
            .field("disk", &self.disk)
            .field("seq", &self.seq)
            .field("planes", &self.attachments.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl LiveEntry {
    pub fn new(disk: DiskState, seq: u64) -> Self {
        Self {
            disk,
            seq,
            attachments: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalRecord {
    pub version: u64,
    pub rel_path: RelPath,
    pub disk: DiskState,
}

/// Whether the watcher can vouch for disk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WatcherState {
    Healthy,
    Overflowed,
    Stopped,
    /// A strict reconcile is running; the index must not prune.
    Reconciling,
}

/// Supplies plane attachments for an entry the switch creates because disk
/// went back to the old generation's bytes. The old generation is pinned for
/// the whole switch, so its keys are protected and nothing is recomputed.
pub type CarryFromGeneration<'a> = dyn Fn(&RelPath, &OpenGeneration) -> LiveEntry + 'a;

type Entries = BTreeMap<RelPath, LiveEntry>;

/// The per-root live delta. Owned by the root actor; callers serialize access
/// with the root's index write lock.
#[derive(Debug)]
pub struct LiveDelta {
    base: Arc<OpenGeneration>,
    epoch: u64,
    version: u64,
    entries: Arc<Entries>,
    journal: Vec<JournalRecord>,
    intent: Arc<BTreeMap<RelPath, u64>>,
    intent_version: u64,
    watcher: WatcherState,
    root_ignore_files: Option<Arc<RootIgnoreFiles>>,
}

/// The disk state of each root ignore file (see
/// `live_delta::IGNORE_FILE_NAMES`), present or absent, as read just
/// before the walk that decided a snapshot's membership.
pub type RootIgnoreFiles = BTreeMap<RelPath, DiskState>;

/// The delta as a builder read it at version `v`.
#[derive(Clone, Debug)]
pub struct DeltaCut {
    pub base: Arc<OpenGeneration>,
    pub epoch: u64,
    pub version: u64,
    entries: Arc<Entries>,
}

impl DeltaCut {
    pub fn keys(&self) -> impl Iterator<Item = &RelPath> {
        self.entries.keys()
    }
}

/// A successor delta derived against a new generation, waiting for its swap.
#[derive(Debug)]
pub struct SuccessorDraft {
    successor: Arc<OpenGeneration>,
    epoch: u64,
    cut_version: u64,
    entries: Entries,
}

impl SuccessorDraft {
    pub fn entries(&self) -> impl Iterator<Item = (&RelPath, &LiveEntry)> {
        self.entries.iter()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SwitchError {
    /// The delta was reconciled (overflow, rescan, membership) after the cut;
    /// rebuild the successor from the reconciled delta.
    EpochChanged { cut: u64, current: u64 },
    /// The draft was derived against a different base than the delta's.
    BaseChanged,
}

impl LiveDelta {
    pub fn new(base: Arc<OpenGeneration>) -> Self {
        Self {
            base,
            epoch: 0,
            version: 0,
            entries: Arc::new(Entries::new()),
            journal: Vec::new(),
            intent: Arc::new(BTreeMap::new()),
            intent_version: 0,
            watcher: WatcherState::Healthy,
            root_ignore_files: None,
        }
    }

    /// Records the root ignore files the latest full membership walk ran
    /// under. The record covers every root ignore file, including one that
    /// ignores itself and so is never a member: it still decides membership.
    pub fn record_root_ignore_files(&mut self, files: Option<RootIgnoreFiles>) {
        self.root_ignore_files = files.map(Arc::new);
    }

    pub fn base(&self) -> &Arc<OpenGeneration> {
        &self.base
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn watcher(&self) -> WatcherState {
        self.watcher
    }

    pub fn set_watcher(&mut self, watcher: WatcherState) {
        if self.watcher != watcher {
            self.version += 1;
        }
        self.watcher = watcher;
    }

    pub fn entry(&self, rel_path: &RelPath) -> Option<&LiveEntry> {
        self.entries.get(rel_path)
    }

    pub fn journal(&self) -> &[JournalRecord] {
        &self.journal
    }

    /// Records an observed disk state. The entry is kept only while it
    /// differs from the base generation. Replay needs only the last changed
    /// state or attachment per path, not repeated observations of one entry.
    pub fn apply(&mut self, rel_path: RelPath, entry: LiveEntry) -> u64 {
        let previous = self
            .entries
            .get(&rel_path)
            .map_or_else(|| self.base.disk_state(&rel_path), |entry| entry.disk);
        self.version += 1;
        let attachments_changed = self.entries.get(&rel_path).is_some_and(|previous| {
            previous.attachments.len() != entry.attachments.len()
                || previous.attachments.iter().any(|(plane, attachment)| {
                    entry
                        .attachments
                        .get(plane)
                        .is_none_or(|new| !Arc::ptr_eq(attachment, new))
                })
        });
        if previous != entry.disk || attachments_changed {
            self.journal.retain(|record| record.rel_path != rel_path);
            self.journal.push(JournalRecord {
                version: self.version,
                rel_path: rel_path.clone(),
                disk: entry.disk,
            });
        }
        let entries = Arc::make_mut(&mut self.entries);
        if entry.disk == self.base.disk_state(&rel_path) {
            entries.remove(&rel_path);
        } else {
            entries.insert(rel_path, entry);
        }
        self.version
    }

    /// Records that an AFT write to `rel_path` was acknowledged and not yet
    /// applied. Queries match such paths on their current bytes.
    pub fn record_intent(&mut self, rel_path: RelPath) -> u64 {
        self.intent_version += 1;
        Arc::make_mut(&mut self.intent).insert(rel_path, self.intent_version);
        self.intent_version
    }

    /// Clears the intent for `rel_path` once an apply at or after
    /// `intent_version` has landed.
    pub fn resolve_intent(&mut self, rel_path: &RelPath, intent_version: u64) {
        let intent = Arc::make_mut(&mut self.intent);
        if intent
            .get(rel_path)
            .is_some_and(|recorded| *recorded <= intent_version)
        {
            intent.remove(rel_path);
            self.intent_version += 1;
        }
    }

    /// Marks the delta as reconciled from scratch (overflow, rescan, bind,
    /// membership change). Successors built from an older cut are discarded.
    pub fn bump_epoch(&mut self) -> u64 {
        self.epoch += 1;
        self.epoch
    }

    /// The delta as a builder reads it now.
    pub fn cut(&self) -> DeltaCut {
        DeltaCut {
            base: Arc::clone(&self.base),
            epoch: self.epoch,
            version: self.version,
            entries: Arc::clone(&self.entries),
        }
    }

    /// A query snapshot: `Arc` clones only.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            inner: Arc::new(SnapshotInner {
                generation: Arc::clone(&self.base),
                delta_version: self.version,
                intent_version: self.intent_version,
                epoch: self.epoch,
                entries: Arc::clone(&self.entries),
                intent: Arc::clone(&self.intent),
                watcher: self.watcher,
                root_ignore_files: self.root_ignore_files.clone(),
            }),
        }
    }

    /// Replays the journal after the draft's cut against the delta as it is
    /// now, then swaps in the successor. Callers hold the root's index write
    /// lock, the same lock every apply takes, so nothing lands in between.
    pub fn replay_and_swap(
        &mut self,
        mut draft: SuccessorDraft,
        carry: &CarryFromGeneration<'_>,
    ) -> Result<(), SwitchError> {
        if draft.epoch != self.epoch {
            return Err(SwitchError::EpochChanged {
                cut: draft.epoch,
                current: self.epoch,
            });
        }
        let replay = self
            .journal
            .iter()
            .filter(|record| record.version > draft.cut_version)
            .map(|record| record.rel_path.clone())
            .collect::<BTreeSet<_>>();
        for path in replay {
            let observed = self.entries.get(&path).cloned();
            rederive_path(
                &path,
                observed,
                &self.base,
                &draft.successor,
                carry,
                &mut draft.entries,
            );
        }
        self.base = draft.successor;
        self.entries = Arc::new(draft.entries);
        // One successor is in flight at a time per root, so nothing older than
        // the swap is needed again.
        self.journal.clear();
        Ok(())
    }
}

impl LiveEntries for LiveDelta {
    fn live_states(&self) -> Box<dyn Iterator<Item = (&RelPath, &DiskState)> + '_> {
        Box::new(self.entries.iter().map(|(path, entry)| (path, &entry.disk)))
    }

    fn live_state(&self, rel_path: &RelPath) -> Option<&DiskState> {
        self.entries.get(rel_path).map(|entry| &entry.disk)
    }
}

#[cfg(test)]
mod mutation_tests {
    use super::*;
    use crate::views::manifest_v2::{ManifestHeader, Producers};

    fn delta() -> LiveDelta {
        LiveDelta::new(Arc::new(OpenGeneration::new(
            "base",
            ManifestV2::new(ManifestHeader {
                producers: Producers {
                    trigram: "test".into(),
                    semantic: None,
                    callgraph: "test".into(),
                },
                head_tree: None,
                ignore_fingerprint: None,
                segment: None,
            }),
            None,
        )))
    }

    #[test]
    fn unchanged_observations_do_not_grow_the_journal() {
        let mut delta = delta();
        let path = RelPath::new(b"a.rs".to_vec()).unwrap();
        for _ in 0..128 {
            delta.apply(path.clone(), LiveEntry::new(DiskState::Absent, 0));
        }
        assert!(delta.journal().is_empty());
        let disk = DiskState::of_bytes(b"a");
        delta.apply(path.clone(), LiveEntry::new(disk, 1));
        for _ in 0..128 {
            delta.apply(path.clone(), LiveEntry::new(disk, 1));
        }
        assert_eq!(delta.journal().len(), 1);
    }

    #[test]
    fn repeated_path_changes_keep_one_replay_record() {
        let mut delta = delta();
        let path = RelPath::new(b"a.rs".to_vec()).unwrap();
        let successor = delta.base().clone();
        let draft = derive_successor(&delta.cut(), successor, &carry_disk_state_only);
        for sequence in 0..128 {
            delta.apply(
                path.clone(),
                LiveEntry::new(
                    DiskState::of_bytes(sequence.to_string().as_bytes()),
                    sequence,
                ),
            );
        }
        assert_eq!(delta.journal().len(), 1);
        let expected = delta.snapshot().disk_state(&path);
        delta
            .replay_and_swap(draft, &carry_disk_state_only)
            .unwrap();
        assert_eq!(delta.snapshot().disk_state(&path), expected);
    }

    #[test]
    fn same_content_attachment_updates_are_replayed() {
        let mut delta = delta();
        let path = RelPath::new(b"a.rs".to_vec()).unwrap();
        let disk = DiskState::of_bytes(b"a");
        let mut old = LiveEntry::new(disk, 1);
        old.attachments
            .insert(FamilyPlane::Trigram, Arc::new(1_usize));
        delta.apply(path.clone(), old);
        let draft = derive_successor(&delta.cut(), delta.base().clone(), &carry_disk_state_only);
        let mut new = LiveEntry::new(disk, 2);
        new.attachments
            .insert(FamilyPlane::Trigram, Arc::new(2_usize));
        delta.apply(path.clone(), new);
        delta
            .replay_and_swap(draft, &carry_disk_state_only)
            .unwrap();
        assert_eq!(
            delta.entry(&path).unwrap().attachments[&FamilyPlane::Trigram].downcast_ref::<usize>(),
            Some(&2)
        );
    }

    #[test]
    fn watcher_and_intent_resolution_advance_the_revision_fence() {
        let mut delta = delta();
        let path = RelPath::new(b"a.rs".to_vec()).unwrap();
        let before = delta.version() + delta.epoch() + delta.snapshot().intent_version();
        delta.set_watcher(WatcherState::Overflowed);
        assert!(delta.version() + delta.epoch() + delta.snapshot().intent_version() > before);
        let intent = delta.record_intent(path.clone());
        let before = delta.version() + delta.epoch() + delta.snapshot().intent_version();
        delta.resolve_intent(&path, intent);
        assert!(delta.version() + delta.epoch() + delta.snapshot().intent_version() > before);
        assert!(delta.snapshot().pending_intent().next().is_none());
    }

    #[test]
    fn retained_cut_installs_after_edit_traffic_pauses() {
        let root = tempfile::tempdir().unwrap();
        let delta = Arc::new(std::sync::Mutex::new(delta()));
        let path = RelPath::new(b"a.rs".to_vec()).unwrap();
        let successor = delta.lock().unwrap().base().clone();
        let (edit_tx, edit_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker_delta = delta.clone();
        let worker_root = root.path().to_path_buf();
        let worker_path = path.clone();
        let worker = std::thread::spawn(move || {
            for sequence in edit_rx {
                std::fs::write(worker_root.join("a.rs"), format!("edit {sequence}")).unwrap();
                crate::views::live_delta::apply_event(
                    &mut worker_delta.lock().unwrap(),
                    &worker_root,
                    &worker_path,
                    &crate::blob_store::v2::TrigramPolicy {
                        max_file_size: 1 << 20,
                    },
                )
                .unwrap();
                done_tx.send(()).unwrap();
            }
        });
        let mut epoch_changes = 0;
        for sequence in 0..64 {
            let draft = derive_successor(
                &delta.lock().unwrap().cut(),
                successor.clone(),
                &carry_disk_state_only,
            );
            edit_tx.send(sequence).unwrap();
            done_rx.recv().unwrap();
            match delta
                .lock()
                .unwrap()
                .replay_and_swap(draft, &carry_disk_state_only)
            {
                Err(SwitchError::EpochChanged { .. }) => epoch_changes += 1,
                other => panic!("unexpected retained-cut result: {other:?}"),
            }
        }
        drop(edit_tx);
        worker.join().unwrap();
        let mut delta = delta.lock().unwrap();
        let draft = derive_successor(&delta.cut(), successor, &carry_disk_state_only);
        delta
            .replay_and_swap(draft, &carry_disk_state_only)
            .unwrap();
        assert_eq!(epoch_changes, 64);
        eprintln!("retained-cut attempts: 64 EpochChanged, 0 obsolete-revision, 1 successful install after edits paused");
    }
}

/// The paths a switch must re-derive: the cut's live keys plus every path
/// whose entry differs between the two generations. Paths outside this set
/// are equal in both generations and had no live entry, so disk equals the
/// successor there.
pub fn rebase_paths(cut: &DeltaCut, successor: &OpenGeneration) -> BTreeSet<RelPath> {
    let mut paths = cut.keys().cloned().collect::<BTreeSet<_>>();
    paths.extend(cut.base.manifest().diff_paths(successor.manifest()));
    paths
}

/// Derives the successor delta of `cut` against `successor`.
pub fn derive_successor(
    cut: &DeltaCut,
    successor: Arc<OpenGeneration>,
    carry: &CarryFromGeneration<'_>,
) -> SuccessorDraft {
    let mut entries = Entries::new();
    for path in rebase_paths(cut, &successor) {
        let observed = cut.entries.get(&path).cloned();
        rederive_path(&path, observed, &cut.base, &successor, carry, &mut entries);
    }
    SuccessorDraft {
        successor,
        epoch: cut.epoch,
        cut_version: cut.version,
        entries,
    }
}

fn rederive_path(
    path: &RelPath,
    observed: Option<LiveEntry>,
    old: &Arc<OpenGeneration>,
    successor: &Arc<OpenGeneration>,
    carry: &CarryFromGeneration<'_>,
    entries: &mut Entries,
) {
    let entry = match observed {
        Some(entry) => entry,
        // No live entry means disk equalled the old generation, exactly.
        None => carry(path, old),
    };
    if entry.disk == successor.disk_state(path) {
        entries.remove(path);
    } else {
        entries.insert(path.clone(), entry);
    }
}

/// A carry function that attaches nothing: the entry records only the old
/// generation's disk state.
pub fn carry_disk_state_only(path: &RelPath, generation: &OpenGeneration) -> LiveEntry {
    LiveEntry::new(generation.disk_state(path), 0)
}

#[derive(Debug)]
struct SnapshotInner {
    generation: Arc<OpenGeneration>,
    delta_version: u64,
    intent_version: u64,
    epoch: u64,
    entries: Arc<Entries>,
    intent: Arc<BTreeMap<RelPath, u64>>,
    watcher: WatcherState,
    root_ignore_files: Option<Arc<RootIgnoreFiles>>,
}

/// Where a path's answer comes from in one snapshot. Exactly one source per
/// path: a live entry supersedes the generation's entry.
#[derive(Debug)]
pub enum Source<'a> {
    Live(&'a LiveEntry),
    Generation(&'a EntryV2),
    Absent,
}

/// One pinned `(generation, delta@version, intent@version)` view.
#[derive(Clone, Debug)]
pub struct Snapshot {
    inner: Arc<SnapshotInner>,
}

impl Snapshot {
    pub fn generation(&self) -> &Arc<OpenGeneration> {
        &self.inner.generation
    }

    pub fn delta_version(&self) -> u64 {
        self.inner.delta_version
    }

    pub fn intent_version(&self) -> u64 {
        self.inner.intent_version
    }

    pub fn epoch(&self) -> u64 {
        self.inner.epoch
    }

    pub fn watcher(&self) -> WatcherState {
        self.inner.watcher
    }

    /// The root ignore files the walk behind this snapshot's membership ran
    /// under, or `None` when no walk recorded them (a snapshot taken without
    /// walking the checkout, such as a read-only load).
    pub fn root_ignore_files(&self) -> Option<&RootIgnoreFiles> {
        self.inner.root_ignore_files.as_deref()
    }

    /// Paths with an acknowledged AFT write that the delta had not applied as
    /// of this snapshot. Queries match them on their current bytes before any
    /// index pruning.
    pub fn pending_intent(&self) -> impl Iterator<Item = &RelPath> {
        self.inner.intent.keys()
    }

    pub fn live_entries(&self) -> impl Iterator<Item = (&RelPath, &LiveEntry)> {
        self.inner.entries.iter()
    }

    pub fn source(&self, rel_path: &RelPath) -> Source<'_> {
        if let Some(entry) = self.inner.entries.get(rel_path) {
            return Source::Live(entry);
        }
        match self.inner.generation.manifest().get(rel_path) {
            Some(entry) => Source::Generation(entry),
            None => Source::Absent,
        }
    }

    /// The disk state this snapshot believes `rel_path` has.
    pub fn disk_state(&self, rel_path: &RelPath) -> DiskState {
        match self.source(rel_path) {
            Source::Live(entry) => entry.disk,
            Source::Generation(_) | Source::Absent => self.inner.generation.disk_state(rel_path),
        }
    }

    /// Every path the snapshot considers present, with its content.
    pub fn membership(&self) -> BTreeMap<RelPath, DiskState> {
        let mut members = BTreeMap::new();
        for (path, entry) in self.inner.generation.manifest().entries() {
            if let EntryV2::Regular { content, size, .. } = entry {
                members.insert(
                    path.clone(),
                    DiskState::Present {
                        content: *content,
                        size: *size,
                    },
                );
            }
        }
        for (path, entry) in self.inner.entries.iter() {
            match entry.disk {
                DiskState::Present { .. } => {
                    members.insert(path.clone(), entry.disk);
                }
                DiskState::Absent => {
                    members.remove(path);
                }
            }
        }
        members
    }
}

impl LiveEntries for Snapshot {
    fn live_states(&self) -> Box<dyn Iterator<Item = (&RelPath, &DiskState)> + '_> {
        Box::new(
            self.inner
                .entries
                .iter()
                .map(|(path, entry)| (path, &entry.disk)),
        )
    }

    fn live_state(&self, rel_path: &RelPath) -> Option<&DiskState> {
        self.inner.entries.get(rel_path).map(|entry| &entry.disk)
    }
}
