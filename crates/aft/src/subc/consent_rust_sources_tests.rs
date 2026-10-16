use super::*;

const SOURCE: &str = "pub fn rust_library_marker() {}\n";

// Each child has its own daemon environment and once-resolved grant roots. Never
// change the environment of the parallel test process to test home overrides.
fn in_rust_home(test: &str, overrides: bool) -> bool {
    if std::env::var("AFT_RUST_SOURCES_TEST").as_deref() == Ok(test) {
        return true;
    }
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let cargo = if overrides {
        dir.path().join("cargo-real")
    } else {
        home.join(".cargo")
    };
    let rustup = if overrides {
        dir.path().join("rustup-real")
    } else {
        home.join(".rustup")
    };
    for (base, relative) in [
        (&cargo, "registry/src/example/lib.rs"),
        (&cargo, "credentials.toml"),
        (&cargo, "config.toml"),
        (&cargo, "registry/cache/example.crate"),
        (&cargo, "registry/index/config.json"),
        (
            &rustup,
            "toolchains/stable/lib/rustlib/src/rust/library/core/src/lib.rs",
        ),
        (&rustup, "settings.toml"),
    ] {
        let path = base.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, SOURCE).unwrap();
    }
    std::fs::write(dir.path().join("outside.rs"), "outside source\n").unwrap();
    std::os::unix::fs::symlink(
        dir.path().join("outside.rs"),
        cargo.join("registry/src/escape.rs"),
    )
    .unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child
        .arg(format!("subc::consent::tests::rust_sources::{test}"))
        .args(["--exact", "--nocapture"])
        .env("AFT_RUST_SOURCES_TEST", test)
        .env("HOME", &home)
        .env("AFT_TEST_CARGO_ROOT", &cargo)
        .env("AFT_TEST_RUSTUP_ROOT", &rustup)
        .env_remove("CARGO_HOME")
        .env_remove("RUSTUP_HOME");
    if overrides {
        let cargo_link = dir.path().join("cargo-link");
        let rustup_link = dir.path().join("rustup-link");
        std::os::unix::fs::symlink(&cargo, &cargo_link).unwrap();
        std::os::unix::fs::symlink(&rustup, &rustup_link).unwrap();
        child
            .env("CARGO_HOME", cargo_link)
            .env("RUSTUP_HOME", rustup_link);
    }
    let output = child.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{test}:\n{stdout}\n{stderr}");
    assert!(
        stdout.contains("1 passed"),
        "child must run exactly one test: {stdout}"
    );
    false
}

fn cargo_root() -> PathBuf {
    PathBuf::from(std::env::var_os("AFT_TEST_CARGO_ROOT").unwrap())
}

fn rustup_root() -> PathBuf {
    PathBuf::from(std::env::var_os("AFT_TEST_RUSTUP_ROOT").unwrap())
}

async fn reply(mut replies: mpsc::Receiver<WriterFrame>) -> Value {
    let frame = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .unwrap()
        .unwrap();
    serde_json::from_slice(&frame.body).unwrap()
}

fn rust_fixture() -> Fixture {
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(50)));
    f
}

fn assert_no_ask(f: &Fixture, response: &Value) {
    assert_eq!(code(response), "", "{response}");
    assert!(text(response).contains("rust_library_marker"), "{response}");
    assert!(f.fake.calls("consent.request").is_empty());
    assert!(f.asks().is_empty());
    assert!(
        f.worker_grants().is_empty(),
        "built-ins are not persisted consent grants"
    );
}

#[tokio::test]
async fn worker_registry_read_no_ask() {
    if !in_rust_home("worker_registry_read_no_ask", false) {
        return;
    }
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = rust_fixture();
    let response = f
        .read(
            &f.worker,
            &cargo_root().join("registry/src/example/lib.rs"),
            1,
        )
        .await;
    assert_no_ask(&f, &response);
}

#[tokio::test]
async fn worker_toolchain_grep_no_ask() {
    if !in_rust_home("worker_toolchain_grep_no_ask", false) {
        return;
    }
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = rust_fixture();
    let response = reply(
        f.send_tool(
            &f.worker,
            "grep",
            json!({
                "path": rustup_root().join("toolchains"), "pattern": "rust_library_marker",
            }),
            1,
            |request, ctx| crate::commands::grep::handle_grep(&request, ctx),
        )
        .await,
    )
    .await;
    assert_no_ask(&f, &response);
}

async fn cargo_read_asks(relative: &str) {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = rust_fixture();
    let response = f.read(&f.worker, &cargo_root().join(relative), 1).await;
    assert_eq!(code(&response), "access_pending", "{response}");
    assert_eq!(f.fake.calls("consent.request").len(), 1);
}

