use super::location::remove_inactive_cache_roots;
use super::prepare_cache_root_for_test as prepare_cache_root;
use super::*;
use crate::config::Config;
use crate::context::AppContext;
use crate::parser::{SymbolCache, TreeSitterProvider};

#[cfg(unix)]
#[test]
fn cache_descendant_symlink_cannot_create_a_view_in_durable_state() {
    let (_temp, storage, cache, _project, _ctx) = fixture(true);
    let durable = storage.join("checkpoints");
    fs::create_dir_all(&durable).unwrap();
    std::os::unix::fs::symlink(&durable, cache.join("views")).unwrap();
    let result = crate::views::ViewStore::open(&cache, "new-scope");
    assert!(
        !durable.join("new-scope").exists(),
        "a cache descendant symlink created durable cache data"
    );
    assert!(
        result.is_err(),
        "the symlinked cache domain must be refused"
    );
}

#[cfg(unix)]
#[test]
fn cache_file_creation_keeps_the_open_parent_after_ancestor_retarget() {
    use std::io::Write;
    let (temp, storage, cache, _project, _ctx) = fixture(true);
    let directory = cache.join("views/scope");
    crate::private_storage::create_dir_all(&directory).unwrap();
    let parent = crate::root_cache::pin_cache_path(&directory.join("payload"))
        .unwrap()
        .unwrap()
        .parent()
        .unwrap()
        .0;
    let durable = storage.join("checkpoints");
    fs::create_dir_all(durable.join("scope")).unwrap();
    let parked = temp.path().join("parked-views");
    fs::rename(cache.join("views"), &parked).unwrap();
    std::os::unix::fs::symlink(&durable, cache.join("views")).unwrap();
    parent
        .repair_create_file(std::ffi::OsStr::new("payload"))
        .unwrap()
        .write_all(b"cache")
        .unwrap();
    assert!(
        !durable.join("scope/payload").exists(),
        "cache creation followed an ancestor after its parent was pinned"
    );
    assert_eq!(fs::read(parked.join("scope/payload")).unwrap(), b"cache");
}

#[test]
fn cache_root_missing_is_unavailable_without_creating_anything() {
    let temp = tempfile::tempdir().unwrap();
    let missing = temp.path().join("offline-volume");
    let storage = temp.path().join("aft");
    let mut config = Config {
        storage_dir: Some(storage.clone()),
        cache_dir: Some(missing.clone()),
        ..Default::default()
    };
    let warning = prepare_cache_root(&mut config);
    assert!(
        !missing.exists(),
        "validation must never create the configured cache root"
    );
    assert!(
        !storage.exists(),
        "unavailable cache validation must not write durable state"
    );
    assert_eq!(warning.unwrap().code, "cache_dir_unavailable");
    assert_eq!(config.cache_dir, Some(missing));
}

#[test]
fn cache_root_unavailable_refuses_inspect_lane_before_open() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    fs::create_dir(&project).unwrap();
    let storage = temp.path().join("storage");
    let missing = temp.path().join("missing");
    let mut config = Config {
        storage_dir: Some(storage.clone()),
        cache_dir: Some(missing.clone()),
        ..Default::default()
    };
    assert!(prepare_cache_root(&mut config).is_some());
    let key = crate::path_identity::project_scope_key(&project);
    configure_artifact_access(
        &project,
        &crate::search_index::artifact_cache_key(&project),
        false,
    );
    let result = crate::inspect::cache::InspectCache::open(
        cache_root(&config).join("inspect").join(key),
        project,
    );
    assert!(
        result.is_err(),
        "an unavailable cache lane must not open or build an empty healthy cache"
    );
    assert!(result
        .err()
        .unwrap()
        .to_string()
        .contains(CACHE_DIR_UNAVAILABLE));
    assert!(!missing.exists());
    assert!(!storage.exists());
}

