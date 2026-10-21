use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use crate::context::AppContext;
use crate::protocol::RawRequest;

/// Read manifests retain the caller's project-root spelling for permission checks.
/// Import resolution may canonicalize paths, so returning them unchanged would
/// make the permission path differ from the one supplied by the caller.
pub(crate) struct ReadManifestPaths {
    canonical_root: Option<PathBuf>,
    requested_root: Option<PathBuf>,
}

impl ReadManifestPaths {
    pub(crate) fn for_request(req: &RawRequest, ctx: &AppContext) -> Self {
        let Some((request_path, is_directory)) = request_path(req) else {
            return Self {
                canonical_root: None,
                requested_root: None,
            };
        };
        let requested_path = ctx.resolve_relative_path(Path::new(request_path));
        let Some(canonical_target) = canonical_path(&requested_path) else {
            return Self {
                canonical_root: None,
                requested_root: None,
            };
        };
        let requested_anchor = if is_directory {
            requested_path
        } else {
            requested_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf()
        };
        let canonical_anchor = if is_directory {
            canonical_target
        } else {
            let Some(parent) = canonical_target.parent() else {
                return Self {
                    canonical_root: None,
                    requested_root: None,
                };
            };
            parent.to_path_buf()
        };

        let configured_root = ctx.config().project_root.clone();
        let configured_root = configured_root.as_deref().and_then(canonical_path);
        let (canonical_root, requested_root) = configured_root
            .filter(|root| canonical_anchor.starts_with(root))
            .and_then(|root| {
                let relative = relative_path(&canonical_anchor, &root)?;
                let requested_root = normalize_path(&requested_anchor.join(relative));
                Some((root, requested_root))
            })
            .unwrap_or_else(|| {
                (
                    normalize_path(&canonical_anchor),
                    normalize_path(&requested_anchor),
                )
            });

        Self {
            canonical_root: Some(canonical_root),
            requested_root: Some(requested_root),
        }
    }

    pub(crate) fn map(&self, path: &Path) -> PathBuf {
        let normalized = normalize_path(path);
        let Some(canonical_root) = &self.canonical_root else {
            return normalized;
        };
        let Some(requested_root) = &self.requested_root else {
            return normalized;
        };
        let Some(canonical_path) = canonical_path(path) else {
            return normalized;
        };
        let Ok(relative) = canonical_path.strip_prefix(canonical_root) else {
            return normalized;
        };
        normalize_path(&requested_root.join(relative))
    }

    pub(crate) fn map_set<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a PathBuf>,
    ) -> BTreeSet<PathBuf> {
        paths.into_iter().map(|path| self.map(path)).collect()
    }
}

pub(crate) fn normalize_response_content_files(
    req: &RawRequest,
    ctx: &AppContext,
    data: &mut serde_json::Value,
) {
    let Some(paths) = data
        .get("content_files")
        .and_then(serde_json::Value::as_array)
    else {
        return;
    };
    let paths: BTreeSet<_> = paths
        .iter()
        .filter_map(serde_json::Value::as_str)
        .map(PathBuf::from)
        .collect();
    let mapper = ReadManifestPaths::for_request(req, ctx);
    data["content_files"] = serde_json::json!(mapper.map_set(paths.iter()));
}

fn request_path(req: &RawRequest) -> Option<(&str, bool)> {
    let params = &req.params;
    if let Some(directory) = params.get("directory").and_then(serde_json::Value::as_str) {
        return local_path(directory).map(|path| (path, true));
    }
    for key in ["file", "target", "url"] {
        if let Some(value) = params.get(key) {
            if let Some(path) = value.as_str().and_then(local_path) {
                return Some((path, false));
            }
            if let Some(path) = value.as_array().and_then(|items| {
                items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .find_map(local_path)
            }) {
                return Some((path, false));
            }
        }
    }
    if let Some(path) = params
        .get("targets")
        .and_then(serde_json::Value::as_array)
        .and_then(|targets| {
            targets.iter().find_map(|target| {
                target
                    .get("file")
                    .and_then(serde_json::Value::as_str)
                    .and_then(local_path)
            })
        })
    {
        return Some((path, false));
    }
    params
        .get("files")
        .and_then(serde_json::Value::as_array)
        .and_then(|files| {
            files
                .iter()
                .filter_map(serde_json::Value::as_str)
                .find_map(local_path)
        })
        .map(|path| (path, false))
}

fn local_path(path: &str) -> Option<&str> {
    (!path.starts_with("http://") && !path.starts_with("https://")).then_some(path)
}

fn canonical_path(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path)
        .ok()
        .map(|path| normalize_path(&path))
}

fn normalize_path(path: &Path) -> PathBuf {
    crate::inspect::job::normalize_path(path)
}

fn relative_path(from: &Path, to: &Path) -> Option<PathBuf> {
    let from = normalize_path(from);
    let to = normalize_path(to);
    let from: Vec<_> = from.components().collect();
    let to: Vec<_> = to.components().collect();
    let common = from
        .iter()
        .zip(&to)
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = PathBuf::new();
    for component in &from[common..] {
        match component {
            Component::Normal(_) => relative.push(".."),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    for component in &to[common..] {
        relative.push(component.as_os_str());
    }
    Some(relative)
}
