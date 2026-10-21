//! Credential masking for durable logs and background completion previews.
//!
//! AFT log lines can quote URLs, error text from child processes and LSP
//! servers, and other strings that occasionally carry a credential. Every line
//! bound for the log file passes through [`aft_redactor`] once before it is
//! written (see `logging::TeeWriter`).
//!
//! Shared token and URL patterns are also exercised by the CLI sanitizer in
//! `packages/aft-cli/src/lib/sanitize.ts` through
//! `tests/fixtures/log_redaction_cases.json`. The durable sink additionally masks
//! contextual secret fields and private-key records before shortening them.
//!
//! What is masked:
//! - GitHub tokens (`ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`, `github_pat_`),
//!   `sk-` API keys, JWTs, AWS access key IDs, and `Authorization: Bearer|Basic`
//!   header values;
//! - URL userinfo (`https://user:pass@host` becomes `https://***@host`);
//! - credential query parameters (`token=`, `access_token=`, `api_key=`,
//!   `…secret=`, `password=`), and `key=` when its value looks like a secret.
//!
//! Deliberately NOT masked: bare `key=value` fields outside a URL query. AFT's
//! own structured lines use `key=` for index keys, and masking them would
//! destroy the forensic value of the log.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::{Captures, Regex, Replacer};

/// Replacement for a token-shaped secret. Matches the CLI sanitizer.
pub const SECRET_PLACEHOLDER: &str = "<REDACTED_SECRET>";
/// Replacement for URL userinfo. Matches the CLI sanitizer.
pub const URL_CREDENTIAL_PLACEHOLDER: &str = "***";

/// A `key=` query value shorter than this is treated as an ordinary word.
const KEY_VALUE_MIN_SECRET_LEN: usize = 16;

fn compile(pattern: &str) -> Regex {
    Regex::new(pattern).expect("log redaction pattern must compile")
}

static AUTHORIZATION_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r"(?i)\b((?:proxy-)?authorization[^\S\r\n]*:[^\S\r\n]*(?:bearer|basic|token)[^\S\r\n]+)[A-Za-z0-9._~+/-]+=*",
    )
});
static API_KEY_HEADER: LazyLock<Regex> =
    LazyLock::new(|| compile(r"(?i)\b(x-api-key[^\S\r\n]*:[^\S\r\n]*)[^\s<>]+"));
static SECRET_JSON_FIELD: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r#"(?i)("(?:password|passwd|pwd|api[_-]?key|apikey|x-api-key|aws_secret_access_key|awsSecretAccessKey|secret_access_key)"\s*:\s*")((?:\\.|[^"\\])+)(")"#,
    )
});
static AWS_SECRET_FIELD: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r#"(?i)\b((?:aws_secret_access_key|secret_access_key)[^\S\r\n]*[=:][^\S\r\n]*)[^\s"'<>]+"#,
    )
});
static SLACK_TOKEN: LazyLock<Regex> = LazyLock::new(|| compile(r"\bxox[baprs]-[A-Za-z0-9-]+\b"));
static PRIVATE_KEY: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
    )
});

static GITHUB_PAT: LazyLock<Regex> = LazyLock::new(|| compile(r"\bgithub_pat_[A-Za-z0-9_]+\b"));
static GITHUB_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| compile(r"\bgh[pousr]_[A-Za-z0-9_]{16,}\b"));
static HEX_32_CANDIDATE: LazyLock<Regex> = LazyLock::new(|| compile(r"\b[0-9a-fA-F]{32}\b"));
static SK_API_KEY: LazyLock<Regex> = LazyLock::new(|| {
    compile(r"\bsk-(?:live-[A-Za-z0-9][A-Za-z0-9_-]{7,}|[A-Za-z0-9][A-Za-z0-9_-]{19,})\b")
});
static JWT: LazyLock<Regex> =
    LazyLock::new(|| compile(r"\beyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b"));
static AWS_ACCESS_KEY_ID: LazyLock<Regex> =
    LazyLock::new(|| compile(r"\b(?:AKIA|ASIA|AGPA|AIDA|AROA)[A-Z0-9]{16}\b"));