#[cfg(unix)]
#[test]
fn cache_root_unavailable_rejects_dangling_and_retargeting_symlinks() {
    // A configured cache_dir may be a symlink the user chose. It is resolved
    // once when first used and pinned by identity: a dangling link is
    // refused, and so is the same spelling pointing somewhere else later.
    let temp = tempfile::tempdir().unwrap();
    let link = temp.path().join("link");
    let target = temp.path().join("target");
    let other = temp.path().join("other");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let config_for = || Config {
        storage_dir: Some(temp.path().join("storage")),
        cache_dir: Some(link.clone()),
        ..Default::default()
    };
    let mut config = config_for();
    assert!(prepare_cache_root(&mut config).is_some());
    assert!(cache_unavailable(&config).is_some());
    assert!(!target.join("aft").exists());

    fs::create_dir(&target).unwrap();
    let mut config = config_for();
    let warning = prepare_cache_root(&mut config);
    assert!(warning.is_none(), "{warning:?}");
    assert!(target.join("aft").is_dir());
    // Paths built from the configured spelling are admitted too.
    crate::private_storage::create_dir_all(link.join("aft/index/through-link")).unwrap();
    assert!(target.join("aft/index/through-link").is_dir());

    fs::create_dir(&other).unwrap();
    fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&other, &link).unwrap();
    let mut config = config_for();
    let warning = prepare_cache_root(&mut config).expect("a retargeted link must be refused");
    assert!(
        warning.message.contains("not the volume it was"),
        "{warning:?}"
    );
    assert!(cache_unavailable(&config).is_some());
    assert!(!other.join("aft").exists());
}

fn fixture(custom: bool) -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf, AppContext) {
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path().join("aft");
    let cache = if custom {
        temp.path().join("cache")
    } else {
        storage.clone()
    };
    if custom {
        fs::create_dir(&cache).unwrap();
    }
    let project = temp.path().join("project");
    fs::create_dir(&project).unwrap();
    fs::write(
        project.join("main.rs"),
        "fn main() { leaf(); }\nfn leaf() {}\n",
    )
    .unwrap();
    let project = fs::canonicalize(project).unwrap();
    let mut config = Config {
        storage_dir: Some(storage.clone()),
        cache_dir: custom.then(|| cache.clone()),
        project_root: Some(project.clone()),
        ..Config::default()
    };
    assert!(prepare_cache_root(&mut config).is_none());
    let cache = cache_root(&config);
    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), config);
    ctx.set_canonical_cache_root(project.clone());
    let key = ctx.memoized_artifact_cache_key(&project);
    configure_artifact_access(&project, &key, false);
    (temp, storage, cache, project, ctx)
}

#[test]
fn cache_root_unset_preserves_path_golden() {
    let (_temp, storage, cache, project, ctx) = fixture(false);
    let key = ctx.memoized_artifact_cache_key(&project);
    let scope = crate::path_identity::project_scope_key(&project);
    let paths = [
        ctx.inspect_dir(),
        ctx.callgraph_store_dir(),
        crate::search_index::resolve_cache_dir_with_key(
            &key,
            ctx.config().cache_storage_dir().as_deref(),
        ),
        crate::symbol_cache_disk::cache_path(&cache, &key),
        crate::views::ViewStore::open(&cache, &scope)
            .unwrap()
            .view_dir()
            .to_path_buf(),
        crate::views::registry::view_dir(&cache, &scope).unwrap(),
        crate::blob_store::v2::family_dir(&cache, &key).unwrap(),
    ];
    assert_eq!(
        paths,
        [
            storage.join("inspect").join(&scope),
            storage.join("callgraph").join(&key),
            storage.join("index").join(&key),
            storage.join("symbols").join(&key).join("symbols.bin"),
            storage.join("views").join(&scope),
            storage.join("views/v2").join(&scope),
            storage.join("blobs/v2").join(&key),
        ]
    );
    assert_eq!(cache, storage);
    assert!(crate::semantic_index::SemanticIndex::new(project, 3).write_to_disk(&cache, &key));
    assert!(storage
        .join("semantic")
        .join(key)
        .join("semantic.bin")
        .is_file());
}

