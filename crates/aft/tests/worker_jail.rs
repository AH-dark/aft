//! A daemon-authenticated background worker gets a native sandbox, even when
//! ordinary sessions have sandboxing disabled. Drive the real `aft --subc`
//! process through a fake daemon that binds such a worker route, then check its
//! shell's allowed work and denied access. Seatbelt runs on macOS; hook and
//! temporary-file probes also run on Linux when Landlock is available.
//! Everything lives under one throwaway HOME and XDG layout, never the live
//! CortexKit data store. Every daemon has a HOME, even on CI runners without one.
#![cfg(unix)]

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
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env("HOME", dir))
}

struct Jail {
    _dir: tempfile::TempDir,
    base: PathBuf,
    #[cfg(target_os = "macos")]
    home: PathBuf,
    main: PathBuf,
    worktree: PathBuf,
    #[cfg(target_os = "macos")]
    other_repo: PathBuf,
    data: PathBuf,
    #[cfg(target_os = "macos")]
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
        Self::start_with_sccache(None, None).await
    }

    async fn start_with_sccache(port: Option<u16>, uds: Option<&Path>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let home = base.join("home");
        let main = base.join("main");
        let data = home.join(".local/share");
        let worktree = data.join("cortexkit/alfonso/worktrees/0123456789abcdef/worker");
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
        std::fs::create_dir_all(data.join("cortexkit")).unwrap();

        // The main checkout, the worker's linked worktree and a sibling worktree.
        std::fs::write(main.join("README.md"), "main\n").unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        // Keep automatic maintenance from adding unrelated shared-metadata
        // writes to these commit and permission probes.
        git(&main, &["config", "gc.auto", "0"]);
        git(&main, &["config", "maintenance.auto", "false"]);
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

        // A local bare remote with one commit the worker has not fetched yet,
        // and packed refs in the main repository, as a clone would have.
        let remote = base.join("remote.git");
        let upstream = base.join("upstream");
        git(
            &base,
            &[
                "clone",
                "-q",
                "--bare",
                main.to_str().unwrap(),
                remote.to_str().unwrap(),
            ],
        );
        git(
            &base,
            &[
                "clone",
                "-q",
                remote.to_str().unwrap(),
                upstream.to_str().unwrap(),
            ],
        );
        std::fs::write(upstream.join("UPSTREAM.md"), "upstream\n").unwrap();
        git(&upstream, &["add", "UPSTREAM.md"]);
        git(&upstream, &["commit", "-q", "-m", "upstream change"]);
        git(&upstream, &["push", "-q", "origin", "main"]);
        git(
            &main,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&main, &["pack-refs", "--all"]);

        // Co-author attribution on, so AFT's managed Git hooks are in play.
        std::fs::create_dir_all(xdg.join("config/cortexkit")).unwrap();
        std::fs::write(
            xdg.join("config/cortexkit/aft.jsonc"),
            r#"{"git": {"co_author": "Jail Bot <jail@example.com>"}}"#,
        )
        .unwrap();

        // A tiny crate in the worktree, and operator cargo configuration
        // (outside the worktree) naming a shared target and an sccache wrapper.
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::write(
            worktree.join("Cargo.toml"),
            "[package]\nname = \"jailcrate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(worktree.join("src/main.rs"), "fn main() {}\n").unwrap();
        let wrapper = base.join("bin/rustc-wrapper");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\n# Stands in for a compile wrapper such as sccache: records that it ran.\necho used >> {}\nexec \"$@\"\n",
                quote(&worktree.join("wrapper-used"))
            ),
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
            .env("SUBC_CONNECTION_FILE", &connection_path)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", xdg.join("config"))
            .env("XDG_DATA_HOME", &data)
            .env("XDG_STATE_HOME", xdg.join("state"))
            .env("XDG_CACHE_HOME", xdg.join("cache"))
            .env("AFT_CACHE_DIR", data.join("cortexkit/aft/cache"))
            .env("RUSTUP_HOME", &rustup)
            // An operator-set shared target in the daemon's environment.
            .env("CARGO_TARGET_DIR", base.join("env-target"))
            .env_remove("SUBC_MODULE_ID")
            .env_remove("SUBC_LAUNCH_NONCE")
            .env_remove("AFT_STORAGE_DIR")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("CARGO_HOME")
            .env_remove("SCCACHE_DIR")
            .env("AFT_TEST_DISABLE_FILE_WATCHER", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
            .env_remove("SCCACHE_SERVER_PORT")
            .env_remove("SCCACHE_SERVER_UDS");
        if let Some(port) = port {
            command.env("SCCACHE_SERVER_PORT", port.to_string());
        }
        if let Some(uds) = uds {
            command.env("SCCACHE_SERVER_UDS", uds);
        }
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
            #[cfg(target_os = "macos")]
            home,
            main,
            worktree,
            #[cfg(target_os = "macos")]
            other_repo,
            data,
            #[cfg(target_os = "macos")]
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
        let response: ModuleControlResponse =
            serde_json::from_slice(&ack.body).unwrap_or_else(|error| {
                panic!(
                    "invalid bind response: {error}: {}",
                    String::from_utf8_lossy(&ack.body)
                )
            });
        assert_eq!(response, ModuleControlResponse::RouteBindAck {});
    }

    async fn tool_reply(&mut self, name: &str, arguments: Value) -> Value {
        for _ in 0..200 {
            let corr = self.next_corr;
            self.next_corr += 1;
            let body = json!({
                "name": name,
                "preset": "worker",
                "arguments": arguments,
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
            return serde_json::from_slice(&frame.body).expect("JSON tool reply");
        }
        panic!("database initialization never completed");
    }

    async fn tool(&mut self, name: &str, arguments: Value) -> String {
        let value = self.tool_reply(name, arguments).await;
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
        format!("{content}\n{value}")
    }

    /// Run one shell command as the worker and return the reply as text.
    async fn bash(&mut self, command: &str) -> String {
        self.tool("bash", json!({"command": command, "timeout": 300_000}))
            .await
    }
}

#[cfg(target_os = "macos")]
fn real_rustup_home() -> PathBuf {
    std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join(".rustup"))
}

#[cfg(target_os = "macos")]
fn real_cargo() -> PathBuf {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cargo"))
        .join("bin/cargo")
}

/// Run `cargo build --offline` from the worker shell with the throwaway
/// CARGO_HOME (the shell's HOME default) and the real toolchain.
#[cfg(target_os = "macos")]
fn cargo_build(jail: &Jail, prefix: &str, marker: &str) -> String {
    format!(
        "cd {} && {prefix} RUSTUP_HOME={} {} build --offline -q && echo \"{marker}_$((1+1))\"",
        quote(&jail.worktree),
        quote(&real_rustup_home()),
        quote(&real_cargo()),
    )
}

#[cfg(target_os = "macos")]
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
    // AFT's managed Git hooks ran on that commit inside the jail.
    assert!(
        git(&jail.main, &["log", "-1", "--format=%B", "work"])
            .contains("Co-authored-by: Jail Bot <jail@example.com>"),
        "managed prepare-commit-msg hook did not run"
    );

    // gh resolves to AFT's governed shim, never to an ungoverned gh.
    let shim = jail
        .bash("test -n \"$AFT_GH_SHIMS_DIR\" && [ \"$(command -v gh)\" = \"$AFT_GH_SHIMS_DIR/gh\" ] && echo \"SHIM_$((1+1))\" && gh --status")
        .await;
    assert!(shim.contains("SHIM_2"), "{shim}");
    assert!(
        shim.contains("rung"),
        "gh --status did not come from the shim: {shim}"
    );

    // The rest of a worker's Git routine: fetch from the remote, rebase onto
    // it, stash and restore a change, and list worktrees.
    let routine = jail
        .bash(&format!(
            "cd {} && git fetch -q origin && git -c user.name=w -c user.email=w@example.com rebase -q origin/main && echo stashed > stash.txt && git add stash.txt && git -c user.name=w -c user.email=w@example.com stash -q && test ! -e stash.txt && git stash pop -q && test -e stash.txt && git worktree list && echo \"ROUTINE_$((1+1))\"",
            quote(&jail.worktree)
        ))
        .await;
    assert!(routine.contains("ROUTINE_2"), "{routine}");
    git(
        &jail.main,
        &["merge-base", "--is-ancestor", "origin/main", "work"],
    );
    assert!(git(&jail.main, &["stash", "list"]).is_empty());

    // Cargo with the operator's target-dir from ~/.cargo/config.toml (outside
    // the worktree). The rustc wrapper that file also names is bypassed in the
    // jail: a wrapper such as sccache would compile outside the sandbox.
    let built = jail.bash(&cargo_build(&jail, "", "CARGO")).await;
    assert!(built.contains("CARGO_2"), "{built}");
    assert!(jail.base.join("shared-target/debug/jailcrate").is_file());
    assert!(
        !jail.worktree.join("wrapper-used").exists(),
        "the configured rustc wrapper ran inside the jail"
    );

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

#[cfg(target_os = "macos")]
fn sccache_connect_script(address: &str, unix: bool) -> String {
    let connect = if unix {
        "s = socket.socket(socket.AF_UNIX); s.settimeout(3); s.connect(sys.argv[1])"
    } else {
        "host, port = sys.argv[1].rsplit(':', 1); s = socket.create_connection((host, int(port)), timeout=3)"
    };
    let python = format!(
        "import socket,sys,errno\ntry:\n {connect}\n print('CONNECTED')\nexcept OSError as e:\n assert e.errno in (errno.EPERM, errno.EACCES), e\n print('DENIED_BY_SANDBOX')"
    );
    format!(
        "python3 -B -c {} {}",
        quote(Path::new(&python)),
        quote(Path::new(address))
    )
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn worker_jail_denies_shared_sccache_tcp_but_preserves_other_loopback() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let ipv6_listener = std::net::TcpListener::bind(("::1", port)).unwrap();
    let mut jail = Jail::start_with_sccache(Some(port), None).await;
    for host in ["127.0.0.1", "localhost", "::1"] {
        let script = sccache_connect_script(&format!("{host}:{port}"), false);
        let outside = run(Command::new("/bin/bash").args(["-c", &script]));
        assert!(outside.contains("CONNECTED"), "{host}: {outside}");
        let reply = jail.bash(&script).await;
        assert!(reply.contains("DENIED_BY_SANDBOX"), "{host}: {reply}");
        assert!(!reply.contains("CONNECTED"), "{host}: {reply}");
    }
    drop(ipv6_listener);

    // A build or test can still serve and connect to its own ephemeral loopback port.
    let python = "import socket\ns = socket.socket(); s.bind(('127.0.0.1', 0)); s.listen()\nc = socket.create_connection(s.getsockname(), timeout=3)\ns.accept()[0].close()\nprint('OWN_LOOPBACK_CONNECTED')";
    let reply = jail
        .bash(&format!("python3 -B -c {}", quote(Path::new(python))))
        .await;
    assert!(reply.contains("OWN_LOOPBACK_CONNECTED"), "{reply}");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn worker_jail_denies_shared_sccache_uds() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("sccache.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let script = sccache_connect_script(path.to_str().unwrap(), true);
    let outside = run(Command::new("/bin/bash").args(["-c", &script]));
    assert!(outside.contains("CONNECTED"), "{outside}");
    let mut jail = Jail::start_with_sccache(None, Some(&path)).await;
    let reply = jail.bash(&script).await;
    assert!(reply.contains("DENIED_BY_SANDBOX"), "{reply}");
    assert!(!reply.contains("CONNECTED"), "{reply}");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn worker_jail_spawns_with_dangling_socket_deny() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("docker.sock");
    let target = root.path().join("not-running.sock");
    std::os::unix::fs::symlink(&target, &path).unwrap();
    let mut jail = Jail::start_with_sccache(None, Some(&path)).await;
    let reply = jail.bash("echo DANGLING_SOCKET_$((1+1))").await;
    assert!(reply.contains("DANGLING_SOCKET_2"), "{reply}");
    assert!(!target.exists());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn worker_jail_commits_and_deletes_packed_ref_preserving_config() {
    let mut jail = Jail::start().await;
    git(&jail.main, &["branch", "packed-delete", "work"]);
    // Give branch deletion a real config section to remove if writes become
    // possible, so the unchanged-bytes assertion also detects weakened policy.
    git(
        &jail.main,
        &[
            "config",
            "branch.packed-delete.description",
            "protected branch config",
        ],
    );
    git(&jail.main, &["pack-refs", "--all"]);
    let common = jail.main.join(".git");
    let packed = std::fs::read_to_string(common.join("packed-refs")).unwrap();
    assert!(packed.contains(" refs/heads/packed-delete\n"));
    assert!(!common.join("refs/heads/packed-delete").exists());
    let committed = jail
        .bash(&format!(
            "cd {} && git -c user.name=w -c user.email=w@example.com commit --allow-empty -q -m packed-probe && echo PACKED_COMMIT_$((1+1))",
            quote(&jail.worktree),
        ))
        .await;
    assert!(committed.contains("PACKED_COMMIT_2"), "{committed}");
    assert!(!committed.contains("error:"), "{committed}");
    assert!(!committed.contains("fatal:"), "{committed}");
    assert!(git(&jail.main, &["log", "-1", "--format=%s", "work"]).contains("packed-probe"));
    let config = common.join("config");
    let config_before = std::fs::read(&config).unwrap();
    let deleted = jail
        .tool_reply(
            "bash",
            json!({
                "command": format!(
                    "cd {} && git branch -D packed-delete && echo PACKED_DELETE_$((1+1))",
                    quote(&jail.worktree),
                ),
                "timeout": 300_000,
            }),
        )
        .await;
    let result = &deleted["structuredContent"];
    assert_eq!(result["success"].as_bool(), Some(true), "{deleted}");
    assert_eq!(result["status"].as_str(), Some("completed"), "{deleted}");
    assert_eq!(result["exit_code"].as_i64(), Some(0), "{deleted}");
    let output = result["output"].as_str().expect("completed bash output");
    assert!(
        output.lines().any(|line| line.trim() == "PACKED_DELETE_2"),
        "{deleted}"
    );
    // Git deletes the ref before trying to remove its branch config section.
    // Shared config stays read-only so a worker cannot redirect core.hooksPath
    // and bypass hooks; tolerate only that exact lock error/warning pair.
    packed_ref_delete_diagnostics(output, &config)
        .unwrap_or_else(|error| panic!("{error}\n{deleted}"));
    assert_eq!(
        std::fs::read(&config).unwrap(),
        config_before,
        "shared config changed during deletion"
    );
    assert!(!common.join("refs/heads/packed-delete").exists());
    let absent = Command::new("git")
        .current_dir(&jail.main)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            "refs/heads/packed-delete",
        ])
        .status()
        .unwrap();
    assert_eq!(absent.code(), Some(1));
    let packed = std::fs::read_to_string(common.join("packed-refs")).unwrap();
    assert!(!packed.contains(" refs/heads/packed-delete\n"));
    assert!(!common.join("packed-refs.lock").exists());
    assert!(!common.join("packed-refs.new").exists());
}

fn packed_ref_delete_diagnostics(output: &str, config: &Path) -> Result<(), String> {
    let expected_error = format!("error: could not lock config file {}", config.display());
    let expected_warning = "warning: update of config-file failed";
    let mut errors = 0;
    let mut warnings = 0;
    for line in output.lines().map(str::trim) {
        if line == expected_error {
            errors += 1;
        } else if line == expected_warning {
            warnings += 1;
        } else if line.contains("error:") || line.contains("fatal:") || line.contains("warning:") {
            return Err(format!("unexpected branch-delete diagnostic: {line}"));
        }
    }
    if errors != warnings || errors > 1 {
        return Err(format!("expected no diagnostics or one config-lock error/warning pair, got {errors} errors and {warnings} warnings"));
    }
    Ok(())
}

#[test]
fn packed_ref_delete_diagnostics_accepts_only_the_config_lock_pair_or_no_diagnostics() {
    let config = Path::new("/fixture/main/.git/config");
    assert!(packed_ref_delete_diagnostics(
        "Deleted branch packed-delete\nPACKED_DELETE_2\n",
        config
    )
    .is_ok());
    let output = format!("Deleted branch packed-delete\nerror: could not lock config file {}\nwarning: update of config-file failed\nPACKED_DELETE_2\n", config.display());
    assert!(packed_ref_delete_diagnostics(&output, config).is_ok());
}

#[test]
fn packed_ref_delete_diagnostics_rejects_other_errors_and_incomplete_pairs() {
    let config = Path::new("/fixture/main/.git/config");
    let error = format!("error: could not lock config file {}", config.display());
    let warning = "warning: update of config-file failed";
    let pair = format!("{error}\n{warning}\n");
    for output in [
        error.clone(),
        warning.to_string(),
        format!("{pair}{pair}"),
        format!("{pair}error: unable to create file /fixture/main/.git/packed-refs.new: Operation not permitted\n"),
        format!("{pair}fatal: failed to delete packed ref\n"),
        format!("{pair}warning: another warning\n"),
        format!("{error}: Operation not permitted\n{warning}\n"),
        format!("error: could not lock config file /other/.git/config\n{warning}\n"),
    ] {
        assert!(packed_ref_delete_diagnostics(&output, config).is_err(), "unexpected diagnostics were accepted: {output}");
    }
}

#[test]
fn packed_ref_delete_writable_config_control_removes_seeded_section() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let main = base.join("main");
    let worktree = base.join("worktree");
    std::fs::create_dir(&main).unwrap();
    git(&main, &["init", "--quiet"]);
    git(
        &main,
        &["commit", "--allow-empty", "--quiet", "-m", "fixture"],
    );
    git(&main, &["branch", "packed-delete", "HEAD"]);
    git(
        &main,
        &[
            "config",
            "branch.packed-delete.description",
            "protected branch config",
        ],
    );
    git(&main, &["pack-refs", "--all"]);
    git(
        &main,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            worktree.to_str().unwrap(),
        ],
    );
    let common = main.join(".git");
    let config_before = std::fs::read(common.join("config")).unwrap();
    assert!(std::fs::read_to_string(common.join("packed-refs"))
        .unwrap()
        .contains(" refs/heads/packed-delete\n"));
    assert!(!common.join("refs/heads/packed-delete").exists());
    git(&worktree, &["branch", "-D", "packed-delete"]);
    let config_after = std::fs::read(common.join("config")).unwrap();
    assert_ne!(
        config_after, config_before,
        "writable config control must change the protected bytes"
    );
    assert!(!String::from_utf8(config_after)
        .unwrap()
        .contains("[branch \"packed-delete\"]"));
    assert!(!std::fs::read_to_string(common.join("packed-refs"))
        .unwrap()
        .contains(" refs/heads/packed-delete\n"));
}

