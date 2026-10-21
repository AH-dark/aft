//! Content digest of everything the crate-wide Rust module index reads.
//!
//! Two resolver lookups read every Rust file of a manifest instead of one
//! file: `module_parent` (which file declares the `mod x;` whose file is a
//! given target) and `inline_scoped_target` (which Rust file defines a
//! qualified name such as `a::b::name`). The answers they gave are recorded as
//! depending on the pseudo-path [`join::VIEW_RUST_MODULE_DOMAIN`], and every
//! dependent of that pseudo-path is checked again when it is seeded. This
//! module keeps a digest of the inputs those lookups can read, so a
//! publication seeds the domain only when the digest changes:
//!
//! - every `.rs` path of the manifest, with its entry kind and whether it has
//!   call-graph parse output (the Rust file set the lookups scan);
//! - per Rust parse blob: its out-of-line `mod` declarations in order (name,
//!   `#[macro_use]` marker, `#[path]` value) and every symbol whose name has a
//!   module or type qualifier (`m::f`, `Type::method`), with its kind. These
//!   are the only names `inline_scoped_target` can find, and the lookup
//!   records no dependency on the file that defines them;
//! - every `Cargo.toml`, whose crate and workspace membership decides crate
//!   roots and so the directory a `mod x;` looks for its file in;
//! - for every `#[path]` attribute, whether its target is a regular file. The
//!   target can be any file, not only a `.rs` one.
//!
//! Each input's digest is a row of `view_module_digest_inputs`, each `#[path]`
//! target's existence a row of `view_module_path_targets`, and their
//! order-independent XOR is stored in `meta` under `view_module_digest`, next
//! to the manifest fingerprint the rows were computed at. A publication
//! updates them from the changed manifest paths only: a changed `.rs` file's
//! blob is decoded, a changed `Cargo.toml` is re-hashed from its entry, and a
//! `#[path]` target is checked again when the diff adds or removes its path.
//! When the rows are missing, or were computed at a manifest other than the
//! base's (a writer that does not maintain them advanced the base), the digest
//! is computed in full and treated as changed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::callgraph_store::facts::{path_bytes, FactPaths, ManifestFacts};
use crate::callgraph_store::{join, CallGraphStoreError, Result};
use crate::views::{Manifest, ManifestEntry, RelPath};

/// `source` is the declaring `.rs` file, `declared` the path the attribute
/// names joined to that file's directory exactly as module resolution joins
/// it, and `target` that path with `.` and `..` removed: the manifest path a
/// diff adds or removes. `indirect` marks a target whose path passes through
/// a symbolic link, so its existence can change without its own path changing.
pub(super) const CREATE_TABLES: &str = "CREATE TABLE IF NOT EXISTS view_module_digest_inputs (
        input TEXT PRIMARY KEY, digest TEXT NOT NULL) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS view_module_path_targets (
        target TEXT NOT NULL, source TEXT NOT NULL, declared TEXT NOT NULL,
        present INTEGER NOT NULL, indirect INTEGER NOT NULL,
        PRIMARY KEY(target, source, declared)) WITHOUT ROWID;
    CREATE INDEX IF NOT EXISTS idx_view_module_path_targets_source
        ON view_module_path_targets(source);
    CREATE INDEX IF NOT EXISTS idx_view_module_path_targets_indirect
        ON view_module_path_targets(target) WHERE indirect = 1;";

const DIGEST_KEY: &str = "view_module_digest";
/// The `view_manifest_fingerprint` the digest rows were computed at.
pub(super) const FINGERPRINT_KEY: &str = "view_module_digest_fingerprint";

#[cfg(test)]
thread_local! {
    /// Reports every incremental digest as changed, which seeds the domain on
    /// every publication as builds before the digest did. Tests use it to
    /// measure what a publication selects through the domain alone.
    pub(crate) static FORCE_CHANGED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// What maintaining the digest did during one materialization.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct DigestOutcome {
    /// The digest differs from the base's, so the module domain is seeded.
    pub changed: bool,
    /// Every row was computed again instead of updated from the diff.
    pub full: bool,
    /// Parse blobs decoded to compute contributions. Decoded blobs are cached
    /// by the reader, so the join reuses them.
    pub decoded_blobs: usize,
}

