#[cfg(any(target_os = "macos", target_os = "linux"))]
use aft::sandbox_profile::SandboxProfile;
use std::ffi::OsString;
use std::fmt;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::fs::{File, OpenOptions};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::{FromRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::Command;

#[cfg(target_os = "linux")]
mod landlock_backend;
#[cfg(any(target_os = "macos", test))]
mod seatbelt;

pub const SANDBOX_UNAVAILABLE_EXIT_CODE: i32 = 78;

#[derive(Debug)]
pub struct SandboxLaunchError {
    message: String,
    exit_code: i32,
}

impl SandboxLaunchError {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: 2,
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn runtime(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: 1,
        }
    }

    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            message: format!("sandbox_unavailable: {}", message.into()),
            exit_code: SANDBOX_UNAVAILABLE_EXIT_CODE,
        }
    }

    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }
}

impl fmt::Display for SandboxLaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SandboxLaunchError {}

pub fn run(args: Vec<OsString>) -> Result<(), SandboxLaunchError> {
    #[cfg(windows)]
    if args == [OsString::from("--support")] {
        return print_support();
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = args;
        return Err(SandboxLaunchError::unavailable(
            "sandbox-launch is supported only on macOS and Linux",
        ));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let args = LaunchArgs::parse(args)?;
        if args.help {
            print_usage();
            return Ok(());
        }
        if args.support {
            return print_support();
        }

        if args.command.is_empty() {
            return Err(SandboxLaunchError::usage("missing target command after --"));
        }

        let source_count = usize::from(args.profile_fd.is_some())
            + usize::from(args.profile_file.is_some())
            + usize::from(args.profile_json.is_some());
        if source_count != 1 {
            return Err(SandboxLaunchError::usage(
                "exactly one of --profile-fd, --profile-file, or --profile-json is required",
            ));
        }
        let profile = if let Some(fd) = args.profile_fd {
            read_profile(fd)?
        } else if let Some(path) = args.profile_file.as_deref() {
            read_profile_file(path)?
        } else {
            read_profile_json(args.profile_json.as_deref().expect("source count checked"))?
        }
        .canonicalize_for_launch()
        .map_err(|error| SandboxLaunchError::unavailable(error.to_string()));
        let applied = profile.and_then(|profile| apply_sandbox(&profile));
        if let Err(error) = applied {
            if error.exit_code == SANDBOX_UNAVAILABLE_EXIT_CODE {
                if let Some(path) = args.failure_marker.as_deref() {
                    let _ = write_failure_marker(path);
                }
                if let Some(fd) = args.failure_fd {
                    let _ = write_failure_fd(fd);
                }
            }
            return Err(error);
        }
        exec_target(&args.command)
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Debug, Default)]
struct LaunchArgs {
    profile_fd: Option<i32>,
    profile_file: Option<PathBuf>,
    profile_json: Option<String>,
    failure_marker: Option<PathBuf>,
    /// Inherited descriptor that receives the failure marker when
    /// confinement cannot be applied. A caller supervising the launch uses
    /// it to tell a launcher failure from a target that exits with the same
    /// status after a successful launch.
    failure_fd: Option<i32>,
    command: Vec<OsString>,
    help: bool,
    support: bool,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl LaunchArgs {
    fn parse(args: Vec<OsString>) -> Result<Self, SandboxLaunchError> {
        let mut parsed = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            if arg == "--" {
                parsed.command.extend(args);
                break;
            }
            if arg == "--help" || arg == "-h" {
                parsed.help = true;
                continue;
            }
            if arg == "--support" {
                parsed.support = true;
                continue;
            }
            if arg == "--profile-fd" {
                let value = args.next().ok_or_else(|| {
                    SandboxLaunchError::usage("--profile-fd requires an integer value")
                })?;
                parsed.profile_fd = Some(parse_fd(&value)?);
                continue;
            }
            if let Some(value) = arg
                .to_str()
                .and_then(|value| value.strip_prefix("--profile-fd="))
            {
                parsed.profile_fd = Some(parse_fd(&OsString::from(value))?);
                continue;
            }
            if arg == "--profile-file" {
                let value = args.next().ok_or_else(|| {
                    SandboxLaunchError::usage("--profile-file requires a path value")
                })?;
                parsed.profile_file = Some(PathBuf::from(value));
                continue;
            }
            if let Some(value) = arg
                .to_str()
                .and_then(|value| value.strip_prefix("--profile-file="))
            {
                parsed.profile_file = Some(PathBuf::from(value));
                continue;
            }
            if arg == "--profile-json" {
                let value = args.next().ok_or_else(|| {
                    SandboxLaunchError::usage("--profile-json requires a JSON value")
                })?;
                parsed.profile_json = Some(
                    value
                        .into_string()
                        .map_err(|_| SandboxLaunchError::usage("--profile-json must be UTF-8"))?,
                );
                continue;
            }
            if arg == "--failure-fd" {
                let value = args.next().ok_or_else(|| {
                    SandboxLaunchError::usage("--failure-fd requires an integer value")
                })?;
                parsed.failure_fd = Some(parse_fd(&value)?);
                continue;
            }
            if arg == "--failure-marker" {
                let value = args.next().ok_or_else(|| {
                    SandboxLaunchError::usage("--failure-marker requires a path value")
                })?;
                parsed.failure_marker = Some(PathBuf::from(value));
                continue;
            }
            if let Some(value) = arg
                .to_str()
                .and_then(|value| value.strip_prefix("--failure-marker="))
            {
                parsed.failure_marker = Some(PathBuf::from(value));
                continue;
            }
            return Err(SandboxLaunchError::usage(format!(
                "unknown sandbox-launch argument: {}",
                arg.to_string_lossy()
            )));
        }
        Ok(parsed)
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn parse_fd(value: &OsString) -> Result<i32, SandboxLaunchError> {
    let value = value
        .to_str()
        .ok_or_else(|| SandboxLaunchError::usage("--profile-fd must be valid UTF-8"))?;
    let fd = value
        .parse::<i32>()
        .map_err(|_| SandboxLaunchError::usage("--profile-fd must be a non-negative integer"))?;
    if fd < 0 {
        return Err(SandboxLaunchError::usage(
            "--profile-fd must be a non-negative integer",
        ));
    }
    Ok(fd)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn read_profile_json(value: &str) -> Result<SandboxProfile, SandboxLaunchError> {
    serde_json::from_str(value).map_err(|error| {
        SandboxLaunchError::usage(format!("invalid sandbox profile JSON: {error}"))
    })
}

#[cfg(unix)]
fn read_profile(fd: RawFd) -> Result<SandboxProfile, SandboxLaunchError> {
    // Validate first because constructing `File` ownership from a closed raw
    // descriptor triggers Rust's IO-safety abort instead of a recoverable error.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        return Err(SandboxLaunchError::runtime(format!(
            "sandbox profile fd {fd} is not open: {}",
            std::io::Error::last_os_error()
        )));
    }

    // The launcher owns this inherited descriptor. Dropping the File before
    // parsing guarantees the target cannot inherit profile contents.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|error| {
        SandboxLaunchError::runtime(format!("failed to read sandbox profile fd {fd}: {error}"))
    })?;
    drop(file);
    serde_json::from_slice(&bytes).map_err(|error| {
        SandboxLaunchError::usage(format!("invalid sandbox profile JSON: {error}"))
    })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn read_profile_file(path: &Path) -> Result<SandboxProfile, SandboxLaunchError> {
    let mut file = File::open(path).map_err(|error| {
        SandboxLaunchError::runtime(format!(
            "failed to open sandbox profile file {}: {error}",
            path.display()
        ))
    })?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|error| {
        SandboxLaunchError::runtime(format!(
            "failed to read sandbox profile file {}: {error}",
            path.display()
        ))
    })?;
    drop(file);
    let _ = std::fs::remove_file(path);
    serde_json::from_slice(&bytes).map_err(|error| {
        SandboxLaunchError::usage(format!("invalid sandbox profile JSON: {error}"))
    })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn write_failure_marker(path: &Path) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(b"sandbox_unavailable")?;
    file.sync_all()
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn write_failure_fd(fd: RawFd) -> std::io::Result<()> {
    // Validate first: building a `File` from a closed descriptor aborts
    // under Rust's IO-safety checks instead of returning an error.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the descriptor is open and belongs to the supervising caller;
    // ManuallyDrop leaves it open so this write does not close it.
    let mut file = std::mem::ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
    file.write_all(b"sandbox_unavailable")
}