fn native_available() -> bool {
    #[cfg(target_os = "linux")]
    if !Command::new(aft_binary::aft_binary())
        .args(["sandbox-launch", "--support"])
        .output()
        .unwrap()
        .status
        .success()
    {
        assert_ne!(
            std::env::var("AFT_REQUIRE_LANDLOCK").as_deref(),
            Ok("1"),
            "required Landlock ABI unavailable"
        );
        eprintln!("Landlock probe skipped: mandatory ABI unavailable");
        return false;
    }
    true
}

fn executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[tokio::test]
async fn worker_jail_private_hook_chain_executes_read_only() {
    if !native_available() {
        return;
    }
    let mut jail = Jail::start().await;
    git(&jail.main, &["config", "extensions.worktreeConfig", "true"]);
    // First create AFT's hook scripts, then copy them under an older content
    // key. A worktree manager can retain that older path in its hook chain
    // even after AFT starts selecting a newer set of scripts.
    let selected = jail.bash("git config --path core.hooksPath").await;
    let hook_root = jail.data.join("cortexkit/aft/cache/git-hooks");
    let managed = std::fs::read_dir(&hook_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir() && path.file_name().unwrap().len() == 32)
        .unwrap();
    assert!(selected.contains(managed.to_str().unwrap()), "{selected}");
    let original = hook_root.join("0123456789abcdef0123456789abcdef");
    std::fs::create_dir_all(&original).unwrap();
    for entry in std::fs::read_dir(&managed).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), original.join(entry.file_name())).unwrap();
    }
    let hooks = jail.data.join("cortexkit/prefrontal-core/hooks/hash-1");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook_link = hooks.parent().unwrap().join("selected");
    std::os::unix::fs::symlink(&hooks, &hook_link).unwrap();
    let original_link = hook_root.join("original-link");
    std::os::unix::fs::symlink(&original, &original_link).unwrap();
    for name in ["pre-commit", "prepare-commit-msg", "post-index-change"] {
        executable(
            &hooks.join(name),
            &format!(
                "#!/bin/sh\nexec \"$(git config --path mason.originalHooksPath)/{name}\" \"$@\"\n"
            ),
        );
    }
    // The core stub execs the copied AFT hook, which returns to the common
    // repository's native hook on its second invocation. The marker proves
    // that whole chain ran, not merely the first wrapper.
    executable(
        &jail.main.join(".git/hooks/pre-commit"),
        "#!/bin/sh\nprintf chained > hook-marker\n",
    );
    git(
        &jail.worktree,
        &[
            "config",
            "--worktree",
            "core.hooksPath",
            hook_link.to_str().unwrap(),
        ],
    );
    git(
        &jail.worktree,
        &[
            "config",
            "--worktree",
            "mason.originalHooksPath",
            original_link.to_str().unwrap(),
        ],
    );
    let direct = jail.bash(&format!(
        "cd {} && cat {}/pre-commit > /dev/null && unset GIT_CONFIG_COUNT && git config --path core.hooksPath && git -c user.name=w -c user.email=w@example.com commit --allow-empty -m direct && echo DIRECT_HOOK_$((1+1))",
        quote(&jail.worktree), quote(&hooks)
    )).await;
    eprintln!("Native direct hook output:\n{direct}");
    assert!(direct.contains("DIRECT_HOOK_2"), "{direct}");
    assert!(direct.contains(hook_link.to_str().unwrap()), "{direct}");
    assert_eq!(
        std::fs::read_to_string(jail.worktree.join("hook-marker")).unwrap(),
        "chained"
    );
    std::fs::remove_file(jail.worktree.join("hook-marker")).unwrap();
    let committed = jail.bash(&format!(
        "cd {} && git config --path core.hooksPath && printf change > hook-change && git add hook-change && git -c user.name=w -c user.email=w@example.com commit -m chained && echo HOOK_COMMIT_$((1+1))",
        quote(&jail.worktree)
    )).await;
    eprintln!("Native hook commit output:\n{committed}");
    assert!(committed.contains("HOOK_COMMIT_2"), "{committed}");
    #[cfg(target_os = "linux")]
    assert!(
        committed.contains("worktree_git_config_mutable_on_landlock"),
        "the writable Git configuration limitation was not reported: {committed}"
    );
    #[cfg(target_os = "linux")]
    assert!(
        committed.contains("worktree_git_binding_mutable_on_landlock"),
        "the writable Git binding limitation was not reported: {committed}"
    );
    assert_eq!(
        std::fs::read_to_string(jail.worktree.join("hook-marker")).unwrap(),
        "chained"
    );
    assert!(git(&jail.main, &["log", "-1", "--format=%B", "work"])
        .contains("Co-authored-by: Jail Bot <jail@example.com>"));
    #[cfg(target_os = "macos")]
    {
        let denied = jail
            .bash("git config --worktree core.hooksPath /tmp/worker-selected-hooks")
            .await;
        assert!(denied.contains("Operation not permitted"), "{denied}");
        assert_eq!(
            git(
                &jail.worktree,
                &["config", "--worktree", "--get", "core.hooksPath"]
            )
            .trim(),
            hook_link.to_str().unwrap()
        );
        for path in [
            jail.worktree.join(".git"),
            jail.main.join(".git/worktrees/worker/commondir"),
            jail.main.join(".git/worktrees/worker/gitdir"),
        ] {
            let before = std::fs::read(&path).unwrap();
            let denied = jail
                .bash(&format!("printf redirect >> {}", quote(&path)))
                .await;
            assert!(denied.contains("Operation not permitted"), "{denied}");
            assert_eq!(
                std::fs::read(&path).unwrap(),
                before,
                "Git binding was writable: {denied}"
            );
        }
    }
    let sibling = hooks.parent().unwrap().join("sibling-secret");
    std::fs::write(&sibling, "private-hook-sibling").unwrap();
    std::fs::write(jail.data.join("cortexkit/aft/aft.db"), "private-db").unwrap();
    for path in [
        &sibling,
        &jail.data.join("cortexkit/aft/aft.db"),
        &jail.base.join("connection.json"),
    ] {
        let denied = jail.bash(&format!("cat {}", quote(path))).await;
        assert!(
            !denied.contains("private-hook-sibling") && !denied.contains("private-db"),
            "{denied}"
        );
        assert!(
            denied.contains("Operation not permitted") || denied.contains("Permission denied"),
            "{denied}"
        );
    }
    let denied = jail
        .bash(&format!("ls {}", quote(&jail.data.join("cortexkit"))))
        .await;
    assert!(
        denied.contains("Operation not permitted") || denied.contains("Permission denied"),
        "{denied}"
    );
    for (key, dir, configured_dir) in [
        ("core.hooksPath", &hooks, &hook_link),
        ("mason.originalHooksPath", &original, &original_link),
    ] {
        let path = dir.join("pre-commit");
        let before = std::fs::read(&path).unwrap();
        for (operation, attempt) in [
            ("append", "printf tamper >> \"$hook_dir/pre-commit\""),
            ("create", "printf new > \"$hook_dir/new-hook\""),
            ("delete", "rm \"$hook_dir/pre-commit\""),
            (
                "rename",
                "mv \"$hook_dir/pre-commit\" \"$hook_dir/renamed-hook\"",
            ),
        ] {
            // Resolve each directory inside the jailed child. Disable AFT's
            // command-scope hooksPath override to exercise the repository's
            // configured core stub as well as its original-hook target.
            let command = format!(
                "cd {} && unset GIT_CONFIG_COUNT && hook_dir=$(git config --path {key}) && test -r \"$hook_dir/pre-commit\" && printf 'HOOK_PROBE_%s path=%s\\n' \"$((1+1))\" \"$hook_dir\" && {attempt}",
                quote(&jail.worktree),
            );
            let reply = jail
                .tool_reply("bash", json!({"command": command, "timeout": 300_000}))
                .await;
            let result = &reply["structuredContent"];
            let output = result["output"].as_str().unwrap_or_default();
            eprintln!(
                "Hook immutability {key} {operation}: status={} exit_code={}\n{output}",
                result["status"], result["exit_code"]
            );
            assert_eq!(
                result["success"], true,
                "{key} {operation} did not launch: {reply}"
            );
            assert!(
                output.contains("HOOK_PROBE_2"),
                "{key} {operation} did not reach the child operation: {reply}"
            );
            assert!(
                output.contains(&format!("path={}", configured_dir.display())),
                "{key} {operation} resolved the wrong directory: {reply}"
            );
            assert_eq!(
                result["status"], "failed",
                "{key} {operation} unexpectedly succeeded: {reply}"
            );
            assert!(
                result["exit_code"].as_i64().is_some_and(|code| code != 0),
                "{key} {operation} has no failing child exit status: {reply}"
            );
            let kernel_denial = if cfg!(target_os = "macos") {
                "Operation not permitted"
            } else {
                "Permission denied"
            };
            assert!(
                output.contains(kernel_denial),
                "{key} {operation} was not a kernel write denial: {reply}"
            );
            assert_eq!(
                std::fs::read(&path).unwrap_or_default(),
                before,
                "{key} {operation} changed the hook bytes: {reply}"
            );
            assert!(
                !dir.join("new-hook").exists(),
                "{key} {operation} created a file: {reply}"
            );
            assert!(
                !dir.join("renamed-hook").exists(),
                "{key} {operation} renamed the hook: {reply}"
            );
        }
    }
}

