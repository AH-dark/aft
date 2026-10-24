//! Collect an HTML page and the files it references, for a sealed page
//! bundle.
//!
//! The phone renders HTML pages agents write, reading files with the user's
//! own authority. So that a page can reach only what its agent could read,
//! core asks AFT (`page.collect`) to read the page and every file its HTML
//! and stylesheets name, under that agent session's read policy, into a
//! private snapshot. Core then reads the snapshot back (`page.read`), seals
//! it, and releases it (`page.release`). The phone renders only from the
//! sealed manifest.
//!
//! This module holds the parts that do not depend on the subc connection:
//! the limits, how a reference becomes a path, the collection walk over a
//! [`PageSource`] (the session-policy reader lives in `subc::page_collect`),
//! and the snapshot store.

pub(crate) mod scan;
pub(crate) mod snapshot;

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::{json, Value};

use scan::{RefKind, Reference};

pub(crate) const COLLECT_OPERATION: &str = "page.collect";
pub(crate) const READ_OPERATION: &str = "page.read";
pub(crate) const RELEASE_OPERATION: &str = "page.release";
/// HELLO capability: this module serves the page-bundle operations.
pub(crate) const CAPABILITY: &str = "page-collect/v1";

/// Largest `page.read` length.
pub(crate) const MAX_READ_LENGTH: u64 = 1024 * 1024;

/// Most `omitted` entries one reply lists; the rest are only counted.
pub(crate) const MAX_OMITTED: usize = 1024;

/// The bounds one `page.collect` runs under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Limits {
    pub entry_max_bytes: u64,
    pub max_files: u64,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub import_depth: u64,
    pub deadline_ms: u64,
}

impl Limits {
    /// What core sends today; used for any limit a request leaves out.
    pub(crate) const DEFAULT: Limits = Limits {
        entry_max_bytes: 2 * 1024 * 1024,
        max_files: 64,
        max_file_bytes: 8 * 1024 * 1024,
        max_total_bytes: 20 * 1024 * 1024,
        import_depth: 4,
        deadline_ms: 5_000,
    };

    /// AFT's own upper bounds: a request asking for more gets these.
    pub(crate) const CEILING: Limits = Limits {
        entry_max_bytes: 8 * 1024 * 1024,
        max_files: 256,
        max_file_bytes: 32 * 1024 * 1024,
        max_total_bytes: 64 * 1024 * 1024,
        import_depth: 8,
        deadline_ms: 30_000,
    };

    /// The limits a request names, each capped at [`Limits::CEILING`].
    /// A missing field takes [`Limits::DEFAULT`]; a field that is not a
    /// non-negative integer, or `max_files` of zero (no room for the page
    /// itself), is an error.
    pub(crate) fn from_request(value: Option<&Value>) -> Result<Limits, String> {
        let object = match value {
            None | Some(Value::Null) => return Ok(Self::DEFAULT),
            Some(Value::Object(object)) => object,
            Some(_) => return Err("limits must be an object".into()),
        };
        let field = |name: &str, default: u64, ceiling: u64| -> Result<u64, String> {
            match object.get(name) {
                None | Some(Value::Null) => Ok(default.min(ceiling)),
                Some(value) => value
                    .as_u64()
                    .map(|value| value.min(ceiling))
                    .ok_or_else(|| format!("limits.{name} must be a non-negative integer")),
            }
        };
        let (d, c) = (Self::DEFAULT, Self::CEILING);
        let limits = Limits {
            entry_max_bytes: field("entry_max_bytes", d.entry_max_bytes, c.entry_max_bytes)?,
            max_files: field("max_files", d.max_files, c.max_files)?,
            max_file_bytes: field("max_file_bytes", d.max_file_bytes, c.max_file_bytes)?,
            max_total_bytes: field("max_total_bytes", d.max_total_bytes, c.max_total_bytes)?,
            import_depth: field("import_depth", d.import_depth, c.import_depth)?,
            deadline_ms: field("deadline_ms", d.deadline_ms, c.deadline_ms)?,
        };
        if limits.max_files == 0 {
            return Err("limits.max_files must be at least 1 (the page itself)".into());
        }
        Ok(limits)
    }
}

/// Why a referenced file is not in the bundle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OmitReason {
    /// The session's read policy refuses it (or it is an AFT database).
    Denied,
    /// No readable regular file is there, or the reference names no file.
    Missing,
    /// It has a scheme, a host or an absolute path; never fetched.
    NotRelative,
    OverFileLimit,
    OverTotalLimit,
    OverCountLimit,
    /// A stylesheet nested deeper than `import_depth`.
    DepthLimit,
    Deadline,
}