#[test]
fn cache_root_routes_all_real_writers_and_keeps_metadata_durable() {
    let (_temp, storage, cache, project, ctx) = fixture(true);
    let key = ctx.memoized_artifact_cache_key(&project);
    let scope = crate::path_identity::project_scope_key(&project);
    let config = ctx.config();
    let artifact_root = config.cache_storage_dir().unwrap();
    let mut search = crate::search_index::SearchIndex::build(&project);
    let search_dir = crate::search_index::resolve_cache_dir_with_key(&key, Some(&artifact_root));
    assert!(search.write_to_disk(&search_dir, None));
    let semantic = crate::semantic_index::SemanticIndex::new(project.clone(), 3);
    assert!(semantic.write_to_disk(&artifact_root, &key));
    let mut symbols = SymbolCache::new();
    symbols.set_project_root(project.clone());
    crate::symbol_cache_disk::write_to_disk(&symbols, &artifact_root, &key).unwrap();
    let (graph, _) = crate::callgraph_store::CallGraphStore::cold_build_with_lease(
        ctx.callgraph_store_dir(),
        project.clone(),
        &[project.join("main.rs")],
    )
    .unwrap();
    let inspect =
        crate::inspect::cache::InspectCache::open(ctx.inspect_dir(), project.clone()).unwrap();
    let view = crate::views::ViewStore::open(&artifact_root, &scope).unwrap();
    let blobs = crate::blob_store::BlobStore::open(
        &artifact_root,
        &key,
        crate::blob_store::BlobPlane::Semantic,
    )
    .unwrap();
    let registry = crate::views::registry::FamilyRegistry::open(&artifact_root, &key).unwrap();
    assert!(view.view_dir().starts_with(cache.join("views")));
    assert!(crate::blob_store::v2::family_dir(&artifact_root, &key)
        .unwrap()
        .starts_with(cache.join("blobs")));
    for domain in CACHE_DOMAINS {
        assert!(
            cache.join(domain).is_dir(),
            "missing cache domain: {domain}"
        );
        assert!(
            !storage.join(domain).exists(),
            "cache domain leaked into durable storage: {domain}"
        );
    }
    crate::storage_retention::record_bind(
        &StoragePaths::new(storage.clone(), artifact_root.clone()),
        &project,
        &key,
    )
    .unwrap();
    let owner = crate::artifact_owner::claim_or_open_read_only(
        Some(&storage),
        &project,
        &key,
        &scope,
        false,
        None,
    )
    .unwrap();
    assert!(Path::new(&owner.status.manifest_path).starts_with(storage.join("artifact-owners")));
    assert!(storage
        .join("retention/roots")
        .join(format!("{scope}.json"))
        .is_file());
    crate::reader_floor::raise(
        &storage,
        &[(crate::persisted_format::PersistedStore::SearchIndex, 1)],
    )
    .unwrap();
    assert!(storage.join("reader-floor.json").is_file());
    for name in [
        "retention",
        "artifact-owners",
        "reader-floor.json",
        "reader-floor.lock",
        "cache-keys.json",
        "aft.db",
    ] {
        assert!(
            !cache.join(name).exists(),
            "durable state leaked into cache root: {name}"
        );
    }
    assert_eq!(
        config.semantic.local_cache_dir,
        Some(cache.join("semantic/models"))
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&cache).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    drop((graph, inspect, view, blobs, registry, owner));
}

#[test]
fn cache_root_unavailable_preserves_requested_location_without_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("file");
    fs::write(&file, "not a directory").unwrap();
    for requested in [file, PathBuf::from("relative")] {
        let storage = temp.path().join("aft");
        let mut config = Config {
            storage_dir: Some(storage.clone()),
            cache_dir: Some(requested.clone()),
            ..Default::default()
        };
        let warning = prepare_cache_root(&mut config).expect("typed warning");
        assert_eq!(warning.code, CACHE_DIR_UNAVAILABLE);
        assert_eq!(warning.key, "storage.cache_dir");
        assert!(warning
            .message
            .contains("AFT is running without its caches until it is back."));
        assert_eq!(cache_root(&config), requested.join("aft"));
        assert!(cache_unavailable(&config).is_some());
        assert!(!storage.exists());
    }
}

