//! Persist ruled method edges and unknown-receiver liveness in the same
//! transaction as ordinary bindings. The input is immutable parse blobs only.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use rusqlite::{params, OptionalExtension, Transaction};

use crate::callgraph_store::join::{
    dispatch::{MemoizedResolver, Resolution, Resolver, SiteHint, Target},
    CallgraphBlob, ManifestBlobReader,
};
use crate::callgraph_store::{CallGraphStoreError, Result};
use crate::views::{Manifest, ManifestEntry};

/// Which rows the ordinary (non-method-call) graph writes earlier in the
/// same transaction already replaced, so the method-call pass knows which
/// persisted rows it can trust.
pub(super) struct EmitScope<'a> {
    /// Manifest paths whose `nodes` rows this transaction rewrote, or `None`
    /// for a cold build, where every path counts as rewritten. Node IDs encode
    /// the AST ordinal, so edges touching these files must look nodes up again.
    pub(super) changed_paths: Option<&'a BTreeSet<Vec<u8>>>,
    /// Ref IDs for which ordinary emission wrote an edge in this transaction.
    /// An ordinary edge on a method-call site's ref ID can only come from this
    /// set: the method-call pass of every earlier build deleted such edges.
    pub(super) ordinary_edge_refs: &'a HashSet<String>,
}

