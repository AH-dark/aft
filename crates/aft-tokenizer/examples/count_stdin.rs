//! Token-counting helper for the description audit.
//!
//! Reads NDJSON from stdin (one `{ "label": "...", "text": "..." }` per
//! line) and writes NDJSON to stdout (`{ "label": "...", "tokens": N }`).
//! Pure dev tooling — not part of the published crate surface.

use std::io::{self, BufRead, Write};

fn main() -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (label, text) = parse_label_text(trimmed).expect("invalid input line");
        let tokens = aft_tokenizer::count_tokens(&text).map_err(io::Error::other)?;
        writeln!(
            out,
            r#"{{"label":{},"tokens":{}}}"#,
            json_escape(&label),
            tokens
        )?;
    }
    Ok(())
}

fn parse_label_text(line: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let obj = value.as_object()?;
    let label = obj.get("label")?.as_str()?.to_string();
    let text = obj.get("text")?.as_str()?.to_string();
    Some((label, text))
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str(r#"\""#),
            '\\' => out.push_str(r"\\"),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            '\t' => out.push_str(r"\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!(r"\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::parse_label_text;

    #[test]
    fn json_input_preserves_unicode_and_surrogate_pairs() {
        let expected = ("é😀".to_string(), "a — b → c 😀".to_string());
        assert_eq!(
            parse_label_text(r#"{"label":"é😀","text":"a — b → c 😀"}"#),
            Some(expected.clone())
        );
        assert_eq!(
            parse_label_text(
                r#"{"label":"\u00e9\ud83d\ude00","text":"a \u2014 b \u2192 c \ud83d\ude00"}"#
            ),
            Some(expected)
        );
    }

    #[test]
    fn json_input_requires_string_fields() {
        assert_eq!(parse_label_text(r#"{"label":1,"text":"x"}"#), None);
        assert_eq!(parse_label_text(r#"{"label":"x"}"#), None);
    }
}
