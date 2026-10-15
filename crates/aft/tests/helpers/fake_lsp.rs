//! Build the workspace member so its locked dependencies are fetched with AFT.

#[allow(dead_code)]
#[path = "binary_cache.rs"]
mod binary_cache;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

const SOURCES: &[&[u8]] = &[
    include_bytes!("../../../../Cargo.toml"),
    include_bytes!("../../../../crates/aft-fake-lsp/Cargo.toml"),
    include_bytes!("../../../../crates/aft-fake-lsp/src/main.rs"),
    include_bytes!("../lsp/fake_server.rs"),
    include_bytes!("../../src/lsp/jsonrpc.rs"),
    include_bytes!("../../src/lsp/transport.rs"),
];

pub fn fake_server_binary() -> PathBuf {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY
        .get_or_init(|| build_in(&binary_cache::root()))
        .clone()
}

#[allow(dead_code)]
pub(crate) fn build_in(root: &Path) -> PathBuf {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let lockfile = std::fs::read(workspace.join("Cargo.lock"))
        .expect("workspace Cargo.lock is required for helper cache identity");
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let version = Command::new(&rustc)
        .arg("-vV")
        .output()
        .expect("rustc is required for test helpers");
    assert!(version.status.success(), "rustc version failed");
    let version = String::from_utf8(version.stdout).expect("rustc version is UTF-8");
    let target = version
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .expect("rustc host target");
    let mut sources = SOURCES.to_vec();
    sources.push(&lockfile);
    let key = binary_cache::key(&sources, &version, target);
    binary_cache::install(root, &key, target, |pending| {
        eprintln!("Building shared AFT test helper fake-lsp-server ({key})");
        // An isolated target directory avoids taking the test runner's Cargo
        // lock recursively; the per-helper lock serializes all nested builds.
        let output = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .current_dir(&workspace)
            .env("RUSTC", &rustc)
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("RUSTFLAGS")
            .env_remove("RUSTC_WRAPPER")
            .env_remove("RUSTC_WORKSPACE_WRAPPER")
            .args([
                "build",
                "--locked",
                "-p",
                "aft-test-fake-lsp-server",
                "--target",
                target,
                "--jobs",
                "2",
            ])
            .arg("--target-dir")
            .arg(root.join("build"))
            .output()?;
        if !output.status.success() {
            return Err(std::io::Error::other(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        let compiled =
            binary_cache::executable(&root.join("build").join(target).join("debug"), target);
        #[cfg(target_os = "linux")]
        {
            // Other test threads fork LSP children. Keep the destination's
            // writable descriptor out of their parent process altogether:
            // close-on-exec does not prevent inheritance during fork/pre_exec.
            let output = Command::new("cp")
                .arg("--")
                .arg(&compiled)
                .arg(pending)
                .output()?;
            if !output.status.success() {
                return Err(std::io::Error::other(
                    String::from_utf8_lossy(&output.stderr).into_owned(),
                ));
            }
        }
        #[cfg(not(target_os = "linux"))]
        std::fs::copy(compiled, pending)?;
        Ok(())
    })
    .expect("build shared fake LSP helper")
}
