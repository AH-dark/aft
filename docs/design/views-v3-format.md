# Views v3 format: namespace and reader-floor foundation

This document records the namespace audit and floor protocol before v3 data
writers are introduced. It does not specify a registry or plane schema yet:
those schemas, application IDs, PRAGMA checks, accounting and replay belong to
the subsequent v3 storage implementation. No production caller publishes v3.

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

## Deferred implementation

This foundation creates no registry or plane store and changes no production
views routing. The remaining 3a storage work must add the exact schemas,
application IDs, checked PRAGMAs, reservation ledger, storage-root lock,
allocated-byte accounting, free-space/budget admission, copy protection,
collector, durability observer steps and power-loss replay result here before
claiming that the complete v3 storage contract is implemented. The v3 view
root remains `blobs/v3/<family>/views/<scope>`.

The subsequent slice-4 work remains deferred: the manifest base header with
PublishedFrontier, tombstones and readiness overrides, donor seals, symbol
plane, v3 import ledger, persisted stat evidence, staging names and token rows.
The rowid layout and layout-tag section belong to 3b. No performance baseline
is needed for the namespace/floor foundation: it makes no performance claim.
