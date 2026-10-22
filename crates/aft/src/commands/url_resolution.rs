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
        let storage_dir = ctx.cache_dir();
        // Trusted configuration sets the private-network ceiling; a request
        // can narrow that permission but cannot grant it.
        let allow_private = config.url_fetch_allow_private
            && req
                .params
                .get("allow_private")
                .and_then(|value| value.as_bool())
                .unwrap_or(true);
        let options = UrlFetchOptions {
            allow_private,
            ..Default::default()
        };
        // While the cache location is unavailable (its volume is missing, or
        // the user config that names it is unreadable), `cache_dir()` falls
        // back to durable storage, which caches must not fill. Serve a fresh
        // copy already cached there, and refuse to download anything new.
        let fetched = match ctx.cache_unavailable_reason() {
            Some(reason) => {
                match crate::url_fetch::cached_url_document(file, &storage_dir, &options) {
                    Ok(Some(cached)) => Ok(cached),
                    Ok(None) => {
                        return Err(Response::error(
                            &req.id,
                            crate::root_cache::CACHE_DIR_UNAVAILABLE,
                            format!(
                                "URL download refused while the cache location is unavailable: {reason}"
                            ),
                        ))
                    }
                    Err(error) => Err(error),
                }
            }
            None => fetch(file, &storage_dir, options),
        };
        return fetched
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

    /// A context whose cache location is unknown because the user config was
    /// unreadable; `cache_dir()` then names the durable storage root.
    fn cache_unavailable_ctx(storage: &Path) -> AppContext {
        AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                storage_dir: Some(storage.to_path_buf()),
                cache_config_unknown: Some("user config is not valid JSONC".into()),
                ..Default::default()
            },
        )
    }

    fn outline_request(url: &str) -> RawRequest {
        serde_json::from_value(serde_json::json!({
            "id": "cache-off-url",
            "command": "outline",
            "file": url,
        }))
        .unwrap()
    }

    #[test]
    fn url_download_is_refused_without_writing_while_the_cache_location_is_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let ctx = cache_unavailable_ctx(&storage);
        assert!(ctx.cache_unavailable_reason().is_some());
        // An address literal keeps the check off the network.
        let url = "https://93.184.216.34/docs.md";
        let refused = resolve_with_fetch(&outline_request(url), &ctx, url, |_, _, _| {
            panic!("a download started while the cache location is unavailable")
        })
        .expect_err("a fresh download must be refused");
        assert_eq!(
            refused.data["code"],
            crate::root_cache::CACHE_DIR_UNAVAILABLE
        );
        let message = refused.data["message"].as_str().unwrap();
        assert!(message.contains("cache_dir_config_unknown"), "{message}");
        assert_eq!(ctx.cache_dir(), storage);
        assert!(
            !storage.join("url_cache").exists(),
            "the refusal wrote a URL cache under durable storage"
        );
    }

    #[test]
    fn url_already_cached_still_serves_while_the_cache_location_is_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let ctx = cache_unavailable_ctx(&storage);
        let url = "https://93.184.216.34/docs.md";
        let cache = ctx.cache_dir();
        let content = crate::url_fetch::cache_content_path_for_url(&cache, url, ".md");
        std::fs::create_dir_all(content.parent().unwrap()).unwrap();
        std::fs::write(&content, "# cached").unwrap();
        let fetched_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        std::fs::write(
            crate::url_fetch::cache_meta_path_for_url(&cache, url),
            serde_json::json!({
                "url": url,
                "contentType": "text/markdown",
                "extension": ".md",
                "fetchedAt": fetched_at,
                "downloaded_bytes": 8,
            })
            .to_string(),
        )
        .unwrap();
        let served = resolve_with_fetch(&outline_request(url), &ctx, url, |_, _, _| {
            panic!("a download started while the cache location is unavailable")
        })
        .expect("the cached copy must still serve");
        assert_eq!(served, content);
    }

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