static URL_USERINFO: LazyLock<Regex> =
    LazyLock::new(|| compile(r"(?i)\b([a-z][a-z0-9+.-]*://)[^@\s/?#]+@"));
/// Query parameters whose name alone says the value is a credential. The value
/// stops at the next parameter, fragment, whitespace, quote or angle bracket;
/// excluding `<` keeps an already-masked value from matching again.
static CREDENTIAL_QUERY: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r#"(?i)([?&](?:[a-z0-9_-]*token|api[_-]?key|apikey|[a-z0-9_-]*secret|password|passwd|pwd)=)[^&\s#"'<>]+"#,
    )
});
/// `key=` is too generic to mask on its name alone; see [`looks_like_secret`].
static KEY_QUERY: LazyLock<Regex> = LazyLock::new(|| compile(r#"(?i)([?&]key=)([^&\s#"'<>]+)"#));

/// Mask credentials in `text`. Returns the input unchanged (borrowed) when it
/// contains nothing to mask, so the common case allocates nothing.
pub fn aft_redactor(text: &str) -> Cow<'_, str> {
    redact_with_range(text, None)
}

/// Redact only a 32-hex candidate whose BLAKE3 digest matches this task's stored
/// ticket digest. This keeps ordinary hashes and identifiers unchanged in previews.
pub(crate) fn redact_ticket_digest<'a>(text: &'a str, digest: Option<&str>) -> Cow<'a, str> {
    redact_ticket_digest_with_range(text, None, digest)
}

/// Range-aware form used when a watch match is anchored in captured output.
pub(crate) fn redact_ticket_digest_range<'a>(
    text: &'a str,
    range: &mut std::ops::Range<usize>,
    digest: Option<&str>,
) -> Cow<'a, str> {
    redact_ticket_digest_with_range(text, Some(range), digest)
}

fn redact_ticket_digest_with_range<'a>(
    text: &'a str,
    range: Option<&mut std::ops::Range<usize>>,
    digest: Option<&str>,
) -> Cow<'a, str> {
    let Some(digest) = digest else {
        return Cow::Borrowed(text);
    };
    if text.len() < 32 {
        return Cow::Borrowed(text);
    }
    replace_in(
        Cow::Borrowed(text),
        range,
        &HEX_32_CANDIDATE,
        |captures: &Captures<'_>| {
            let candidate = captures.get(0).unwrap().as_str();
            if blake3::hash(candidate.as_bytes()).to_hex().as_str() == digest {
                SECRET_PLACEHOLDER.to_string()
            } else {
                candidate.to_string()
            }
        },
    )
}

