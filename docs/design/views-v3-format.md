# Views v3 format: namespace and reader-floor foundation

This document records the v3 namespace audit, the floor protocol, and the v3
registry and plane store contract: schemas, application IDs, PRAGMA checks,
the reservation ledger and lock, open modes, accounting, budgets and floor,
the retention collector and the power-loss replay result. No production
caller publishes v3: v3 writes happen only in tests and in harness-selected
isolated storage.

## Release walker audit

Audit sources are the immutable local release tags (authorized because the
campaign's frozen evidence pack contains no release-source copies):

- **A — v0.60.0:** `988f4f3e52b9b825ad71cc75e41352dc6856efa4`.
- **B — v0.61.0:** `8aceb397b5d9836004b548ea1a5aa8f1d75cd0e8`.

`git fetch origin tag v0.61.0 --force` and `git rev-parse <tag>^{commit}`
verified these identities. References below are repository-relative source
paths and line ranges **at the indicated release**, not current-main lines.
The audit reads `git show <sha>:<path>`; it does not inspect a live store.

The namespace under review is
`<storage>/blobs/v3/<family>/`, including `members.sqlite`, plane stores and
their WAL/SHM files, and member directories at `views/<scope>/` beneath that
family. A v3 member must never appear in a v2 registry. In particular, v3
member directories must **not** be placed at `<storage>/views/v3/`: the old v1
GC visits every direct child of `<storage>/views/` except `v2`.

The verdicts concern ordinary AFT-created layouts: no forged registry paths,
no path-traversal family IDs, and no symlink/alias redirect from an old
namespace into v3. These are namespace-isolation verdicts, not a security
boundary against corrupt or hostile storage metadata. Both releases trust
`view_dir` text in v2 registry rows (A `crates/aft/src/views/registry.rs:1037-1065`;
B `:1047-1075`). A forged row could send marking, measurement and removal into
v3. That is why separate registration is a mandatory invariant, not merely a
naming convention.

### GC, eviction and explicit legacy pruning

| Walker | Release A source | Release B source / difference | Namespace and verdict |
|---|---|---|---|
| `gc::sweep_planes_bounded`, sibling views marking | `crates/aft/src/gc/mod.rs:131-173,245-325` | Same views-root/filter, `:132-173,245-317` | Direct children of `<storage>/views/`, excluding `v2`; reads each child's manifests, pins and readers and may reclaim dead protection. **No v3 descent** with the chosen nested blob root. |
| v1 plane sweep and reclaim | `crates/aft/src/gc/mod.rs:174-224,327-459` | `:175-216,327-436`; bounded incremental reclaim replaces full vacuum/retry-marker creation | Addresses only `<storage>/blobs/<family>/{semantic,callgraph}.sqlite` and the associated old reclaim marker. Does not recursively list `blobs`. **No v3 files** for valid old family IDs. |
| v2 family GC marking, dead protections and missing-root deregistration | `crates/aft/src/gc/family.rs:234-309,430-465,513-557,677-820` | `:234-307,453-484,535-579,697-841`; page-capacity skip and bounded reclaim added | Opens plane stores through the v2 path builder, walks only registered v2 members' view directories. **No v3 descent**, conditional on no v3 row in v2 registry. |
| v2 member measurement, SQLite-open check, eviction and interrupted eviction | `crates/aft/src/views/eviction.rs:301-400,421-474,558-745` | `:303-554,573-641,720-907`; immutable-generation size cache added and payload aggregates removed from family measurement | Family tree is `blobs/v2/<family>`; member trees come from v2 registry `view_dir`. Counts and removes only those trees. **No v3 descent** under the registration invariant. |
| v2 store path/registry registration | `crates/aft/src/blob_store/v2.rs:370-378`; `crates/aft/src/views/registry.rs:199-203,388-434` | Same roots; registry adds a process-lifetime size cache, not a namespace | `blobs/v2/<family>` and `views/v2/<scope>` remain separate from v3. |
| `aft cache prune-legacy` CLI (only native `cache` subcommand) | `crates/aft/src/cli/prune_legacy.rs:45-89`; `crates/aft/src/main.rs:225-231` | Native CLI/helper unchanged byte-for-byte; package forwarding at `packages/aft-cli/src/commands/cache.ts:10-25` | Parses dry-run/`--yes` then delegates to migration pruning; **no independent v3 walk**. |
| Pruner inventory, sizing, rename-aside, re-census and deletion | `crates/aft/src/migration/prune_legacy.rs:45-77,157-187,242-331,694-735,752-886` | File unchanged byte-for-byte between A and B | Only valid-key children under `index`, `semantic`, `callgraph`, `artifact-owners`, plus root children with `.prune-legacy-` prefix. Import eligibility reads `migration/v2/<key>/imports.sqlite`. Neither inventory nor deletion selects `blobs/v3`. **No v3 count/touch/delete**. |

A leftover `.prune-legacy-*` tree is owned by the old pruner; manually moving
v3 data into such a tree would make it prune-eligible. Similarly the public v1
sweep path builder does not itself reject `family = "v3"`, but ordinary callers
supply valid family identities, and retention filters for 16 hex characters.
Neither exception is a recursive route into the chosen v3 family tree.

### Every storage-retention lane

All paths in the next table are in `crates/aft/src/storage_retention.rs`.
The direct-child storage listing used to discover harness domains is not a
recursive walk: only `opencode`, `pi`, `runner`, `mcp--*` and `fed--*` add flat
`callgraph`/`inspect` domains. Seeing the name `blobs` or `v3` in a parent
listing is not descent into it.

| Lane | A lines | B lines | Root/filter and v3 verdict |
|---|---|---|---|
| Root/domain discovery and lexical cursor | `44-51,830-974,1946-2040` | `44-51,835-979,2405-2503` | Only `views`, `callgraph`, `inspect`, `index`, `semantic`, `symbols`, and the harness flat domains above. Requires a 16-hex key and directory, or flat `<key>.current`/`<key>.sqlite`. **No v3 count/touch/delete**. |
| Binding history, ownership, eligibility and unknown-key clocks | `296-319,619-827,1361-1405` | `299-322,622-832,1784-1828` | `cache-keys.json`, `retention/roots`, `artifact-owners`, `retention/unknown`. Metadata only; **no v3 walk**. |
| Due-first | `1728-1836,1971-1984` | `2181-2291,2431-2448` | Due valid keys in the non-flat domains. B raises cap 64→256, prioritizes views and batches retirement admission rechecks; roots remain unchanged. **No v3 walk**. |
| Orphan views / membership lookup | `1407-1449,1607-1662,1855-1929` | `1830-1872,2035-2089,2309-2388` | Direct valid-key children of `views`, membership from existing `blobs/v2/*/members.sqlite`. The key filter rejects `v3`; the actual v3 directory is elsewhere. **No v3 walk**. |
| V1 blob-family collection | `980-999,2087-2103,3780-3817` | `985-1005,2572-2589,5250-5287` | One direct family under `blobs` per cursor; family must be a 16-hex directory. `v2` and `v3` fail before GC. **No v3 descent**. |
| V2 family collection / generation sweep / family GC | `2120-2211` | `2605-2696` | One valid family under `blobs/v2`; opens existing registry, then registered members' generation directories and v2 planes. **No v3 descent** under the separate-registration invariant. |
| Cache retirement / validation / trash reaper | `1099-1355,2081` | `1104-1778,2562-2566` | Selected cache domain trees and their `.retention-trash/retired-*` trees. B additionally retires mount-point contents into mount-local trash and records pending reaps. These are still selected old cache roots, never `blobs/v3`. **No v3 count/touch/delete**. |
| Binding prune | `1950-1951,3819-3972` | `2405-2411,5289-5442` | Visits `retention/roots/*.json`, checks old caches, ownership, residency and v2 registry/views, then removes only a due binding JSON. **No v3 walk**; v3 presence does not independently preserve an old binding record. |
| Barrier, pass/cursor/report and reclaim metadata | `980-999,2087-2103,4054` | `985-1005,2572-2589,5546` | Coordination files beneath `retention`; not blob payload enumeration. **No v3 descent**. |

### Other view and storage entry points

| Walker / entry point | A source | B source / difference | Verdict |
|---|---|---|---|
| Alias store | `crates/aft/src/alias/mod.rs:313-332,973-982` | Same path construction and key validation; only module registration changes | Opens only `blobs/<artifact_key>/oid-alias.sqlite`, with validated key. No directory enumeration. **No v3 touch**. |
| V1 blob open, corruption quarantine and sidecars | `crates/aft/src/blob_store/mod.rs:470-508,708-721,845-868` | `:475-513,722-735,859-882`; reclaim policy and membership-index optimization change, not path routing | Validated artifact key selects `blobs/<key>/<plane>.sqlite`; quarantine renames that file and its WAL/SHM only. **No v3 touch**. |
| `ViewStore` open / existing / permissions | `crates/aft/src/views/mod.rs:747-784` | `:749-786`; module additions only | V1 opens `views/<scope>`; explicit v2 paths come from registration, never v3. Existing opens can tighten directory permissions, but only at the supplied old view directory. **No v3 touch** under separate registration. |
| Generation reference listing, GC, temp removal and deferred derived checkpoint | `crates/aft/src/views/generation.rs:64-89,95-285,289-559` | File unchanged byte-for-byte | Enumerates one `self.view_dir()`, handles named generation files plus WAL/SHM and pins/read markers; does not discover new storage roots. **No v3 count/touch/delete** unless explicitly passed a v3 directory, which old routing never does. |
| Context cache routing and view binding | `crates/aft/src/context.rs:6880-6960,7835-7850,8551-8558` | `:7622-7721,8593-8603,9311-9318`; runtime coordination changes, no v3 route | Fixed `inspect/<scope>`, `callgraph/<artifact_key>` and existing view/registry paths. Production context has no recursive storage walk; its deletion calls are test-only. **No v3 walk**. |
| Inspect command | `crates/aft/src/commands/inspect.rs` | Same module in B, with diagnostics/rendering changes | No storage directory enumeration, unlink, recursive removal, rename or chmod in this command. Persistence is delegated to the inspect cache, not a walk of `blobs`. |
| Inspect cache open, old-generation deletion, scope inventory and age-policy GC | `crates/aft/src/inspect/cache.rs:565-647,1641-1650,1732-1749,1787-1986` | `:574-673,1677-1687,1768-1785,1823-2029` | Keyed `inspect/<scope>` trees only. Generation cleanup removes matching `<key>.g*.sqlite` and sidecars; scope GC sizes/removes children beneath its supplied inspect root (production eviction delegates to retention). **No v3 walk**. |
| Inspect manager file walk, view projection and non-view callgraph maintenance | `crates/aft/src/inspect/manager.rs:1503-1573,4526-4532,4983-5026,5190-5350,5570-5589` | `:1583-1666,4858-4866,5377-5480,5584-5775,5964-5985`; current-checkout projection now receives its file list | Project walk is rooted at the bound project and scope-filtered, not at storage. View projection uses `views/<scope>` and query pins; views-off maintenance uses `callgraph/<key>`. **No v3 storage walk** in ordinary routing. |
| Legacy partition inventory / sizing | `crates/aft/src/legacy_partitions.rs:202-244,281-358,447-479` | Same roots and key filters; lexical normalization now delegates to `path_identity` (including unmatched parent components) | Lists direct storage children, then only their `callgraph` and `inspect` partition-key entries; recursively sizes selected legacy partitions. **No v3 count/touch/delete**. |
| Private storage permission tightening | `crates/aft/src/private_storage.rs:87-147,150-195` | File unchanged byte-for-byte | Root opening tightens the root and immediate directory children only; keyed opening tightens only its ancestor chain. May chmod `blobs` itself, never descends to `blobs/v3`. **No v3 member/store mode change**. |
| Explicit migration preflight, copy and staging cleanup | `crates/aft/src/migrate_storage.rs:145-157,260-272,356-407,450-533,720-765` | `:145-157,265-277,361-411,455-539,725-770,842-907`; B groups free-space admission by destination volume | **Read-only count exception:** recursive source sizing enters v3 when `--from` contains it. Actual migration items exclude `blobs` and `views`; staging cleanup selects only `staging-*` under migration targets. No v3 migration/deletion in ordinary layouts. |

### Audit decision and explicit migration exception

Neither release's automatic startup, retention, GC, family sweep, eviction or
legacy pruning discovers, counts, changes permissions on or deletes the chosen
v3 family/member trees. The two releases differ in reclaim policy, measurement
caching, due-view scheduling and mounted-cache trash handling, not in their
v1/v2 namespace roots. This is a source audit, not the later actual-binary G7
coexistence test.

One user-invoked operation is an explicit exception: `aft migrate-storage
--from <storage>` recursively sizes the entire supplied source root, including
v3 (A `crates/aft/src/migrate_storage.rs:145-157,752-765`; B
`:145-157,757-770`). Its fixed migration list excludes blobs/views, so it
counts those bytes without moving them. The owner approved continuing with
this shipped-release exception: it cannot delete or corrupt v3; at worst its
space estimate refuses a migration that could have fit. Forged v2 `view_dir`
paths remain a corrupt-metadata caveat only, not an ordinary routing path.

**Required before v3 registration in current code — slice 3a-2:** treat
`blobs/v3` consistently in migration sizing and copying. V3 is rebuildable, so
leave it out of **both** the source-size/free-space accounting and the migration
items rather than copying it. Add a test seeding a v3 family/member tree which
proves it contributes no migration byte requirement, is not copied, and remains
unchanged at the source. Preserve accounting for actual migrated items and the
existing destination-volume checks. This foundation does not edit
`migrate_storage.rs` or `legacy_partitions.rs`; that follow-up requires its own
file-fence grant. The exception must not be mistaken for a claim that current
migration behavior already meets the requirement.

## Floor domains and publication protocol

The permanent format names are `views_v3_registry` (family `members.sqlite`)
and `views_v3_store` (plane payload stores). Each has supported and written
format version **1**. Both belong to `PersistedStore::ALL` and therefore to
`aft --formats` and known-store floor refusals, but neither belongs to the
startup baseline. A fresh prepare writes only the pre-v3 stores; repeated
prepare must not add a v3 floor or create `blobs/v3`.

`reader_floor::publish_v3_with_floor(storage, registry_version, store_version,
write)` is the combined publication boundary. It calls `raise` once with both
requirements, then invokes the closure. The closure must reach `V3FloorRaised`
before creating any v3 file; that durability-observer integration belongs to
the later v3 writer. Every publication uses this boundary, even if the floor
already covers its versions. There is no first-write decision or pending flag.

`raise` holds the cross-process floor lock and preserves every existing store
name, including unknown names, taking a separate maximum for each requirement.
It never rewrites an uninterpretable floor. Changed floors use a temporary
file, file sync, rename and storage-root directory sync. A request naming either
v3 store is strict: a failed directory sync returns `FloorError::Io`, leaving
the closure uncalled. A rename may have already installed the higher floor
when sync fails. Consequently, an already-covered **strict** raise writes no
floor bytes but always re-syncs the storage root, propagating failure again.
A subsequent process can safely retry without relying on the previous
process's memory.

Requests naming no v3 store preserve the legacy behavior: an already-covered
raise performs no directory sync, and a changed floor attempts directory sync
but ignores every resulting error, including EIO, EACCES, EINVAL and ENOTSUP.
The startup baseline and old-store `publish_with_floor` therefore gain neither
a covered-request fsync nor a new directory-sync failure. Strict v3 requests
refuse all directory-sync errors. `durability::sync_dir` itself retains its
existing platform policy; the legacy exception belongs only to reader floors.

On non-Unix, `durability::sync_dir` remains a no-op `Ok`. **Non-Unix directory
durability is unverified**; a cross-compile is not a durability test. All
floor directory-fsync injection tests are `#[cfg(unix)]`.

## Registry and plane stores

Source: `crates/aft/src/blob_store/v3.rs` and `blob_store/v3/initialization.rs`.

| | Registry | Plane store |
|---|---|---|
| Path | `blobs/v3/<family>/members.sqlite` | `blobs/v3/<family>/<plane>.sqlite` (`semantic`, `callgraph`, `trigram`) |
| Persisted store name | `views_v3_registry` | `views_v3_store` |
| Format version / schema version | 1 / 1 | 1 / 1 |
| `PRAGMA application_id` | `0x41465452` (`REGISTRY_APPLICATION_ID`, "AFTR") | `0x41465450` (`PLANE_APPLICATION_ID`, "AFTP") |
| Layout tag (`format_meta.layout_tag`) | 0 | 1 (`LAYOUT_TAG`) |

Both stores carry the same metadata singleton:

```sql
CREATE TABLE format_meta (singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  format_version INTEGER NOT NULL, schema_version INTEGER NOT NULL,
  layout_tag INTEGER NOT NULL, schema_text TEXT NOT NULL);
```

Registry tables:

```sql
CREATE TABLE members (
 scope TEXT NOT NULL PRIMARY KEY, kind TEXT NOT NULL CHECK(kind IN ('view','base')),
 root BLOB, view_dir TEXT NOT NULL, registered_at_ms INTEGER NOT NULL,
 last_bind_ms INTEGER NOT NULL, last_publish_ms INTEGER,
 state TEXT NOT NULL, missing_sweeps INTEGER NOT NULL DEFAULT 0);
CREATE TABLE view_sessions (session_id TEXT NOT NULL PRIMARY KEY, scope TEXT NOT NULL,
 owner_pid INTEGER NOT NULL, owner_start INTEGER NOT NULL, opened_at_ms INTEGER NOT NULL);
CREATE TABLE reservations (id INTEGER PRIMARY KEY AUTOINCREMENT,
 owner_pid INTEGER NOT NULL, owner_start INTEGER NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('reserved','committed','released')),
 reserved_bytes INTEGER NOT NULL CHECK(reserved_bytes>=0),
 charge_bytes INTEGER NOT NULL CHECK(charge_bytes>=0), completed_seq INTEGER);
CREATE TABLE accounting (singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 measured_bytes INTEGER NOT NULL, watermark INTEGER NOT NULL);
```

`members.view_dir` is always `blobs/v3/<family>/views/<scope>`, never under
`<storage>/views/`. A member's `root` is the checkout path's bytes, or NULL.

Plane store tables:

```sql
CREATE TABLE store_meta (singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 plane TEXT NOT NULL, gc_epoch INTEGER NOT NULL);
CREATE TABLE blob_payloads (full_key BLOB NOT NULL PRIMARY KEY CHECK(length(full_key)=32),
 payload BLOB NOT NULL, payload_digest BLOB NOT NULL CHECK(length(payload_digest)=32),
 payload_schema INTEGER NOT NULL, ref_epoch INTEGER NOT NULL, created_at_ms INTEGER NOT NULL) WITHOUT ROWID;
CREATE INDEX blob_membership ON blob_payloads(full_key, ref_epoch);
CREATE TABLE blob_quarantine (full_key BLOB NOT NULL PRIMARY KEY CHECK(length(full_key)=32)) WITHOUT ROWID;
CREATE TRIGGER immutable_payload BEFORE UPDATE OF payload, payload_digest ON blob_payloads
 BEGIN SELECT RAISE(ABORT, 'immutable_payload'); END;
```

The payload column is **`payload`** and the digest column is
**`payload_digest`** (BLAKE3 of `payload`, checked again on every `get`).
`payload_schema` is preserved from the copy source.

### Schema identity

A new store is bootstrapped in one transaction: `auto_vacuum = INCREMENTAL`
(2) is selected first, then the schema, `application_id`, seed rows and the
final `format_meta` row. Every open then checks, and refuses through
`persisted_format` with `RefusalSource::Incompatible`:

- `format_version` readable (0 when not), and not newer than supported (a
  newer version keeps the `Artifact` refusal);
- `application_id` equals the kind's value (`application_id`);
- `layout_tag` equals the kind's value (`layout_tag:<value>`);
- `schema_version`, the stored `schema_text`, and the live `sqlite_schema`
  rows (type, name, tbl_name, sql, excluding `sqlite_%`) equal this build's
  compiled-in text (`schema_drift`), and the seed rows exist.

No table is created lazily; a later table bumps the schema version through
an exact-predecessor forward migration.

### PRAGMAs per open path

Each value is set and read back; a mismatch refuses as `pragma:<name>`.

| Open path | PRAGMAs |
|---|---|
| Bootstrap (creation, before the schema transaction) | `auto_vacuum=2`; `application_id` inside the transaction |
| Writer open (publication, collector, recovery) | `busy_timeout=5000`, `journal_mode=WAL` (must answer `wal`), `synchronous=2` (FULL), `foreign_keys=1`, `temp_store=2`, `wal_autocheckpoint=0`; `auto_vacuum` must read 2 |
| Ledger read (read-only) | `busy_timeout=5000`, `query_only=1`; `auto_vacuum` must read 2 |

## Ledger, lock and open modes

- Reservation rows live in each family's v3 `members.sqlite`. A row moves
  `reserved` → `committed` on successful publication, or `reserved` →
  `released` on failure, each in one transaction. It records the owner's pid
  and process start time.
- The storage-root reservation lock is `blobs/v3/reservations.lock`, an
  `fs_lock` lease (dead holder or a heartbeat older than 15 s is taken over).
  Lock order is always this lock first, then the family registry barrier (an
  IMMEDIATE transaction). The lock is not reentrant: a holder passes its
  `ReservationLock` to nested writers (`publish_locked`).
- **Writer open** (`RegistryWriter::open_existing`, and publication's private
  creating open): under the lock, then the barrier; takes a covering
  measurement and releases rows whose owner is dead.
- **Ledger read** (`LedgerReader::open_existing`): opens an existing registry
  read-only, takes neither the lock nor the barrier, takes no measurement and
  writes nothing. Used for the floor sum over other families and for reports.
- Only publication creates a registry. Recovery and the retention collector
  open existing registries only.

## Accounting

- The namespace inventory is every file under `blobs/v3/<family>/`: the
  registry, plane stores, their `-wal`/`-shm`, and the member view
  directories. Sizes are allocated sizes (`st_blocks * 512`); directories are
  walked, not charged.
- A covering measurement checkpoints the registry (`wal_checkpoint(TRUNCATE)`),
  records the inventory size in `accounting.measured_bytes` and the watermark
  (the last `completed_seq`), and repeats until recording it does not change
  the inventory. It runs on every writer open, after every collector pass and
  collector deletion, and on every release.
- Committed usage = last measurement + charges of reservations committed after
  its watermark + bytes of outstanding `reserved` rows.
- On commit, the measured growth of the inventory during the attempt (never
  less than the payload bytes written) replaces the reserved bytes, in the
  transaction that publishes the closure.
- On failure (owner alive or dead) one transaction takes a covering
  measurement, which includes plane rows already committed, and releases the
  row. Orphan plane rows stay charged until they are reclaimed.
- Deletions lower committed usage only through the next covering measurement,
  by the allocated bytes actually freed. Plane row deletions count once their
  pages are reclaimed or the WAL is checkpointed; v3 plane stores use
  `auto_vacuum = INCREMENTAL` so `reclaim_space_bounded` can shrink them.

## Budgets and floor

- Namespace budget per family: 4 GiB (`NAMESPACE_BUDGET`, the family hard
  limit) until it is measured. A reservation is refused as
  `v3_namespace_budget` when committed usage plus the request exceeds it.
- Free-space floor: refused as `device_free_space_floor` when free space,
  minus the request and every outstanding `reserved` row in all
  `blobs/v3/*/members.sqlite`, would fall below max(4 GiB, 5% of capacity).
  Free space and capacity come from `statvfs` (`f_bavail*f_frsize`,
  `f_blocks*f_frsize`). Other families are summed through ledger reads; a
  registry that cannot be opened or read refuses as
  `reservation_ledger_unreadable`.
- The check runs before anything is created, and again under the lock after
  the floor raise. A refusal at the first check leaves `blobs/v3/` and
  `reader-floor.json` untouched; at the re-check it may leave `blobs/v3/`, the
  lock file and an empty registry, but no reservation row or plane store.
- Assumptions: different storage roots on one device are not coordinated. On
  non-unix the probe reports unsupported, so every reservation is refused as
  `device_free_space_floor` (unverified on a native non-unix run).
- Wiring these budgets into `DiskBudget` (`views/eviction.rs`) is slice 4's job.

## Retention collector

`storage_retention::collect_one_v3_family` runs in every retention pass, after
the v1 and v2 blob lanes. It visits one family directory under `blobs/v3/`
per pass through its own cursor (`retention/cursors/families-v3.json`), and
starts the next lap in the same pass so a lone family is visited every pass.

- **Existing registries only.** A root without `blobs/v3/` lists nothing and
  writes no cursor. A family directory without `members.sqlite` is skipped
  before the reservation lock is taken, because taking it creates
  `blobs/v3/reservations.lock`. Then: lock, `RegistryWriter::open_existing`.
- **v2's missing-root rule** (as `gc/family.rs` `deregister_if_missing`). A
  `kind = view` member counts a missed pass when its root is certainly missing,
  past the recency guard (`ROOT_GRACE_MS`, or the linked-worktree margin) and
  the mount guard (`missing_root_eligibility`), and it has no protection: no
  `view_sessions` row with a live owner, no live pin in `pins/`, and no
  protected read marker in `readers/`. Session rows, pins and markers whose
  owner is dead are removed on the way and never block collection. An
  unreadable pin or a `pins/`/`readers/` listing over its bound (4096) is
  uncertainty: the member stays and its count is not advanced.
- `missing_sweeps` must reach `MISSING_ROOT_SWEEPS` (2); a reappeared root or
  any live protection resets it to 0. Members with a NULL root and
  `kind = base` members are never eligible. A row whose `view_dir` is not
  `blobs/v3/<family>/views/<scope>` is kept and reported.
- **Deletion.** Under the held lock, one barrier transaction rereads the row
  and rechecks kind, root, recency, mount, count and every protection, then
  commits `state = 'evicting'`. The collector then removes the view directory
  (refusing while a SQLite connection in the process is open under it),
  deletes the member row, and takes a covering measurement. A crash leaves an
  `evicting` member, which the next pass finishes.
- **Bounds** match `collect_one_v2_family`: a `PASS_BUDGET` (5 s) deadline and
  at most `HISTORY_ENTRIES` (8192) members per pass. Plane rows are left for
  slice 4's v3 plane sweep.

Tests (`storage_retention::v3_collector_tests`):
`v3_collector_applies_the_missing_root_rule_to_view_members_only`,
`v3_eviction_interrupted_after_its_evicting_commit_is_finished_next_pass`,
`collecting_a_view_member_lowers_committed_usage_by_its_allocated_bytes`,
`v3_collector_never_creates_a_registry` and
`no_production_path_writes_v3_over_a_v0_60_0_floor`.

## Replay prototype

The replayed publication is the copy closure: v3 plane-row inserts or
touches, `ClosureFlushed`, then the registry transaction from `reserved` to
`committed`. `blob_store/v3/replay.rs` wraps SQLite's platform VFS and records
what a power cut would lose (unsynced writes and truncations, files created
since their directory's last sync, unsynced deletions); each case cuts power
before every mutating call in turn, restores the last synced state and
reopens the stores.

Invariant after every cut: the registry shows no row for the attempt, or
`reserved`/`released` claiming no rows, or `committed` with every copied row
passing `get` with a matching digest.

Result: passing. Tests in `blob_store::v3::replay_tests` (unix only):
`replayed_copying_closure_is_whole_or_absent_after_any_power_cut`,
`replayed_reuse_only_closure_is_whole_or_absent_after_any_power_cut`,
`replayed_retry_after_a_failed_floor_directory_sync_is_whole_or_absent`, and
the red control `red_control_without_the_closure_flush_a_power_cut_tears_the_copy`
(without `ClosureFlushed`, a replay shows `committed` with a missing row).

## Release isolation

`crates/aft/tests/integration/v3_isolation.rs` runs the binary built from tag
v0.60.0 (commit `988f4f3e52b9`) over a storage root holding a v3 tree
(`members.sqlite` with `-wal`/`-shm`, a plane store, and
`views/<scope>/` with `pins/`, `readers/` and a pointer database) next to a
v1 and a v2 store holding collectable rows. Both the freshly started and the
already-running case wait for a retention pass and an enforcement pass after
the seed, then run `aft cache prune-legacy` dry and with `--yes`. The second
configure of each case flips `format_on_edit`: v0.60.0 queues the configure
maintenance that schedules retention only when the effective configuration
changed, and answers an equivalent configure without it. The digest
of bytes and `st_mode` over `blobs/v3/` must be unchanged, and no v3 member
may appear in a v2 registry. The tests are `#[ignore]`d (they take seven and
thirteen minutes) and need `AFT_V0_60_0_BINARY`.

## Deferred to slice 4

The manifest base header with PublishedFrontier, tombstones and
readiness_overrides, donor seal records, the symbol plane, the v3 import
ledger, persisted stat evidence, staging names and token rows. The rowid
layout and its layout-tag section belong to 3b.