// This Cargo probe is macOS-only because it uses the host's real toolchain.
// Linux runners may install Cargo outside HOME and the system read roots, where
// the jail correctly denies it. Hook and temporary-file probes still use Landlock.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn worker_jail_operator_build_slot_and_container_reads() {
    if !native_available() {
        return;
    }
    let mut jail = Jail::start().await;
    let container = jail.worktree.parent().unwrap().to_path_buf();
    let cargo_config = container.join(".cargo/config.toml");
    let budget = container.join(".cargo/alfonso-test-threads");
    let seeds = container.join(".rust-build-seeds");
    let slots = jail.data.join("cortexkit/prefrontal-core/build-slots");
    for path in [cargo_config.parent().unwrap(), &seeds, &slots] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::write(&budget, "2\n").unwrap();
    std::fs::write(seeds.join("seed"), "build-seed").unwrap();
    for (name, value) in [
        (".root", "slot-root"),
        (".cache", "slot-cache"),
        ("limit", "2\n"),
    ] {
        std::fs::write(slots.join(name), value).unwrap();
    }
    let wrapper = slots.join("prefrontal-build-slot");
    executable(
        &wrapper,
        r#"#!/usr/bin/env python3
import fcntl, os, pathlib, sys
base = pathlib.Path(__file__).parent
assert (base / '.root').read_text() == 'slot-root'
assert (base / '.cache').read_text() == 'slot-cache'
assert (base / 'limit').read_text().strip() == '2'
with (base / 'queue.lock').open('a') as queue, (base / '0.lock').open('a') as slot:
    fcntl.flock(queue, fcntl.LOCK_EX)
    fcntl.flock(slot, fcntl.LOCK_EX)
    pathlib.Path('slot-wrapper-ran').write_text('locked')
    fcntl.flock(queue, fcntl.LOCK_UN)
    child = os.fork()
    if child == 0:
        direct = str(base / 'rustc-lowprio-direct')
        os.execv(direct, [direct, *sys.argv[1:]])
    _, status = os.waitpid(child, 0)
    sys.exit(os.waitstatus_to_exitcode(status))
"#,
    );
    executable(
        &slots.join("rustc-lowprio-direct"),
        "#!/bin/sh\nexec \"$@\"\n",
    );
    std::fs::write(
        &cargo_config,
        format!(
            "[build]\nrustc-wrapper = {:?}\ntarget-dir = {:?}\n",
            wrapper,
            jail.base.join("shared-target")
        ),
    )
    .unwrap();
    let excludes = jail
        .main
        .join(".git/worktrees/worker/alfonso-hydration-exclude");
    // Git names the administrative directory after the checkout's basename.
    std::fs::write(&excludes, "hydration-excluded\n").unwrap();
    git(
        &jail.worktree,
        &["config", "core.excludesFile", excludes.to_str().unwrap()],
    );
    std::fs::write(jail.worktree.join("hydration-excluded"), "ignored").unwrap();
    let read = jail.bash(&format!(
        "cat {} {} {} {} && git check-ignore hydration-excluded && git config --local --get core.excludesFile && echo CONTAINER_$((1+1))",
        quote(&cargo_config), quote(&budget), quote(&seeds.join("seed")), quote(&excludes)
    )).await;
    assert!(read.contains("CONTAINER_2"), "{read}");
    assert!(read.contains("build-seed"), "{read}");
    let built = jail.bash(&cargo_build(&jail, "", "BUILD_SLOT")).await;
    eprintln!("Native build-slot output:\n{built}");
    assert!(built.contains("BUILD_SLOT_2"), "{built}");
    assert_eq!(
        std::fs::read_to_string(jail.worktree.join("slot-wrapper-ran")).unwrap(),
        "locked"
    );
    assert!(jail.base.join("shared-target/debug/jailcrate").is_file());
    for name in ["queue.lock", "0.lock", "1.lock", "2.lock"] {
        let locked = jail
            .bash(&format!(
                "printf locked >> {} && echo SLOT_LOCK_$((1+1))",
                quote(&slots.join(name))
            ))
            .await;
        assert!(locked.contains("SLOT_LOCK_2"), "{locked}");
    }
    for path in [
        &wrapper,
        &slots.join("rustc-lowprio-direct"),
        &slots.join(".root"),
        &slots.join(".cache"),
        &slots.join("limit"),
        &cargo_config,
        &budget,
        &seeds.join("seed"),
    ] {
        let before = std::fs::read(path).unwrap();
        let denied = jail
            .bash(&format!("printf tamper >> {}", quote(path)))
            .await;
        assert!(
            denied.contains("Operation not permitted") || denied.contains("Permission denied"),
            "{denied}"
        );
        assert_eq!(std::fs::read(path).unwrap(), before, "{denied}");
    }
    for path in [
        slots.join("3.lock"),
        slots.join("arbitrary"),
        container.join("other-container-data"),
    ] {
        let denied = jail
            .bash(&format!("printf tamper > {}", quote(&path)))
            .await;
        assert!(
            denied.contains("Operation not permitted") || denied.contains("Permission denied"),
            "{denied}"
        );
        assert!(!path.exists(), "{denied}");
    }
}