/// The part of one Rust parse blob the module index reads.
#[derive(Debug, Serialize)]
pub(super) struct RustParseFacts {
    /// Out-of-line `mod` declarations in source order: module name, the
    /// recorded reference text (which carries the `#[macro_use]` marker), and
    /// the `#[path]` value. Order matters because the first declaration of a
    /// name wins.
    modules: Vec<(Option<String>, Option<String>, Option<String>)>,
    /// `(scoped name, name, kind)` of every symbol with a qualified name.
    qualified: BTreeSet<(String, String, String)>,
}

impl RustParseFacts {
    pub(super) fn of(blob: &join::ParseBlob) -> Option<Self> {
        if blob.language != "rust" {
            return None;
        }
        let modules = blob
            .refs
            .iter()
            .filter(|reference| reference.kind == join::BlobRefKind::Module)
            .map(|reference| {
                (
                    reference.module_path.clone(),
                    reference.full_ref.clone(),
                    reference.path_override.clone(),
                )
            })
            .collect();
        let qualified = blob
            .symbols
            .iter()
            .filter(|symbol| symbol.scoped_name.contains("::") || symbol.name.contains("::"))
            .map(|symbol| {
                (
                    symbol.scoped_name.clone(),
                    symbol.name.clone(),
                    symbol.kind.clone(),
                )
            })
            .collect();
        Some(Self { modules, qualified })
    }

    fn path_overrides(&self) -> impl Iterator<Item = &str> {
        self.modules
            .iter()
            .filter_map(|(_, _, path)| path.as_deref())
    }
}

fn is_rust_path(path: &str) -> bool {
    path.ends_with(".rs")
}

fn is_cargo_path(path: &str) -> bool {
    path.rsplit('/').next() == Some("Cargo.toml")
}

/// Entry facts that membership and module-file lookups observe. A regular
/// file's content enters through its parse facts, not its blob key, so an
/// edit that leaves those facts alone leaves the digest alone.
fn entry_tag(entry: &ManifestEntry) -> String {
    match entry {
        ManifestEntry::Regular { planes, .. } => {
            format!("regular:{}", planes.callgraph.is_some())
        }
        ManifestEntry::Symlink { target_bytes } => {
            format!(
                "symlink:{}",
                String::from_utf8_lossy(target_bytes.as_bytes())
            )
        }
        ManifestEntry::Gitlink { oid } => format!("gitlink:{oid}"),
        ManifestEntry::Synthetic { name, .. } => format!("synthetic:{name}"),
    }
}

fn hash_json(value: &impl Serialize) -> String {
    blake3::hash(&serde_json::to_vec(value).expect("module digest input serializes"))
        .to_hex()
        .to_string()
}

fn rust_input_digest(entry: &ManifestEntry, parse: Option<&RustParseFacts>) -> String {
    hash_json(&(entry_tag(entry), parse))
}

/// A Cargo manifest's content is its call-graph blob key, so the crate and
/// workspace membership it declares changes this digest.
fn cargo_input_digest(entry: &ManifestEntry) -> String {
    match entry {
        ManifestEntry::Regular {
            mode,
            planes,
            resolution_input,
        } => hash_json(&(
            "regular",
            mode,
            resolution_input,
            planes.callgraph.as_deref(),
        )),
        other => hash_json(&entry_tag(other)),
    }
}

/// One `#[path]` target's row.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TargetRow {
    target: String,
    source: String,
    declared: String,
    present: bool,
    indirect: bool,
}

fn mix(aggregate: &mut [u8; 32], parts: &[&[u8]]) {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    for (byte, other) in aggregate.iter_mut().zip(hasher.finalize().as_bytes()) {
        *byte ^= other;
    }
}

fn mix_input(aggregate: &mut [u8; 32], input: &str, digest: &str) {
    mix(aggregate, &[b"input", input.as_bytes(), digest.as_bytes()]);
}

fn mix_target(aggregate: &mut [u8; 32], row: &TargetRow) {
    mix(
        aggregate,
        &[
            b"target",
            row.target.as_bytes(),
            row.source.as_bytes(),
            row.declared.as_bytes(),
            &[u8::from(row.present), u8::from(row.indirect)],
        ],
    );
}

/// Existence of the file one `#[path]` attribute of `source` names, decided
/// the way module resolution decides it: the attribute joined to the
/// declaring file's directory under the join's `/` root, then a regular-file
/// check through the manifest, which follows symbolic links.
fn target_row(manifest: &Manifest, source: &str, path_override: &str) -> TargetRow {
    let root = Path::new("/");
    let declaring = root.join(source);
    let joined = declaring.parent().unwrap_or(root).join(path_override);
    let declared =
        String::from_utf8_lossy(&path_bytes(joined.strip_prefix(root).unwrap_or(&joined)))
            .into_owned();
    declared_target_row(manifest, source, declared)
}

