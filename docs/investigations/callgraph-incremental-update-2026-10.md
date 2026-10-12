# Incremental call-graph update cost — October 2026

Design-stage report: measurements, dependency analysis and a proposed design.
The commit that adds this report changes nothing in the update path except
opt-in stage timing (`AFT_VIEW_PROFILE`) and an ignored measurement harness.

## Summary

- The update a view takes when files change is
  `views::materialization::materialize_reading` (`apply_manifest_diff`), which
  calls `callgraph_store::join` for ordinary references and
  `views::materialization::dispatch::emit` for method calls. The legacy
  `CallGraphStore::refresh_files` path is not on the views publication path.
- On this repository (4,911 manifest entries, 3,464 parsed files, 272,977
  method-call sites) a **one-file edit costs 181 s and a cold build 301 s**
  (Linux, 4 vCPU). 99% of the one-file cost does not depend on the edit:
  - **125 s: method-call resolution's type-name pass**
    (`join::dispatch_type_targets`). It rebinds every file, rebuilds the whole
    project symbol index and resolves 66,892 type names. 121 s of it is the
    name resolution itself.
  - **35 s: surface replay** in the selected join (the join that re-resolves
    only callers whose recorded resolver answers may have changed). Every
    update marks the crate-wide Rust module index as changed, so ~860
    unchanged Rust callers that consulted it replay ~238,000 recorded
    resolver queries.
  - **10 s: the per-site method-call write loop**, one SQL round trip per site
    for all 272,977 sites (resolution itself is 1.3 s of it).
  - **8.6 s: decoding every blob** again for the method-call pass.
- **Root cause of the two largest buckets is one defect**: the in-memory
  resolver index (`ProjectIndex`) answers `module_parent` and
  `inline_scoped_target` by scanning every file — `inline_scoped_target` also
  clones and sorts the full path list — on every call. The SQLite-backed
  `DiskProjectIndex` already memoizes both. An uncommitted prototype that
  memoizes the same two scans for the in-memory index cut:
  - one-file edit **181 s → 22.8 s**, 10 files 184.5 → 22.2 s, 100 files
    204.6 → 24.5 s;
  - branch switch (50 commits, 133 paths) **310.6 → 31.6 s** and
    305.1 → 31.3 s back;
  - cold build **300.2 → 38.8 s**.

  Every derived table of the memoized cold build equals the unmodified cold
  build row for row, and every incremental result equals its own cold build
  the same way (`AFT_UPDATE_BENCH_PARITY=1`; the check used in
  `views-incremental-parity-2026-10.md`).
- Recommended design, in order, none of which changes the stored format:
  1. Memoize the crate-wide scans of the in-memory resolver index (the
     prototype, made permanent with tests).
  2. Make the method-call pass compute every site in memory but write only
     the difference from the persisted rows, load raw payloads lazily, and
     decode blobs in parallel.

  Estimated one-file edit after both: **~10 s**. A third step — recomputing
  only the method-call sites and type names an edit can affect — would save a
  further ~5–6 s. It needs persisted dependency data, which is a stored-format
  change, so it is not recommended without approval.

## What was measured

### Method

- Harness: `views::materialization::update_bench::bench_repository_incremental_update`
  (ignored test, added in this delivery). It clones this repository with
  `git clone --shared` (read-only on the source) and publishes two revisions
  into scratch view storage with `publish_checkout`, which yields the real
  manifests and blobs. It derives 1-, 10- and 100-file edits of the newer
  revision; each edited file gains a comment line at its middle and one new
  function at its end, so later lines and AST ordinals shift and a definition
  is added. Edited files are Rust and TypeScript sources spread evenly over
  the manifest; the list is printed. For each case it clones a cold base
  database and times `materialize(.., Some(base))`, the same call
  publication makes. `AFT_UPDATE_BENCH_PARITY=1` also cold-builds every
  target and requires every derived table to be equal.
- Revisions: `fe06abac1b1e` (HEAD~50) and `99ba8bbb3dac` (the measured base),
  134 Git paths and 133 manifest entries apart. The only resolver
  configuration change is `crates/aft/Cargo.toml` (dependency versions).
- Runs on a shared Linux build server with 4 vCPUs requested, Rust/Cargo
  1.99.0. `AFT_VIEW_PROFILE=1` prints every stage.