fn redact_with_range<'a>(
    text: &'a str,
    mut range: Option<&mut std::ops::Range<usize>>,
) -> Cow<'a, str> {
    let mut out = Cow::Borrowed(text);
    if text.contains("PRIVATE KEY-----") {
        out = replace_in(out, range.as_deref_mut(), &PRIVATE_KEY, SECRET_PLACEHOLDER);
    }
    if text.contains("xox") {
        out = replace_in(out, range.as_deref_mut(), &SLACK_TOKEN, SECRET_PLACEHOLDER);
    }
    if contains_ignore_ascii_case(text, "x-api-key") {
        out = replace_in(
            out,
            range.as_deref_mut(),
            &API_KEY_HEADER,
            |caps: &Captures<'_>| format!("{}{SECRET_PLACEHOLDER}", &caps[1]),
        );
    }
    if text.contains('"') {
        out = replace_in(
            out,
            range.as_deref_mut(),
            &SECRET_JSON_FIELD,
            |caps: &Captures<'_>| format!("{}{SECRET_PLACEHOLDER}{}", &caps[1], &caps[3]),
        );
    }
    if contains_ignore_ascii_case(text, "secret_access_key") {
        out = replace_in(
            out,
            range.as_deref_mut(),
            &AWS_SECRET_FIELD,
            |caps: &Captures<'_>| format!("{}{SECRET_PLACEHOLDER}", &caps[1]),
        );
    }
    if contains_ignore_ascii_case(text, "authorization") {
        out = replace_in(
            out,
            range.as_deref_mut(),
            &AUTHORIZATION_HEADER,
            |caps: &Captures<'_>| format!("{}{SECRET_PLACEHOLDER}", &caps[1]),
        );
    }
    if text.contains("github_pat_") {
        out = replace_in(out, range.as_deref_mut(), &GITHUB_PAT, SECRET_PLACEHOLDER);
    }
    if needs_github_token_scan(text) {
        out = replace_in(out, range.as_deref_mut(), &GITHUB_TOKEN, SECRET_PLACEHOLDER);
    }
    if text.contains("sk-") {
        out = replace_in(out, range.as_deref_mut(), &SK_API_KEY, SECRET_PLACEHOLDER);
    }
    if text.contains("eyJ") {
        out = replace_in(out, range.as_deref_mut(), &JWT, SECRET_PLACEHOLDER);
    }
    if ["AKIA", "ASIA", "AGPA", "AIDA", "AROA"]
        .iter()
        .any(|prefix| text.contains(prefix))
    {
        out = replace_in(
            out,
            range.as_deref_mut(),
            &AWS_ACCESS_KEY_ID,
            SECRET_PLACEHOLDER,
        );
    }
    if text.contains("://") {
        out = replace_in(
            out,
            range.as_deref_mut(),
            &URL_USERINFO,
            |caps: &Captures<'_>| format!("{}{URL_CREDENTIAL_PLACEHOLDER}@", &caps[1]),
        );
    }
    if text.contains('=') && (text.contains('?') || text.contains('&')) {
        out = replace_in(
            out,
            range.as_deref_mut(),
            &CREDENTIAL_QUERY,
            |caps: &Captures<'_>| format!("{}{SECRET_PLACEHOLDER}", &caps[1]),
        );
        out = replace_in(
            out,
            range.as_deref_mut(),
            &KEY_QUERY,
            |caps: &Captures<'_>| {
                if looks_like_secret(&caps[2]) {
                    format!("{}{SECRET_PLACEHOLDER}", &caps[1])
                } else {
                    caps[0].to_string()
                }
            },
        );
    }
    out
}

/// A command's credential values, retained only with its live in-memory task.
/// Deliberately has no Debug or serialization implementation.
#[derive(Default)]
pub(crate) struct SecretValues(Vec<String>);

/// Whether an environment variable name marks its value as a completion secret.
pub fn is_secret_environment_key(key: &str) -> bool {
    if key.eq_ignore_ascii_case(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV) {
        return true;
    }
    // Git's numbered config variables carry a setting NAME in GIT_CONFIG_KEY_<n>
    // (AFT itself injects core.hooksPath this way). The name is not a secret,
    // and dropping it alone while GIT_CONFIG_COUNT and GIT_CONFIG_VALUE_<n>
    // stay makes every git child exit with "missing config key".
    if key
        .get(.."GIT_CONFIG_KEY_".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("GIT_CONFIG_KEY_"))
    {
        return false;
    }
    [
        "TOKEN",
        "SECRET",
        "KEY",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "AUTH",
    ]
    .iter()
    .any(|part| contains_ignore_ascii_case(key, part))
}

impl SecretValues {
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn from_environment<'a>(
        env: impl IntoIterator<Item = (&'a std::ffi::OsStr, &'a std::ffi::OsStr)>,
    ) -> Self {
        let mut values = Vec::new();
        for (key, value) in env {
            let (Some(key), Some(value)) = (key.to_str(), value.to_str()) else {
                continue;
            };
            if value.chars().count() >= 8 && is_secret_environment_key(key) {
                values.push(value.to_owned());
            }
        }
        values.sort_unstable_by_key(|value| std::cmp::Reverse(value.len()));
        values.dedup();
        Self(values)
    }

    pub(crate) fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        self.redact_range(text, None)
    }

    pub(crate) fn redact_range<'a>(
        &self,
        text: &'a str,
        mut range: Option<&mut std::ops::Range<usize>>,
    ) -> Cow<'a, str> {
        // Exact matches run first: a format-based replacement inside an opaque
        // value must not leave the rest of that value visible.
        let mut spans = Vec::new();
        for value in &self.0 {
            let mut start = 0;
            while let Some(offset) = text[start..].find(value) {
                let from = start + offset;
                spans.push((from, from + value.len()));
                start = from + text[from..].chars().next().unwrap().len_utf8();
            }
        }
        let out = if spans.is_empty() {
            Cow::Borrowed(text)
        } else {
            spans.sort_unstable();
            let mut masked = String::new();
            let mut copied = 0;
            let mut spans = spans.into_iter().peekable();
            while let Some((start, mut end)) = spans.next() {
                while spans.peek().is_some_and(|(next, _)| *next <= end) {
                    end = end.max(spans.next().unwrap().1);
                }
                masked.push_str(&text[copied..start]);
                if let Some(range) = range.as_deref_mut() {
                    map_redacted_range(
                        range,
                        masked.len(),
                        masked.len() + end - start,
                        SECRET_PLACEHOLDER.len(),
                    );
                }
                masked.push_str(SECRET_PLACEHOLDER);
                copied = end;
            }
            masked.push_str(&text[copied..]);
            Cow::Owned(masked)
        };
        match redact_with_range(&out, range) {
            Cow::Borrowed(_) => out,
            Cow::Owned(masked) => Cow::Owned(masked),
        }
    }
}

