//! The page-bundle management operations against a real `AppContext`:
//! which route's policy a collection uses, and that a file the session may
//! not read is never opened.

use super::*;
use crate::subc::test_support::{route_identity_with_harness, test_root};
use std::sync::Mutex;

const HARNESS: &str = "test-harness";
const SESSION: &str = "page-session";

fn core() -> Option<Principal> {
    Some(Principal::Reserved {
        module_id: "prefrontal-core".into(),
    })
}

fn identity(root: &ProjectRootId, trust: BindTrust) -> RouteIdentity {
    // A harness without a plugin prompt: a question the policy would ask
    // is refused outright, and no temp-directory exemption applies.
    route_identity_with_harness(root, SESSION, trust, HARNESS)
}

struct Fixture {
    _dirs: Vec<tempfile::TempDir>,
    root: PathBuf,
    outside: PathBuf,
    ctx: Arc<AppContext>,
    executor: Arc<Executor>,
    routes: HashMap<RouteChannel, RouteIdentity>,
}

/// A bound root holding `index.html`, whose references cover every way a
/// file can be out of the session's reach: a configured read_deny folder,
/// symlinks out of the root (to a file, a folder, and an AFT storage
/// database), and SQLite files in the root.
fn fixture(trust: BindTrust) -> Fixture {
    let (root_dir, root_id) = test_root("page-collect-root");
    let outside_dir = tempfile::tempdir().unwrap();
    let root = root_id.as_path().to_path_buf();
    let outside = outside_dir.path().canonicalize().unwrap();
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.clone());
    config.sandbox.read_deny = vec![root.join("secret")];
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        config,
    ));
    let executor = Arc::new(Executor::new());
    executor.register_actor(root_id.clone(), Arc::clone(&ctx));
    let routes = HashMap::from([(route_key(1, 1), identity(&root_id, trust))]);

    std::fs::write(
        root.join("index.html"),
        r#"<!doctype html><img src="ok.png"><img src="secret/key.png"><img src="out.png">
        <img src="dir-link/x.png"><img src="cache.db"><img src="db-link.png"><img src="store.png">"#,
    )
    .unwrap();
    std::fs::write(root.join("ok.png"), "OK").unwrap();
    std::fs::create_dir(root.join("secret")).unwrap();
    std::fs::write(root.join("secret/key.png"), "KEY").unwrap();
    std::fs::write(outside.join("outside.png"), "OUT").unwrap();
    std::fs::write(outside.join("x.png"), "X").unwrap();
    std::fs::write(root.join("cache.db"), "SQLITE").unwrap();
    let storage = ctx.storage_dir();
    std::fs::create_dir_all(&storage).unwrap();
    std::fs::write(storage.join("aft.db"), "SQLITE").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink(outside.join("outside.png"), root.join("out.png")).unwrap();
        symlink(&outside, root.join("dir-link")).unwrap();
        symlink(root.join("cache.db"), root.join("db-link.png")).unwrap();
        symlink(storage.join("aft.db"), root.join("store.png")).unwrap();
    }
    Fixture {
        _dirs: vec![root_dir, outside_dir],
        // Opened paths are fully resolved; compare against the same form.
        root: crate::inspect::job::normalize_path(&root.canonicalize().unwrap()),
        outside,
        ctx,
        executor,
        routes,
    }
}

fn call(f: &Fixture, principal: Option<Principal>, params: Value) -> CollectCall {
    CollectCall {
        params,
        principal,
        routes: f.routes.clone(),
        executor: Arc::clone(&f.executor),
        store: Arc::new(SnapshotStore::open(&f.ctx.storage_dir())),
    }
}

/// Hooks whose open is `open` and that swap nothing.
fn no_open<'a>(open: OpenFile<'a>) -> ReadHooks<'a> {
    ReadHooks {
        open,
        after_check: None,
    }
}

fn collect_params(entry: &str) -> Value {
    json!({"harness": HARNESS, "session": SESSION, "entry": entry})
}