- Build: `--profile stage` (release opt-level, no LTO, 16 codegen units) with
  `CARGO_PROFILE_STAGE_DEBUG_ASSERTIONS=true`. The release library test
  target does not compile without debug assertions: tests call
  `hash_file_if_small_count_for_debug`, which is gated by
  `#[cfg(debug_assertions)]` at `crates/aft/src/cache_freshness.rs:415`. The
  production daemon uses fat LTO without debug assertions, so its absolute
  times can be somewhat lower; the stage shares are what this report relies
  on.
- Repeatability: a third run of the committed harness (edit-1 only) measured
  edit-1 at 181.1 s (surface replay 33.2 s, type-name resolution 119.3 s,
  per-site loop 9.7 s) and cold HEAD at 301.0 s, against 181.0 s and 301.0 s
  in the first run. Each case ran once; the per-stage shares are stable
  across all cases.
- Stage names below are the materializer's existing phases plus the new
  sub-stages: `dispatch.*` inside the method-call pass (previously all
  charged to `commit`), and `join.surface_replay` inside
  `index_and_surface_replay`.

Command:

```sh
AFT_VIEW_PROFILE=1 CARGO_PROFILE_STAGE_DEBUG_ASSERTIONS=true \
  cargo test --profile stage -p agent-file-tools --lib \
  views::materialization::update_bench::bench_repository_incremental_update \
  -- --ignored --exact --nocapture --test-threads=1
```

Optional: `AFT_UPDATE_BENCH_OLD`/`_NEW` (revisions), `AFT_UPDATE_BENCH_EDITS`
(sizes), `AFT_UPDATE_BENCH_ONLY` (case labels), `AFT_UPDATE_BENCH_PARITY=1`.

Case labels in the tables: `edit-N` is the N-file synthetic edit of HEAD;
`switch → X` is the branch switch whose target revision is X.

### Baseline: unmodified code (seconds)

| case | changed entries | wall | CPU | full resolution | resolved files / refs |
|---|---:|---:|---:|---|---:|
| cold, HEAD~50 | — | 294.7 | 293.7 | — | 3,454 / 576,843 |
| cold, HEAD | — | 301.0 | 299.7 | — | 3,464 / 580,719 |
| edit-1 | 1 | **181.0** | 180.6 | no | 1 / 120 |
| edit-10 | 10 | **184.5** | 182.2 | no | 13 / 3,466 |
| edit-100 | 100 | **204.6** | 204.3 | no | 201 / 107,876 |
| switch HEAD~50 → HEAD | 133 | **310.6** | 309.9 | yes | 3,464 / 580,719 |
| switch HEAD → HEAD~50 | 133 | **305.1** | 304.3 | yes | 3,454 / 576,843 |

Both branch switches are slower than a cold build. The product's cutoff
(`views::assembly::diff_exceeds_incremental_cutoff`) still chooses the
incremental path for them, because 133 of 4,911 entries is under its 10%
threshold.

Stage breakdown (seconds; sub-stages indented):

| stage | edit-1 | edit-10 | edit-100 | switch → HEAD | switch → HEAD~50 | cold HEAD |
|---|---:|---:|---:|---:|---:|---:|
| load bindings / select | 0.10 | 0.09 | 0.11 | 0.12 | 0.11 | 0.00 |
| delete owned rows | 0.00 | 0.01 | 0.16 | 0.43 | 0.43 | 0.00 |
| owned decode + insert | 0.00 | 0.03 | 0.39 | 1.25 | 1.13 | 7.72 |
| selected join | 35.33 | 36.74 | 65.41 | 167.56 | 161.81 | 151.02 |
| ↳ decode / bind / index entries | 0.14 | 0.14 | 0.24 | 7.78 | 7.65 | 1.35 |
| ↳ **surface replay** | **35.05** | **35.84** | **27.68** | 0.00 | 0.00 | 0.00 |
| ↳ resolve + record | 0.03 | 0.63 | 36.44 | **159.28** | **153.76** | **149.31** |
| binding writes | 0.05 | 0.04 | 0.10 | 0.25 | 0.22 | 0.47 |
| ref/edge emission | 0.00 | 0.41 | 2.29 | 3.19 | 2.47 | 3.95 |
| method-call pass + commit | 144.84 | 146.47 | 135.25 | 136.66 | 137.56 | 136.57 |
| ↳ decode every blob | 8.61 | 7.36 | 4.74 | 0.00 | 0.00 | 0.00 |
| ↳ **type names: load payloads** | 2.04 | 2.07 | 2.03 | 2.30 | 1.93 | 2.39 |
| ↳ **type names: bind every file** | 1.67 | 1.67 | 1.55 | 1.68 | 1.64 | 1.59 |
| ↳ **type names: resolve 66,892** | **121.45** | **122.60** | **115.78** | **120.35** | **120.16** | **115.53** |
| ↳ wholesale delete | 0.20 | 1.42 | 0.18 | 0.05 | 0.06 | 0.01 |
| ↳ **per-site loop** | **9.88** | **10.24** | **10.11** | **10.82** | **12.21** | **15.07** |
| ↳↳ of which resolution | 1.32 | 1.30 | 1.34 | 1.29 | 1.50 | 1.49 |