fn needs_github_token_scan(text: &str) -> bool {
    ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"]
        .iter()
        .any(|prefix| text.contains(prefix))
}

/// Byte-level entry point for the log sink. Invalid UTF-8 is decoded with
/// replacement characters before masking so malformed bytes cannot bypass it.
pub fn aft_redact_bytes(bytes: &[u8]) -> Cow<'_, [u8]> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Cow::Owned(
            aft_redactor(&String::from_utf8_lossy(bytes))
                .into_owned()
                .into_bytes(),
        );
    };
    match aft_redactor(text) {
        Cow::Borrowed(_) => Cow::Borrowed(bytes),
        Cow::Owned(masked) => Cow::Owned(masked.into_bytes()),
    }
}

/// Cut `text` to at most `max_bytes` (on a char boundary) and note how much
/// was dropped as `…(+N bytes)`. Used for payloads that are useful to glimpse
/// in a debug line but can be arbitrarily large (LSP params, parser errors).
pub fn truncate_for_log(text: &str, max_bytes: usize) -> Cow<'_, str> {
    if text.len() <= max_bytes {
        return Cow::Borrowed(text);
    }
    let mut cut = max_bytes;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    Cow::Owned(format!("{}…(+{} bytes)", &text[..cut], text.len() - cut))
}

/// Describe a request line that must not itself be logged: its byte length and
/// a short SHA-256 prefix, enough to correlate with a copy the caller kept.
pub fn unlogged_input_summary(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(input.as_bytes());
    let short: String = digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("bytes={} sha256={short}", input.len())
}

/// A `key=` value is treated as a secret when it is long, uses only
/// token-safe characters, and mixes digits with upper- and lower-case letters
/// (an API key such as `AIzaSy…`). Words, paths and single-case hex hashes do
/// not qualify.
fn looks_like_secret(value: &str) -> bool {
    value.len() >= KEY_VALUE_MIN_SECRET_LEN
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "_-.~+/=".contains(ch))
        && value.chars().any(|ch| ch.is_ascii_digit())
        && value.chars().any(|ch| ch.is_ascii_uppercase())
        && value.chars().any(|ch| ch.is_ascii_lowercase())
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

/// Apply one pattern, keeping the borrowed input when nothing changed. A match
/// can re-produce its own text (an already-masked `***@`, or a `key=` value
/// that does not look secret), so equality is checked, not just match count.
fn replace_in<'a, R: Replacer>(
    text: Cow<'a, str>,
    mut range: Option<&mut std::ops::Range<usize>>,
    pattern: &Regex,
    mut replacer: R,
) -> Cow<'a, str> {
    // Capture storage is allocated by replace_all even when it finds no match.
    // Keep the overwhelmingly common no-secret path allocation-free.
    if !pattern.is_match(&text) {
        return text;
    }
    let mut masked = String::new();
    let mut copied = 0;
    for caps in pattern.captures_iter(&text) {
        let found = caps.get(0).unwrap();
        masked.push_str(&text[copied..found.start()]);
        let from = masked.len();
        replacer.replace_append(&caps, &mut masked);
        if masked[from..] != *found.as_str() {
            if let Some(range) = range.as_deref_mut() {
                map_redacted_range(range, from, from + found.len(), masked.len() - from);
            }
        }
        copied = found.end();
    }
    masked.push_str(&text[copied..]);
    if masked == *text {
        text
    } else {
        Cow::Owned(masked)
    }
}

