//! Exercise actual store writers rather than reproducing their creation logic.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

#[test]
fn storage_creation_walk_is_owner_only() {
    let scratch = tempfile::tempdir().unwrap();
    let storage = scratch.path().join("storage");
    let project = scratch.path().join("project");
    fs::create_dir(&project).unwrap();
    let source = project.join("main.rs");
    fs::write(&source, "fn main() { leaf(); }\nfn leaf() {}\n").unwrap();
    let key = crate::search_index::artifact_cache_key(&project);
    crate::root_cache::configure_artifact_access(&project, &key, false);

    let db = crate::db::open(&storage.join("aft.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE private_test(value TEXT); INSERT INTO private_test VALUES ('secret');",
    )
    .unwrap();
    let harness = storage.join("opencode");
    let task =
        crate::bash_background::persistence::allocate_task_layout(&harness, "session").unwrap();
    task.dirs
        .io
        .open_new_file(std::ffi::OsStr::new("stdout"))
        .unwrap();

    let mut backups = crate::backup::BackupStore::new();
    backups.set_storage_dir(harness.clone(), 72);
    assert!(backups
        .snapshot("session", &source, "snapshot")
        .unwrap()
        .is_some());
    let mut checkpoints = crate::checkpoint::CheckpointStore::new();
    checkpoints.set_storage_dir_for_harness(storage.clone(), crate::harness::Harness::Opencode);
    checkpoints
        .create_for_files("session", "snapshot", vec![source.clone()])
        .unwrap();

    let (callgraph, _) = crate::callgraph_store::CallGraphStore::cold_build_with_lease(
        storage.join("callgraph").join(&key),
        project.clone(),
        std::slice::from_ref(&source),
    )
    .unwrap();
    let mut search = crate::search_index::SearchIndex::build(&project);
    assert!(search.write_to_disk(&storage.join("index").join(&key), None));
    let semantic = crate::semantic_index::SemanticIndex::new(project, 3);
    assert!(semantic.write_to_disk(&storage, &key));
    crate::gh_shim::write_storage_permission_fixture(&storage.join("state"));
    crate::logging::write_storage_permission_fixture(&storage.join("logs"));
    crate::agent_child_env::write_storage_permission_fixture(&storage);
    let view = crate::views::ViewStore::open(&storage, &key).unwrap();
    let blobs =
        crate::blob_store::BlobStore::open(&storage, &key, crate::blob_store::BlobPlane::Semantic)
            .unwrap();
    let inspect = crate::inspect::cache::InspectCache::open(
        storage.join("inspect"),
        source.parent().unwrap().to_path_buf(),
    )
    .unwrap();

    let mut paths = vec![storage];
    let mut checked = 0;
    while let Some(path) = paths.pop() {
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert_eq!(
            metadata.permissions().mode() & 0o077,
            0,
            "{} has group/world bits: {:o}",
            path.display(),
            metadata.permissions().mode() & 0o777
        );
        checked += 1;
        if metadata.is_dir() {
            paths.extend(
                fs::read_dir(path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path()),
            );
        }
    }
    assert!(checked > 25, "walk must reach all stores: {checked}");
    drop((callgraph, db, view, blobs, inspect));
}

#[test]
fn storage_open_tightens_existing_root_without_changing_parent() {
    let scratch = tempfile::tempdir().unwrap();
    let parent = scratch.path().join("public-parent");
    let storage = parent.join("storage");
    let cache = storage.join("semantic").join("root-key");
    fs::create_dir_all(&cache).unwrap();
    for path in [&parent, &storage, &storage.join("semantic"), &cache] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let old = cache.join("old.bin");
    fs::write(&old, "old secret").unwrap();
    fs::set_permissions(&old, fs::Permissions::from_mode(0o644)).unwrap();
    let project = scratch.path().join("project");
    fs::create_dir(&project).unwrap();
    let semantic = crate::semantic_index::SemanticIndex::new(project, 3);
    assert!(semantic.write_to_disk(&storage, "root-key"));
    assert_mode(&cache, 0o700);
    assert_mode(&storage, 0o700);
    assert_mode(&storage.join("semantic"), 0o700);
    assert_mode(&old, 0o644);
    assert_mode(&parent, 0o755);
}

#[test]
fn storage_open_does_not_chmod_symlink_targets() {
    let scratch = tempfile::tempdir().unwrap();
    let target = scratch.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    let storage = scratch.path().join("storage");
    fs::create_dir_all(storage.join("semantic")).unwrap();
    std::os::unix::fs::symlink(&target, storage.join("semantic/root-key")).unwrap();
    let project = scratch.path().join("project");
    fs::create_dir(&project).unwrap();
    let semantic = crate::semantic_index::SemanticIndex::new(project, 3);
    assert!(semantic.write_to_disk(&storage, "root-key"));
    assert_mode(&target, 0o755);
    let nested = target.join("nested");
    fs::create_dir(&nested).unwrap();
    fs::set_permissions(&nested, fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(&target, storage.join("alias")).unwrap();
    crate::private_storage::open_dir(&storage, &storage.join("alias/nested")).unwrap();
    assert_mode(&nested, 0o755);
}

#[test]
fn storage_debug_archive_ignores_public_member_modes() {
    let scratch = tempfile::tempdir().unwrap();
    let archive = scratch.path().join("debug.zip");
    write_zip_member_with_mode(&archive, "secret.txt", b"debug data", 0o100644);
    assert!(
        which::which("unzip").is_ok(),
        "required executable `unzip` is missing; debug archive extraction uses it"
    );
    let destination = scratch.path().join("storage/dsym/download");
    crate::private_storage::extract_zip(&archive, &destination).unwrap();
    assert_mode(&destination, 0o700);
    assert_mode(&destination.join("secret.txt"), 0o600);
    assert_eq!(
        fs::read_to_string(destination.join("secret.txt")).unwrap(),
        "debug data"
    );
}

#[test]
fn storage_existing_database_new_sidecars_are_private() {
    let scratch = tempfile::tempdir().unwrap();
    let storage = scratch.path().canonicalize().unwrap().join("storage");
    let path = storage.join("aft.db");
    drop(crate::db::open(&path).unwrap());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let db = crate::db::open(&path).unwrap();
    db.execute_batch(
        "CREATE TABLE private_test(value TEXT); INSERT INTO private_test VALUES ('secret');",
    )
    .unwrap();
    for path in [
        &path,
        &storage.join("aft.db-wal"),
        &storage.join("aft.db-shm"),
    ] {
        assert_mode(path, 0o600);
    }
}

// Build a stored ZIP member directly so this fixture needs no `zip` executable.
fn write_zip_member_with_mode(path: &Path, name: &str, contents: &[u8], mode: u32) {
    let name = name.as_bytes();
    let name_len = u16::try_from(name.len()).unwrap();
    let contents_len = u32::try_from(contents.len()).unwrap();
    let crc = crc32fast::hash(contents);
    let local_size = 30 + name.len() + contents.len();
    let central_size = 46 + name.len();
    let mut archive = Vec::with_capacity(local_size + central_size + 22);

    archive.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
    archive.extend_from_slice(&20u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&crc.to_le_bytes());
    archive.extend_from_slice(&contents_len.to_le_bytes());
    archive.extend_from_slice(&contents_len.to_le_bytes());
    archive.extend_from_slice(&name_len.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(name);
    archive.extend_from_slice(contents);

    archive.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
    archive.extend_from_slice(&0x0314u16.to_le_bytes());
    archive.extend_from_slice(&20u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&crc.to_le_bytes());
    archive.extend_from_slice(&contents_len.to_le_bytes());
    archive.extend_from_slice(&contents_len.to_le_bytes());
    archive.extend_from_slice(&name_len.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&(mode << 16).to_le_bytes());
    archive.extend_from_slice(&0u32.to_le_bytes());
    archive.extend_from_slice(name);

    archive.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&1u16.to_le_bytes());
    archive.extend_from_slice(&1u16.to_le_bytes());
    archive.extend_from_slice(&(central_size as u32).to_le_bytes());
    archive.extend_from_slice(&(local_size as u32).to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());

    fs::write(path, archive).unwrap();
}

fn assert_mode(path: &Path, mode: u32) {
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        mode,
        "{}",
        path.display()
    );
}