/// The row of an already joined `#[path]` target, as recorded in `declared`.
fn declared_target_row(manifest: &Manifest, source: &str, declared: String) -> TargetRow {
    let root = Path::new("/");
    let joined = root.join(&declared);
    let reader = |_: &String| None;
    let facts = ManifestFacts {
        manifest,
        blobs: &reader,
    };
    let present = FactPaths {
        root,
        facts: &facts,
    }
    .is_file(&joined);
    // Remove `.` and `..` the way the manifest's path walk does, noting any
    // prefix that is a symbolic link: past one, the walk no longer follows
    // the written path.
    let mut components = Vec::<&str>::new();
    let mut indirect = false;
    let mut escaped = false;
    for component in declared.split('/') {
        match component {
            "" | "." => continue,
            ".." => {
                if components.pop().is_none() {
                    escaped = true;
                    break;
                }
                continue;
            }
            _ => components.push(component),
        }
        let prefix = components.join("/");
        if RelPath::new(prefix.into_bytes())
            .ok()
            .and_then(|path| manifest.get(&path))
            .is_some_and(|entry| matches!(entry, ManifestEntry::Symlink { .. }))
        {
            indirect = true;
        }
    }
    TargetRow {
        target: if escaped {
            declared.clone()
        } else {
            components.join("/")
        },
        source: source.to_string(),
        declared,
        present,
        indirect,
    }
}

fn target_rows(manifest: &Manifest, source: &str, parse: &RustParseFacts) -> Vec<TargetRow> {
    let mut rows = parse
        .path_overrides()
        .map(|path| target_row(manifest, source, path))
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        (&left.target, &left.declared).cmp(&(&right.target, &right.declared))
    });
    rows.dedup_by(|left, right| left.target == right.target && left.declared == right.declared);
    rows
}

fn insert_target(connection: &Connection, row: &TargetRow) -> Result<()> {
    connection.execute(
        "INSERT OR REPLACE INTO view_module_path_targets
             (target, source, declared, present, indirect) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            row.target,
            row.source,
            row.declared,
            i64::from(row.present),
            i64::from(row.indirect)
        ],
    )?;
    Ok(())
}

fn read_meta(connection: &Connection, key: &str) -> Result<Option<String>> {
    Ok(connection
        .query_row("SELECT v FROM meta WHERE k = ?1", [key], |row| row.get(0))
        .optional()?)
}

fn parse_aggregate(value: &str) -> Option<[u8; 32]> {
    blake3::Hash::from_hex(value)
        .ok()
        .map(|hash| *hash.as_bytes())
}

fn write_aggregate(connection: &Connection, aggregate: &[u8; 32]) -> Result<()> {
    connection.execute(
        "INSERT OR REPLACE INTO meta(k, v) VALUES(?1, ?2)",
        params![
            DIGEST_KEY,
            blake3::Hash::from(*aggregate).to_hex().to_string()
        ],
    )?;
    Ok(())
}

/// Records the manifest fingerprint the digest rows now describe. Called in
/// the transaction that records `view_manifest_fingerprint`.
pub(super) fn record_fingerprint(connection: &Connection, fingerprint: &str) -> Result<()> {
    connection.execute(
        "INSERT OR REPLACE INTO meta(k, v) VALUES(?1, ?2)",
        params![FINGERPRINT_KEY, fingerprint],
    )?;
    Ok(())
}

/// Moves the digest rows to a manifest that differs from the base in no
/// call-graph input: same paths, kinds and call-graph planes, so every input
/// above is unchanged. Rows recorded at another manifest stay stale.
pub(super) fn advance_unchanged(
    connection: &Connection,
    base_fingerprint: &str,
    fingerprint: &str,
) -> Result<()> {
    connection.execute(
        "UPDATE meta SET v = ?2 WHERE k = ?1 AND v = ?3",
        params![FINGERPRINT_KEY, fingerprint, base_fingerprint],
    )?;
    Ok(())
}

