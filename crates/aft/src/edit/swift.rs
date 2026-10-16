//! Confirm Swift tree-sitter errors with the compiler's syntax-only parser.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::format::{FormatError, SubprocessOutcome};

const PARSE_TIMEOUT: Duration = Duration::from_secs(5);
static SWIFTC: OnceLock<Result<PathBuf, String>> = OnceLock::new();

#[derive(Debug)]
enum ParseOutcome {
    Exited(Option<i32>),
    Missing,
    TimedOut,
    Unavailable(String),
}

pub(super) fn confirm_if_needed(
    path: &Path,
    source: &str,
    was_valid: Option<bool>,
    is_valid: Option<bool>,
) -> Option<bool> {
    if path.extension().and_then(|ext| ext.to_str()) != Some("swift")
        || was_valid != Some(true)
        || is_valid != Some(false)
    {
        return is_valid;
    }

    let outcome = parse(source);
    match outcome {
        ParseOutcome::Exited(Some(0)) => {
            log::debug!(
                "swiftc confirmed syntax after tree-sitter errors: {}",
                path.display()
            );
            Some(true)
        }
        ParseOutcome::Unavailable(reason) => {
            log::debug!(
                "Swift syntax confirmation unavailable for {}: {reason}",
                path.display()
            );
            is_valid
        }
        other => {
            log::debug!(
                "Swift syntax confirmation failed for {}: {other:?}",
                path.display()
            );
            is_valid
        }
    }
}

fn find_swiftc() -> &'static Result<PathBuf, String> {
    SWIFTC.get_or_init(|| {
        if let Ok(path) = which::which("swiftc") {
            return Ok(path);
        }
        #[cfg(target_os = "macos")]
        {
            let mut command = Command::new("xcrun");
            command.args(["--find", "swiftc"]);
            match run_command(command, "", Instant::now() + PARSE_TIMEOUT) {
                Ok(result) if result.status.success() => {
                    let path = PathBuf::from(result.stdout.trim());
                    if path.is_absolute() && path.is_file() {
                        return Ok(path);
                    }
                    return Err("xcrun returned no swiftc path".into());
                }
                Ok(result) => return Err(format!("xcrun exited with {}", result.status)),
                Err(error) => return Err(format!("xcrun lookup failed: {error}")),
            }
        }
        #[cfg(not(target_os = "macos"))]
        Err("swiftc is not on PATH".into())
    })
}

#[cfg(all(test, target_os = "macos"))]
pub(super) fn available() -> bool {
    match parse("") {
        ParseOutcome::Exited(Some(0)) => true,
        outcome => {
            eprintln!("swiftc availability check: {outcome:?}");
            false
        }
    }
}

fn parse(source: &str) -> ParseOutcome {
    #[cfg(test)]
    if let Some(outcome) =
        RUNNER.with(|runner| runner.borrow_mut().as_mut().map(|runner| runner(source)))
    {
        return outcome;
    }
    let path = match find_swiftc() {
        Ok(path) => path,
        Err(reason) => {
            log::debug!("Swift syntax confirmation lookup failed: {reason}");
            return ParseOutcome::Missing;
        }
    };
    let mut command = Command::new(path);
    // Parsing stdin avoids temporary files. -parse performs no type checking or imports.
    command.args(["-parse", "-"]);
    parse_outcome(run_command(command, source, Instant::now() + PARSE_TIMEOUT))
}

fn parse_outcome(result: Result<SubprocessOutcome, FormatError>) -> ParseOutcome {
    match result {
        Ok(result) => ParseOutcome::Exited(result.status.code()),
        Err(FormatError::NotFound { .. }) => ParseOutcome::Missing,
        Err(FormatError::Timeout { .. }) => ParseOutcome::TimedOut,
        Err(error) => ParseOutcome::Unavailable(error.to_string()),
    }
}

fn run_command(
    mut command: Command,
    source: &str,
    deadline: Instant,
) -> Result<SubprocessOutcome, FormatError> {
    // Do not set an environment or use pre_exec: both force fork on macOS.
    // process_group keeps posix_spawn available while allowing timeout tree cleanup.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                FormatError::NotFound {
                    tool: "swiftc".into(),
                }
            } else {
                FormatError::Failed {
                    tool: "swiftc".into(),
                    stderr: error.to_string(),
                }
            }
        })?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input = source.as_bytes().to_vec();
    // A large file can fill stdin; start the deadline wait before waiting for its writer.
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let result = crate::format::wait_with_deadline(child, "swiftc", deadline, || false);
    if result.is_ok() {
        writer
            .join()
            .unwrap_or_else(|_| Err(std::io::Error::other("stdin writer panicked")))
            .map_err(|error| FormatError::Failed {
                tool: "swiftc".into(),
                stderr: error.to_string(),
            })?;
    }
    result
}

