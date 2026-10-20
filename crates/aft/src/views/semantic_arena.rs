//! The resident semantic vector arena of one repository family.
//!
//! Every view of the family that is resident in this process reads its
//! semantic vectors from here. A run (one file's embedded chunks under one
//! content key) is decoded from the family blob store once, when a generation
//! is admitted or when a fill stores it, and is then shared by `Arc` with every
//! view whose manifest names that key. The vectors of a key are therefore held
//! and counted once per process however many checkouts use them, and queries
//! read only runs that are already resident: they never decode store rows.
//!
//! The arena also makes embedding single-flight across views. A fill claims
//! each key it is about to embed; another view that needs the same key while
//! the claim is held waits for it instead of calling the model a second time.
//!
//! Vectors are shared below the run level too. A run entering the arena has
//! each vector swapped for a bit-identical one already resident, so a fill
//! that reuses an unchanged chunk of an edited file, or two keys that embed
//! the same text, hold one allocation instead of one per run.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

use crate::blob_store::v2::{FamilyKey, FamilyStoreReader};
use crate::semantic_index::{SemanticVectors, ViewPayloadProducer};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Resident runs of one family, shared by every view of it in this process.
#[derive(Debug)]
pub struct SemanticArena {
    storage: PathBuf,
    family: String,
    runs: Mutex<HashMap<FamilyKey, Arc<SemanticVectors>>>,
    /// Every distinct vector held by a resident run, by content digest.
    vectors: Mutex<VectorPool>,
    inflight: Mutex<HashSet<FamilyKey>>,
    released: Condvar,
    decodes: AtomicU64,
    store_reads: AtomicU64,
    chunk_batch: Mutex<()>,
    #[cfg(test)]
    chunk_shortcut: std::sync::atomic::AtomicBool,
}

/// Resident arena memory. Every key is counted once, whatever the number of
/// views that reference it, and every vector allocation once, whatever the
/// number of runs that share it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ArenaMemory {
    pub runs: usize,
    pub chunks: usize,
    pub bytes: u64,
    /// Distinct vector allocations, at most `chunks`.
    pub vectors: usize,
    /// Bytes of floats in those allocations; part of `bytes`.
    pub vector_bytes: u64,
}

/// Weak handles to the arena's vectors, keyed by a digest of their bits.
///
/// A dead `Weak` keeps its allocation until it is dropped, so dead handles
/// are pruned whenever `trim` releases runs and whenever the table has
/// doubled since the last prune.
#[derive(Debug, Default)]
struct VectorPool {
    by_digest: HashMap<blake3::Hash, Weak<[f32]>>,
    // Populate only from chunk-row vectors that were read or stored; a file
    // payload can contain different legacy floats.
    by_chunk: HashMap<FamilyKey, Weak<[f32]>>,
    live_after_prune: usize,
    scratch: Vec<u8>,
}

impl VectorPool {
    /// Shares only bit-identical vectors, preserving cached norms and scores.
    fn share(&mut self, run: &mut SemanticVectors) {
        for vector in run.vectors_mut() {
            self.share_vector(vector);
        }
        if self.by_digest.len() + self.by_chunk.len()
            > self.live_after_prune.max(1024).saturating_mul(2)
        {
            self.prune();
        }
    }

    fn share_vector(&mut self, vector: &mut Arc<[f32]>) {
        self.scratch.clear();
        for value in vector.iter() {
            self.scratch
                .extend_from_slice(&value.to_bits().to_le_bytes());
        }
        let digest = blake3::hash(&self.scratch);
        match self.by_digest.get(&digest).and_then(Weak::upgrade) {
            Some(resident) if same_bits(&resident, vector) => *vector = resident,
            // Keep unequal allocations on a digest collision.
            Some(_) => {}
            None => {
                self.by_digest.insert(digest, Arc::downgrade(vector));
            }
        }
    }

    fn prune(&mut self) {
        self.by_digest.retain(|_, vector| vector.strong_count() > 0);
        self.by_chunk.retain(|_, vector| vector.strong_count() > 0);
        self.live_after_prune = self.by_digest.len() + self.by_chunk.len();
    }
}

fn same_bits(left: &[f32], right: &[f32]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.to_bits() == right.to_bits())
}

