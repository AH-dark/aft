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
    let response = collect_with_open(call(f, core(), collect_params("index.html")), &open);
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
        let response = collect_with_open(call(&f, core(), collect_params(entry)), &|_, _| {
            panic!("{entry}: nothing may be opened")
        });
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
        let response = collect_with_open(
            call(&f, principal.clone(), collect_params("index.html")),
            &|_, _| panic!("a refused caller opened a file"),
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
    let response = collect_with_open(
        call(
            &f,
            core(),
            json!({"harness": HARNESS, "session": "other", "entry": "index.html"}),
        ),
        &|_, _| panic!("opened a file for an unbound session"),
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
