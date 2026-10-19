use super::*;
use crate::log_redact::aft_redactor;
use std::borrow::Cow;

// Every host displays this Rust-owned text verbatim instead of reconstructing
// an unsafe preview from bash_status or the capture files after a restart.
pub(super) const RESTART_PREVIEW_WITHHELD: &str =
    "Output preview withheld after a restart; read the output with bash_status.";
pub(super) const TRUNCATED_PREVIEW_WITHHELD: &str =
    "Output preview withheld because intact output is unavailable for credential masking; use bash_status.";

impl BgTaskRegistry {
    pub(super) fn mask_watch_text(
        &self,
        task_id: &str,
        match_text: &mut String,
        context: &mut String,
    ) {
        let task = self.task(task_id);
        let guard = task.as_ref().map(|task| {
            task.completion_secrets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        if let Some(secrets) = guard.as_deref().and_then(Option::as_ref) {
            *match_text = secrets.redact(match_text).into_owned();
            *context = secrets.redact(context).into_owned();
        } else {
            if match_text != WATCH_TASK_EXIT_TEXT && match_text != WATCH_TARGET_ERASED_TEXT {
                *match_text = crate::log_redact::SECRET_PLACEHOLDER.to_string();
            }
            *context = RESTART_PREVIEW_WITHHELD.to_string();
        }
    }

    pub(super) fn render_watch_preview(
        &self,
        task_id: &str,
        text: &str,
        mut range: std::ops::Range<usize>,
    ) -> (String, String) {
        let task = self.task(task_id);
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
        let masked = secrets.redact_range(text, Some(&mut range));
        (
            masked[range.clone()].to_string(),
            super::super::watches::context_snippet(&masked, range.start, range.end),
        )
    }

    pub(super) fn completion_preview_for_notice(
        &self,
        task_id: &str,
        command: &str,
        cache: &TerminalOutputCache,
        exit_code: Option<i64>,
    ) -> (String, bool) {
        let task = self.task(task_id);
        let guard = task.as_ref().map(|task| {
            task.completion_secrets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let Some(secrets) = guard.as_deref().and_then(Option::as_ref) else {
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
        let buffer = BgBuffer::registered(&task.as_ref().unwrap().paths, BgMode::Pipes);
        let Some(mut raw) = buffer.read_complete_for_notice() else {
            return withheld_preview(TRUNCATED_PREVIEW_WITHHELD, cache);
        };
        normalize_piped_display_output(&mut raw);
        let masked = secrets.redact(&raw);
        let mut safe_cache = cache.clone();
        if let Cow::Owned(masked) = masked {
            // Re-render only credential-bearing output. Ordinary completions
            // reuse the terminal cache and never invoke the compressor twice.
            safe_cache.output_preview = if cache.kind == TerminalOutputKind::Compressed {
                self.compress_output(command, masked, exit_code).text
            } else {
                masked
            };
        }
        safe_cache.output_preview = secrets.redact(&safe_cache.output_preview).into_owned();
        if let Some(recovery) = &mut safe_cache.recovery {
            for line in &mut recovery.final_lines {
                *line = secrets.redact(line).into_owned();
            }
        }
        completion_preview_for_cache(&safe_cache, exit_code)
    }

    pub(super) fn mask_completion_text(&self, task_id: &str, text: &mut String) {
        let task = self.task(task_id);
        let guard = task.as_ref().map(|task| {
            task.completion_secrets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let masked = match guard.as_deref().and_then(Option::as_ref) {
            Some(secrets) => secrets.redact(text),
            None => aft_redactor(text),
        };
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