impl OmitReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Denied => "denied",
            Self::Missing => "missing",
            Self::NotRelative => "not_relative",
            Self::OverFileLimit => "over_file_limit",
            Self::OverTotalLimit => "over_total_limit",
            Self::OverCountLimit => "over_count_limit",
            Self::DepthLimit => "depth_limit",
            Self::Deadline => "deadline",
        }
    }
}

/// How a reference resolves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Resolved {
    /// It names the referring document itself (empty, `#frag`, `?query`).
    Skip,
    NotRelative,
    /// It cannot name a file: undecodable, a directory, or a decoded
    /// segment that would change the path's shape.
    Invalid,
    /// Path segments relative to the page's folder. Leading `..` segments
    /// mean the file is above that folder.
    Path(Vec<String>),
}

/// Resolve `raw` against `base`, the referring file's folder as segments
/// relative to the page's folder. The fragment and query are dropped, each
/// segment is percent-decoded, and `.`/`..` are applied lexically.
pub(crate) fn resolve_reference(raw: &str, base: &[String]) -> Resolved {
    // URL parsing removes tabs and newlines anywhere and trims spaces and
    // control characters at both ends.
    let cleaned: String = raw
        .chars()
        .filter(|character| !matches!(character, '\t' | '\n' | '\r'))
        .collect();
    let cleaned = cleaned.trim_matches(|character: char| character <= ' ');
    if cleaned.is_empty() || cleaned.starts_with('#') || cleaned.starts_with('?') {
        return Resolved::Skip;
    }
    // Browsers read a backslash in a relative URL as a slash.
    let cleaned = cleaned.replace('\\', "/");
    if cleaned.starts_with('/') || has_scheme(&cleaned) {
        return Resolved::NotRelative;
    }
    let path_part = cleaned
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .to_string();
    let mut segments: Vec<String> = base.to_vec();
    let raw_segments = path_part.split('/').collect::<Vec<_>>();
    let last = raw_segments.len() - 1;
    for (index, segment) in raw_segments.into_iter().enumerate() {
        let Some(decoded) = percent_decode(segment) else {
            return Resolved::Invalid;
        };
        // A decoded separator, NUL or drive colon would make one URL segment
        // into several path components (or a different root).
        if decoded.contains(['/', '\\', '\0', ':']) {
            return Resolved::Invalid;
        }
        match decoded.as_str() {
            "" if index == last => return Resolved::Invalid,
            "" | "." => {}
            ".." => {
                if segments.last().is_some_and(|last| last != "..") {
                    segments.pop();
                } else {
                    segments.push("..".to_string());
                }
            }
            _ => segments.push(decoded),
        }
    }
    if segments.is_empty() || segments.last().is_some_and(|last| last == "..") {
        return Resolved::Invalid;
    }
    Resolved::Path(segments)
}

fn has_scheme(value: &str) -> bool {
    let mut chars = value.chars();
    if !chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
    {
        return false;
    }
    for character in chars {
        match character {
            ':' => return true,
            character if character.is_ascii_alphanumeric() || "+-.".contains(character) => {}
            _ => return false,
        }
    }
    false
}

/// Percent-decode one URL segment into UTF-8, or `None` when the bytes are
/// not UTF-8. A `%` not followed by two hex digits is kept literally, as
/// URL parsing does.
pub(crate) fn percent_decode(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |byte: u8| (byte as char).to_digit(16);
            if let (Some(high), Some(low)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((high * 16 + low) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

/// The MIME type for a file name, by extension.
pub(crate) fn mime_for(path: &str) -> &'static str {
    let extension = path
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" | "cjs" => "text/javascript",
        "json" | "map" => "application/json",
        "txt" => "text/plain",
        "csv" => "text/csv",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

/// Whether a name has an HTML extension.
pub(crate) fn has_html_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("html") || extension.eq_ignore_ascii_case("htm")
        })
}

/// Whether bytes look like an HTML document: text, not binary, whose first
/// non-blank character (after an optional byte-order mark) opens a tag,
/// doctype or comment.
pub(crate) fn sniff_html(bytes: &[u8]) -> bool {
    if content_inspector::inspect(bytes).is_binary() {
        return false;
    }
    let text = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    text.iter()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| *byte == b'<')
}

/// Why a [`PageSource`] did not return a file's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadFailure {
    Denied,
    Missing,
    /// Larger than the limit the read was given; `size` is at least the
    /// file's size as far as it was seen (its length, or the bytes read).
    TooLarge {
        size: u64,
    },
    Deadline,
}