/// Replaces every row with the digest of `manifest`. `parse` returns the
/// Rust parse facts of one `.rs` path's call-graph blob key.
pub(super) fn compute_full(
    connection: &Connection,
    manifest: &Manifest,
    mut parse: impl FnMut(&str, &str) -> Result<Option<RustParseFacts>>,
) -> Result<()> {
    connection.execute_batch(
        "DELETE FROM view_module_digest_inputs; DELETE FROM view_module_path_targets;",
    )?;
    let mut aggregate = [0u8; 32];
    let mut insert_input = connection
        .prepare("INSERT INTO view_module_digest_inputs(input, digest) VALUES (?1, ?2)")?;
    for (path, entry) in manifest.entries() {
        let Ok(path) = std::str::from_utf8(path.as_bytes()) else {
            // Non-UTF-8 paths are never bound into the resolver index.
            continue;
        };
        if is_rust_path(path) {
            let facts = match entry {
                ManifestEntry::Regular { planes, .. } => match planes.callgraph.as_deref() {
                    Some(key) => parse(path, key)?,
                    None => None,
                },
                _ => None,
            };
            let input = format!("rust:{path}");
            let digest = rust_input_digest(entry, facts.as_ref());
            mix_input(&mut aggregate, &input, &digest);
            insert_input.execute(params![input, digest])?;
            for row in facts
                .as_ref()
                .map(|facts| target_rows(manifest, path, facts))
                .unwrap_or_default()
            {
                mix_target(&mut aggregate, &row);
                insert_target(connection, &row)?;
            }
        }
        if is_cargo_path(path) {
            let input = format!("cargo:{path}");
            let digest = cargo_input_digest(entry);
            mix_input(&mut aggregate, &input, &digest);
            insert_input.execute(params![input, digest])?;
        }
    }
    write_aggregate(connection, &aggregate)
}

/// Replaces one input row, or removes it when `digest` is `None`.
fn replace_input(
    connection: &Connection,
    aggregate: &mut [u8; 32],
    input: &str,
    digest: Option<&str>,
) -> Result<()> {
    let old: Option<String> = connection
        .query_row(
            "SELECT digest FROM view_module_digest_inputs WHERE input = ?1",
            [input],
            |row| row.get(0),
        )
        .optional()?;
    if old.as_deref() == digest {
        return Ok(());
    }
    if let Some(old) = &old {
        mix_input(aggregate, input, old);
    }
    match digest {
        Some(digest) => {
            mix_input(aggregate, input, digest);
            connection.execute(
                "INSERT OR REPLACE INTO view_module_digest_inputs(input, digest) VALUES (?1, ?2)",
                params![input, digest],
            )?;
        }
        None => {
            connection.execute(
                "DELETE FROM view_module_digest_inputs WHERE input = ?1",
                [input],
            )?;
        }
    }
    Ok(())
}

fn read_targets(
    connection: &Connection,
    sql: &str,
    parameter: Option<&str>,
) -> Result<Vec<TargetRow>> {
    let mut statement = connection.prepare_cached(sql)?;
    let map = |row: &rusqlite::Row<'_>| {
        Ok(TargetRow {
            target: row.get(0)?,
            source: row.get(1)?,
            declared: row.get(2)?,
            present: row.get::<_, i64>(3)? != 0,
            indirect: row.get::<_, i64>(4)? != 0,
        })
    };
    let rows = match parameter {
        Some(parameter) => statement
            .query_map([parameter], map)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        None => statement
            .query_map([], map)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
    };
    Ok(rows)
}

const TARGET_COLUMNS: &str = "SELECT target, source, declared, present, indirect
     FROM view_module_path_targets";

fn delete_target(connection: &Connection, row: &TargetRow) -> Result<()> {
    connection.execute(
        "DELETE FROM view_module_path_targets WHERE target = ?1 AND source = ?2 AND declared = ?3",
        params![row.target, row.source, row.declared],
    )?;
    Ok(())
}