#[cfg(test)]
thread_local! {
    /// Lets a test or measurement write every site row by row, as the pass did
    /// before it wrote only differences, to compare the two databases.
    pub(crate) static WRITE_ROW_BY_ROW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(super) fn emit(
    transaction: &Transaction<'_>,
    manifest: &Manifest,
    reader: &impl ManifestBlobReader,
    scope: &EmitScope<'_>,
) -> Result<()> {
    // With `AFT_VIEW_PROFILE` set, each stage of this pass prints its wall
    // time to stderr; without it nothing is timed or printed.
    let mut stage = super::profile::StageTimer::new("dispatch");
    let mut keys = Vec::new();
    for (path, entry) in manifest.entries() {
        let ManifestEntry::Regular { planes, .. } = entry else {
            continue;
        };
        let Some(key) = &planes.callgraph else {
            continue;
        };
        let Ok(path) = std::str::from_utf8(path.as_bytes()) else {
            continue;
        };
        keys.push((path, key.as_str()));
    }
    let blobs = reader
        .read_callgraph_blobs_decoded(&keys.iter().map(|(_, key)| *key).collect::<Vec<_>>())
        .map_err(|e| CallGraphStoreError::Unavailable(e.to_string()))?;
    let mut decoded = BTreeMap::<String, Arc<CallgraphBlob>>::new();
    for ((path, key), blob) in keys.iter().zip(blobs) {
        let blob = blob.ok_or_else(|| {
            CallGraphStoreError::Unavailable(format!("missing dispatch blob {key}"))
        })?;
        decoded.insert((*path).to_string(), blob);
    }
    stage.finish("decode_all_blobs", format_args!("files={}", decoded.len()));
    let mut resolver = Resolver::new(
        decoded
            .iter()
            .filter_map(|(file, blob)| blob.parse().map(|p| (file.clone(), p)))
            .collect(),
    );
    resolver.type_targets =
        crate::callgraph_store::join::dispatch_type_targets(manifest, reader, &resolver.files)
            .map_err(|e| CallGraphStoreError::Unavailable(e.to_string()))?;
    stage.finish(
        "type_targets",
        format_args!("targets={}", resolver.type_targets.len()),
    );
    {
        let mut query = transaction.prepare("SELECT caller_file, module_path, target_file FROM refs WHERE kind='import' AND module_path IS NOT NULL AND target_file IS NOT NULL ORDER BY caller_file, module_path, target_file")?;
        for row in query.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })? {
            let (caller, module, target) = row?;
            resolver.import_targets.insert((caller, module), target);
        }
    }
    stage.finish(
        "import_targets",
        format_args!("targets={}", resolver.import_targets.len()),
    );
    // The signature is persisted metadata that cold and incremental builds
    // must agree on. The row-by-row writer also uses it to decide whether the
    // site tables must be rebuilt.
    let inputs = resolver
        .files
        .iter()
        .map(|(file, parse)| (file, &parse.language, &parse.dispatch, &parse.imports))
        .collect::<Vec<_>>();
    let signature = blake3::hash(
        &serde_json::to_vec(&(
            inputs,
            resolver.import_targets.iter().collect::<Vec<_>>(),
            resolver.type_targets.iter().collect::<Vec<_>>(),
        ))
        .map_err(|e| CallGraphStoreError::Unavailable(e.to_string()))?,
    )
    .to_hex()
    .to_string();
    let old: Option<String> = transaction
        .query_row(
            "SELECT v FROM meta WHERE k='view_dispatch_signature'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    transaction.execute_batch("CREATE TABLE IF NOT EXISTS view_dispatch_sites (
        caller_file TEXT NOT NULL, ordinal INTEGER NOT NULL, language TEXT NOT NULL,
        unresolved_receiver_sites INTEGER NOT NULL, dynamic_member_sites INTEGER NOT NULL,
        external INTEGER NOT NULL, PRIMARY KEY(caller_file, ordinal));
        CREATE TABLE IF NOT EXISTS view_unknown_live (
        caller_file TEXT NOT NULL, ordinal INTEGER NOT NULL, target_file TEXT NOT NULL,
        target_symbol TEXT NOT NULL, PRIMARY KEY(caller_file, ordinal, target_file, target_symbol));")?;
    let signature_changed = old.as_deref() != Some(&signature);
    stage.finish("signature", format_args!("changed={signature_changed}"));
    let memoized = MemoizedResolver::new(&resolver);
    let mut sites = Vec::new();
    let mut ordinals = HashSet::new();
    let mut duplicate_ordinal = false;
    for (file, parse) in &resolver.files {
        for site in &parse.dispatch.sites {
            duplicate_ordinal |= !ordinals.insert((file.as_str(), site.ordinal));
            sites.push(ResolvedSite {
                file,
                language: &parse.language,
                ref_id: format!("view:{file}:{}", site.ordinal),
                site,
                resolution: memoized.resolve(file, site),
            });
        }
    }
    let targets = sites
        .iter()
        .map(|site| site.resolution.targets.len())
        .sum::<usize>();
    stage.finish(
        "resolve_sites",
        format_args!("sites={} targets={targets}", sites.len()),
    );
    #[cfg(test)]
    let row_by_row = WRITE_ROW_BY_ROW.with(std::cell::Cell::get);
    #[cfg(not(test))]
    let row_by_row = false;
    if duplicate_ordinal || row_by_row {
        // Two sites of one file sharing an ordinal share one ref ID and one
        // `view_dispatch_sites` key, so only the original row-by-row order
        // defines the result (including its constraint error).
        write_row_by_row(transaction, &sites, signature_changed)?;
        stage.finish("write_row_by_row", format_args!(""));
    } else {
        let counts = write_differences(transaction, &sites, scope)?;
        stage.finish("write_differences", format_args!("{counts:?}"));
    }
    transaction.execute(
        "INSERT OR REPLACE INTO meta(k,v) VALUES ('view_dispatch_signature',?1)",
        [signature],
    )?;
    Ok(())
}

/// One method-call site with its resolution, in writer order: files in path
/// order, sites in extraction order.
struct ResolvedSite<'a> {
    file: &'a str,
    language: &'a str,
    ref_id: String,
    site: &'a SiteHint,
    resolution: Rc<Resolution>,
}

impl ResolvedSite<'_> {
    fn status(&self) -> &'static str {
        if self.resolution.targets.is_empty() {
            "unresolved"
        } else {
            "resolved"
        }
    }

    fn edge_id(&self, target: &Target) -> String {
        format!(
            "dispatch:{}:{}:{}:{}",
            self.file, self.site.ordinal, target.file, target.symbol
        )
    }
}