#[tokio::test]
async fn worker_jail_temp_and_cortexkit_scratch_match_tools() {
    if !native_available() {
        return;
    }
    let mut jail = Jail::start().await;
    let temp = jail.bash("printf 'TMPDIR=%s TMP=%s TEMP=%s\\n' \"$TMPDIR\" \"$TMP\" \"$TEMP\"; set -e; file=$(mktemp); dir=$(mktemp -d); test -f \"$file\"; test -d \"$dir\"; test \"${file%/*}\" = \"$TMPDIR\"; test \"${dir%/*}\" = \"$TMPDIR\"; test \"$TMP\" = \"$TMPDIR\"; test \"$TEMP\" = \"$TMPDIR\"; echo TEMP_$((1+1))").await;
    eprintln!("Native mktemp output:\n{temp}");
    assert!(temp.contains("TEMP_2"), "{temp}");
    let explicit = jail.bash("file=$(mktemp ./explicit.XXXXXXXX) && test -f \"$file\" && rm \"$file\" && echo EXPLICIT_TEMP_$((1+1))").await;
    assert!(explicit.contains("EXPLICIT_TEMP_2"), "{explicit}");
    let outside = jail.bash("TMPDIR=/tmp mktemp").await;
    assert!(
        outside.contains("Operation not permitted") || outside.contains("Permission denied"),
        "{outside}"
    );
    let scratch = jail.bash("mkdir -p .cortexkit/tmp .cortexkit/internal-docs && printf shell > .cortexkit/tmp/shell && printf docs > .cortexkit/internal-docs/note && cat .cortexkit/tmp/shell && echo SCRATCH_$((1+1))").await;
    assert!(scratch.contains("SCRATCH_2"), "{scratch}");
    let written = jail
        .tool(
            "write",
            json!({"filePath": jail.worktree.join(".cortexkit/tmp/tool"), "content": "tool"}),
        )
        .await;
    assert_eq!(
        std::fs::read_to_string(jail.worktree.join(".cortexkit/tmp/tool")).unwrap_or_default(),
        "tool",
        "{written}"
    );
    // Seatbelt protects .cortexkit/aft.jsonc and .cortexkit/aft/filters
    // inside a writable worktree. Landlock's additive rules cannot exclude
    // those children; its nested_write_deny warning reports that limitation.
    #[cfg(target_os = "macos")]
    for path in [
        jail.worktree.join(".cortexkit/aft.jsonc"),
        jail.worktree.join(".cortexkit/aft/filters/policy.toml"),
    ] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "protected").unwrap();
        let denied = jail
            .bash(&format!("printf tamper >> {}", quote(&path)))
            .await;
        assert!(denied.contains("Operation not permitted"), "{denied}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "protected",
            "{denied}"
        );
    }
}