/// Brings the digest rows from `base` to `manifest` through the paths of
/// `changed`, decoding only the changed `.rs` files' blobs, and reports
/// whether the digest changed. Falls back to [`compute_full`] when the rows
/// do not describe `base`.
pub(super) fn update(
    connection: &Connection,
    base: &Manifest,
    base_fingerprint: &str,
    manifest: &Manifest,
    changed: &BTreeSet<Vec<u8>>,
    reader: &impl join::ManifestBlobReader,
) -> Result<DigestOutcome> {
    let mut decoded_blobs = 0;
    let mut parse = |path: &str, key: &str| -> Result<Option<RustParseFacts>> {
        decoded_blobs += 1;
        let blob = reader
            .read_callgraph_blob_decoded(key)
            .map_err(|error| CallGraphStoreError::Unavailable(error.to_string()))?
            .ok_or_else(|| {
                CallGraphStoreError::Unavailable(format!(
                    "missing manifest callgraph blob {key} for {path}"
                ))
            })?;
        Ok(blob.parse().and_then(RustParseFacts::of))
    };
    let recorded = read_meta(connection, DIGEST_KEY)?
        .as_deref()
        .and_then(parse_aggregate);
    let recorded_fingerprint = read_meta(connection, FINGERPRINT_KEY)?;
    let Some(recorded) =
        recorded.filter(|_| recorded_fingerprint.as_deref() == Some(base_fingerprint))
    else {
        compute_full(connection, manifest, &mut parse)?;
        return Ok(DigestOutcome {
            changed: true,
            full: true,
            decoded_blobs,
        });
    };
    let mut aggregate = recorded;
    let mut refreshed_sources = BTreeSet::new();
    let mut membership_changed = Vec::new();
    let mut symlink_changed = false;
    for path in changed {
        let Ok(path_text) = std::str::from_utf8(path) else {
            continue;
        };
        let Ok(rel) = RelPath::new(path.clone()) else {
            continue;
        };
        let (old, new) = (base.get(&rel), manifest.get(&rel));
        if old.map(ManifestEntry::kind) != new.map(ManifestEntry::kind) {
            membership_changed.push(path_text.to_string());
        }
        symlink_changed |= [old, new]
            .into_iter()
            .flatten()
            .any(|entry| matches!(entry, ManifestEntry::Symlink { .. }));
        if is_rust_path(path_text) {
            let facts = match new {
                Some(ManifestEntry::Regular { planes, .. }) => match planes.callgraph.as_deref() {
                    Some(key) => parse(path_text, key)?,
                    None => None,
                },
                _ => None,
            };
            let digest = new.map(|entry| rust_input_digest(entry, facts.as_ref()));
            replace_input(
                connection,
                &mut aggregate,
                &format!("rust:{path_text}"),
                digest.as_deref(),
            )?;
            let old_rows = read_targets(
                connection,
                &format!("{TARGET_COLUMNS} WHERE source = ?1"),
                Some(path_text),
            )?;
            let new_rows = facts
                .as_ref()
                .map(|facts| target_rows(manifest, path_text, facts))
                .unwrap_or_default();
            if old_rows != new_rows {
                for row in &old_rows {
                    mix_target(&mut aggregate, row);
                    delete_target(connection, row)?;
                }
                for row in &new_rows {
                    mix_target(&mut aggregate, row);
                    insert_target(connection, row)?;
                }
            }
            refreshed_sources.insert(path_text.to_string());
        }
        if is_cargo_path(path_text) {
            let digest = new.map(cargo_input_digest);
            replace_input(
                connection,
                &mut aggregate,
                &format!("cargo:{path_text}"),
                digest.as_deref(),
            )?;
        }
    }
    // Existence of an unchanged file's `#[path]` targets: check the rows whose
    // target the diff added or removed, and the rows behind a symbolic link.
    // A changed link can redirect any target, so then check every row.
    let mut stale = BTreeMap::<(String, String, String), TargetRow>::new();
    let mut collect = |rows: Vec<TargetRow>| {
        for row in rows {
            if !refreshed_sources.contains(&row.source) {
                stale.insert(
                    (row.target.clone(), row.source.clone(), row.declared.clone()),
                    row,
                );
            }
        }
    };
    if symlink_changed {
        collect(read_targets(connection, TARGET_COLUMNS, None)?);
    } else {
        for path in &membership_changed {
            collect(read_targets(
                connection,
                &format!("{TARGET_COLUMNS} WHERE target = ?1"),
                Some(path),
            )?);
        }
        collect(read_targets(
            connection,
            &format!("{TARGET_COLUMNS} INDEXED BY idx_view_module_path_targets_indirect WHERE indirect = 1"),
            None,
        )?);
    }
    for row in stale.into_values() {
        let fresh = declared_target_row(manifest, &row.source, row.declared.clone());
        if fresh != row {
            mix_target(&mut aggregate, &row);
            delete_target(connection, &row)?;
            mix_target(&mut aggregate, &fresh);
            insert_target(connection, &fresh)?;
        }
    }
    write_aggregate(connection, &aggregate)?;
    #[cfg(test)]
    let forced = FORCE_CHANGED.with(std::cell::Cell::get);
    #[cfg(not(test))]
    let forced = false;
    Ok(DigestOutcome {
        changed: forced || aggregate != recorded,
        full: false,
        decoded_blobs,
    })
}
