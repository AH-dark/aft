//! A stamped Alfonso worker's shell runs in the worktree jail. This drives the
//! real `aft --subc` process from a fake daemon: a route bound with a worker
//! scope stamp, bash calls as a worker would make them, and the real native
//! sandbox (Seatbelt on macOS). Everything lives under one throwaway directory
//! with its own HOME and XDG roots; the live CortexKit store is never touched.
//!
//! macOS only: the Linux CI runner cannot build a native sandbox profile (it
//! has no HOME), and these checks are about what a worker on the Mac can do.
#![cfg(target_os = "macos")]

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use subc_protocol::session::{ModuleControlRequest, ModuleControlResponse};
use subc_protocol::{
    BindIdentity, Flags, Frame, FrameType, ModuleHelloAckBody, Principal, Priority, RouteTarget,
    PROTOCOL_VERSION,
};
use subc_transport::{
    authenticate_server,
    connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION},
    read_frame, write_frame,
};
use tokio::net::{TcpListener, TcpStream};

#[path = "helpers/aft_binary.rs"]
mod aft_binary;

fn flags() -> Flags {
    Flags::new(false, Priority::Interactive, false)
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn run(command: &mut Command) -> String {
    let output = command.output().expect("run fixture command");
    assert!(
        output.status.success(),
        "{command:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn git(dir: &Path, args: &[&str]) -> String {
    run(Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.com",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir))
}

struct Jail {
    _dir: tempfile::TempDir,
    base: PathBuf,
    home: PathBuf,
    main: PathBuf,
    worktree: PathBuf,
    other_repo: PathBuf,
    data: PathBuf,
    rustup: PathBuf,
    child: Child,
    stream: TcpStream,
    next_corr: u64,
}

impl Drop for Jail {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn next_frame(stream: &mut TcpStream) -> Frame {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(120), read_frame(stream))
            .await
            .expect("module answers in time")
            .expect("module frame")
            .expect("module stays connected");
        if frame.header.ty == FrameType::Ping {
            let pong =
                Frame::build(FrameType::Pong, flags(), 0, 0, frame.header.corr, vec![]).unwrap();
            write_frame(stream, &pong).await.unwrap();
            continue;
        }
        if frame.header.ty == FrameType::Push {
            continue;
        }
        return frame;
    }
}

impl Jail {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let home = base.join("home");
        let main = base.join("main");
        let worktree = base.join("worktree");
        let other_repo = base.join("other-repo");
        let rustup = base.join("rustup");
        let xdg = base.join("xdg");
        for path in [
            home.join(".ssh"),
            home.join(".cargo/bin"),
            home.join(".npm"),
            home.join(".bun/install/cache"),
            main.clone(),
            other_repo.clone(),
            rustup.clone(),
            base.join("lexar"),
            base.join("bin"),
        ] {
            std::fs::create_dir_all(path).unwrap();
        }
        for name in ["config", "data", "state", "cache"] {
            std::fs::create_dir_all(xdg.join(name)).unwrap();
        }
        let data = xdg.join("data");
        std::fs::create_dir_all(data.join("cortexkit")).unwrap();

        // The main checkout, the worker's linked worktree and a sibling worktree.
        std::fs::write(main.join("README.md"), "main\n").unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        git(&main, &["add", "README.md"]);
        git(&main, &["commit", "-q", "-m", "initial"]);
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "work",
                worktree.to_str().unwrap(),
            ],
        );
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "sibling",
                base.join("sibling").to_str().unwrap(),
            ],
        );
        std::fs::write(other_repo.join("README.md"), "other\n").unwrap();
        git(&other_repo, &["init", "-q"]);

        // A tiny crate in the worktree, and operator cargo configuration
        // (outside the worktree) naming a shared target and an sccache wrapper.
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::write(
            worktree.join("Cargo.toml"),
            "[package]\nname = \"jailcrate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(worktree.join("src/main.rs"), "fn main() {}\n").unwrap();
        let wrapper = base.join("bin/fake-sccache");
        std::fs::write(
            &wrapper,
            "#!/bin/sh\n# Stands in for sccache: records use in the sccache cache directory.\ncache=\"$HOME/Library/Caches/Mozilla.sccache\"\nmkdir -p \"$cache\" && echo used > \"$cache/marker\" || exit 97\nexec \"$@\"\n",
        )
        .unwrap();
        run(Command::new("chmod").arg("755").arg(&wrapper));
        std::fs::write(
            home.join(".cargo/config.toml"),
            format!(
                "[build]\ntarget-dir = {:?}\nrustc-wrapper = {:?}\n",
                base.join("shared-target").display().to_string(),
                wrapper.display().to_string()
            ),
        )
        .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connection = ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: Some(PROTOCOL_VERSION),
            endpoints: vec![Endpoint {
                host: "127.0.0.1".into(),
                port: listener.local_addr().unwrap().port(),
            }],
            key: vec![0x42; subc_transport::KEY_LEN],
            daemon_id: [0x24; subc_transport::DAEMON_ID_LEN],
            pid: std::process::id(),
            daemon_ver: "worker-jail".into(),
        };
        let connection_path = base.join("connection.json");
        connection_file::write_atomic(&connection_path, &connection).unwrap();
        let mut command = Command::new(aft_binary::aft_binary());
        command
            .current_dir(&base)
            .arg("--subc")
            .arg(&connection_path)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", xdg.join("config"))
            .env("XDG_DATA_HOME", &data)
            .env("XDG_STATE_HOME", xdg.join("state"))
            .env("XDG_CACHE_HOME", xdg.join("cache"))
            .env("RUSTUP_HOME", &rustup)
            // An operator-set shared target in the daemon's environment.
            .env("CARGO_TARGET_DIR", base.join("env-target"))
            .env_remove("SUBC_MODULE_ID")
            .env_remove("SUBC_LAUNCH_NONCE")
            .env_remove("AFT_STORAGE_DIR")
            .env_remove("CARGO_HOME")
            .env_remove("SCCACHE_DIR")
            .env("AFT_TEST_DISABLE_FILE_WATCHER", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().unwrap();
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(20), listener.accept())
            .await
            .unwrap()
            .unwrap();
        authenticate_server(
            &mut stream,
            &[0x42; subc_transport::KEY_LEN],
            &[0x24; subc_transport::DAEMON_ID_LEN],
            "worker-jail",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        let hello = next_frame(&mut stream).await;
        assert_eq!(hello.header.ty, FrameType::Hello);
        let ack = ModuleHelloAckBody {
            negotiated_ver: PROTOCOL_VERSION,
            subc_ops: vec![],
            subc_capabilities: vec![],
            storage: None,
            machine_id: None,
        };
        write_frame(
            &mut stream,
            &Frame::build(
                FrameType::HelloAck,
                flags(),
                0,
                0,
                hello.header.corr,
                serde_json::to_vec(&ack).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let mut jail = Self {
            _dir: dir,
            base,
            home,
            main,
            worktree,
            other_repo,
            data,
            rustup,
            child,
            stream,
            next_corr: 100,
        };
        jail.bind_worker().await;
        jail
    }

    /// Bind channel 1 the way the daemon binds a worker route: principal
    /// reserved:broca, scope owned by reserved:prefrontal-core, kind worker,
    /// with its head as parent.
    async fn bind_worker(&mut self) {
        let owner = Principal::Reserved {
            module_id: "prefrontal-core".into(),
        };
        let scope: subc_protocol::scope::ScopeStamp = serde_json::from_value(json!({
            "owner": owner,
            "ref": "worker-scope",
            "scope_epoch": 3,
            "kind": "worker",
            "parent": {"owner": owner, "ref": "head-scope", "scope_epoch": 7},
            "owner_authorized": true,
        }))
        .unwrap();
        let bind = ModuleControlRequest::RouteBind {
            route_channel: 1,
            epoch: 1,
            target: RouteTarget::ToolProvider {
                module_id: "aft".into(),
            },
            identity: BindIdentity::new(&self.worktree, "runner", "worker-session"),
            principal: Some(Principal::Reserved {
                module_id: "broca".into(),
            }),
            consumer_capabilities: None,
            admission_facts: None,
            scope: Some(scope),
            role_versions: Some(std::collections::BTreeMap::from([(
                "tool-provider".into(),
                "v1".into(),
            )])),
            call_mode: None,
        };
        let corr = self.next_corr;
        self.next_corr += 1;
        write_frame(
            &mut self.stream,
            &Frame::build(
                FrameType::Request,
                flags(),
                0,
                0,
                corr,
                serde_json::to_vec(&bind).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let ack = next_frame(&mut self.stream).await;
        assert_eq!(ack.header.corr, corr);
        let response: ModuleControlResponse = serde_json::from_slice(&ack.body).unwrap();
        assert_eq!(response, ModuleControlResponse::RouteBindAck {});
    }

    /// Run one shell command as the worker and return the reply as text.
    async fn bash(&mut self, command: &str) -> String {
        for _ in 0..200 {
            let corr = self.next_corr;
            self.next_corr += 1;
            let body = json!({
                "name": "bash",
                "preset": "worker",
                "arguments": {"command": command, "timeout": 300_000},
            });
            write_frame(
                &mut self.stream,
                &Frame::build(
                    FrameType::Request,
                    flags(),
                    1,
                    1,
                    corr,
                    serde_json::to_vec(&body).unwrap(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
            let frame = loop {
                let frame = next_frame(&mut self.stream).await;
                if frame.header.channel == 1 && frame.header.corr == corr {
                    break frame;
                }
            };
            let text = String::from_utf8_lossy(&frame.body).into_owned();
            if text.contains("database_initializing")
                || text.contains("Project persistence is still initializing")
            {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            let value: Value = serde_json::from_slice(&frame.body).unwrap_or(Value::Null);
            let content = value["content"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            return format!("{content}\n{text}");
        }
        panic!("database initialization never completed");
    }
}

fn real_rustup_home() -> PathBuf {
    std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join(".rustup"))
}

fn real_cargo() -> PathBuf {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cargo"))
        .join("bin/cargo")
}

/// Run `cargo build --offline` from the worker shell with the throwaway
/// CARGO_HOME (the shell's HOME default) and the real toolchain.
fn cargo_build(jail: &Jail, prefix: &str, marker: &str) -> String {
    format!(
        "cd {} && {prefix} RUSTUP_HOME={} {} build --offline -q && echo \"{marker}_$((1+1))\"",
        quote(&jail.worktree),
        quote(&real_rustup_home()),
        quote(&real_cargo()),
    )
}

#[tokio::test]
async fn worker_jail_allows_normal_worker_work_and_refuses_everything_else() {
    let mut jail = Jail::start().await;

    // git add and git commit in the linked worktree write the worktree's own
    // Git directory and the main repository's object, ref and log stores.
    let committed = jail
        .bash(&format!(
            "cd {} && echo change > change.txt && git add change.txt && git -c user.name=w -c user.email=w@example.com commit -q -m jailed && echo \"GIT_$((1+1))\"",
            quote(&jail.worktree)
        ))
        .await;
    assert!(committed.contains("GIT_2"), "{committed}");
    assert!(git(&jail.main, &["log", "--oneline", "work"]).contains("jailed"));

    // Cargo with the operator's target-dir and sccache wrapper from
    // ~/.cargo/config.toml (outside the worktree).
    let built = jail.bash(&cargo_build(&jail, "", "CARGO")).await;
    assert!(built.contains("CARGO_2"), "{built}");
    assert!(jail.base.join("shared-target/debug/jailcrate").is_file());
    assert!(jail
        .home
        .join("Library/Caches/Mozilla.sccache/marker")
        .is_file());

    // The daemon environment's CARGO_TARGET_DIR is operator-set too.
    let env_target = jail.base.join("env-target");
    let built = jail
        .bash(&cargo_build(
            &jail,
            &format!("CARGO_TARGET_DIR={}", quote(&env_target)),
            "ENVTARGET",
        ))
        .await;
    assert!(built.contains("ENVTARGET_2"), "{built}");
    assert!(env_target.join("debug/jailcrate").is_file());

    // A target the worker picks itself, inline or exported, fails like any
    // other outside write and leaves nothing behind.
    for (prefix, label) in [
        (
            format!(
                "CARGO_TARGET_DIR={}",
                quote(&jail.other_repo.join("target"))
            ),
            "INLINE",
        ),
        (
            format!(
                "export CARGO_TARGET_DIR={} &&",
                quote(&jail.home.join("chosen-target"))
            ),
            "EXPORTED",
        ),
    ] {
        let refused = jail.bash(&cargo_build(&jail, &prefix, label)).await;
        assert!(!refused.contains(&format!("{label}_2")), "{refused}");
    }
    assert!(!jail.other_repo.join("target").exists());
    assert!(!jail.home.join("chosen-target").exists());

    // Cargo and rustup caches, the package manager caches, and TMPDIR.
    let caches = jail
        .bash(&format!(
            "touch \"$HOME/.cargo/registry/probe\" \"$HOME/.cargo/git/probe\" \"$HOME/.cargo/.package-cache\" {} \"$HOME/.npm/probe\" \"$HOME/.bun/install/cache/probe\" \"$TMPDIR/probe\" && echo \"CACHES_$((1+1))\"",
            quote(&jail.rustup.join("downloads/probe"))
        ))
        .await;
    assert!(caches.contains("CACHES_2"), "{caches}");
    let bun = PathBuf::from(std::env::var_os("HOME").unwrap()).join(".bun/bin/bun");
    if bun.is_file() {
        let installed = jail
            .bash(&format!(
                "cd {} && printf '{{\"name\":\"jail\",\"version\":\"1.0.0\"}}' > package.json && {} install && echo \"BUN_$((1+1))\"",
                quote(&jail.worktree),
                quote(&bun)
            ))
            .await;
        assert!(installed.contains("BUN_2"), "{installed}");
    }

    // A target/ symlink inside the worktree does not widen the grant: its
    // real path is outside and stays unwritable.
    let linked = jail
        .bash(&format!(
            "cd {} && ln -s {} target-link && if (echo x > target-link/f) 2>/dev/null; then echo \"WRO\"\"TE\"; else echo \"REF\"\"USED\"; fi",
            quote(&jail.worktree),
            quote(&jail.base.join("lexar"))
        ))
        .await;
    assert!(linked.contains("REFUSED"), "{linked}");
    assert!(!jail.base.join("lexar/f").exists());

    // Everything else outside the worktree stays refused.
    let refused_paths = [
        jail.main.join("README.md"),
        jail.main.join("new-file"),
        jail.main.join(".git/config"),
        jail.main.join(".git/hooks/post-commit"),
        jail.main.join(".git/info/exclude"),
        jail.main.join(".git/worktrees/sibling/HEAD"),
        jail.base.join("sibling/README.md"),
        jail.other_repo.join("README.md"),
        jail.home.join(".ssh/authorized_keys"),
        jail.home.join(".cargo/config.toml"),
        jail.home.join(".cargo/bin/cargo"),
        jail.data.join("cortexkit/probe"),
        jail.home.join("probe"),
    ];
    let before: Vec<_> = refused_paths
        .iter()
        .map(|path| std::fs::read(path).ok())
        .collect();
    let script = refused_paths
        .iter()
        .map(|path| {
            format!(
                "if (printf x >> {0}) 2>/dev/null; then echo \"WRO\"\"TE {1}\"; else echo \"REF\"\"USED {1}\"; fi",
                quote(path),
                path.display()
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let refusals = jail.bash(&script).await;
    for (path, before) in refused_paths.iter().zip(before) {
        assert!(
            refusals.contains(&format!("REFUSED {}", path.display())),
            "{} was writable:\n{refusals}",
            path.display()
        );
        assert_eq!(std::fs::read(path).ok(), before, "{}", path.display());
    }
}
