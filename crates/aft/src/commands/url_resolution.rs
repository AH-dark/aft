//! Shared local-path and remote-document resolution for read-only commands.

use std::path::{Path, PathBuf};

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use crate::url_fetch::{fetch_url_to_cache, is_http_url, UrlFetchError, UrlFetchOptions};

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
        .map_err(|error| Response::error(&req.id, "url_fetch_failed", error.to_string()));
    }
    ctx.validate_read_path(&req.id, req.session(), Path::new(file))
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