/// Keep a watch's raw match anchored when a replacement changes byte lengths.
/// A match touching a secret expands to cover the entire visible placeholder.
fn map_redacted_range(
    range: &mut std::ops::Range<usize>,
    from: usize,
    to: usize,
    replacement_len: usize,
) {
    let shift = |position: usize| {
        position
            .saturating_sub(to - from)
            .saturating_add(replacement_len)
    };
    if range.start >= to {
        range.start = shift(range.start);
        range.end = shift(range.end);
    } else if range.end > from {
        range.start = range.start.min(from);
        range.end = if range.end >= to {
            shift(range.end)
        } else {
            from + replacement_len
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_exact_env_values_are_case_sensitive_and_overlap_safe() {
        let environment = [
            ("DEPLOY_tOkEn", "abcdefgh"),
            ("MY_SECRET", "fghijklm"),
            ("api_KEY", "key-value-long"),
            ("PASSWORD", "password-value"),
            ("PASSWD", "passwd-value"),
            ("CREDENTIALS", "credential-value"),
            ("AUTHORIZATION", "auth-value"),
            ("tiny_TOKEN", "short"),
            ("NORMAL_VALUE", "public-value"),
        ];
        let secrets = SecretValues::from_environment(
            environment
                .iter()
                .map(|(key, value)| (std::ffi::OsStr::new(key), std::ffi::OsStr::new(value))),
        );
        assert_eq!(
            secrets.redact("abcdefghijklm/abcdefgh"),
            format!("{SECRET_PLACEHOLDER}/{SECRET_PLACEHOLDER}")
        );
        for (_, value) in &environment[..7] {
            assert_eq!(
                secrets.redact(&format!("before{value}after")),
                format!("before{SECRET_PLACEHOLDER}after")
            );
        }
        assert!(matches!(
            secrets.redact("ABCDEFGH short public-value"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn completion_non_secret_corpus_stays_borrowed() {
        for text in [
            "running 42 tests\ntest task::sk-settings ... ok\ntest result: ok. 42 passed; 0 failed\n",
            "commit 0f78c1bcb3ab4d501d6ebc491617b097416f9138\nAuthor: Example <user@example.com>\n    add task-settings\n",
            "added 42 packages, and audited 128 packages in 2s\n12 packages are looking for funding\nfound 0 vulnerabilities\n",
            "request 550e8400-e29b-41d4-a716-446655440000 fixture SGVsbG8gd29ybGQhYWJjZGVmZ2g=\n",
            "sk-settings sk-something sk-words docs say sk- is a prefix; https://example.com/path\n",
        ] {
            assert!(matches!(SecretValues::default().redact(text), Cow::Borrowed(_)), "mangled: {text}");
        }
    }

    #[test]
    fn completion_64k_noop_is_allocation_free() {
        let mut text = "running 42 tests\ntest task::sk-settings ... ok\n{\"status\":\"ok\"}\nhttps://example.com/path\n".repeat(1024);
        text.truncate(64 * 1024);
        let secrets = SecretValues::default();
        let _ = secrets.redact(&text); // Initialize regex automata outside the measurement.
        let start = std::time::Instant::now();
        let (out, allocations) = crate::test_allocations::count(|| secrets.redact(&text));
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(allocations, 0);
        eprintln!(
            "64 KiB completion redaction: {:?}, {allocations} allocations",
            start.elapsed()
        );
    }
    #[test]
    fn github_prefilter_only_scans_actual_token_prefixes() {
        for ordinary in [
            "high throughput",
            "right path",
            "bright light",
            "github repository",
        ] {
            assert!(
                !super::needs_github_token_scan(ordinary),
                "unnecessary scan: {ordinary}"
            );
        }
        for prefix in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"] {
            let token = format!("{prefix}1234567890abcdefghij");
            assert!(super::needs_github_token_scan(&token));
            assert_eq!(super::aft_redactor(&token), super::SECRET_PLACEHOLDER);
        }
    }

    #[derive(serde::Deserialize)]
    struct Fixture {
        masked: Vec<MaskedCase>,
        untouched: Vec<String>,
    }

    #[derive(serde::Deserialize)]
    struct MaskedCase {
        input: String,
        expected: String,
    }

    fn fixture() -> Fixture {
        serde_json::from_str(include_str!("../tests/fixtures/log_redaction_cases.json"))
            .expect("shared redaction fixture parses")
    }

    #[test]
    fn shared_fixture_masked_cases_match_the_cli_sanitizer_output() {
        let fixture = fixture();
        assert!(!fixture.masked.is_empty());
        for case in fixture.masked {
            assert_eq!(
                aft_redactor(&case.input),
                case.expected,
                "input: {}",
                case.input
            );
        }
    }

    #[test]
    fn shared_fixture_untouched_cases_are_borrowed_unchanged() {
        let fixture = fixture();
        assert!(!fixture.untouched.is_empty());
        for input in fixture.untouched {
            let out = aft_redactor(&input);
            assert!(matches!(out, Cow::Borrowed(_)), "mangled: {input} -> {out}");
        }
    }

    #[test]
    fn masks_each_github_token_prefix() {
        let body = "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";
        for prefix in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"] {
            let line = format!("token {prefix}{body} end");
            assert_eq!(
                aft_redactor(&line),
                "token <REDACTED_SECRET> end",
                "{prefix}"
            );
        }
        assert_eq!(
            aft_redactor("x github_pat_11AB_cd34 y"),
            "x <REDACTED_SECRET> y"
        );
    }

    #[test]
    fn masks_url_userinfo_for_any_scheme() {
        assert_eq!(
            aft_redactor("https://user:pass@host.example/path"),
            "https://***@host.example/path"
        );
        assert_eq!(
            aft_redactor("ssh://deploy@host.example:22/repo"),
            "ssh://***@host.example:22/repo"
        );
    }

    #[test]
    fn masks_credential_query_parameters() {
        assert_eq!(
            aft_redactor("u?token=abc&access_token=def&api_key=ghi&client_secret=jkl"),
            "u?token=<REDACTED_SECRET>&access_token=<REDACTED_SECRET>&api_key=<REDACTED_SECRET>&client_secret=<REDACTED_SECRET>"
        );
        assert_eq!(
            aft_redactor("u?key=AIzaSyA1b2C3d4E5f6G7h8I9j0 done"),
            "u?key=<REDACTED_SECRET> done"
        );
    }

    #[test]
    fn masks_header_jwt_sk_and_aws_shapes() {
        assert_eq!(
            aft_redactor("Authorization: Bearer abc.def-ghi"),
            "Authorization: Bearer <REDACTED_SECRET>"
        );
        assert_eq!(
            aft_redactor("jwt eyJhbGciOi.eyJzdWIiOi.c2lnbmF0dXJl end"),
            "jwt <REDACTED_SECRET> end"
        );
        assert_eq!(
            aft_redactor("key sk-live-abcdefgh123"),
            "key <REDACTED_SECRET>"
        );
        assert_eq!(
            aft_redactor("id AKIAABCDEFGHIJKLMNOP"),
            "id <REDACTED_SECRET>"
        );
    }

    #[test]
    fn redaction_is_idempotent() {
        for case in fixture().masked {
            let once = aft_redactor(&case.input).into_owned();
            assert!(matches!(aft_redactor(&once), Cow::Borrowed(_)), "{once}");
        }
    }

    #[test]
    fn ordinary_log_content_is_not_mangled() {
        for line in [
            "[aft] index_event kind=ready plane=search root=/Users/me/proj key=search:v3",
            "[aft] format: src/main.rs (rustfmt)",
            "sha256=9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            "blake3 AF1349B9F5F9A1A6A0404DEA36DCC9499BCB25C9ADC112B7CC9A93CAE41F3262",
            "task_id=bgb_0123456789 session=ses_abcDEF0123456789",
            "https://example.com/search?key=ab12CD34&q=tokenizer",
            "url https://example.com/a?page_size=10&sort=asc",
            "ghost gh_pages ghp_tooShort",
        ] {
            let out = aft_redactor(line);
            assert!(matches!(out, Cow::Borrowed(_)), "mangled: {line} -> {out}");
        }
    }

    #[test]
    fn truncate_for_log_bounds_bytes_and_reports_the_remainder() {
        assert_eq!(truncate_for_log("short", 10), "short");
        assert_eq!(truncate_for_log("abcdefghij", 4), "abcd…(+6 bytes)");
        // "é" is two bytes; the cut backs off to the char boundary.
        assert_eq!(truncate_for_log("aéb", 2), "a…(+3 bytes)");
    }

    #[test]
    fn unlogged_input_summary_carries_length_and_hash_only() {
        let input = "{\"command\":\"secret\"";
        let summary = unlogged_input_summary(input);
        let prefix = format!("bytes={} sha256=", input.len());
        assert!(summary.starts_with(&prefix), "{summary}");
        assert_eq!(summary.len(), prefix.len() + 12);
        assert!(!summary.contains("secret"));
    }

    #[test]
    fn contextual_secret_formats_are_masked() {
        for (input, secret) in [
            ("x-api-key: private-key-value", "private-key-value"),
            (
                "Authorization: Token private-token-value",
                "private-token-value",
            ),
            (
                r#"{"password":"private-password-value"}"#,
                "private-password-value",
            ),
            (
                "token xoxb-12345-private-slack-value",
                "xoxb-12345-private-slack-value",
            ),
            (
                "-----BEGIN RSA PRIVATE KEY-----\nprivate-pem-value\n-----END RSA PRIVATE KEY-----",
                "private-pem-value",
            ),
            (
                "AWS_SECRET_ACCESS_KEY=private-aws-secret-value",
                "private-aws-secret-value",
            ),
        ] {
            let masked = aft_redactor(input);
            assert!(!masked.contains(secret), "secret survived: {input}");
            assert!(masked.contains(SECRET_PLACEHOLDER));
            assert_eq!(aft_redactor(&masked), masked);
        }
        assert_eq!(
            aft_redactor("plain private-aws-secret-value"),
            "plain private-aws-secret-value"
        );
    }

    #[test]
    fn invalid_utf8_does_not_bypass_masking() {
        let mut bytes = vec![0xff];
        bytes.extend_from_slice(b" ghp_1234567890abcdefghij");
        let masked = aft_redact_bytes(&bytes);
        assert!(!String::from_utf8_lossy(&masked).contains("ghp_"));
        assert!(std::str::from_utf8(&masked).is_ok());
    }

    #[test]
    fn non_utf8_bytes_are_safely_decoded() {
        let bytes = [0xff, 0xfe, b'g', b'h'];
        assert_eq!(aft_redact_bytes(&bytes).as_ref(), "��gh".as_bytes());
    }

    #[test]
    fn git_numbered_config_names_are_not_secret_names() {
        // Scrubbing GIT_CONFIG_KEY_0 alone leaves GIT_CONFIG_COUNT pointing at a
        // missing key, and git refuses to run.
        for key in ["GIT_CONFIG_KEY_0", "GIT_CONFIG_KEY_12", "git_config_key_1"] {
            assert!(!is_secret_environment_key(key), "{key}");
        }
        for key in [
            "GITHUB_TOKEN",
            "NPM_AUTH",
            "AWS_SECRET_ACCESS_KEY",
            "API_KEY",
        ] {
            assert!(is_secret_environment_key(key), "{key}");
        }
    }

    #[test]
    fn git_runs_after_the_secret_name_scrub() {
        let mut command = std::process::Command::new("git");
        command
            .args(["config", "--get", "core.hooksPath"])
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "core.hooksPath")
            .env("GIT_CONFIG_VALUE_0", "/dev/null");
        for key in ["GIT_CONFIG_COUNT", "GIT_CONFIG_KEY_0", "GIT_CONFIG_VALUE_0"] {
            if is_secret_environment_key(key) {
                command.env_remove(key);
            }
        }
        let output = command.output().expect("git runs");
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "/dev/null");
    }
}