#[tokio::test]
async fn worker_cargo_credentials_still_ask() {
    if !in_rust_home("worker_cargo_credentials_still_ask", false) {
        return;
    }
    cargo_read_asks("credentials.toml").await;
}

#[tokio::test]
async fn worker_cargo_config_still_ask() {
    if !in_rust_home("worker_cargo_config_still_ask", false) {
        return;
    }
    cargo_read_asks("config.toml").await;
}

#[tokio::test]
async fn worker_cargo_cache_still_ask() {
    if !in_rust_home("worker_cargo_cache_still_ask", false) {
        return;
    }
    cargo_read_asks("registry/cache/example.crate").await;
}

#[tokio::test]
async fn worker_cargo_index_still_ask() {
    if !in_rust_home("worker_cargo_index_still_ask", false) {
        return;
    }
    cargo_read_asks("registry/index/config.json").await;
}

#[tokio::test]
async fn worker_rustup_settings_still_ask() {
    if !in_rust_home("worker_rustup_settings_still_ask", false) {
        return;
    }
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = rust_fixture();
    let response = f
        .read(&f.worker, &rustup_root().join("settings.toml"), 1)
        .await;
    assert_eq!(code(&response), "access_pending", "{response}");
    assert_eq!(f.fake.calls("consent.request").len(), 1);
}

#[tokio::test]
async fn worker_registry_write_still_ask() {
    if !in_rust_home("worker_registry_write_still_ask", false) {
        return;
    }
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = rust_fixture();
    let path = cargo_root().join("registry/src/example/lib.rs");
    let response = reply(
        f.send_tool(
            &f.worker,
            "write",
            json!({
                "filePath": path, "content": "changed\n",
            }),
            1,
            |request, ctx| crate::commands::write::handle_write(&request, ctx),
        )
        .await,
    )
    .await;
    assert_eq!(code(&response), "access_pending", "{response}");
    assert_eq!(f.fake.calls("consent.request").len(), 1);
    assert_eq!(std::fs::read_to_string(path).unwrap(), SOURCE);
}

#[tokio::test]
async fn worker_rust_home_overrides_are_canonical() {
    if !in_rust_home("worker_rust_home_overrides_are_canonical", true) {
        return;
    }
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = rust_fixture();
    for (corr, path) in [
        cargo_root().join("registry/src/example/lib.rs"),
        rustup_root().join("toolchains/stable/lib/rustlib/src/rust/library/core/src/lib.rs"),
    ]
    .iter()
    .enumerate()
    {
        let response = f.read(&f.worker, path, corr as u64 + 1).await;
        assert_no_ask(&f, &response);
    }
}

#[tokio::test]
async fn worker_registry_symlink_escape_still_ask() {
    if !in_rust_home("worker_registry_symlink_escape_still_ask", false) {
        return;
    }
    cargo_read_asks("registry/src/escape.rs").await;
}

#[tokio::test]
async fn worker_rust_sources_floor_still_denies() {
    if !in_rust_home("worker_rust_sources_floor_still_denies", false) {
        return;
    }
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = rust_fixture();
    let path = cargo_root().join("registry/src/example/lib.rs");
    f.ctx
        .update_config(|config| config.sandbox.read_deny.push(path.clone()));
    let response = f.read(&f.worker, &path, 1).await;
    assert_eq!(code(&response), "denied_floor", "{response}");
    assert!(f.fake.calls("consent.request").is_empty());
}

#[tokio::test]
async fn head_explicit_ask_does_not_get_worker_rust_grants() {
    if !in_rust_home("head_explicit_ask_does_not_get_worker_rust_grants", false) {
        return;
    }
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = rust_fixture();
    let response = f
        .read(
            &f.head,
            &cargo_root().join("registry/src/example/lib.rs"),
            1,
        )
        .await;
    assert_eq!(code(&response), "access_pending", "{response}");
    assert_eq!(f.fake.calls("consent.request").len(), 1);
}

#[tokio::test]
async fn worker_codemode_inherits_rust_read_grants() {
    if !in_rust_home("worker_codemode_inherits_rust_read_grants", false) {
        return;
    }
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = rust_fixture();
    let run = identity(
        &f.worker.project_root,
        Some(stamp("worker-run", "ephemeral", Some("worker"))),
    );
    access_grants::register(&run, &f.ctx).unwrap();
    let response = f
        .read(&run, &cargo_root().join("registry/src/example/lib.rs"), 1)
        .await;
    assert_no_ask(&f, &response);
}
