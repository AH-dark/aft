use super::*;
use crate::log_redact::aft_redactor;
use std::borrow::Cow;

// Every host displays this Rust-owned text verbatim instead of reconstructing
// an unsafe preview from bash_status or the capture files after a restart.
pub(super) const RESTART_PREVIEW_WITHHELD: &str =
    "Output preview withheld: this task's secret values aren't available after the daemon restarted; read the output with bash_status.";
pub(super) const TRUNCATED_PREVIEW_WITHHELD: &str =
    "Output preview withheld because intact output is unavailable for credential masking; use bash_status.";

const WATCH_SECRET_HALO_BYTES: usize = 1024;
pub(super) const WATCH_PREVIEW_SOURCE_BYTES: usize = 8 * 1024;

impl BgTaskRegistry {
    pub(super) fn saved_completion_preview(
        &self,
        metadata: &PersistedTask,
    ) -> Option<crate::bash_background::persistence::PersistedCompletionPreview> {
        if metadata.completion_has_non_ticket_env_secrets.is_none() {
            return None;
        }
        self.task(&metadata.task_id)
            .and_then(|task| task.lock_state().ok()?.metadata.completion_preview.clone())
            .or_else(|| metadata.completion_preview.clone())
    }

    pub(super) fn save_completion_preview(
        &self,
        task_id: &str,
        output_preview: &str,
        output_truncated: bool,
        bash_output_list_envelope: &Option<crate::list_envelope::ListEnvelope>,
    ) -> crate::bash_background::persistence::PersistedCompletionPreview {
        let preview = crate::bash_background::persistence::PersistedCompletionPreview {
            output_preview: output_preview.to_string(),
            output_truncated,
            bash_output_list_envelope: bash_output_list_envelope.clone(),
        };
        let Some(task) = self.task(task_id) else {
            return preview;
        };
        let mut db = DeferredDbWrites::new(self, &task);
        let Ok(mut state) = task.lock_state() else {
            return preview;
        };
        if state
            .metadata
            .completion_has_non_ticket_env_secrets
            .is_some()
        {
            if let Some(saved) = &state.metadata.completion_preview {
                return saved.clone();
            }
        }
        state.metadata.completion_preview = Some(preview.clone());
        if let Err(error) = self.persist_task_locked(&task, &state.metadata, &mut db) {
            crate::slog_warn!("failed to persist masked completion preview for {task_id}: {error}");
        }
        preview
    }