type Registry = HashMap<(PathBuf, String), Weak<SemanticArena>>;

fn arenas() -> &'static Mutex<Registry> {
    static ARENAS: OnceLock<Mutex<Registry>> = OnceLock::new();
    ARENAS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Every arena alive in this process, one per `(storage, family)`, so the
/// memory census can count each family's runs exactly once.
pub fn live_arenas() -> Vec<Arc<SemanticArena>> {
    lock(arenas()).values().filter_map(Weak::upgrade).collect()
}

/// The process's arena for `family` in `storage`. Views of the same family
/// get the same arena while any of them holds it.
pub fn arena_for(storage: &Path, family: &str) -> Arc<SemanticArena> {
    let mut arenas = lock(arenas());
    arenas.retain(|_, arena| arena.strong_count() > 0);
    let key = (storage.to_path_buf(), family.to_owned());
    if let Some(arena) = arenas.get(&key).and_then(Weak::upgrade) {
        return arena;
    }
    let arena = Arc::new(SemanticArena {
        storage: key.0.clone(),
        family: key.1.clone(),
        runs: Mutex::new(HashMap::new()),
        vectors: Mutex::new(VectorPool::default()),
        inflight: Mutex::new(HashSet::new()),
        released: Condvar::new(),
        decodes: AtomicU64::new(0),
        store_reads: AtomicU64::new(0),
        chunk_batch: Mutex::new(()),
        #[cfg(test)]
        chunk_shortcut: std::sync::atomic::AtomicBool::new(true),
    });
    arenas.insert(key, Arc::downgrade(&arena));
    arena
}

/// Exclusive right to embed one key. Dropping it wakes views waiting on the
/// key, whether or not the run was installed.
#[derive(Debug)]
pub struct ArenaClaim {
    arena: Arc<SemanticArena>,
    key: FamilyKey,
}

impl ArenaClaim {
    pub fn key(&self) -> &FamilyKey {
        &self.key
    }
}

impl Drop for ArenaClaim {
    fn drop(&mut self) {
        lock(&self.arena.inflight).remove(&self.key);
        self.arena.released.notify_all();
    }
}

impl SemanticArena {
    #[cfg(test)]
    pub(crate) fn set_chunk_shortcut(&self, enabled: bool) {
        self.chunk_shortcut.store(enabled, Ordering::Relaxed);
    }

    /// Looks up the key derived from the producer identity and exact text.
    /// Weak handles do not keep floats alive. Cold file loads must first validate
    /// chunk rows before those vectors become eligible for this shortcut.
    pub(crate) fn chunk_vector(&self, key: &FamilyKey) -> Option<Arc<[f32]>> {
        #[cfg(test)]
        if !self.chunk_shortcut.load(Ordering::Relaxed) {
            return None;
        }
        lock(&self.vectors).by_chunk.get(key)?.upgrade()
    }

