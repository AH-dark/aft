use super::*;
use crate::log_redact::aft_redactor;
use std::borrow::Cow;

// Every host displays this Rust-owned text verbatim instead of reconstructing
// an unsafe preview from bash_status or the capture files after a restart.
pub(super) const RESTART_PREVIEW_WITHHELD: &str =
    "Output preview withheld: this task's secret values aren't available after the daemon restarted; read the output with bash_status.";
pub(super) const TRUNCATED_PREVIEW_WITHHELD: &str =
    "Output preview withheld because intact output is unavailable for credential masking; use bash_status.";

impl BgTaskRegistry {
    pub(super) fn saved_completion_preview(
        &self,
        metadata: &PersistedTask,
    ) -> Option<crate::bash_background::persistence::PersistedCompletionPreview> {
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
        if let Some(saved) = &state.metadata.completion_preview {
            return saved.clone();
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
        let text = String::from_utf8_lossy(bytes);
        // A scan window can end inside a character that is intact in the full
        // capture. Map source offsets using the full capture's decoding, not
        // independently decoded prefixes, then include the whole character.
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
        // The scanner finds the watch on raw bytes. Only its presentation is
        // masked, before the context window can split a credential.
        let masked_values = secrets.redact_range(&text, Some(&mut range));
        let masked = crate::log_redact::redact_ticket_digest_range(
            &masked_values,
            &mut range,
            ticket_digest.as_deref(),
        );
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
        let masked_values = secrets.redact(&raw);
        let masked = crate::log_redact::redact_ticket_digest(
            &masked_values,
            metadata.completion_ticket_digest.as_deref(),
        );
        let mut safe_cache = cache.clone();
        if let Some(recovery) = &mut safe_cache.recovery {
            for line in &mut recovery.final_lines {
                let masked_values = secrets.redact(line);
                *line = crate::log_redact::redact_ticket_digest(
                    &masked_values,
                    metadata.completion_ticket_digest.as_deref(),
                )
                .into_owned();
            }
        }
        if let Cow::Owned(masked) = masked {
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
                    crate::log_redact::redact_ticket_digest(
                        &masked_values,
                        metadata.completion_ticket_digest.as_deref(),
                    )
                    .into_owned()
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
        safe_cache.output_preview = crate::log_redact::redact_ticket_digest(
            &masked_values,
            metadata.completion_ticket_digest.as_deref(),
        )
        .into_owned();
        completion_preview_for_cache(&safe_cache, metadata.exit_code)
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
        let masked = crate::log_redact::redact_ticket_digest(&masked, ticket_digest.as_deref());
        if let Cow::Owned(masked) = masked {
            *text = masked;
        }
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