/// Collect `index.html`, recording every path the reader opens.
fn collect_recording(f: &Fixture) -> (Response, Vec<PathBuf>) {
    let opened = Mutex::new(Vec::new());
    let open = |root: &Path, target: &Path| {
        opened.lock().unwrap().push(target.to_path_buf());
        file_read_exact::open_no_follow(root, target)
    };
    let response = collect_with_hooks(
        call(f, core(), collect_params("index.html")),
        ReadHooks {
            open: &open,
            after_check: None,
        },
    );
    (response, opened.into_inner().unwrap())
}

fn omitted(response: &Response) -> Vec<(String, String)> {
    response.data["omitted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["path"].as_str().unwrap().to_string(),
                entry["reason"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn page_collect_denied_references_are_never_opened() {
    for trust in [BindTrust::FirstParty, BindTrust::Untrusted] {
        let f = fixture(trust);
        let (response, opened) = collect_recording(&f);
        assert!(response.success, "{:?}", response.data);
        let files = response.data["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file["path"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(files, vec!["index.html", "ok.png"]);
        // Without symlinks (Windows here), the linked names do not exist.
        let linked = if cfg!(unix) { "denied" } else { "missing" };
        let expected = vec![
            ("secret/key.png", "denied"),
            ("out.png", linked),
            ("dir-link/x.png", linked),
            ("cache.db", "denied"),
            ("db-link.png", linked),
            ("store.png", linked),
        ];
        assert_eq!(
            omitted(&response),
            expected
                .into_iter()
                .map(|(path, reason)| (path.to_string(), reason.to_string()))
                .collect::<Vec<_>>()
        );
        // The open-counting hook saw exactly the two readable files: no
        // denied file, symlink target outside the root, or database was
        // ever opened.
        assert_eq!(
            opened,
            vec![f.root.join("index.html"), f.root.join("ok.png")],
            "trust {trust:?}"
        );
        assert!(!opened.iter().any(|path| path.starts_with(&f.outside)));
    }
}

#[test]
fn page_collect_entry_refusals_name_no_denied_contents() {
    let f = fixture(BindTrust::FirstParty);
    std::fs::write(f.root.join("secret/page.html"), "<p>TOP-SECRET</p>").unwrap();
    for (entry, code, reason) in [
        ("secret/page.html", "entry_unreadable", "denied"),
        ("absent.html", "entry_unreadable", "missing"),
        ("ok.png", "entry_not_html", "extension"),
    ] {
        let response = collect_with_hooks(
            call(&f, core(), collect_params(entry)),
            no_open(&|_, _| panic!("{entry}: nothing may be opened")),
        );
        assert!(!response.success);
        assert_eq!(response.data["code"], code, "{entry}");
        assert_eq!(
            response.data["detail"],
            json!({"reason": reason}),
            "{entry}"
        );
        assert!(!response.data.to_string().contains("TOP-SECRET"));
    }
}

#[test]
fn page_collect_refuses_callers_other_than_core() {
    let f = fixture(BindTrust::FirstParty);
    let store = SnapshotStore::open(&f.ctx.storage_dir());
    for principal in [
        None,
        Some(Principal::Direct),
        Some(Principal::Reserved {
            module_id: "prefrontal".into(),
        }),
        Some(Principal::Reserved {
            module_id: "aft".into(),
        }),
    ] {
        let response = collect_with_hooks(
            call(&f, principal.clone(), collect_params("index.html")),
            no_open(&|_, _| panic!("a refused caller opened a file")),
        );
        assert_eq!(
            response.data["code"], "page_caller_refused",
            "{principal:?}"
        );
        let ReadReply::Refused(response) = read(
            json!({"snapshot": "x", "path": "index.html", "offset": 0, "length": 1}),
            principal.as_ref(),
            &store,
        ) else {
            panic!("page.read answered a refused caller");
        };
        assert_eq!(response.data["code"], "page_caller_refused");
        let response = release(json!({"snapshot": "x"}), principal.as_ref(), &store);
        assert_eq!(response.data["code"], "page_caller_refused");
    }
}

#[test]
fn page_collect_unbound_session_is_refused() {
    let f = fixture(BindTrust::FirstParty);
    let response = collect_with_hooks(
        call(
            &f,
            core(),
            json!({"harness": HARNESS, "session": "other", "entry": "index.html"}),
        ),
        no_open(&|_, _| panic!("opened a file for an unbound session")),
    );
    assert_eq!(response.data["code"], SESSION_NOT_BOUND);
}

fn channel(number: u16) -> RouteChannel {
    route_key(number, 1)
}

fn no_policy(_: &RouteIdentity) -> codemode::SavedPolicyRead {
    Ok(codemode::EffectiveSavedPolicy::default())
}

#[test]
fn page_collect_routes_that_differ_are_ambiguous() {
    let (_dir, root) = test_root("page-collect-routes");
    let first = identity(&root, BindTrust::FirstParty);
    let untrusted = identity(&root, BindTrust::Untrusted);

    assert_eq!(
        choose_route(Vec::new(), Path::new("index.html"), no_policy)
            .unwrap_err()
            .code,
        SESSION_NOT_BOUND
    );
    let chosen = choose_route(
        vec![(channel(1), first.clone()), (channel(2), first.clone())],
        Path::new("index.html"),
        no_policy,
    )
    .unwrap();
    assert!(Arc::ptr_eq(&chosen.0, &first.0));

    let error = choose_route(
        vec![(channel(1), first.clone()), (channel(2), untrusted)],
        Path::new("index.html"),
        no_policy,
    )
    .unwrap_err();
    assert_eq!(error.code, SESSION_ROUTE_AMBIGUOUS);
    let fields = error.detail.as_ref().unwrap()["fields"].clone();
    assert!(
        fields.as_array().unwrap().contains(&json!("trust")),
        "{fields}"
    );

    // Equal access settings but different saved restrictions.
    let mut reads = 0;
    let error = choose_route(
        vec![(channel(1), first.clone()), (channel(2), first.clone())],
        Path::new("index.html"),
        |_| {
            reads += 1;
            Ok(if reads == 1 {
                codemode::EffectiveSavedPolicy::default()
            } else {
                codemode::EffectiveSavedPolicy::confined_for_test()
            })
        },
    )
    .unwrap_err();
    assert_eq!(error.code, SESSION_ROUTE_AMBIGUOUS);
    assert_eq!(error.detail.unwrap()["fields"], json!(["saved_policy"]));
}

#[test]
fn page_collect_prefers_the_route_bound_where_the_page_is() {
    let (_a_dir, root_a) = test_root("page-collect-a");
    let (_b_dir, root_b) = test_root("page-collect-b");
    let a = identity(&root_a, BindTrust::FirstParty);
    let b = identity(&root_b, BindTrust::Untrusted);
    let routes = vec![(channel(1), a.clone()), (channel(2), b.clone())];
    let in_b = root_b.as_path().join("index.html");
    let chosen = choose_route(routes.clone(), &in_b, no_policy).unwrap();
    assert!(Arc::ptr_eq(&chosen.0, &b.0));
    // A relative page names no root, so the routes must agree.
    assert_eq!(
        choose_route(routes, Path::new("index.html"), no_policy)
            .unwrap_err()
            .code,
        SESSION_ROUTE_AMBIGUOUS
    );
}

#[test]
fn page_collect_session_routes_skip_other_sessions_and_codemode_runs() {
    let (_dir, root) = test_root("page-collect-filter");
    let mine = identity(&root, BindTrust::FirstParty);
    let other_session = route_identity_with_harness(&root, "other", BindTrust::FirstParty, HARNESS);
    let other_harness = route_identity_with_harness(&root, SESSION, BindTrust::FirstParty, "pi");
    let mut run = identity(&root, BindTrust::FirstParty);
    Arc::get_mut(&mut run.0).unwrap().call_mode = subc_protocol::call_mode::CallMode::Codemode;
    let routes = HashMap::from([
        (channel(3), mine.clone()),
        (channel(1), mine.clone()),
        (channel(2), other_session),
        (channel(4), other_harness),
        (channel(5), run),
    ]);
    let found = session_routes(&routes, HARNESS, SESSION)
        .into_iter()
        .map(|(channel, _)| channel.channel)
        .collect::<Vec<_>>();
    assert_eq!(found, vec![1, 3]);
}

#[test]
fn page_collect_snapshot_round_trip_through_the_handlers() {
    let f = fixture(BindTrust::FirstParty);
    let store = Arc::new(SnapshotStore::open(&f.ctx.storage_dir()));
    let mut call = call(&f, core(), collect_params("index.html"));
    call.store = Arc::clone(&store);
    let response = collect(call);
    assert!(response.success, "{:?}", response.data);
    let snapshot = response.data["snapshot"].as_str().unwrap().to_string();
    let ReadReply::Bytes(bytes) = read(
        json!({"snapshot": snapshot, "path": "ok.png", "offset": 1, "length": 10}),
        core().as_ref(),
        &store,
    ) else {
        panic!("page.read refused");
    };
    assert_eq!(bytes, b"K");
    // The snapshot answers even after the project file changes.
    std::fs::write(f.root.join("ok.png"), "CHANGED").unwrap();
    let ReadReply::Bytes(bytes) = read(
        json!({"snapshot": snapshot, "path": "ok.png", "length": 10}),
        core().as_ref(),
        &store,
    ) else {
        panic!("page.read refused");
    };
    assert_eq!(bytes, b"OK");
    for (params, code) in [
        (
            json!({"snapshot": snapshot, "path": "secret/key.png", "length": 1}),
            "not_in_snapshot",
        ),
        (
            json!({"snapshot": snapshot, "path": "ok.png", "offset": 3, "length": 1}),
            "bad_range",
        ),
        (
            json!({"snapshot": snapshot, "path": "ok.png", "length": 1_048_577}),
            "bad_range",
        ),
        (
            json!({"snapshot": "0000", "path": "ok.png", "length": 1}),
            "snapshot_expired",
        ),
    ] {
        let ReadReply::Refused(response) = read(params.clone(), core().as_ref(), &store) else {
            panic!("{params}: answered");
        };
        assert_eq!(response.data["code"], code, "{params}");
    }
    let released = release(json!({"snapshot": snapshot}), core().as_ref(), &store);
    assert_eq!(
        released.data,
        json!({"snapshot": snapshot, "released": true})
    );
    let again = release(json!({"snapshot": snapshot}), core().as_ref(), &store);
    assert_eq!(again.data, json!({"snapshot": snapshot, "released": false}));
}

/// A symlink the page names is re-pointed at a database after its policy
/// check and before it is resolved. The resolved-path storage floor must
/// refuse it before the open: opening and closing a descriptor on a live
/// SQLite file drops this process's locks on it.
#[cfg(unix)]
#[test]
fn page_collect_symlink_swapped_to_a_database_after_the_check_is_never_opened() {
    use std::os::unix::fs::symlink;
    let f = fixture(BindTrust::FirstParty);
    let storage_db = f.ctx.storage_dir().join("aft.db");
    for target in [f.root.join("cache.db"), storage_db] {
        let link = f.root.join("swap.png");
        let _ = std::fs::remove_file(&link);
        symlink(f.root.join("ok.png"), &link).unwrap();
        std::fs::write(f.root.join("swap.html"), r#"<img src="swap.png">"#).unwrap();
        let swapped = Mutex::new(false);
        let swap = |path: &Path| {
            if path.ends_with("swap.png") {
                std::fs::remove_file(path).unwrap();
                symlink(&target, path).unwrap();
                *swapped.lock().unwrap() = true;
            }
        };
        let opened = Mutex::new(Vec::new());
        let open = |root: &Path, path: &Path| {
            opened.lock().unwrap().push(path.to_path_buf());
            file_read_exact::open_no_follow(root, path)
        };
        let response = collect_with_hooks(
            call(&f, core(), collect_params("swap.html")),
            ReadHooks {
                open: &open,
                after_check: Some(&swap),
            },
        );
        assert!(*swapped.lock().unwrap(), "the swap hook never ran");
        assert!(response.success, "{:?}", response.data);
        assert_eq!(
            omitted(&response),
            vec![("swap.png".to_string(), "denied".to_string())],
            "{}",
            target.display()
        );
        assert_eq!(
            opened.into_inner().unwrap(),
            vec![f.root.join("swap.html")],
            "a database was opened through the swapped link {}",
            target.display()
        );
    }
}

/// The real `read` tool handler, as production dispatch runs it.
fn read_dispatch(request: RawRequest, ctx: &AppContext) -> Response {
    crate::commands::read::handle_read(&request, ctx)
}

/// The routes the parity test compares on: a first-party route whose
/// questions go to a plugin prompt, an untrusted route, and a confined
/// scoped worker of each trust.
#[derive(Clone, Copy, Debug)]
enum ParityRoute {
    FirstPartyPlugin,
    Untrusted,
    ConfinedFirstParty,
    ConfinedUntrusted,
}

struct ParityFixture {
    _dirs: Vec<tempfile::TempDir>,
    _temp: crate::access_policy::temp::RootsOverride,
    ctx: Arc<AppContext>,
    executor: Arc<Executor>,
    identity: RouteIdentity,
    paths: Vec<String>,
}

/// One project root and the paths its page names: an allowed file, a
/// read_deny folder, symlinks inside and out of the root, a file outside
/// the root that the policy would ask about, and a file in a directory the
/// policy treats as system temp (exempt for a plugin host's own reads).
fn parity_fixture(route: ParityRoute) -> ParityFixture {
    let (root_dir, root_id) = test_root("page-parity-root");
    let outside_dir = tempfile::tempdir().unwrap();
    let scratch_dir = tempfile::tempdir().unwrap();
    let root = root_id.as_path().to_path_buf();
    let outside = outside_dir.path().canonicalize().unwrap();
    let scratch = scratch_dir.path().canonicalize().unwrap();
    // Only the scratch directory counts as temp, so the outside file is a
    // real outside-the-root path, not a temp exemption.
    let temp = crate::access_policy::temp::override_roots_for_test(vec![scratch.clone()]);
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.clone());
    config.sandbox.read_deny = vec![root.join("secret")];
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        config,
    ));
    std::fs::write(root.join("ok.txt"), "ok").unwrap();
    std::fs::create_dir(root.join("secret")).unwrap();
    std::fs::write(root.join("secret/key.txt"), "key").unwrap();
    std::fs::write(outside.join("outside.txt"), "outside").unwrap();
    std::fs::write(scratch.join("scratch.txt"), "scratch").unwrap();
    let relative = |dir: &Path, file: &str| {
        format!("../{}/{file}", dir.file_name().unwrap().to_str().unwrap())
    };
    let mut paths = vec![
        "ok.txt".to_string(),
        "secret/key.txt".to_string(),
        relative(&outside, "outside.txt"),
        relative(&scratch, "scratch.txt"),
    ];
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join("ok.txt"), root.join("in-link.txt")).unwrap();
        std::os::unix::fs::symlink(outside.join("outside.txt"), root.join("out-link.txt")).unwrap();
        paths.extend(["in-link.txt".to_string(), "out-link.txt".to_string()]);
    }
    let page = paths
        .iter()
        .map(|path| format!(r#"<img src="{path}">"#))
        .collect::<String>();
    std::fs::write(root.join("index.html"), format!("<!doctype html>{page}")).unwrap();

    let (trust, harness, confined) = match route {
        ParityRoute::FirstPartyPlugin => (BindTrust::FirstParty, "opencode", false),
        ParityRoute::Untrusted => (BindTrust::Untrusted, HARNESS, false),
        ParityRoute::ConfinedFirstParty => (BindTrust::FirstParty, HARNESS, true),
        ParityRoute::ConfinedUntrusted => (BindTrust::Untrusted, HARNESS, true),
    };
    let mut identity = route_identity_with_harness(&root_id, SESSION, trust, harness);
    if confined {
        // A scoped worker whose saved policy is the root confinement fence.
        Arc::get_mut(&mut identity.0).unwrap().scope = Some(
            serde_json::from_value(json!({
                "owner": {"kind": "direct"}, "ref": "parity-worker", "scope_epoch": 1,
                "kind": "worker", "owner_authorized": true,
            }))
            .unwrap(),
        );
        ctx.remember_confined_policy_key(remote_policy::key(&identity, Some("worker")).unwrap());
    }
    let executor = Arc::new(Executor::new());
    executor.register_actor(root_id, Arc::clone(&ctx));
    ParityFixture {
        _dirs: vec![root_dir, outside_dir, scratch_dir],
        _temp: temp,
        ctx,
        executor,
        identity,
        paths,
    }
}

/// Whether the `read` tool, run through the real tool-call job with the
/// route's own ask route, reads `path`.
async fn read_tool_allows(f: &ParityFixture, path: &str, request: usize) -> bool {
    let request_id = format!("parity-read-{request}");
    let arguments = json!({ "path": path });
    let run = PlainToolRun {
        bare_name: "read".into(),
        arguments: arguments.clone(),
        format_context: Default::default(),
        tool_call_context: ToolCallContext {
            project_root: f.identity.project_root.clone(),
            session_id: Some(f.identity.session.clone()),
            request_id: request_id.clone(),
            diagnostics_on_edit: false,
            preview: false,
            edit_slot_survives: None,
            report_registration_downgrade: false,
            standard_edit_grammar: true,
            disabled_tools: None,
            worker_session: false,
            host_tool_names: None,
        },
        dispatch: read_dispatch,
        bind_trust: f.identity.trust,
        identity: f.identity.clone(),
        request_id,
        ledger_key: None,
        io_budget: crate::bounded_io::Budget::after(Duration::from_secs(30)),
        approved_io_window: None,
        reply_deadline: Instant::now() + Duration::from_secs(30),
    };
    let (done, completion) = oneshot::channel();
    let job = run.job(
        access_grants::ask_route_for_call(&f.identity, "read", &arguments),
        PhaseTrace::new(Instant::now()),
        done,
    );
    let response = job(&f.ctx);
    completion.await.unwrap();
    response.success
}

/// Whether `page.collect` of the fixture's page puts `path` in the bundle.
fn page_collect_verdicts(f: &ParityFixture) -> HashMap<String, bool> {
    let call = CollectCall {
        params: json!({
            "harness": f.identity.harness, "session": SESSION, "entry": "index.html",
        }),
        principal: core(),
        routes: HashMap::from([(route_key(1, 1), f.identity.clone())]),
        executor: Arc::clone(&f.executor),
        store: Arc::new(SnapshotStore::open(&f.ctx.storage_dir())),
    };
    let response = collect(call);
    assert!(response.success, "{:?}", response.data);
    let mut verdicts = HashMap::new();
    for file in response.data["files"].as_array().unwrap() {
        verdicts.insert(file["path"].as_str().unwrap().to_string(), true);
    }
    for omitted in response.data["omitted"].as_array().unwrap() {
        assert_eq!(omitted["reason"], "denied", "{omitted}");
        verdicts.insert(omitted["path"].as_str().unwrap().to_string(), false);
    }
    verdicts
}

/// For every path of one fixture, on each route, `page.collect` includes
/// exactly the files the session's `read` tool reads: both take their
/// per-call policy from `call_policy::with_tool_call_policy`.
#[tokio::test]
async fn page_collect_reads_exactly_what_the_read_tool_reads() {
    let mut request = 0;
    for route in [
        ParityRoute::FirstPartyPlugin,
        ParityRoute::Untrusted,
        ParityRoute::ConfinedFirstParty,
        ParityRoute::ConfinedUntrusted,
    ] {
        let f = parity_fixture(route);
        let mut read = Vec::new();
        for path in &f.paths {
            request += 1;
            read.push((path.clone(), read_tool_allows(&f, path, request).await));
        }
        let collected = page_collect_verdicts(&f);
        let collected = f
            .paths
            .iter()
            .map(|path| (path.clone(), collected[path.as_str()]))
            .collect::<Vec<_>>();
        println!("{route:?}: {read:?}");
        assert_eq!(read, collected, "{route:?}: read tool vs page.collect");
        // The fixture separates allowed from denied on every route.
        assert!(
            read.iter().any(|(_, allowed)| *allowed),
            "{route:?}: {read:?}"
        );
        assert!(
            read.iter().any(|(_, allowed)| !*allowed),
            "{route:?}: {read:?}"
        );
    }
}
