//! Root-local status push projection; explicit status reads keep their full census.
use crate::context::AppContext;
use serde_json::Value;

const REFUSAL_DETAILS: usize = 8;

pub(crate) fn project(ctx: &AppContext, mut snapshot: Value) -> Value {
    let Some(object) = snapshot.as_object_mut() else {
        return snapshot;
    };
    // Process-wide scalars retain their meaning; cross-root detail lists never enter a push.
    object.retain(|key, _| {
        matches!(
            key.as_str(),
            "version"
                | "project_root"
                | "canonical_root"
                | "cache_role"
                | "degraded"
                | "degraded_reasons"
                | "message"
                | "config_error"
                | "features"
                | "search_index"
                | "semantic_index"
                | "semantic"
                | "semantic_config"
                | "disk"
                | "symbol_cache"
                | "runtime"
                | "lsp_servers"
                | "storage_dir"
                | "checkpoints_total"
                | "compression"
                | "status_bar"
                | "status_bar_values"
                | "session"
                | "views"
                | "watcher"
                | "artifact_owner"
                | "callgraph_refresh"
                | "lsp"
        )
    });
    if let Some(runtime) = object.get_mut("runtime").and_then(Value::as_object_mut) {
        runtime.retain(|key, _| {
            matches!(
                key.as_str(),
                "live_watchers" | "live_actor_roots" | "open_routes"
            )
        });
    }
    let config = ctx.config();
    let root = config.project_root.clone().unwrap_or_default();
    let normalized = crate::inspect::job::canonicalize_normalized(&root)
        .display()
        .to_string();
    let own = object
        .get("lsp")
        .and_then(|lsp| lsp["applicability_by_root"].get(&normalized))
        .cloned();
    let mut roots = serde_json::Map::new();
    if let Some(own) = own {
        roots.insert(normalized, own);
    }
    object.insert(
        "lsp".into(),
        serde_json::json!({"applicability_by_root":roots}),
    );

    let storage = crate::bash_background::storage_dir(config.storage_dir.as_deref());
    let canonical = ctx
        .canonical_cache_root_opt()
        .unwrap_or_else(|| root.clone());
    let key = ctx.cached_artifact_cache_key(&canonical);
    let (owned, unattributed) =
        crate::persisted_format::refusals_for_push(&storage, key.as_deref());
    let total = owned.len();
    let mut reasons: Vec<Value> = object
        .get("degraded_reasons")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|reason| {
            !reason
                .as_str()
                .is_some_and(|reason| reason.starts_with(crate::persisted_format::CODE))
        })
        .cloned()
        .collect();
    for refusal in &owned {
        if refusal.store == crate::persisted_format::PersistedStore::BashTask
            && refusal.source == crate::persisted_format::RefusalSource::Artifact
        {
            continue;
        }
        let reason = serde_json::json!(refusal.reason());
        if !reasons.contains(&reason) {
            reasons.push(reason);
        }
    }
    object.insert("degraded".into(), serde_json::json!(!reasons.is_empty()));
    object.insert("degraded_reasons".into(), Value::Array(reasons));
    let details: Vec<_> = owned
        .iter()
        .take(REFUSAL_DETAILS)
        .map(|refusal| {
            serde_json::json!({
                "code":crate::persisted_format::CODE,"store":refusal.store.name(),
                "path":refusal.path.display().to_string(),"found":refusal.found,
                "supported":refusal.supported,"message":refusal.to_string(),
            })
        })
        .collect();
    let omitted = total - details.len();
    object.insert("storage_refusals".into(), Value::Array(details));
    object.insert("storage_refusals_total".into(), serde_json::json!(total));
    object.insert(
        "storage_refusals_omitted".into(),
        serde_json::json!(omitted),
    );
    object.insert(
        "storage_refusals_unattributed".into(),
        serde_json::json!(unattributed),
    );
    object.insert(
        "push_details".into(),
        serde_json::json!({"omitted":omitted>0 || unattributed>0,"read":"status"}),
    );
    snapshot
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::bash_background::push_projection_fixture::busy_root;
    use crate::persisted_format::{self, PersistedStore, UnsupportedPersistedFormat};
    use crate::protocol::StatusChangedFrame;
    use std::path::PathBuf;

    #[test]
    fn status_push_busy_projection_is_root_local_and_small() {
        let fixture = busy_root();
        let full = fixture.ctx.build_status_snapshot();
        assert_eq!(fixture.contexts.len(), 192);
        assert_eq!(fixture.app.open_route_count(), 90);
        assert_eq!(
            full["lsp"]["applicability_by_root"]
                .as_object()
                .unwrap()
                .len(),
            192
        );
        let frame = StatusChangedFrame::new(None, project(&fixture.ctx, full));
        let body = serde_json::to_vec(&frame).unwrap();
        assert!(
            body.len() < 8 * 1024,
            "busy projected body must be comfortably below 16 KiB, got {}",
            body.len()
        );
        let roots = frame.snapshot["lsp"]["applicability_by_root"]
            .as_object()
            .unwrap();
        assert_eq!(roots.len(), 1);
        assert!(roots.contains_key(&fixture.root.display().to_string()));
        for field in ["memory", "sqlite", "threads", "children", "fds"] {
            assert!(
                frame.snapshot.get(field).is_none(),
                "process census {field} must remain in explicit reads only"
            );
        }
    }

    #[test]
    fn status_push_does_not_change_full_explicit_status_read() {
        let fixture = busy_root();
        let first = fixture.ctx.build_status_snapshot();
        let pushed = project(&fixture.ctx, first.clone());
        let after = fixture.ctx.build_status_snapshot();
        assert_eq!(
            after["lsp"]["applicability_by_root"],
            first["lsp"]["applicability_by_root"]
        );
        assert_eq!(
            after["memory"]["roots_total"],
            first["memory"]["roots_total"]
        );
        assert_eq!(after["memory"]["roots"].as_object().unwrap().len(), 8);
        assert_eq!(
            pushed["lsp"]["applicability_by_root"]
                .as_object()
                .unwrap()
                .len(),
            1,
            "explicit census must not leak into push"
        );
        assert_eq!(
            after["lsp"]["applicability_by_root"]
                .as_object()
                .unwrap()
                .len(),
            192
        );
    }

    struct Refusals(Vec<(PersistedStore, PathBuf)>);
    impl Drop for Refusals {
        fn drop(&mut self) {
            for (store, path) in &self.0 {
                persisted_format::clear(*store, path);
            }
        }
    }

    #[test]
    fn status_push_refusals_preserve_owned_totals_and_unattributed_signal() {
        let fixture = busy_root();
        let mut guard = Refusals(Vec::new());
        for (key, count, store) in [
            (fixture.key.as_str(), 12, PersistedStore::SearchIndex),
            ("another-project", 40, PersistedStore::SearchIndex),
            ("unknown", 5, PersistedStore::BashTask),
        ] {
            for i in 0..count {
                let plane = if store == PersistedStore::BashTask {
                    "bash"
                } else {
                    "index"
                };
                let path = fixture
                    .storage
                    .join(plane)
                    .join(key)
                    .join(format!("artifact-{i}.bin"));
                let refusal = UnsupportedPersistedFormat::artifact(store, &path, 99);
                persisted_format::record(&refusal, &path);
                guard.0.push((store, path));
            }
        }
        let full = fixture.ctx.build_status_snapshot();
        assert_eq!(full["storage_refusals"].as_array().unwrap().len(), 57);
        let pushed = project(&fixture.ctx, full);
        assert_eq!(pushed["storage_refusals_total"], 12);
        assert_eq!(pushed["storage_refusals_unattributed"], 5);
        let shown = pushed["storage_refusals"].as_array().unwrap();
        assert!(!shown.is_empty() && shown.len() < 12);
        assert_eq!(
            pushed["storage_refusals_omitted"].as_u64().unwrap(),
            12 - shown.len() as u64
        );
        assert!(shown
            .iter()
            .all(|row| row["path"].as_str().unwrap().contains(&fixture.key)));
        assert_eq!(pushed["push_details"]["read"], "status");
        assert_eq!(pushed["degraded"], true);
        assert_eq!(
            fixture.ctx.build_status_snapshot()["storage_refusals"]
                .as_array()
                .unwrap()
                .len(),
            57
        );
    }

    #[test]
    fn status_push_consumer_field_inventory_is_preserved() {
        let fixture = busy_root();
        let mut full = fixture.ctx.build_status_snapshot();
        let fields: Vec<String> = serde_json::from_str(include_str!(
            "../tests/fixtures/status_push_consumer_fields.json"
        ))
        .unwrap();
        // Populate missing optional, error and legacy fields so the inventory
        // test verifies that the projection preserves them too.
        for field in &fields {
            if full.pointer(field).is_none() {
                let mut cursor = &mut full;
                let parts: Vec<_> = field.trim_start_matches('/').split('/').collect();
                for part in &parts[..parts.len() - 1] {
                    if !cursor.is_object() {
                        *cursor = serde_json::json!({});
                    }
                    cursor = cursor
                        .as_object_mut()
                        .unwrap()
                        .entry((*part).to_owned())
                        .or_insert(Value::Null);
                }
                if !cursor.is_object() {
                    *cursor = serde_json::json!({});
                }
                cursor[parts[parts.len() - 1]] = serde_json::json!(format!("inventory:{field}"));
            }
        }
        let frame = StatusChangedFrame::new(None, project(&fixture.ctx, full.clone()));
        for field in fields {
            assert_eq!(
                frame.snapshot.pointer(&field),
                Some(
                    full.pointer(&field)
                        .expect("all inventory fields populated")
                ),
                "consumer reads {field}"
            );
        }
        assert!(frame.snapshot.get("session").is_none());
        assert!(frame.snapshot["compression"].get("session").is_none());
    }

    #[test]
    fn status_push_foreign_refusal_does_not_degrade_root() {
        let fixture = busy_root();
        let path = fixture
            .storage
            .join("index")
            .join("foreign-only")
            .join("cache.bin");
        let refusal = UnsupportedPersistedFormat::artifact(PersistedStore::SearchIndex, &path, 99);
        persisted_format::record(&refusal, &path);
        let _guard = Refusals(vec![(PersistedStore::SearchIndex, path)]);
        let full = fixture.ctx.build_status_snapshot();
        assert_eq!(full["degraded"], true);
        let pushed = project(&fixture.ctx, full);
        assert_eq!(pushed["degraded"], false);
        assert_eq!(pushed["storage_refusals_total"], 0);
        assert_eq!(pushed["storage_refusals_unattributed"], 0);
    }

    #[test]
    fn status_push_shared_storage_and_cached_artifact_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path();
        let mut guard = Refusals(Vec::new());
        for (store, path) in [
            (
                PersistedStore::SemanticIndex,
                storage.join("semantic/shared-key/semantic.bin"),
            ),
            (
                PersistedStore::SearchIndex,
                storage.join("index/other-key/cache.bin"),
            ),
            (
                PersistedStore::BashTask,
                storage.join("bash/unknown/task.json"),
            ),
            (PersistedStore::AftDb, storage.join("aft.db")),
        ] {
            persisted_format::record(
                &UnsupportedPersistedFormat::artifact(store, &path, 99),
                &path,
            );
            guard.0.push((store, path));
        }
        persisted_format::record(
            &UnsupportedPersistedFormat::floor(
                PersistedStore::AftDb,
                storage.join("reader-floor.json"),
                99,
            ),
            storage,
        );
        let (own, unknown) = persisted_format::refusals_for_push(storage, Some("shared-key"));
        assert_eq!(own.len(), 3);
        assert_eq!(unknown, 1);
        assert!(!own
            .iter()
            .any(|refusal| refusal.path.to_string_lossy().contains("other-key")));
        let (worktree, _) = persisted_format::refusals_for_push(storage, Some("shared-key"));
        assert_eq!(
            worktree, own,
            "repository artifacts can be shared by worktrees, not guessed from task session hashes"
        );
        let (uncached, unknown) = persisted_format::refusals_for_push(storage, None);
        assert_eq!(uncached.len(), 2);
        assert_eq!(unknown,3,"uncached artifact ownership remains unknown rather than deriving a key on the push path");
    }
}
