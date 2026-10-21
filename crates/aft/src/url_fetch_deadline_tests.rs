//! Deadline behaviour of URL fetches against local servers that stall in one
//! phase. Each test runs the fetch under a short call budget, the same way a
//! tool call does, and checks the typed timeout and how long it took.

use super::*;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Starts a loopback server. `handler` gets each connection after the request
/// head has been read, together with the request path.
fn serve(handler: impl Fn(&str, TcpStream) + Send + Sync + 'static) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let address = listener.local_addr().expect("test server address");
    let handler = Arc::new(handler);
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let handler = Arc::clone(&handler);
            thread::spawn(move || {
                let mut stream = stream;
                let path = read_request_path(&mut stream);
                handler(&path, stream);
            });
        }
    });
    address
}

fn read_request_path(stream: &mut TcpStream) -> String {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut head = Vec::new();
    let mut buffer = [0u8; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => head.extend_from_slice(&buffer[..n]),
        }
    }
    String::from_utf8_lossy(&head)
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string()
}

fn url(address: SocketAddr, path: &str) -> String {
    format!("http://127.0.0.1:{}{path}", address.port())
}

fn local() -> UrlFetchOptions {
    UrlFetchOptions {
        allow_private: true,
        ..UrlFetchOptions::default()
    }
}

/// Runs one fetch inside a call budget of `budget`, as a tool call would.
fn fetch_within(
    budget: Duration,
    url: &str,
    storage: &Path,
    options: UrlFetchOptions,
) -> (Result<PathBuf, UrlFetchError>, Duration) {
    let _call = crate::bounded_io::enter(Some(crate::bounded_io::Budget::after(budget)));
    let started = Instant::now();
    let result = fetch_url_to_cache(url, storage, options);
    (result, started.elapsed())
}

/// Long enough that a fetch which ignored its deadline could not finish a
/// test in time, short enough that leftover server threads end soon.
const SERVER_STALL: Duration = Duration::from_secs(60);

#[test]
fn fetch_deadline_is_the_call_budget_less_the_margin_capped_at_the_ceiling() {
    let near = |deadline: FetchDeadline, expected: Duration| {
        let allowed = deadline.allowed;
        assert!(
            allowed <= expected && allowed + Duration::from_millis(200) >= expected,
            "allowed {allowed:?}, expected about {expected:?}"
        );
    };
    near(FetchDeadline::for_current_call(), URL_FETCH_CEILING);
    for (budget, expected) in [
        (Duration::from_secs(3), Duration::from_secs(2)),
        (Duration::from_secs(60), URL_FETCH_CEILING),
        (Duration::from_millis(500), Duration::ZERO),
    ] {
        let _call = crate::bounded_io::enter(Some(crate::bounded_io::Budget::after(budget)));
        near(FetchDeadline::for_current_call(), expected);
    }
}

#[test]
fn header_stall_times_out_in_the_headers_phase_within_the_call_budget() {
    let storage = tempfile::tempdir().unwrap();
    let address = serve(|_, _stream| thread::sleep(SERVER_STALL));
    let (result, elapsed) = fetch_within(
        Duration::from_secs(3),
        &url(address, "/silent.md"),
        storage.path(),
        local(),
    );
    let error = result.expect_err("a server that never answers must fail");
    assert_eq!(error.code(), "url_fetch_timeout", "{error}");
    assert_eq!(error.phase(), Some(FetchPhase::Headers), "{error}");
    assert!(error.to_string().contains("headers phase"), "{error}");
    assert!(
        elapsed < Duration::from_secs(3),
        "the fetch outlived its call budget: {elapsed:?}"
    );
}

#[test]
fn body_drip_stops_at_the_whole_body_deadline() {
    let storage = tempfile::tempdir().unwrap();
    // Each gap is far below the per-chunk stall limit, so only a deadline for
    // the whole body can stop this response.
    let address = serve(|_, mut stream| {
        let _ = stream.write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/markdown\r\nconnection: close\r\n\r\n",
        );
        let started = Instant::now();
        while started.elapsed() < SERVER_STALL {
            if stream.write_all(b"## drip\n\nline\n\n").is_err() {
                return;
            }
            thread::sleep(Duration::from_millis(200));
        }
    });
    let (result, elapsed) = fetch_within(
        Duration::from_secs(3),
        &url(address, "/drip.md"),
        storage.path(),
        local(),
    );
    let error = result.expect_err("an endless body must fail at the deadline");
    assert_eq!(error.code(), "url_fetch_timeout", "{error}");
    assert_eq!(error.phase(), Some(FetchPhase::Body), "{error}");
    assert!(
        elapsed < Duration::from_secs(3),
        "the body read outlived its call budget: {elapsed:?}"
    );
}

/// A server that accepts and closes every connection without answering, which
/// reqwest reports as a transient failure worth a retry.
fn hang_up_server() -> (SocketAddr, Arc<AtomicUsize>) {
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    let address = serve(move |_, stream| {
        counter.fetch_add(1, Ordering::SeqCst);
        drop(stream);
    });
    (address, accepted)
}

#[test]
fn transient_failures_retry_while_the_backoff_and_an_attempt_fit() {
    let storage = tempfile::tempdir().unwrap();
    let (address, accepted) = hang_up_server();
    let (result, _) = fetch_within(
        Duration::from_secs(10),
        &url(address, "/closed.md"),
        storage.path(),
        local(),
    );
    assert!(result.is_err());
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        1 + TRANSIENT_RETRY_ATTEMPTS
    );
}

#[test]
fn transient_failures_do_not_retry_when_the_backoff_and_an_attempt_do_not_fit() {
    let storage = tempfile::tempdir().unwrap();
    let (address, accepted) = hang_up_server();
    // 1.6 s of budget leaves a 0.6 s fetch deadline: less than the first
    // backoff plus one useful attempt.
    let (result, elapsed) = fetch_within(
        Duration::from_millis(1_600),
        &url(address, "/closed.md"),
        storage.path(),
        local(),
    );
    assert!(result.is_err());
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
    assert!(elapsed < Duration::from_millis(600), "{elapsed:?}");
}

#[test]
fn outline_reports_the_typed_timeout_and_its_phase() {
    let storage = tempfile::tempdir().unwrap();
    let address = serve(|_, _stream| thread::sleep(SERVER_STALL));
    let ctx = crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            url_fetch_allow_private: true,
            storage_dir: Some(storage.path().to_path_buf()),
            ..Default::default()
        },
    );
    let req = serde_json::from_value(serde_json::json!({
        "id": "silent-url",
        "command": "outline",
        "file": url(address, "/silent.md"),
    }))
    .unwrap();
    let _call = crate::bounded_io::enter(Some(crate::bounded_io::Budget::after(
        Duration::from_secs(3),
    )));
    let started = Instant::now();
    let response = crate::commands::outline::handle_outline(&req, &ctx);
    assert!(!response.success);
    assert_eq!(
        response.data["code"], "url_fetch_timeout",
        "{:?}",
        response.data
    );
    assert_eq!(response.data["phase"], "headers", "{:?}", response.data);
    assert!(started.elapsed() < Duration::from_secs(3));
}