#[cfg(target_os = "macos")]
fn apply_sandbox(profile: &SandboxProfile) -> Result<(), SandboxLaunchError> {
    if profile.mode == aft::config::SandboxMode::Off {
        return Ok(());
    }
    seatbelt::apply(profile).map_err(|error| {
        SandboxLaunchError::unavailable(format!("failed to apply Seatbelt profile: {error}"))
    })
}

#[cfg(target_os = "linux")]
fn apply_sandbox(profile: &SandboxProfile) -> Result<(), SandboxLaunchError> {
    if profile.mode == aft::config::SandboxMode::Off {
        return Ok(());
    }
    let applied = landlock_backend::apply(profile).map_err(|error| {
        SandboxLaunchError::unavailable(format!("failed to apply Landlock ruleset: {error}"))
    })?;
    emit_linux_warning(profile, applied);
    Ok(())
}

#[cfg(target_os = "linux")]
fn emit_linux_warning(profile: &SandboxProfile, applied: landlock_backend::AppliedLandlock) {
    if profile.mode == aft::config::SandboxMode::Relaxed {
        let mut denies = Vec::new();
        for path in profile.read_deny.iter().chain(&profile.data_policy.deny) {
            denies.push(format!("read:{}", path.display()));
        }
        for path in profile
            .write_deny
            .iter()
            .chain(&profile.write_deny_nested)
            .chain(&profile.data_policy.deny)
        {
            denies.push(format!("write:{}", path.display()));
        }
        for path in &profile.deny_prefixes {
            denies.push(format!("read:{}*", path.display()));
            denies.push(format!("write:{}*", path.display()));
        }
        eprintln!("sandbox-launch: profile=relaxed unenforced=[{}] (Landlock cannot subtract denies from default-allow root grants)", denies.join(","));
        return;
    }
    let write_roots = profile.write_allow_roots();
    let mut unenforced = Vec::new();
    if profile
        .write_deny_nested
        .iter()
        .any(|deny| write_roots.iter().any(|root| paths_overlap(deny, root)))
    {
        unenforced.push("nested_write_deny");
    }
    if profile.read_deny.iter().any(|deny| {
        profile
            .read_allow
            .iter()
            .any(|grant| paths_overlap(deny, grant))
    }) {
        unenforced.push("read_deny");
    }
    if !profile.socket_deny.is_empty() {
        unenforced.push("socket_deny");
    }
    if !profile.loopback_tcp_connect_deny.is_empty() {
        // Handling TCP connect in Landlock would deny every unlisted port;
        // leave networking alone rather than break builds and local test servers.
        unenforced.push("sccache_server_unblockable_on_landlock");
    }
    if profile.write_deny_nested.iter().any(|deny| {
        deny.file_name() == Some(std::ffi::OsStr::new("config.worktree"))
            && write_roots.iter().any(|root| paths_overlap(deny, root))
    }) {
        // The linked Git directory must be writable for index updates, but
        // Landlock cannot subtract its operator-owned hooks configuration.
        unenforced.push("worktree_git_config_mutable_on_landlock");
    }
    if profile.write_deny_nested.iter().any(|deny| {
        matches!(
            deny.file_name().and_then(|name| name.to_str()),
            Some("commondir" | "gitdir")
        ) && write_roots.iter().any(|root| paths_overlap(deny, root))
    }) {
        unenforced.push("worktree_git_binding_mutable_on_landlock");
    }

    eprint!("sandbox-launch: unenforced=[{}]", unenforced.join(","));
    if applied.partially_enforced {
        eprint!(
            " landlock_abi={} landlock_required={}",
            landlock_backend::abi_label(applied.effective_abi),
            landlock_backend::abi_label(landlock_backend::REQUIRED_WRITE_ABI)
        );
    }
    if applied.yama_same_uid_exposed {
        eprint!(" proc_same_uid_exposure=[environ,maps,mem] yama_ptrace_scope=zero_or_unknown");
    }
    eprintln!();
}