/// Reads files for a collection. The session-policy implementation checks
/// every path under the agent session's read rules before opening it.
pub(crate) trait PageSource {
    /// Read the whole file at `path` (absolute, lexically normalized), or
    /// fail with [`ReadFailure::TooLarge`] when it holds more than `limit`
    /// bytes.
    fn read(&mut self, path: &Path, limit: u64) -> Result<Vec<u8>, ReadFailure>;
}

/// Why `page.collect` refused. Each maps to one refusal code.
#[derive(Debug)]
pub(crate) enum CollectRefusal {
    /// The page is missing or the policy denies it. `reason` is `missing`
    /// or `denied`; nothing about a denied file's contents is reported.
    EntryUnreadable {
        reason: &'static str,
    },
    EntryTooLarge {
        size: u64,
        limit: u64,
    },
    EntryNotHtml {
        reason: &'static str,
    },
    /// The snapshot could not be written.
    Storage(std::io::Error),
}

impl CollectRefusal {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::EntryUnreadable { .. } => "entry_unreadable",
            Self::EntryTooLarge { .. } => "entry_too_large",
            Self::EntryNotHtml { .. } => "entry_not_html",
            Self::Storage(_) => "snapshot_unavailable",
        }
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::EntryUnreadable { reason } => {
                format!("the page cannot be read under this session's read policy ({reason})")
            }
            Self::EntryTooLarge { size, limit } => {
                format!("the page is {size} bytes, over the {limit}-byte limit")
            }
            Self::EntryNotHtml { reason } => format!("the page is not HTML ({reason})"),
            Self::Storage(error) => format!("the page snapshot could not be written: {error}"),
        }
    }

    pub(crate) fn detail(&self) -> Value {
        match self {
            Self::EntryUnreadable { reason } => json!({ "reason": reason }),
            Self::EntryTooLarge { size, limit } => json!({ "size": size, "limit": limit }),
            Self::EntryNotHtml { reason } => json!({ "reason": reason }),
            Self::Storage(_) => json!({}),
        }
    }
}

/// One file in a collected bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileRecord {
    /// Relative to the page's folder, `/`-separated.
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub mime: &'static str,
}

/// A file the page names that is not in the bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Omitted {
    pub path: String,
    pub reason: OmitReason,
}

/// What a collection found.
#[derive(Debug, Default)]
pub(crate) struct Collected {
    /// The page's name, as listed in `files`.
    pub entry: String,
    pub files: Vec<FileRecord>,
    pub omitted: Vec<Omitted>,
    /// Omissions past [`MAX_OMITTED`], and references past
    /// [`scan::MAX_REFERENCES_PER_FILE`] in one file, counted but not listed.
    pub omitted_unlisted: u64,
}

/// Where collected bytes go. The snapshot builder writes each file to the
/// snapshot's private folder.
pub(crate) trait SnapshotSink {
    fn add(&mut self, path: &str, bytes: &[u8]) -> std::io::Result<FileRecord>;
}

struct Pending {
    reference: Reference,
    base: Vec<String>,
    /// For a stylesheet: how many stylesheets deep it is (one for a
    /// stylesheet the page links or imports from a `<style>` element).
    level: u64,
}