/// Writes every site's rows, statement by statement. When the signature
/// changed, the site tables and all method-call edges are first emptied.
fn write_row_by_row(
    transaction: &Transaction<'_>,
    sites: &[ResolvedSite<'_>],
    signature_changed: bool,
) -> Result<()> {
    if signature_changed {
        transaction.execute_batch(
            "DELETE FROM view_dispatch_sites; DELETE FROM view_unknown_live;
            DELETE FROM edges WHERE provenance IN ('exact', 'dispatch', 'name_match');",
        )?;
    }
    for site in sites {
        let resolution = &site.resolution;
        transaction.execute(
            "DELETE FROM edges WHERE ref_id=?1 AND provenance NOT IN ('exact', 'dispatch', 'name_match')",
            [&site.ref_id],
        )?;
        transaction.execute("UPDATE refs SET status=?2, target_node=NULL, target_file=NULL, target_symbol=NULL WHERE ref_id=?1", params![site.ref_id, site.status()])?;
        if signature_changed {
            transaction.execute(
                "INSERT INTO view_dispatch_sites VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    site.file,
                    site.site.ordinal,
                    site.language,
                    resolution.unresolved,
                    resolution.dynamic,
                    resolution.external
                ],
            )?;
            for (target_file, target_symbol) in &resolution.protected {
                transaction.execute(
                    "INSERT INTO view_unknown_live VALUES (?1,?2,?3,?4)",
                    params![site.file, site.site.ordinal, target_file, target_symbol],
                )?;
            }
        }
        let caller: Option<String> = site
            .site
            .caller
            .as_ref()
            .map(|symbol| node(transaction, site.file, symbol))
            .transpose()?
            .flatten();
        let Some(caller) = caller else {
            continue;
        };
        for target in &resolution.targets {
            let target_node = node(transaction, &target.file, &target.symbol)?;
            transaction.execute("INSERT OR REPLACE INTO edges (edge_id, ref_id, source_node, target_node, target_file, target_symbol, kind, line, provenance)
                VALUES (?1,?2,?3,?4,?5,?6,'call',?7,?8)", params![site.edge_id(target), site.ref_id, caller, target_node, target.file, target.symbol, site.site.line, target.provenance])?;
        }
    }
    Ok(())
}

/// Row operations performed by [`write_differences`], for profiling.
#[derive(Debug, Default)]
struct DifferenceCounts {
    ordinary_edges_deleted: usize,
    statuses_updated: usize,
    sites_written: usize,
    sites_deleted: usize,
    unknown_live_inserted: usize,
    unknown_live_deleted: usize,
    edges_kept: usize,
    edges_written: usize,
    edges_deleted: usize,
}

type EdgeRow = (String, String, String, i64, String);

/// Brings the method-call rows to exactly what [`write_row_by_row`] leaves in
/// a database whose site tables and method-call edges start empty (a cold
/// build), but writes only rows that differ from the current transaction
/// state.
///
/// The current state already reflects this transaction's ordinary writes:
/// rewritten refs carry ordinary statuses and edges, and the edges of
/// rewritten or relinked refs are gone. Rows of files outside
/// `scope.changed_paths` keep their node IDs, so an existing method-call edge
/// between two such files that matches its site and target needs no lookup.
fn write_differences(
    transaction: &Transaction<'_>,
    sites: &[ResolvedSite<'_>],
    scope: &EmitScope<'_>,
) -> Result<DifferenceCounts> {
    let mut counts = DifferenceCounts::default();
    let rewritten = |file: &str| {
        scope
            .changed_paths
            .is_none_or(|paths| paths.contains(file.as_bytes()))
    };

    // Ordinary edges for a site's ref ID exist only if this transaction wrote them.
    {
        let mut delete = transaction.prepare(
            "DELETE FROM edges WHERE ref_id=?1 AND provenance NOT IN ('exact', 'dispatch', 'name_match')",
        )?;
        for site in sites {
            if scope.ordinary_edge_refs.contains(&site.ref_id) {
                counts.ordinary_edges_deleted += delete.execute([&site.ref_id])?;
            }
        }
    }

    // Statuses: every site ref is unresolved or resolved with no target columns.
    {
        let mut current = HashMap::<String, (String, bool)>::new();
        let mut read = transaction.prepare(
            "SELECT ref_id, status, target_node IS NULL AND target_file IS NULL AND target_symbol IS NULL
             FROM refs WHERE caller_file=?1",
        )?;
        let mut file = None;
        let mut update = transaction.prepare("UPDATE refs SET status=?2, target_node=NULL, target_file=NULL, target_symbol=NULL WHERE ref_id=?1")?;
        for site in sites {
            if file != Some(site.file) {
                file = Some(site.file);
                current.clear();
                for row in read.query_map([site.file], |row| {
                    Ok((row.get::<_, String>(0)?, (row.get(1)?, row.get(2)?)))
                })? {
                    let (ref_id, state) = row?;
                    current.insert(ref_id, state);
                }
            }
            let Some((status, no_targets)) = current.get(&site.ref_id) else {
                continue;
            };
            if status != site.status() || !no_targets {
                counts.statuses_updated += update.execute(params![site.ref_id, site.status()])?;
            }
        }
    }

    // One `view_dispatch_sites` row per site.
    {
        let mut current = HashMap::<(String, i64), (String, i64, i64, i64)>::new();
        let mut read = transaction.prepare(
            "SELECT caller_file, ordinal, language, unresolved_receiver_sites,
                    dynamic_member_sites, external FROM view_dispatch_sites",
        )?;
        for row in read.query_map([], |row| {
            Ok((
                (row.get::<_, String>(0)?, row.get::<_, i64>(1)?),
                (row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?),
            ))
        })? {
            let (key, value) = row?;
            current.insert(key, value);
        }
        let mut write = transaction
            .prepare("INSERT OR REPLACE INTO view_dispatch_sites VALUES (?1,?2,?3,?4,?5,?6)")?;
        for site in sites {
            let resolution = &site.resolution;
            let wanted = (
                site.language.to_string(),
                resolution.unresolved as i64,
                resolution.dynamic as i64,
                resolution.external as i64,
            );
            if current.remove(&(site.file.to_string(), i64::from(site.site.ordinal)))
                == Some(wanted.clone())
            {
                continue;
            }
            counts.sites_written += write.execute(params![
                site.file,
                site.site.ordinal,
                wanted.0,
                wanted.1,
                wanted.2,
                wanted.3
            ])?;
        }
        let mut delete = transaction
            .prepare("DELETE FROM view_dispatch_sites WHERE caller_file=?1 AND ordinal=?2")?;
        for (file, ordinal) in current.keys() {
            counts.sites_deleted += delete.execute(params![file, ordinal])?;
        }
    }

    // Unknown-receiver liveness: one row per site and protected target.
    {
        let mut wanted = BTreeSet::<(String, i64, String, String)>::new();
        for site in sites {
            for (file, symbol) in &site.resolution.protected {
                wanted.insert((
                    site.file.to_string(),
                    i64::from(site.site.ordinal),
                    file.clone(),
                    symbol.clone(),
                ));
            }
        }
        let mut stale = Vec::new();
        let mut read = transaction.prepare(
            "SELECT caller_file, ordinal, target_file, target_symbol FROM view_unknown_live",
        )?;
        for row in read.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })? {
            let row = row?;
            if !wanted.remove(&row) {
                stale.push(row);
            }
        }
        let mut delete = transaction.prepare(
            "DELETE FROM view_unknown_live
             WHERE caller_file=?1 AND ordinal=?2 AND target_file=?3 AND target_symbol=?4",
        )?;
        for row in &stale {
            counts.unknown_live_deleted += delete.execute(params![row.0, row.1, row.2, row.3])?;
        }
        let mut insert =
            transaction.prepare("INSERT INTO view_unknown_live VALUES (?1,?2,?3,?4)")?;
        for row in &wanted {
            counts.unknown_live_inserted += insert.execute(params![row.0, row.1, row.2, row.3])?;
        }
    }

    // Method-call edges.
    {
        let mut current = HashMap::<String, EdgeRow>::new();
        let mut read = transaction.prepare(
            "SELECT edge_id, ref_id, target_file, target_symbol, kind, line, provenance
             FROM edges WHERE provenance IN ('exact', 'dispatch', 'name_match')",
        )?;
        for row in read.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                (
                    row.get::<_, String>(1)?,
                    format!(
                        "{}\0{}",
                        row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        row.get::<_, Option<String>>(3)?.unwrap_or_default()
                    ),
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, String>(6)?,
                ),
            ))
        })? {
            let (edge_id, edge) = row?;
            current.insert(edge_id, edge);
        }
        // Several targets of one site can map to one edge ID (same file and
        // symbol, different provenance). Writing them one `INSERT OR REPLACE`
        // at a time keeps the last, so `by_id` keeps the last target per edge
        // ID. Edge IDs of different sites never collide.
        let mut wanted = Vec::<(String, &ResolvedSite<'_>, &Target)>::new();
        for site in sites {
            if site.site.caller.is_none() {
                continue;
            }
            let mut by_id = BTreeMap::new();
            for target in &site.resolution.targets {
                by_id.insert(site.edge_id(target), target);
            }
            let mut ordered = site
                .resolution
                .targets
                .iter()
                .map(|target| site.edge_id(target))
                .collect::<Vec<_>>();
            ordered.dedup();
            for edge_id in ordered {
                if let Some(target) = by_id.remove(&edge_id) {
                    wanted.push((edge_id, site, target));
                }
            }
        }
        let mut callers = HashMap::<(&str, &str), Option<String>>::new();
        let mut write = transaction.prepare("INSERT OR REPLACE INTO edges (edge_id, ref_id, source_node, target_node, target_file, target_symbol, kind, line, provenance)
            VALUES (?1,?2,?3,?4,?5,?6,'call',?7,?8)")?;
        for (edge_id, site, target) in wanted {
            let expected = (
                site.ref_id.clone(),
                format!("{}\0{}", target.file, target.symbol),
                "call".to_string(),
                i64::from(site.site.line),
                target.provenance.to_string(),
            );
            if !rewritten(site.file)
                && !rewritten(&target.file)
                && current.get(&edge_id) == Some(&expected)
            {
                current.remove(&edge_id);
                counts.edges_kept += 1;
                continue;
            }
            let symbol = site
                .site
                .caller
                .as_deref()
                .expect("sites without a caller have no edges");
            let caller = match callers.get(&(site.file, symbol)) {
                Some(caller) => caller.clone(),
                None => {
                    let caller = node(transaction, site.file, symbol)?;
                    callers.insert((site.file, symbol), caller.clone());
                    caller
                }
            };
            let Some(caller) = caller else {
                // The row-by-row writer emits no edges for a site whose caller
                // has no node; a current row with this ID is stale.
                continue;
            };
            let target_node = node(transaction, &target.file, &target.symbol)?;
            current.remove(&edge_id);
            counts.edges_written += write.execute(params![
                edge_id,
                site.ref_id,
                caller,
                target_node,
                target.file,
                target.symbol,
                site.site.line,
                target.provenance
            ])?;
        }
        let mut delete = transaction.prepare("DELETE FROM edges WHERE edge_id=?1")?;
        for edge_id in current.keys() {
            counts.edges_deleted += delete.execute([edge_id])?;
        }
    }
    Ok(counts)
}