#[test]
fn cache_root_change_is_fresh_and_doctor_reclaims_only_marked_inactive_domains() {
    let (_temp, storage, old, _project, ctx) = fixture(true);
    for root in [&storage, &old] {
        for domain in CACHE_DOMAINS {
            fs::create_dir_all(root.join(domain)).unwrap();
            fs::write(root.join(domain).join("canary"), "rebuildable").unwrap();
        }
    }
    fs::write(old.join("index/old-artifact"), "before root change").unwrap();
    let new_parent = old.parent().unwrap().with_file_name("new-cache");
    fs::create_dir(&new_parent).unwrap();
    let mut config = Config {
        storage_dir: Some(storage.clone()),
        cache_dir: Some(new_parent),
        ..Default::default()
    };
    let warning = prepare_cache_root(&mut config);
    assert!(warning.is_none(), "{warning:?}");
    let new = cache_root(&config);
    for root in [&new] {
        for domain in CACHE_DOMAINS {
            fs::create_dir_all(root.join(domain)).unwrap();
            fs::write(root.join(domain).join("canary"), "rebuildable").unwrap();
        }
    }
    for name in [
        "aft.db",
        "aft.db-wal",
        "aft.db-shm",
        "engram-catalog.json",
        "trusted-filter-projects.json",
    ] {
        fs::write(storage.join(name), "durable").unwrap();
    }
    for domain in [
        "opencode/backups",
        "opencode/checkpoints",
        "opencode/bash-tasks",
        "shims",
        "artifact-owners",
        "dsym",
        "logs",
    ] {
        fs::create_dir_all(storage.join(domain)).unwrap();
        fs::write(storage.join(domain).join("canary"), "durable").unwrap();
    }
    assert!(
        old.join("index/canary").is_file(),
        "startup must not delete old caches"
    );
    assert!(
        !new.join("index/old-artifact").exists(),
        "startup must not migrate caches"
    );
    drop(ctx);
    let report = remove_inactive_cache_roots(&storage, &[new.clone()]);
    assert!(report.error.is_none(), "{report:?}");
    assert_eq!(report.removed.len(), CACHE_DOMAINS.len());
    for domain in CACHE_DOMAINS {
        assert!(!old.join(domain).exists());
        assert!(storage.join(domain).join("canary").is_file());
        assert!(new.join(domain).join("canary").is_file());
    }
    assert_eq!(
        fs::read_to_string(storage.join("aft.db")).unwrap(),
        "durable"
    );
    assert!(storage.join("opencode/backups/canary").is_file());
    assert!(storage.join("artifact-owners/canary").is_file());
    assert!(old.is_dir());
    assert!(storage.is_dir());
}

#[test]
fn cache_root_doctor_keeps_in_use_root_and_active_alias() {
    let (_temp, storage, cache, _project, ctx) = fixture(true);
    fs::create_dir_all(cache.join("views")).unwrap();
    fs::write(cache.join("views/canary"), "live").unwrap();
    let in_use = remove_inactive_cache_roots(&storage, &[]);
    assert!(
        in_use
            .error
            .as_deref()
            .is_some_and(|error| error.contains("in use")),
        "{in_use:?}"
    );
    assert!(cache.join("views/canary").is_file());
    drop(ctx);
    let alias = cache.join("..").join(cache.file_name().unwrap());
    let report = remove_inactive_cache_roots(&storage, &[alias]);
    assert!(report.error.is_none(), "{report:?}");
    assert!(cache.join("views/canary").is_file());
}

#[test]
fn cache_root_reader_floor_refusals_cover_external_artifacts() {
    let (_temp, storage, cache, _project, _ctx) = fixture(true);
    use crate::persisted_format::{self, PersistedStore, UnsupportedPersistedFormat};
    let refusal = UnsupportedPersistedFormat::floor(
        PersistedStore::SearchIndex,
        &storage.join("reader-floor.json"),
        999,
    );
    persisted_format::record(&refusal, &storage);
    crate::reader_floor::prepare_artifact_root(&storage, &cache);
    assert!(persisted_format::refusal_covering(
        PersistedStore::SearchIndex,
        &cache.join("index/key/cache.bin")
    )
    .is_some());
    assert!(persisted_format::gate(
        PersistedStore::SearchIndex,
        &cache.join("index/key/cache.bin"),
        &cache.join("index/key/cache.bin"),
        None
    )
    .is_err());
}

#[test]
fn cache_root_doctor_ignores_harness_redirects_and_keeps_user_base() {
    let (_temp, storage, cache, _project, ctx) = fixture(true);
    fs::create_dir_all(cache.join("index")).unwrap();
    fs::create_dir_all(storage.join("index")).unwrap();
    let parent = cache.parent().unwrap();
    let tiers = [crate::config_resolve::ConfigTier { tier: "user".into(), source: "user".into(), doc: serde_json::json!({
        "storage": {"cache_dir": parent}, "harnesses": {"pi": {"storage": {"cache_dir": "/unavailable-harness-root"}}}
    }).to_string() }];
    // Cleanup excludes live storage owners, including this fixture's context.
    drop(ctx);
    let report = reclaim_configured_inactive_caches(&tiers, &storage);
    assert!(report.error.is_none(), "{report:?}");
    assert!(cache.join("index").is_dir());
    assert!(
        storage.join("index").is_dir(),
        "doctor leaves legacy cleanup to the explicit command"
    );
}