#[cfg(target_os = "linux")]
fn paths_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

#[cfg(unix)]
fn exec_target(command: &[OsString]) -> Result<(), SandboxLaunchError> {
    let error = Command::new(&command[0]).args(&command[1..]).exec();
    Err(SandboxLaunchError::runtime(format!(
        "failed to exec target {}: {error}",
        command[0].to_string_lossy()
    )))
}

#[cfg(target_os = "macos")]
fn print_support() -> Result<(), SandboxLaunchError> {
    print_json(serde_json::json!({
        "platform": "macos",
        "supported": true,
        "backend": "seatbelt"
    }))
}

#[cfg(target_os = "linux")]
fn print_support() -> Result<(), SandboxLaunchError> {
    let applied = landlock_backend::probe().map_err(|error| {
        SandboxLaunchError::unavailable(format!("failed to probe Landlock: {error}"))
    })?;
    print_json(serde_json::json!({
        "platform": "linux",
        "supported": true,
        "backend": "landlock",
        "landlock_abi": landlock_backend::abi_label(applied.effective_abi),
        "target_abi": landlock_backend::abi_label(landlock_backend::TARGET_ABI),
        "required_refer_abi": landlock_backend::abi_label(landlock_backend::REQUIRED_REFER_ABI),
        "required_write_abi": landlock_backend::abi_label(landlock_backend::REQUIRED_WRITE_ABI),
        "partially_enforced": applied.partially_enforced,
        "unenforced": ["sccache_server_unblockable_on_landlock"]
    }))
}