fn node(transaction: &Transaction<'_>, file: &str, symbol: &str) -> Result<Option<String>> {
    transaction
        .query_row(
            "SELECT id FROM nodes WHERE file_path=?1 AND scoped_name=?2",
            params![file, symbol],
            |r| r.get(0),
        )
        .optional()
        .map_err(Into::into)
}

/// Persisted counts include explicit zero rows for languages with no syntactic
/// dynamic member access. Reflection does not change these counts.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SiteCounts {
    pub unresolved_receiver_sites: usize,
    pub dynamic_member_sites: usize,
    pub external: usize,
}

pub fn counts(connection: &rusqlite::Connection) -> rusqlite::Result<BTreeMap<String, SiteCounts>> {
    let mut result = [
        "python",
        "javascript",
        "typescript",
        "rust",
        "go",
        "java",
        "csharp",
        "kotlin",
    ]
    .into_iter()
    .map(|l| (l.to_string(), SiteCounts::default()))
    .collect::<BTreeMap<_, _>>();
    let mut query = connection.prepare("SELECT language, SUM(unresolved_receiver_sites), SUM(dynamic_member_sites), SUM(external) FROM view_dispatch_sites GROUP BY language")?;
    for row in query.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            SiteCounts {
                unresolved_receiver_sites: r.get(1)?,
                dynamic_member_sites: r.get(2)?,
                external: r.get(3)?,
            },
        ))
    })? {
        let (language, counts) = row?;
        result.insert(language, counts);
    }
    Ok(result)
}