#[tokio::test]
async fn worker_jail_short_temp_binds_and_connects_unix_socket() {
    if !native_available() {
        return;
    }
    let mut jail = Jail::start().await;
    let script = r#"import os, pathlib, socket, stat
root = pathlib.Path(os.environ['TMPDIR'])
assert os.environ['TMP'] == str(root) == os.environ['TEMP']
assert root == root.resolve()
assert stat.S_IMODE(root.stat().st_mode) == 0o700
nested = root / ('a' * 30)
nested.mkdir()
path = str(nested / ('b' * 30 + '.sock'))
server = socket.socket(socket.AF_UNIX)
server.settimeout(3)
server.bind(path)
server.listen(1)
client = socket.socket(socket.AF_UNIX)
client.settimeout(3)
client.connect(path)
peer, _ = server.accept()
peer.sendall(b'hello')
assert client.recv(5) == b'hello'
print('SOCKET_ROUNDTRIP_OK')
"#;
    let reply = jail
        .bash(&format!("python3 -B -c {}", quote(Path::new(script))))
        .await;
    assert!(reply.contains("SOCKET_ROUNDTRIP_OK"), "{reply}");
}

#[tokio::test]
async fn worker_jail_temp_denies_other_task_directory() {
    if !native_available() {
        return;
    }
    let mut jail = Jail::start().await;
    let recorded = jail.worktree.join("first-temp.txt");
    let reply = jail.bash(&format!("printf '%s' \"$TMPDIR\" > {}; printf original > \"$TMPDIR/sentinel\" && echo FIRST_TEMP_OK", quote(&recorded))).await;
    assert!(reply.contains("FIRST_TEMP_OK"), "{reply}");
    let first = PathBuf::from(std::fs::read_to_string(recorded).unwrap());
    assert!(first.join("sentinel").is_file());
    let script = r#"import errno, os, pathlib, sys
own = pathlib.Path(os.environ['TMPDIR'])
assert own != pathlib.Path(sys.argv[1])
(own / 'positive-control').write_text('allowed')
try:
    (pathlib.Path(sys.argv[1]) / 'sentinel').write_text('tampered')
except OSError as e:
    assert e.errno in (errno.EPERM, errno.EACCES), e
    print('OTHER_TASK_DENIED')
else:
    raise AssertionError('other task directory was writable')
"#;
    let reply = jail
        .bash(&format!(
            "python3 -B -c {} {}",
            quote(Path::new(script)),
            quote(&first)
        ))
        .await;
    assert!(reply.contains("OTHER_TASK_DENIED"), "{reply}");
    assert_eq!(
        std::fs::read_to_string(first.join("sentinel")).unwrap(),
        "original"
    );
}

#[tokio::test]
async fn worker_jail_temp_denies_shared_tmp_write() {
    if !native_available() {
        return;
    }
    let mut jail = Jail::start().await;
    // Use a fresh name so even a broken policy cannot overwrite a host file.
    let outside = PathBuf::from(format!(
        "/tmp/aft-denied-{}",
        jail.base.file_name().unwrap().to_str().unwrap()
    ));
    let script = r#"import errno, os, pathlib, sys
(pathlib.Path(os.environ['TMPDIR']) / 'positive-control').write_text('allowed')
try:
    with open(sys.argv[1], 'x') as f:
        f.write('unexpected')
except OSError as e:
    assert e.errno in (errno.EPERM, errno.EACCES), e
    print('SHARED_TMP_DENIED')
else:
    os.unlink(sys.argv[1])
    raise AssertionError('shared tmp was writable')
"#;
    let reply = jail
        .bash(&format!(
            "python3 -B -c {} {}",
            quote(Path::new(script)),
            quote(&outside)
        ))
        .await;
    assert!(reply.contains("SHARED_TMP_DENIED"), "{reply}");
    assert!(!outside.exists());
}