/// Collect the page at `entry` (absolute, lexically normalized) and the
/// files it references, reading through `source` and storing through
/// `sink`. A referenced file that cannot be included is listed in
/// `omitted`; only a problem with the page itself refuses.
pub(crate) fn collect(
    entry: &Path,
    limits: &Limits,
    started: Instant,
    source: &mut dyn PageSource,
    sink: &mut dyn SnapshotSink,
) -> Result<Collected, CollectRefusal> {
    let deadline = started + std::time::Duration::from_millis(limits.deadline_ms);
    let entry_name = entry
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .ok_or(CollectRefusal::EntryNotHtml {
            reason: "extension",
        })?;
    if !has_html_extension(entry) {
        return Err(CollectRefusal::EntryNotHtml {
            reason: "extension",
        });
    }
    let entry_limit = limits.entry_max_bytes.min(limits.max_total_bytes);
    let html = match source.read(entry, entry_limit) {
        Ok(bytes) => bytes,
        Err(ReadFailure::Denied) => {
            return Err(CollectRefusal::EntryUnreadable { reason: "denied" })
        }
        Err(ReadFailure::Missing) => {
            return Err(CollectRefusal::EntryUnreadable { reason: "missing" })
        }
        Err(ReadFailure::Deadline) => {
            return Err(CollectRefusal::EntryUnreadable { reason: "deadline" })
        }
        Err(ReadFailure::TooLarge { size }) => {
            return Err(CollectRefusal::EntryTooLarge {
                size,
                limit: entry_limit,
            })
        }
    };
    if !sniff_html(&html) {
        return Err(CollectRefusal::EntryNotHtml { reason: "content" });
    }
    let entry_dir = entry.parent().unwrap_or(entry).to_path_buf();
    let mut collected = Collected {
        entry: entry_name.clone(),
        ..Collected::default()
    };
    let record = sink
        .add(&entry_name, &html)
        .map_err(CollectRefusal::Storage)?;
    let mut total = record.size;
    collected.files.push(record);

    let mut seen: HashSet<String> = HashSet::from([entry_name]);
    let mut seen_raw: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<Pending> = VecDeque::new();
    let (references, dropped) = scan::html_references(&html);
    collected.omitted_unlisted += dropped as u64;
    drop(html);
    queue.extend(references.into_iter().map(|reference| Pending {
        reference,
        base: Vec::new(),
        level: 1,
    }));

    let omit = |collected: &mut Collected, path: String, reason: OmitReason| {
        if collected.omitted.len() < MAX_OMITTED {
            collected.omitted.push(Omitted { path, reason });
        } else {
            collected.omitted_unlisted += 1;
        }
    };

    while let Some(Pending {
        reference,
        base,
        level,
    }) = queue.pop_front()
    {
        let segments = match resolve_reference(&reference.raw, &base) {
            Resolved::Skip => continue,
            Resolved::NotRelative | Resolved::Invalid
                if !seen_raw.insert(reference.raw.clone()) =>
            {
                continue
            }
            Resolved::NotRelative => {
                omit(&mut collected, reference.raw, OmitReason::NotRelative);
                continue;
            }
            Resolved::Invalid => {
                omit(&mut collected, reference.raw, OmitReason::Missing);
                continue;
            }
            Resolved::Path(segments) => segments,
        };
        let display = segments.join("/");
        if !seen.insert(display.clone()) {
            continue;
        }
        let stylesheet = reference.kind == RefKind::Stylesheet;
        if stylesheet && level > limits.import_depth {
            omit(&mut collected, display, OmitReason::DepthLimit);
            continue;
        }
        if collected.files.len() as u64 >= limits.max_files {
            omit(&mut collected, display, OmitReason::OverCountLimit);
            continue;
        }
        if Instant::now() >= deadline {
            omit(&mut collected, display, OmitReason::Deadline);
            continue;
        }
        let remaining = limits.max_total_bytes.saturating_sub(total);
        let limit = limits.max_file_bytes.min(remaining);
        let path = absolute(&entry_dir, &segments);
        let bytes = match source.read(&path, limit) {
            Ok(bytes) => bytes,
            Err(failure) => {
                let reason = match failure {
                    ReadFailure::Denied => OmitReason::Denied,
                    ReadFailure::Missing => OmitReason::Missing,
                    ReadFailure::Deadline => OmitReason::Deadline,
                    ReadFailure::TooLarge { size } if size > limits.max_file_bytes => {
                        OmitReason::OverFileLimit
                    }
                    ReadFailure::TooLarge { .. } => OmitReason::OverTotalLimit,
                };
                omit(&mut collected, display, reason);
                continue;
            }
        };
        let record = sink
            .add(&display, &bytes)
            .map_err(CollectRefusal::Storage)?;
        total += record.size;
        collected.files.push(record);
        if stylesheet {
            let folder = segments[..segments.len() - 1].to_vec();
            let (references, dropped) = scan::css_references(&bytes);
            collected.omitted_unlisted += dropped as u64;
            queue.extend(references.into_iter().map(|reference| Pending {
                reference,
                base: folder.clone(),
                level: level + 1,
            }));
        }
    }
    Ok(collected)
}

/// `segments` joined onto `dir`, with `..` applied lexically.
fn absolute(dir: &Path, segments: &[String]) -> PathBuf {
    let mut path = dir.to_path_buf();
    for segment in segments {
        path.push(segment);
    }
    crate::path_identity::lexical_normalize(&path)
}

impl Collected {
    /// The `page.collect` reply body.
    pub(crate) fn reply(&self, snapshot: &str, expires_at_ms: u64) -> Value {
        let mut body = json!({
            "snapshot": snapshot,
            "expires_at_ms": expires_at_ms,
            "entry": self.entry,
            "files": self.files.iter().map(|file| json!({
                "path": file.path,
                "size": file.size,
                "sha256": file.sha256,
                "mime": file.mime,
            })).collect::<Vec<_>>(),
            "omitted": self.omitted.iter().map(|omitted| json!({
                "path": omitted.path,
                "reason": omitted.reason.as_str(),
            })).collect::<Vec<_>>(),
        });
        if self.omitted_unlisted > 0 {
            body["omitted_unlisted"] = json!(self.omitted_unlisted);
        }
        body
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