#[cfg(windows)]
fn print_support() -> Result<(), SandboxLaunchError> {
    print_json(serde_json::json!({
        "platform": "windows",
        "supported": false,
        "backend": null,
        "unenforced": ["sccache_server_unblockable_without_native_sandbox"]
    }))
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn print_json(value: serde_json::Value) -> Result<(), SandboxLaunchError> {
    println!(
        "{}",
        serde_json::to_string(&value).map_err(|error| {
            SandboxLaunchError::runtime(format!("failed to serialize support information: {error}"))
        })?
    );
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn print_usage() {
    println!("Usage: aft sandbox-launch --profile-fd <fd> -- <command> [args...]");
    println!("       aft sandbox-launch --profile-file <path> -- <command> [args...]");
    println!("       aft sandbox-launch --profile-json <json> -- <command> [args...]");
    println!("       aft sandbox-launch --support");
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;

    fn os_args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_profile_fd_and_command() {
        let args = LaunchArgs::parse(os_args(&[
            "--profile-fd",
            "9",
            "--",
            "/bin/bash",
            "-c",
            "true",
        ]))
        .expect("valid launcher arguments");
        assert_eq!(args.profile_fd, Some(9));
        assert_eq!(args.profile_file, None);
        assert_eq!(args.profile_json, None);
        assert_eq!(args.failure_marker, None);
        assert_eq!(args.command, os_args(&["/bin/bash", "-c", "true"]));
    }

    #[test]
    fn parses_profile_file_and_command() {
        let args = LaunchArgs::parse(os_args(&[
            "--profile-file",
            "/tmp/task.sandbox-profile.json",
            "--failure-marker",
            "/tmp/task.sandbox-unavailable",
            "--",
            "/bin/sh",
            "-c",
            "true",
        ]))
        .expect("valid launcher arguments");
        assert_eq!(
            args.profile_file,
            Some(PathBuf::from("/tmp/task.sandbox-profile.json"))
        );
        assert_eq!(args.profile_fd, None);
        assert_eq!(args.profile_json, None);
        assert_eq!(
            args.failure_marker,
            Some(PathBuf::from("/tmp/task.sandbox-unavailable"))
        );
        assert_eq!(args.command, os_args(&["/bin/sh", "-c", "true"]));
    }

    #[test]
    fn failure_fd_receives_the_marker_and_stays_open() {
        use std::os::fd::AsRawFd;
        let args = LaunchArgs::parse(os_args(&[
            "--profile-json",
            "{}",
            "--failure-fd",
            "4",
            "--",
            "/bin/true",
        ]))
        .expect("valid launcher arguments");
        assert_eq!(args.failure_fd, Some(4));

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("failure");
        let file = File::create(&path).unwrap();
        write_failure_fd(file.as_raw_fd()).unwrap();
        assert!(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } >= 0);
        drop(file);
        assert_eq!(std::fs::read(path).unwrap(), b"sandbox_unavailable");
    }

    #[test]
    fn parses_profile_json_as_a_verified_buffer() {
        let args = LaunchArgs::parse(os_args(&[
            "--profile-json",
            r#"{"write_allow":[]}"#,
            "--",
            "/bin/true",
        ]))
        .unwrap();
        assert_eq!(args.profile_json.as_deref(), Some(r#"{"write_allow":[]}"#));
        assert_eq!(args.command, os_args(&["/bin/true"]));
    }

    #[test]
    fn rejects_negative_profile_fd() {
        let error = LaunchArgs::parse(os_args(&["--profile-fd", "-1", "--", "true"]))
            .expect_err("negative descriptor must fail");
        assert_eq!(error.exit_code(), 2);
    }
}