    pub(super) fn render_watch_preview(
        &self,
        task_id: &str,
        bytes: &[u8],
        range: std::ops::Range<usize>,
    ) -> (String, String) {
        let Some(source) = watch_preview_source_range(range.clone(), bytes.len()) else {
            return withheld_watch_preview();
        };
        let range = range.start.min(bytes.len()).saturating_sub(source.start)
            ..range.end.min(bytes.len()).saturating_sub(source.start);
        let bytes = &bytes[source];
        let text = String::from_utf8_lossy(bytes);
        // A scan window can end inside a character that is intact in the full
        // capture. Decode the expanded source neighborhood rather than separate
        // match prefixes, then include the whole character at either match edge.
        let mut range = lossy_watch_offset(bytes, range.start, false)
            ..lossy_watch_offset(bytes, range.end, true);
        align_watch_range(&text, &mut range);
        let task = self.task(task_id);
        let ticket_digest = task.as_ref().and_then(|task| {
            task.lock_state()
                .ok()
                .and_then(|state| state.metadata.completion_ticket_digest.clone())
        });
        let guard = task.as_ref().map(|task| {
            task.completion_secrets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let Some(secrets) = guard.as_deref().and_then(Option::as_ref) else {
            return (
                crate::log_redact::SECRET_PLACEHOLDER.to_string(),
                RESTART_PREVIEW_WITHHELD.to_string(),
            );
        };
        if secrets.longest_value_len() > WATCH_SECRET_HALO_BYTES {
            return withheld_watch_preview();
        }
        // Exact environment values need their complete spelling before the
        // smaller ticket-only window is selected.
        let masked_values = secrets.redact_range(&text, Some(&mut range));
        let text = masked_values.as_ref();
        // Hash the displayed context plus 31 bytes on either side, enough to
        // include any 32-hex ticket crossing a display boundary.
        let context = super::super::watches::context_range(&text, range.start, range.end);
        let start = crate::compress::generic::floor_char_boundary(
            &text,
            context
                .start
                .saturating_sub(crate::log_redact::TICKET_HALO_BYTES),
        );
        let end = crate::compress::generic::ceil_char_boundary(
            &text,
            context
                .end
                .saturating_add(crate::log_redact::TICKET_HALO_BYTES)
                .min(text.len()),
        );
        range = range.start - start..range.end - start;
        let text = &text[start..end];
        // The scanner finds the watch on raw bytes. Only its presentation is
        // masked, before the context window can split a credential.
        let Ok(masked) = crate::log_redact::redact_ticket_digest_range(
            text,
            &mut range,
            ticket_digest.as_deref(),
        ) else {
            return withheld_watch_preview();
        };
        align_watch_range(&masked, &mut range);
        let Some(matched) = masked.get(range.clone()) else {
            return (
                crate::log_redact::SECRET_PLACEHOLDER.to_string(),
                TRUNCATED_PREVIEW_WITHHELD.to_string(),
            );
        };
        (
            matched.to_string(),
            super::super::watches::context_snippet(&masked, range.start, range.end),
        )
    }

    pub(super) fn completion_preview_for_notice(
        &self,
        metadata: &PersistedTask,
        cache: &TerminalOutputCache,
        paths: Option<&TaskPaths>,
    ) -> (String, bool) {
        let task = self.task(&metadata.task_id);
        let guard = task.as_ref().map(|task| {
            task.completion_secrets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let empty_secrets = crate::log_redact::SecretValues::default();
        let secrets = guard.as_deref().and_then(Option::as_ref).or_else(|| {
            metadata
                .has_safe_completion_redaction_after_restart()
                .then_some(&empty_secrets)
        });
        let Some(secrets) = secrets else {
            return withheld_preview(RESTART_PREVIEW_WITHHELD, cache);
        };
        if cache.output_preview.is_empty() {
            return (String::new(), cache.output_truncated);
        }
        if cache
            .recovery
            .as_ref()
            .is_some_and(|recovery| recovery.disk_truncated_prefix_bytes > 0)
        {
            return withheld_preview(TRUNCATED_PREVIEW_WITHHELD, cache);
        }
        let Some(paths) = task.as_ref().map(|task| &task.paths).or(paths) else {
            return withheld_preview(TRUNCATED_PREVIEW_WITHHELD, cache);
        };
        let buffer = BgBuffer::registered(paths, BgMode::Pipes);
        let Some((stdout, stderr)) = buffer.read_complete_for_notice() else {
            return withheld_preview(TRUNCATED_PREVIEW_WITHHELD, cache);
        };
        let mut raw = combine_streams(&stdout, &stderr);
        normalize_piped_display_output(&mut raw);
        let masked_raw = secrets.redact(&raw);
        let secret_redaction_changed = matches!(&masked_raw, Cow::Owned(_));
        let mut safe_cache = cache.clone();
        if let Some(recovery) = &mut safe_cache.recovery {
            for line in &mut recovery.final_lines {
                let masked_values = secrets.redact(line);
                *line = masked_values.into_owned();
            }
        }
        if secret_redaction_changed {
            let masked = masked_raw.to_string();
            // Re-render only credential-bearing output. Ordinary completions
            // reuse the terminal cache and never invoke the compressor twice.
            let body = if cache.kind == TerminalOutputKind::Compressed {
                // Repeat the terminal render's per-stream ending repair, or a
                // compressor that keeps stderr's last line can drop stdout's
                // verdict. Each stream is masked the same way as the body, so
                // a restored line carries placeholders, never the credential.
                let [stdout, stderr] = [stdout, stderr].map(|mut stream| {
                    normalize_piped_display_output(&mut stream);
                    let masked_values = secrets.redact(&stream);
                    masked_values.into_owned()
                });
                let endings = stream_endings([&stdout, &stderr]);
                let mut text = self
                    .compress_output(&metadata.command, masked, metadata.exit_code)
                    .text;
                let final_lines = restore_stream_endings(&mut text, &endings);
                if let Some(recovery) = &mut safe_cache.recovery {
                    recovery.final_lines = final_lines;
                }
                text
            } else {
                masked
            };
            // Replacing the body also removes terminal-rendering trailers.
            // Restore recovery notes and pipeline warnings before the smaller
            // completion cap, including when the new body fits under that cap.
            safe_cache.output_preview = if let Some(recovery) = &mut safe_cache.recovery {
                let (preview, truncated) = if cache.kind == TerminalOutputKind::Raw {
                    render_raw_body_with_recovery_marker(&body, recovery)
                } else {
                    render_body_with_recovery_marker(&body, recovery)
                };
                safe_cache.output_truncated |= truncated;
                preview
            } else {
                body
            };
            normalize_piped_display_output(&mut safe_cache.output_preview);
            append_pipeline_warning(
                &mut safe_cache,
                metadata,
                task.as_ref().map(|task| &task.paths),
            );
        }
        let masked_values = secrets.redact(&safe_cache.output_preview);
        safe_cache.output_preview = masked_values.into_owned();
        let (preview, truncated) = completion_preview_for_cache(&safe_cache, metadata.exit_code);
        let Some(digest) = metadata.completion_ticket_digest.as_deref() else {
            return (preview, truncated);
        };
        let intact_cache = !safe_cache
            .recovery
            .as_ref()
            .is_some_and(|recovery| recovery.byte_truncated);
        let marker = safe_cache.recovery.as_ref().and_then(|recovery| {
            let mut completion_recovery = recovery.clone();
            completion_recovery.byte_truncated = true;
            recovery_marker(&completion_recovery)
        });
        match mask_rendered_ticket_preview(
            &masked_raw,
            &safe_cache.output_preview,
            intact_cache,
            safe_cache
                .recovery
                .as_ref()
                .map(|recovery| recovery.final_lines.as_slice())
                .unwrap_or(&[]),
            marker.as_deref(),
            &preview,
            digest,
        ) {
            Ok(preview) => (preview, truncated),
            Err(_) => withheld_preview(TRUNCATED_PREVIEW_WITHHELD, cache),
        }
    }

    pub(super) fn mask_completion_text(&self, task_id: &str, text: &mut String) {
        let task = self.task(task_id);
        let ticket_digest = task.as_ref().and_then(|task| {
            task.lock_state()
                .ok()
                .and_then(|state| state.metadata.completion_ticket_digest.clone())
        });
        let guard = task.as_ref().map(|task| {
            task.completion_secrets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let masked = match guard.as_deref().and_then(Option::as_ref) {
            Some(secrets) => secrets.redact(text),
            None => aft_redactor(text),
        };
        let secret_redaction_changed = matches!(&masked, Cow::Owned(_));
        let Ok(masked) = crate::log_redact::redact_ticket_digest(&masked, ticket_digest.as_deref())
        else {
            *text = TRUNCATED_PREVIEW_WITHHELD.to_string();
            if let Some(task) = task.as_ref() {
                for path in [&task.paths.stdout, &task.paths.stderr] {
                    text.push_str("\nOutput file: ");
                    text.push_str(&aft_redactor(&path.to_string_lossy()));
                }
            }
            return;
        };
        let ticket_redaction_changed = matches!(&masked, Cow::Owned(_));
        if secret_redaction_changed || ticket_redaction_changed {
            *text = masked.into_owned();
        }
    }
}

pub(super) fn watch_preview_source_range(
    range: std::ops::Range<usize>,
    len: usize,
) -> Option<std::ops::Range<usize>> {
    let start = range
        .start
        .min(len)
        .saturating_sub(100 * 4 + WATCH_SECRET_HALO_BYTES + 4);
    let end = range
        .end
        .min(len)
        .max(range.start.min(len))
        .saturating_add(500 * 4 + WATCH_SECRET_HALO_BYTES + 4)
        .min(len);
    (end - start <= WATCH_PREVIEW_SOURCE_BYTES).then_some(start..end)
}

fn withheld_watch_preview() -> (String, String) {
    (
        crate::log_redact::SECRET_PLACEHOLDER.to_string(),
        TRUNCATED_PREVIEW_WITHHELD.to_string(),
    )
}

fn mask_rendered_ticket_preview(
    raw: &str,
    cache: &str,
    intact_cache: bool,
    final_lines: &[String],
    marker: Option<&str>,
    preview: &str,
    digest: &str,
) -> Result<String, crate::log_redact::TicketRedactionBudgetExceeded> {
    use crate::log_redact::{
        TicketRedactionBudgetExceeded, TICKET_HALO_BYTES, TICKET_REDACTION_BYTES,
        TICKET_REDACTION_WINDOWS,
    };
    if preview.len() > TICKET_REDACTION_BYTES || cache.len() > TICKET_REDACTION_BYTES {
        return Err(TicketRedactionBudgetExceeded);
    }
    // Locate whole bounded raw head/tail slices in the terminal cache first.
    // Mapping the preview's cache offsets through those slices avoids confusing
    // an identical fragment elsewhere in the capture with the displayed one.
    // A middle line can use the cache itself only if no byte cut destroyed its
    // neighboring bytes; otherwise withhold rather than guess.
    let width = preview.len() + TICKET_HALO_BYTES;
    let head_end = crate::compress::generic::ceil_char_boundary(raw, width.min(raw.len()));
    let tail_start =
        crate::compress::generic::floor_char_boundary(raw, raw.len().saturating_sub(width));
    let sources = [
        (&raw[..head_end], false, head_end < raw.len()),
        (&raw[tail_start..], tail_start > 0, false),
    ];
    let mappings = sources.map(|(source, cut_start, cut_end)| {
        cache
            .find(source)
            .map(|start| (source, start, cut_start, cut_end))
    });
    let mut remaining = TICKET_REDACTION_WINDOWS;
    let mut output = String::new();
    for line in preview.split_inclusive('\n') {
        let newline = line.ends_with('\n');
        let line = line.strip_suffix('\n').unwrap_or(line);
        if line.is_empty() {
            output.push('\n');
            continue;
        }
        let edge_hex = line.as_bytes()[0].is_ascii_hexdigit()
            || line.as_bytes()[line.len() - 1].is_ascii_hexdigit();
        let (slice, mut range) =
            if let Some(cache_start) = cache.find(line) {
                // If a line starts or ends with a hex digit and occurs repeatedly,
                // its neighbors may differ and change the 32-hex ticket candidates.
                // Withhold rather than choose an occurrence with the wrong neighbors.
                if edge_hex && cache.match_indices(line).nth(1).is_some() {
                    return Err(TicketRedactionBudgetExceeded);
                }
                let origin = mappings.iter().flatten().find_map(
                    |(source, mapped_start, cut_start, cut_end)| {
                        let start = cache_start.checked_sub(*mapped_start)?;
                        let end = start + line.len();
                        if end > source.len()
                            || (*cut_start && start < TICKET_HALO_BYTES)
                            || (*cut_end && end + TICKET_HALO_BYTES > source.len())
                        {
                            return None;
                        }
                        Some((*source, start))
                    },
                );
                // Compression can remove lines before the stream ending, leaving
                // no complete tail slice in the cache. If a displayed line matches
                // a saved final line and a complete line at the raw output's end,
                // its displayed bytes can still be located in the raw tail.
                let ending_origin = final_lines
                    .iter()
                    .any(|final_line| final_line == line)
                    .then(|| {
                        let tail = &raw[tail_start..];
                        let prefix = tail.trim_end_matches('\n').strip_suffix(line)?;
                        (prefix.ends_with('\n') || (tail_start == 0 && prefix.is_empty()))
                            .then_some((tail, prefix.len()))
                    })
                    .flatten();
                let (source, start) = match origin.or(ending_origin) {
                    Some(origin) => origin,
                    None if intact_cache || !edge_hex => (cache, cache_start),
                    None => return Err(TicketRedactionBudgetExceeded),
                };
                let from = crate::compress::generic::floor_char_boundary(
                    source,
                    start.saturating_sub(TICKET_HALO_BYTES),
                );
                let to = crate::compress::generic::ceil_char_boundary(
                    source,
                    (start + line.len() + TICKET_HALO_BYTES).min(source.len()),
                );
                (&source[from..to], start - from..start - from + line.len())
            } else {
                let truncation_marker = line
                    .strip_prefix("...<truncated ")
                    .and_then(|line| line.strip_suffix(" bytes>..."))
                    .is_some_and(|count| count.parse::<usize>().is_ok());
                // These exact generated markers have no missing source neighbors.
                if marker != Some(line) && !truncation_marker {
                    return Err(TicketRedactionBudgetExceeded);
                }
                (line, 0..line.len())
            };
        remaining = remaining
            .checked_sub(crate::log_redact::ticket_digest_windows(slice))
            .ok_or(TicketRedactionBudgetExceeded)?;
        let masked =
            crate::log_redact::redact_ticket_digest_range(slice, &mut range, Some(digest))?;
        output.push_str(&masked[range]);
        if newline {
            output.push('\n');
        }
    }
    Ok(output)
}

#[cfg(test)]
mod ticket_preview_tests {
    use super::*;

    #[test]
    fn completion_ticket_fragment_requires_cache_source_mapping() {
        let ticket = "d34db33f89ab01cd7654ef2098ac1357";
        let fragment = &ticket[16..];
        let raw = format!("{fragment}\n{}{ticket}\n", "x".repeat(10_000));
        let cache = format!("unrelated compressed head\n[cut]\n{fragment}\n");
        let digest = blake3::hash(ticket.as_bytes()).to_hex().to_string();
        // A matching fragment at the start of raw output is not evidence for
        // the bytes neighboring the displayed, already-truncated occurrence.
        assert!(
            mask_rendered_ticket_preview(&raw, &cache, false, &[], None, fragment, &digest)
                .is_err()
        );
    }
}

fn withheld_preview(reason: &str, cache: &TerminalOutputCache) -> (String, bool) {
    let mut text = reason.to_string();
    for path in [&cache.output_path, &cache.stderr_path]
        .into_iter()
        .flatten()
    {
        text.push_str("\nOutput file: ");
        text.push_str(&aft_redactor(path));
    }
    (text, true)
}

fn align_watch_range(text: &str, range: &mut std::ops::Range<usize>) {
    range.start = text.floor_char_boundary(range.start.min(text.len()));
    range.end = text.ceil_char_boundary(range.end.min(text.len()).max(range.start));
}

fn lossy_watch_offset(bytes: &[u8], offset: usize, round_up: bool) -> usize {
    let mut source_start = 0;
    let mut text_start = 0;
    // utf8_chunks groups invalid bytes exactly as from_utf8_lossy does, so an
    // invalid sequence contributes one replacement character in both mappings.
    for chunk in bytes.utf8_chunks() {
        let valid_end = source_start + chunk.valid().len();
        if offset <= valid_end {
            return text_start + offset.saturating_sub(source_start);
        }
        text_start += chunk.valid().len();
        source_start = valid_end + chunk.invalid().len();
        if offset < source_start {
            return text_start + if round_up { '\u{FFFD}'.len_utf8() } else { 0 };
        }
        if !chunk.invalid().is_empty() {
            text_start += '\u{FFFD}'.len_utf8();
        }
    }
    text_start
}