#[cfg(test)]
type Runner = Box<dyn FnMut(&str) -> ParseOutcome>;

#[cfg(test)]
thread_local! {
    static RUNNER: std::cell::RefCell<Option<Runner>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn with_runner<T>(runner: Runner, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<Runner>);
    impl Drop for Restore {
        fn drop(&mut self) {
            RUNNER.with(|runner| *runner.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(RUNNER.with(|slot| slot.replace(Some(runner))));
    f()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::edit::{validate_syntax_str, write_format_validate, WriteResult};

    const ORIGINAL: &str = "let value = 1\n";
    const TUPLE: &str = "_ = (Int, Result<String, Error>).self\n";
    const BROKEN: &str = "_ = (Int, Result<String, Error>.self\n";

    fn write_case(source: &str, outcome: ParseOutcome) -> (WriteResult, String) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tuple.swift");
        std::fs::write(&path, ORIGINAL).unwrap();
        assert_eq!(validate_syntax_str(source, &path), Some(false));
        let expected = source.to_owned();
        let mut outcome = Some(outcome);
        let result = with_runner(
            Box::new(move |actual| {
                assert_eq!(actual, expected);
                outcome.take().expect("only one parser call per edit")
            }),
            || {
                write_format_validate(
                    &path,
                    source,
                    &Config {
                        format_on_edit: false,
                        ..Config::default()
                    },
                    &serde_json::json!({}),
                )
                .unwrap()
            },
        );
        (result, std::fs::read_to_string(&path).unwrap())
    }

    #[test]
    fn swift_parser_success_overrides_tree_sitter() {
        let (result, text) = write_case(TUPLE, ParseOutcome::Exited(Some(0)));
        assert!(!result.rolled_back);
        assert_eq!(result.syntax_valid, Some(true));
        assert_eq!(text, TUPLE);
        assert!(result.reformatted_excerpt.is_none());
        assert!(result.validation_errors.is_empty());
    }

    #[test]
    fn swift_parser_nonzero_rolls_back() {
        let (result, text) = write_case(BROKEN, ParseOutcome::Exited(Some(1)));
        assert!(result.rolled_back);
        assert_eq!(result.syntax_valid, Some(false));
        assert_eq!(text, ORIGINAL);
    }

    #[test]
    fn swift_parser_missing_binary_rolls_back() {
        let (result, text) = write_case(TUPLE, ParseOutcome::Missing);
        assert!(result.rolled_back);
        assert_eq!(result.syntax_valid, Some(false));
        assert_eq!(text, ORIGINAL);
    }

    #[test]
    fn swift_parser_unavailable_rolls_back() {
        for outcome in [
            ParseOutcome::TimedOut,
            ParseOutcome::Unavailable("permission denied".into()),
            ParseOutcome::Exited(None),
        ] {
            let (result, text) = write_case(TUPLE, outcome);
            assert!(result.rolled_back);
            assert_eq!(result.syntax_valid, Some(false));
            assert_eq!(text, ORIGINAL);
        }
    }

    #[test]
    fn swift_parser_never_runs_for_non_swift() {
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let count = calls.clone();
        with_runner(
            Box::new(move |_| {
                count.set(count.get() + 1);
                ParseOutcome::Exited(Some(0))
            }),
            || {
                let temp = tempfile::tempdir().unwrap();
                let path = temp.path().join("bad.ts");
                std::fs::write(&path, "const value = 1;\n").unwrap();
                let result = write_format_validate(
                    &path,
                    "function {",
                    &Config {
                        format_on_edit: false,
                        ..Config::default()
                    },
                    &serde_json::json!({}),
                )
                .unwrap();
                assert_eq!(calls.get(), 0, "non-Swift edit must not spawn swiftc");
                assert!(result.rolled_back);
                assert_eq!(
                    std::fs::read_to_string(&path).unwrap(),
                    "const value = 1;\n"
                );
            },
        );
    }

    #[test]
    fn swift_parser_runs_only_before_clean_to_invalid_rollback() {
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let count = calls.clone();
        with_runner(
            Box::new(move |_| {
                count.set(count.get() + 1);
                ParseOutcome::Exited(Some(0))
            }),
            || {
                for (was_valid, is_valid) in [
                    (Some(true), Some(true)),
                    (Some(false), Some(false)),
                    (None, Some(false)),
                    (Some(true), None),
                ] {
                    assert_eq!(
                        confirm_if_needed(Path::new("test.swift"), TUPLE, was_valid, is_valid),
                        is_valid
                    );
                }
            },
        );
        assert_eq!(
            calls.get(),
            0,
            "no parser call without a pending syntax rollback"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn swift_parser_real_missing_binary_fallback() {
        if find_swiftc().is_ok() {
            eprintln!("skipping missing-swiftc test: swiftc is available on PATH");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tuple.swift");
        std::fs::write(&path, ORIGINAL).unwrap();
        let result = write_format_validate(
            &path,
            TUPLE,
            &Config {
                format_on_edit: false,
                ..Config::default()
            },
            &serde_json::json!({}),
        )
        .unwrap();
        assert!(result.rolled_back);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), ORIGINAL);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn swift_parser_rejects_broken_swift() {
        if !available() {
            eprintln!("skipping native Swift parser test: swiftc is unavailable");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("broken.swift");
        std::fs::write(&path, ORIGINAL).unwrap();
        let result = write_format_validate(
            &path,
            BROKEN,
            &Config {
                format_on_edit: false,
                ..Config::default()
            },
            &serde_json::json!({}),
        )
        .unwrap();
        assert!(result.rolled_back);
        assert_eq!(result.syntax_valid, Some(false));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), ORIGINAL);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn swift_parser_does_not_typecheck_or_load_imports() {
        if !available() {
            eprintln!("skipping native Swift parser test: swiftc is unavailable");
            return;
        }
        assert!(matches!(
            parse("import AFTModuleThatDoesNotExist\n_ = undefinedName\n"),
            ParseOutcome::Exited(Some(0))
        ));
    }

    #[test]
    fn swift_parser_spawn_missing_binary_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        let command = Command::new(temp.path().join("absent-swiftc"));
        assert!(matches!(
            parse_outcome(run_command(command, TUPLE, Instant::now() + PARSE_TIMEOUT)),
            ParseOutcome::Missing
        ));
    }

    #[cfg(unix)]
    #[test]
    fn swift_parser_timeout_rolls_back_and_reaps_child() {
        struct Cleanup(i32);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                // Also clean up if an assertion exposes a timeout-cleanup regression.
                unsafe {
                    libc::kill(self.0, libc::SIGKILL);
                    libc::waitpid(self.0, std::ptr::null_mut(), 0);
                }
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tuple.swift");
        let pid_path = temp.path().join("child.pid");
        std::fs::write(&path, ORIGINAL).unwrap();
        let child_pid_path = pid_path.clone();
        let started = Instant::now();
        let result = with_runner(
            Box::new(move |source| {
                let mut command = Command::new("/bin/sh");
                command
                    .args(["-c", "echo $$ > \"$1\"; exec sleep 30", "swiftc-test"])
                    .arg(&child_pid_path);
                let outcome = parse_outcome(run_command(
                    command,
                    source,
                    Instant::now() + Duration::from_millis(150),
                ));
                assert!(matches!(outcome, ParseOutcome::TimedOut));
                outcome
            }),
            || {
                write_format_validate(
                    &path,
                    &TUPLE.repeat(4_096),
                    &Config {
                        format_on_edit: false,
                        ..Config::default()
                    },
                    &serde_json::json!({}),
                )
                .unwrap()
            },
        );
        // Read the PID published by the real child so the liveness check targets that process.
        let pid_deadline = Instant::now() + Duration::from_secs(2);
        let pid = loop {
            if let Ok(text) = std::fs::read_to_string(&pid_path) {
                if let Ok(pid) = text.trim().parse::<i32>() {
                    break pid;
                }
            }
            assert!(
                Instant::now() < pid_deadline,
                "child did not publish its PID"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        let _cleanup = Cleanup(pid);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "deadline must bound blocked stdin too"
        );
        assert!(result.rolled_back);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), ORIGINAL);
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "timed-out child is still alive or unreaped"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}