    /// Serializes bounded chunk-vector batches across all views of the family.
    pub(crate) fn chunk_batch(&self) -> MutexGuard<'_, ()> {
        lock(&self.chunk_batch)
    }

    #[cfg(test)]
    pub(crate) fn pause_resources_are_free(&self) -> bool {
        self.chunk_batch.try_lock().is_ok() && lock(&self.inflight).is_empty()
    }

    pub fn storage(&self) -> &Path {
        &self.storage
    }

    pub fn family(&self) -> &str {
        &self.family
    }

    /// The resident run for `key`, if one was admitted.
    pub fn get(&self, key: &FamilyKey) -> Option<Arc<SemanticVectors>> {
        lock(&self.runs).get(key).cloned()
    }

    /// Admits `key` from the family store, decoding it once. Returns `None`
    /// when the store has no row for it. A resident run is returned without
    /// touching the store.
    pub fn load(
        &self,
        store: &FamilyStoreReader,
        key: &FamilyKey,
        rel_path: &Path,
        producer: &ViewPayloadProducer<'_>,
    ) -> Result<Option<Arc<SemanticVectors>>, String> {
        if let Some(run) = self.get(key) {
            return Ok(Some(run));
        }
        self.store_reads.fetch_add(1, Ordering::Relaxed);
        let Some(payload) = store.get(key).map_err(|error| error.to_string())? else {
            return Ok(None);
        };
        self.decodes.fetch_add(1, Ordering::Relaxed);
        let run = SemanticVectors::decode_view_payload(&payload, rel_path, producer)?;
        Ok(Some(self.install(*key, run)))
    }

    /// Makes a stored run resident, sharing equal allocations. File payloads
    /// alone cannot populate the chunk index: legacy floats may not match the
    /// independently immutable chunk rows.
    pub fn install(&self, key: FamilyKey, mut run: SemanticVectors) -> Arc<SemanticVectors> {
        if let Some(resident) = self.get(&key) {
            return resident;
        }
        lock(&self.vectors).share(&mut run);
        Arc::clone(lock(&self.runs).entry(key).or_insert_with(|| Arc::new(run)))
    }

    /// Records a vector only after its exact chunk row was read or stored.
    /// The caller keeps this allocation alive until the file run shares it.
    pub(crate) fn remember_chunk(&self, key: FamilyKey, vector: Vec<f32>) -> Arc<[f32]> {
        // Cap index entries separately: distinct chunk keys can share the same
        // vector allocation. Weak references never keep the floats alive.
        const MAX_CHUNK_KEYS: usize = 65_536;
        let mut vector: Arc<[f32]> = vector.into();
        let mut pool = lock(&self.vectors);
        pool.share_vector(&mut vector);
        if pool.by_chunk.len() >= MAX_CHUNK_KEYS {
            if let Some(old) = pool.by_chunk.keys().next().copied() {
                pool.by_chunk.remove(&old);
            }
        }
        pool.by_chunk.insert(key, Arc::downgrade(&vector));
        if pool.by_digest.len() + pool.by_chunk.len()
            > pool.live_after_prune.max(1024).saturating_mul(2)
        {
            pool.prune();
        }
        vector
    }

    /// Claims `key` for embedding, or returns `None` while another fill holds
    /// it. A claim is only worth taking after `get` and the store missed.
    pub fn claim(self: &Arc<Self>, key: FamilyKey) -> Option<ArenaClaim> {
        lock(&self.inflight).insert(key).then(|| ArenaClaim {
            arena: Arc::clone(self),
            key,
        })
    }

    /// Waits until no fill holds a claim on `key`, or `timeout` passes.
    /// Returns whether the claim was released.
    pub fn wait_released(&self, key: &FamilyKey, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut inflight = lock(&self.inflight);
        while inflight.contains(key) {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            inflight = self
                .released
                .wait_timeout(inflight, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        true
    }

    /// Drops runs no view references any more, after a view unbinds or moves
    /// to another generation. Returns how many runs were released.
    pub fn trim(&self) -> usize {
        let released = {
            let mut runs = lock(&self.runs);
            let before = runs.len();
            runs.retain(|_, run| Arc::strong_count(run) > 1);
            before - runs.len()
        };
        if released > 0 {
            lock(&self.vectors).prune();
        }
        released
    }

    pub fn memory(&self) -> ArenaMemory {
        let runs = lock(&self.runs);
        let mut seen = HashSet::new();
        let mut memory = ArenaMemory {
            runs: runs.len(),
            ..ArenaMemory::default()
        };
        for run in runs.values() {
            memory.chunks += run.chunk_count();
            memory.bytes = memory
                .bytes
                .saturating_add(run.resident_bytes_without_vectors());
            for vector in run.vectors() {
                if seen.insert(Arc::as_ptr(vector).cast::<f32>()) {
                    memory.vectors += 1;
                    memory.vector_bytes = memory
                        .vector_bytes
                        .saturating_add(crate::semantic_index::vector_bytes(vector));
                }
            }
        }
        memory.bytes = memory.bytes.saturating_add(memory.vector_bytes);
        memory.bytes = memory.bytes.saturating_add(
            (lock(&self.vectors).by_chunk.capacity()
                * std::mem::size_of::<(FamilyKey, Weak<[f32]>)>()) as u64,
        );
        memory
    }

    /// Payloads decoded from the store since the arena was created. Queries
    /// never add to it; only admission and fills do.
    pub fn decode_count(&self) -> u64 {
        self.decodes.load(Ordering::Relaxed)
    }

    /// Store rows read since the arena was created.
    pub fn store_reads(&self) -> u64 {
        self.store_reads.load(Ordering::Relaxed)
    }
}