Notes on the baseline:

- **Surface replay.** It covered 863 / 882 / 1,413 selected callers, about
  238,000 recorded queries each time. The selection is that large for a
  one-file edit because `dependent_closure` always seeds
  `VIEW_RUST_MODULE_DOMAIN` when anything changed
  (`views/materialization.rs`, `dependent_closure`). Every Rust caller that
  recorded a `Parent` or `Inline` query is therefore replayed.
- **Full resolution on the branch switches.** The diff touches
  `crates/aft/Cargo.toml`, and 623 cached bindings have unattributed
  configuration reads (`unattributed_callers=623`). The materializer
  therefore falls back to resolving every caller (the
  `reason=unattributed_reads` path).
- **Method-call pass.** `import_targets` is empty on this corpus (no `import`
  refs carry a target file). The method-call signature changed in every
  case: any line shift changes `SiteHint.line`, so the wholesale-delete
  branch runs on every real edit.
- **Live daemon cross-check.** The pre-existing daemon log
  (`~/.local/share/cortexkit/aft/logs/aft-43044.log`, 2026-10-12) shows the
  same split, inflated by machine load. In the publication logged with
  `derived_ms=181551` (a worktree seeded from a sibling checkout's graph):
  - `materialize_commit_ms=131819` is the method-call pass;
  - `materialize_join_index_surface_replay_ms=43821` is the surface replay;
  - the owned work and resolution were under 1 s.

  The same log's `derived_ms=163689` entry was a cold build
  (`cold_build_reason=large_diff`), so it is not an incremental update.

### Why type-name resolution and replay are slow

The in-memory `ProjectIndex` (`callgraph_store/mod.rs`, `impl ResolverIndex
for ProjectIndex`) implements two crate-wide lookups with a full scan per
call:

- `module_parent(target)` walks every file's `declared_module_targets` and
  sorts the matches.
- `inline_scoped_target(caller, segments, name)` clones every indexed path,
  **sorts it**, and recomputes `rust_src_prefix` and
  `rust_module_segments_for_rel` for every file. This is O(N log N) string
  work with N ≈ 4,900 per call.

Both are called hundreds of thousands of times. In the prototype run the
resolve pass made 295,000–328,000 `module_parent` calls and
32,000–66,000 `inline_scoped_target` calls; surface replay made 50,000
`inline_scoped_target` calls. The SQLite-backed `DiskProjectIndex`, used by
legacy cold builds, already memoizes both (`module_parents()`,
`rust_files()`).

This one defect, rescanning on every call instead of memoizing, explains 121 s of type-name resolution, 35 s of surface
replay, and most of the 150–160 s of full resolution and cold resolution.

### Prototype: memoized crate-wide scans (uncommitted)

The prototype kept, per `ProjectIndex` instance and only when the index is
fully in memory (`lazy` is `None`):

- a path-sorted list of Rust files with their precomputed source prefix and
  module segments;
- a `target → min (declaring file, module)` map.

Lookups returned the same first match in the same order as the scans. It was
measured in the same harness, reverted, and is **not committed**.

Seconds; "before" is the unmodified code in the same harness.

| case | wall before | wall with memo | CPU with memo | parity |
|---|---:|---:|---:|---|
| cold HEAD (same run, unmodified vs memo) | 300.2 | **38.8** | 38.0 | memoized cold == unmodified cold, every table |
| edit-1 | 181.0 | **22.8** | 22.6 | incremental == cold |
| edit-10 | 184.5 | **22.2** | 21.9 | incremental == cold |
| edit-100 | 204.6 | **24.5** | 24.1 | incremental == cold |
| switch → HEAD | 310.6 | **31.6** | 30.9 | incremental == cold |
| switch → HEAD~50 | 305.1 | **31.3** | 30.6 | incremental == cold |

Remaining stages with the memo (seconds):

| stage | edit-1 | edit-10 | edit-100 | switch → HEAD | cold HEAD |
|---|---:|---:|---:|---:|---:|
| selected join | 0.41 | 0.47 | 1.86 | 9.95 | 4.72 |
| ↳ surface replay | 0.23 | 0.23 | 0.21 | 0 | 0 |
| ↳ decode/bind (full resolution) | 0.10 | 0.12 | 0.21 | 6.51 | 1.44 |
| ↳ resolve + record | 0.00 | 0.02 | 0.71 | 3.10 | 2.98 |
| ref/edge emission | 0.00 | 0.34 | 1.92 | 2.17 | 3.41 |
| method-call pass + commit | 21.64 | 20.61 | 19.33 | 16.55 | 20.18 |
| ↳ decode every blob | 6.80 | 6.17 | 4.16 | 0.00 | 0.00 |
| ↳ type names: payloads / bind / resolve | 1.77 / 1.43 / 1.73 | 1.65 / 1.41 / 1.72 | 1.66 / 1.42 / 1.72 | 1.79 / 1.52 / 1.74 | 2.30 / 1.94 / 1.75 |
| ↳ per-site loop total (of which resolution) | 8.96 (1.25) | 8.67 (1.15) | 9.30 (1.20) | 10.29 (1.19) | 12.60 (1.16) |

After the memo, almost all of a small edit's cost is the method-call pass,
and two thirds of that is SQL in the per-site loop plus decoding blobs.

## Dependency analysis

### Ordinary references and the project symbol index

**Inputs.** Each file's resolver surface (`DbFileIndex`):
- language, exports, default export, export aliases;
- nodes by scoped and bare name, and node kinds;
- `module_targets`, `declared_module_targets` and re-exports.

Crate- and workspace-wide answers come from:
- `module_parent` and `inline_scoped_target` (Rust module tree);
- `crate_src_prefix` (workspace `Cargo.toml` membership);
- `rust_crate_root_file`.

**Existing invalidation.** It is per caller and persisted:
- `view_bindings` holds dependencies, `surface_queries` with their answer
  hashes, and consulted configuration facts;
- `view_file_surfaces` holds the compact surfaces.

An update rebuilds surfaces only for changed files. It replays the recorded
queries of every caller in the dependency closure, and re-resolves only the
callers whose answers changed. That design is already incremental. Its cost
problem was the crate-wide domain: `Parent` and `Inline` queries depend on
`VIEW_RUST_MODULE_DOMAIN`, which is seeded on every non-empty diff. Replaying
~238,000 queries was 35 s only because each `Inline` and `Parent` answer
scanned the whole index. With the memo it is 0.2 s, so narrowing the domain
seed is not worth its parity risk.

### Method-call resolution (`dispatch::emit`, `callgraph_store::dispatch::Resolver`)

Resolving a site in file F (receiver R, member m) reads:

1. **F's own facts:** language, `dispatch.types` / `fields` / `returns`,
   `imports`, and the site itself.
2. **`type_targets[(F, name)]`** (Rust only). Every type-ish name in F is
   resolved through the Rust resolver over the whole project index:
   - F's `use` block (`FileCallData.import_block`);
   - module tree, re-exports and crate prefixes;
   - target nodes.
3. **`import_targets[(F, module)]`**, read from `refs` import rows (empty on
   this corpus).
4. **Type lookup (`project_type`):**
   - F's own types, then `type_targets`;
   - for Go, Java and C#, **every file in F's directory** with the same
     language (package membership);
   - for relative imports, candidate files `base`, `base.{ts,tsx,js,py,rs,go}`.
   - `@field:` and `@return:` chains recurse through field and return hints of
     the files they reach.
5. **Method sets:**
   - `methods_named(m)` is a global index of every method with that name, each
     filtered by resolving its owner's type in its own file.
   - `nearest()` walks base types.
   - Rust prefers inherent methods; with more than one trait method it falls
     back to `unknown()`.
6. **Interfaces and open types:** every same-language method named m whose
   trait or owner is a subtype of the receiver type (`subtype()` walks
   bases). Go also matches structurally by name and shape against the
   interface's required methods.
7. **Unknown receivers (`unknown()`):**
   - every same-language method named m. These candidates are kept alive in
     dead-code analysis ("protected") because the call could reach any of
     them, and are persisted in `view_unknown_live`;
   - name-only targets where the method has a trait, the owner is an
     interface, or the owner's bases are interfaces;
   - in Go, any Go interface requiring that name and shape.
8. **Rows written:** these depend on the `nodes` table. Caller node by
   `(file, scoped_name)`, target node per target. Node IDs encode path, scoped
   name and AST ordinal.

What each kind of edit can change:

| edit | inputs it changes | sites whose result can change |
|---|---|---|
| new / removed definition | `methods_named(name)`, the protected set for that name, type lookup for that type name, node ordinals of later symbols in the file | every site with that member name (same language); every site whose receiver resolves to that type name (in-file, via Rust `type_targets`, same directory for Go/Java/C#, TS relative importers); incoming dispatch edges' `target_node` for the file |
| a type's method set | `methods(file, owner, name)`, inherent-vs-trait counts | sites whose receiver type, or a subtype's base chain, is that type; interface receivers whose implementers include it |
| imports and re-exports | F's import list; Rust `use` and `pub use` chains | F's own sites, and (Rust) `type_targets` of **any** file whose names resolve through the changed module or re-export. This is the one edge that cannot be bounded by name, because aliases and glob re-exports rename |
| trait / interface implementation | owner `bases`, `trait_name` | every site typed as that trait/interface with that member; every unknown-receiver site with that member (name-only targets) |
| Go package membership (add/remove/move a file) | same-directory type scan, Go structural interface set | sites in every file of both directories; Go interface sites with matching names |
| rename | removal + addition: path identity, Rust module tree, relative-import candidates, all node IDs of the path | union of the rows above for both paths, plus Rust module-tree consumers |
| body-only edit (lines shift) | `SiteHint.line`, symbol and site ordinals of the file | the file's own sites and rows; incoming edges' `target_node` when ordinals shift |

Today none of this is used: one signature over every file's facts flips on
any line shift. Every update then deletes all dispatch rows and rewrites
273,000 sites, resolving 66,892 type names on the way.

## Proposed design

### Step 1: memoize the in-memory index's crate-wide scans (exact)

- Add, to `ProjectIndex`, lazily built per-instance memos:
  - the path-sorted Rust file list with precomputed source prefix and module
    segments;
  - the module-parent map (target → minimum `(file, module)`).
- Use them only when `lazy` is `None`. The incremental `CallGraphStore`
  refresh path keeps its current behaviour.
- Lookup order must match the scans exactly:
  - for `inline_scoped_target`, the caller first, then path order, Rust only;
  - for `module_parent`, the minimum pair.
- The memo is sound because a `ProjectIndex`'s `files` never change after
  `from_parts`. Only `unbound_non_utf8_paths` is assigned afterwards. A
  clone copies a memo built from identical files, and the join builds a new
  instance when it adds callers.
- The memo does not change which surface queries are recorded or which
  dependencies are written: recording happens in the `ViewSurfaceIndex`
  wrapper, not in the inner index.

Measured effect: the prototype table above. It also benefits cold builds and
full-resolution fallbacks (`resolve_and_record` 149–159 s → 3 s).

### Step 2: method-call pass writes only what changed (exact)

The pass still computes every site's resolution in memory, which takes about
1.2 s, and compares it with the persisted state.

- **Persisted state read in bulk:**
  - `view_dispatch_sites` (273,000 rows);
  - `view_unknown_live`;
  - dispatch-provenance edges (73,500);
  - the status of every site ref.
- **Always rewritten:** site refs that ordinary emission rewrote in this
  transaction (owned rows of changed files and relinked callers). Their
  status is re-applied, and the ordinary edge for that `ref_id` is deleted,
  exactly as the loop does now. The materializer already knows these ref IDs
  (`pending_refs`).
- **Dispatch edges:** edges whose caller file or target file changed are
  re-derived with the existing per-row `node()` SQL lookup. Duplicate
  `scoped_name` rows make an in-memory node map unsafe. Other edges compare
  by `edge_id`; within one site's target set, the last target in `BTreeSet`
  order wins, as `INSERT OR REPLACE` does now. A site whose caller node is
  missing emits no edges.
- **Site and unknown-live rows:** inserted, updated and deleted by key.
- **Other parts of the pass:**
  - `view_dispatch_signature` is still computed and written: a cold build
    writes it to `meta`, and the parity check compares the `meta` table too.
  - Raw payloads for type-name resolution are loaded lazily. Only
    configuration blobs are read through `ManifestFacts`; today every payload
    is loaded first (1.7–2.4 s).
  - Blobs are read sequentially but decoded in parallel (4.2–6.8 s today, on
    one core).

Estimated effect in seconds, starting from the per-stage timings measured
with the step 1 memo applied:

| case | with memo | per-site SQL removed | lazy payloads | parallel decode (4 cores) | estimate |
|---|---:|---:|---:|---:|---:|
| edit-1 | 22.8 | −6.7 | −1.8 | −4.5 | **~10** |
| edit-10 | 22.2 | −6.5 | −1.7 | −4.1 | **~10** |
| edit-100 | 24.5 | −7.1 | −1.7 | −2.8 | **~13** |
| switch → HEAD | 31.6 | −7.6 | −1.8 | 0 | **~22** |
| cold | 36.5–38.8 | 0 (all rows new) | −1.9 | 0 | **~35** |

The per-site SQL saving assumes about 1–1.5 s to read the persisted state;
this is an estimate, not a measurement. The switch stays at ~22 s because it
takes the full-resolution fallback (see below).

### Step 3 (not recommended without approval): bounded recompute

After steps 1–2, the edit-independent remainder for a small edit is about
5–6 s:
- decode ~2 s;
- type names: bind 1.4 s and resolve 1.7 s;
- site resolution 1.2 s.

Removing it requires knowing which sites and type names an edit can affect.

- **Name-keyed affected set.** By the table above, most rows can be bounded
  by keys: member names, type names, directories, crates. A conservative rule
  set:
  - recompute the sites of changed files;
  - recompute sites whose member is a changed or removed method name;
  - recompute sites whose receiver text mentions a changed type, field or
    return name;
  - recompute all sites in a changed file's directory for Go, Java and C#;
  - recompute all Go interface sites with a changed name or shape.
- **Rust `type_targets` cannot be bounded by name.** `use` aliases and glob
  re-exports rename, so any Rust edit that changes module declarations or
  re-exports would fall back to recomputing every type name. That fallback
  costs only ~3 s after step 1.
- **Precise bounding needs persisted data.** Per-Rust-file type-target
  answers with their recorded surface queries (the same `ViewSurfaceQuery`
  mechanism ordinary references use), plus either a compact per-file
  dispatch-facts table or a cache that avoids decoding every blob. Both are
  **stored-format changes** (`view_bindings` payload contents or new
  derived tables, with a `MATERIALIZATION_VERSION` bump).
- **Fallback when the affected set cannot be bounded.** Recompute everything
  in memory as in step 2. This is cheap after step 1, so the fallback is
  safe.
- **Estimated saving:** one-file edit ~10 s → ~4–5 s.

### Separate lever: the branch-switch full resolution

Any `Cargo.toml` or `package.json` change takes the unattributed-read
fallback, which here is 623 bindings with unattributed configuration reads.
That fallback re-resolves every caller. With step 1 it costs ~10 s (decode
6.5 s + resolve 3.1 s) instead of 167 s. Attributing those configuration
reads would turn a dependency-version bump into a bounded update. That is
resolver work outside the update internals; it is listed here because it
decides the branch-switch row.

### Interaction with the incremental cutoff

The incremental-versus-cold cutoff percentages in `views/assembly.rs` were
calibrated against the old costs. With cold at ~35–39 s they may deserve
recalibration. That file is being changed by separate work that makes
worktree call graphs build on first use, so the recalibration is deferred
until that lands.

## Risks to exact cold-build parity

**Step 1:**
- **Order of first match.** The memo must return the scans' first match in
  the same order: caller first, then path-sorted, Rust only (language
  checked through the file's index); for `module_parent`, the minimum
  `(file, module)` pair.
- **Mutation.** A future change that mutates `ProjectIndex.files` after
  construction would make the memo stale. Guard it with a constructor-only
  `files` and a comment.
- **Legacy refresh path.** The lazy (SQLite-backed incremental refresh) index
  must keep the scan, or include stored files exactly as `scan_paths` does.
- **Evidence so far.** On the 4,911-entry corpus every table of the memoized
  cold build equals the unmodified cold build, and all five incremental cases
  equal their cold builds.
- **Mutation-tested fixtures for the build step:**
  - several modules declaring the same file (minimum pair wins);
  - the caller's own inline module winning over an earlier path;
  - a non-Rust file with a matching path;
  - memo versus scan equivalence over the existing Rust resolver fixtures.

**Step 2:**
1. Re-emitted ordinary edges for site ref IDs must still be deleted. They
   reappear only for owned and relinked callers, so those must be tracked
   exactly. A missed relinked caller leaves an ordinary edge that cold
   deletes.
2. Site-ref status must be re-applied to every re-emitted ref. Otherwise
   ordinary emission's `unresolved` status survives where cold says
   `resolved`.
3. Node lookups must stay SQL lookups on written rows. Duplicate
   `(file_path, scoped_name)` node rows make the chosen ID depend on
   SQLite's access path.
4. `edge_id` collisions within one site's target set (same file and symbol,
   different provenance) must keep last-wins order.
5. Owned deletes already removed dispatch edges of changed caller files (they
   share the ref IDs). Persisted site and unknown-live rows of removed files
   must be deleted by the diff.
6. `meta.view_dispatch_signature` must keep its current value.
7. Ordinary emission replays every row of a reference ordinal that several
   references share, so the cold writer's last-row-wins result is kept
   (`25af9b510`). It runs before this pass and its behaviour is unchanged,
   but this pass must read `refs` after that replay, not before.

**Parity gate for the build:**
- `AFT_UPDATE_BENCH_PARITY=1` on the five real cases (every stored derived
  table compared row for row against a cold build);
- the materialization suite, including the dispatch-proof and colliding-ordinal
  fixtures;
- new fixtures, each with a mutation proof (revert the fix, show that exact
  test fails, restore):
  - a relinked caller with a dispatch site;
  - a removed file with sites;
  - an edit that shifts target ordinals only;
  - a missing caller node.

**Step 3** carries the risk that the conservative rules miss an input
(aliases, Go directory membership, `@field`/`@return` chains). That is why it
needs persisted query recording and the always-safe full fallback.

## Stored format

- **Steps 1 and 2: no change.** Same tables, same rows, same meta keys.
  `MATERIALIZATION_VERSION` stays `10`, because outputs are identical and
  existing bases remain valid seeds.
- **Step 3: changes the stored format** (persisted dispatch dependency data
  and/or per-file dispatch facts). Not proposed without approval.

## Evidence and files

- Raw logs (outside the repository and build directories):
  - `~/.local/share/cortexkit/mason-evidence/callgraph-incremental-update-2026-10/baseline-run1.log`
    (unmodified code);
  - `~/.local/share/cortexkit/mason-evidence/callgraph-incremental-update-2026-10/prototype-memo-run2.log`
    (memo prototype, with memo-versus-unmodified cold parity and per-case
    incremental-versus-cold parity);
  - `~/.local/share/cortexkit/mason-evidence/callgraph-incremental-update-2026-10/committed-harness-run3.log`
    (the committed harness, unmodified code, one-file edit case only).

  These logs are on the measuring machine only; their key numbers are in the
  tables above.
- Committed in this delivery:
  - the ignored harness `crates/aft/src/views/materialization/update_bench.rs`
    (Unix only; it reads CPU time with `getrusage`);
  - opt-in stage timing: `profile::StageTimer`, used by
    `dispatch::emit`, `join::dispatch_type_targets` and the surface replay.
    It prints only when `AFT_VIEW_PROFILE` is set.
- Not committed: the memo prototype and its per-method call counters.
