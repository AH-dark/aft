//! Shared local-path and remote-document resolution for read-only commands.

use std::path::{Path, PathBuf};

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use crate::url_fetch::{
    fetch_url_to_cache, is_http_url, UrlFetchError, UrlFetchOptions, URL_REPLY_MARGIN,
};
pub(super) fn resolve_file_or_url(
    req: &RawRequest,
    ctx: &AppContext,
    file: &str,
) -> Result<PathBuf, Response> {
    resolve_with_fetch(req, ctx, file, fetch_url_to_cache)
}

fn resolve_with_fetch(
    req: &RawRequest,
    ctx: &AppContext,
    file: &str,
    fetch: impl FnOnce(&str, &Path, UrlFetchOptions) -> Result<PathBuf, UrlFetchError>,
) -> Result<PathBuf, Response> {
    if is_http_url(file) {
        let config = ctx.config();
        let storage_dir = crate::bash_background::storage_dir(config.storage_dir.as_deref());
        // Trusted configuration sets the private-network ceiling; a request
        // can narrow that permission but cannot grant it.
        let allow_private = config.url_fetch_allow_private
            && req
                .params
                .get("allow_private")
                .and_then(|value| value.as_bool())
                .unwrap_or(true);
        return fetch(
            file,
            &storage_dir,
            UrlFetchOptions {
                allow_private,
                ..Default::default()
            },
        )
        .map(|path| {
            // The download stopped early enough to leave time for reading the
            // document back. Those reads must not use the reply margin too: a
            // stalled read then fails while the reply still reaches the caller
            // before the call's deadline.
            crate::bounded_io::keep_reply_margin(URL_REPLY_MARGIN);
            path
        })
        .map_err(|error| url_fetch_error_response(&req.id, &error));
    }
    ctx.validate_read_path(&req.id, req.session(), Path::new(file))
}

/// A failed fetch keeps its typed code; a timeout also names its phase, and
/// a failure repeated from memory says so and when a new fetch is possible.
fn url_fetch_error_response(id: &str, error: &UrlFetchError) -> Response {
    let mut data = serde_json::Map::new();
    if let Some(phase) = error.phase() {
        data.insert("phase".into(), phase.as_str().into());
    }
    if let Some(retry_after) = error.retry_after() {
        data.insert("cached".into(), true.into());
        data.insert(
            "retry_after_ms".into(),
            (retry_after.as_millis() as u64).into(),
        );
    }
    if data.is_empty() {
        return Response::error(id, error.code(), error.to_string());
    }
    Response::error_with_data(
        id,
        error.code(),
        error.to_string(),
        serde_json::Value::Object(data),
    )
}

#[cfg(test)]
mod url_tests {
    use super::*;

    #[test]
    fn private_fetch_permission_cannot_widen_trusted_ceiling() {
        for trusted in [false, true] {
            let ctx = AppContext::new(
                Box::new(crate::parser::TreeSitterProvider::new()),
                crate::config::Config {
                    url_fetch_allow_private: trusted,
                    ..Default::default()
                },
            );
            for requested in [None, Some(false), Some(true)] {
                let mut value = serde_json::json!({"id": "ceiling", "command": "outline"});
                if let Some(requested) = requested {
                    value["allow_private"] = requested.into();
                }
                let req = serde_json::from_value(value).unwrap();
                let resolved = resolve_with_fetch(
                    &req,
                    &ctx,
                    "https://example.com/doc.md",
                    |_, _, options| {
                        assert_eq!(options.allow_private, trusted && requested.unwrap_or(true));
                        Ok(PathBuf::from("cached.md"))
                    },
                );
                assert!(resolved.is_ok());
            }
        }
    }

    #[test]
    fn a_fetched_document_is_read_back_without_using_the_reply_margin() {
        let storage = tempfile::tempdir().unwrap();
        let document = storage.path().join("fetched.md");
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                storage_dir: Some(storage.path().to_path_buf()),
                ..Default::default()
            },
        );
        let req =
            serde_json::from_value(serde_json::json!({"id": "slow-fetch", "command": "outline"}))
                .unwrap();
        let budget = crate::bounded_io::Budget::after(std::time::Duration::from_secs(10));
        let call_deadline = budget.deadline;
        let _call = crate::bounded_io::enter(Some(budget));
        let fetched = || {
            resolve_with_fetch(&req, &ctx, "https://example.com/doc.md", |_, _, _| {
                std::fs::write(&document, "# Title\n").unwrap();
                Ok(document.clone())
            })
            .expect("the fetch succeeded")
        };
        let path = fetched();
        assert_eq!(
            crate::bounded_io::read_to_string(&path).unwrap(),
            "# Title\n"
        );
        let read_back_deadline = |budget: Option<crate::bounded_io::Budget>| {
            budget.expect("the call budget is still in force").deadline
        };
        assert_eq!(
            read_back_deadline(crate::bounded_io::current()),
            call_deadline - URL_REPLY_MARGIN
        );
        // A second document in the same call keeps the one margin.
        fetched();
        assert_eq!(
            read_back_deadline(crate::bounded_io::current()),
            call_deadline - URL_REPLY_MARGIN
        );
    }

    #[test]
    fn untrusted_private_flag_is_refused_by_outline_and_zoom() {
        for (trusted, requested) in [(false, true), (true, false)] {
            let ctx = AppContext::new(
                Box::new(crate::parser::TreeSitterProvider::new()),
                crate::config::Config {
                    url_fetch_allow_private: trusted,
                    ..Default::default()
                },
            );
            for command in ["outline", "zoom"] {
                let req = serde_json::from_value(serde_json::json!({"id": "private-denied", "command": command, "file": "http://127.0.0.1:0/doc.md", "symbol": "Heading", "allow_private": requested})).unwrap();
                let response = if command == "outline" {
                    super::super::outline::handle_outline(&req, &ctx)
                } else {
                    super::super::zoom::handle_zoom(&req, &ctx)
                };
                assert!(!response.success);
                assert_eq!(response.data["code"], "url_fetch_failed");
                assert!(
                    response.data["message"]
                        .as_str()
                        .unwrap()
                        .to_ascii_lowercase()
                        .contains("private"),
                    "refusal must be the network policy, not a failed connection: {:?}",
                    response.data
                );
            }
        }
    }
}
