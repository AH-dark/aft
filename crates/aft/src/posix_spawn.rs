//! Session-isolated pipe children without a fork of the daemon.
//!
//! File actions retain only stdio and the two wrapper marker descriptors. All
//! preparation, including PATH lookup and privacy attributes, happens in the parent.
use std::collections::BTreeMap;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

#[derive(Debug)]
enum Handle {
    Std(std::process::Child),
    Native {
        pid: libc::pid_t,
        status: Option<ExitStatus>,
    },
}

/// Like std's Child, dropping this handle does not kill or reap a running child.
#[derive(Debug)]
pub(crate) struct Child {
    handle: Handle,
    pub(crate) stdin: Option<File>,
}
impl From<std::process::Child> for Child {
    fn from(mut child: std::process::Child) -> Self {
        let stdin = child
            .stdin
            .take()
            .map(|pipe| File::from(OwnedFd::from(pipe)));
        Self {
            handle: Handle::Std(child),
            stdin,
        }
    }
}
impl Child {
    #[cfg(test)]
    pub(crate) fn kill(&mut self) -> io::Result<()> {
        match &mut self.handle {
            Handle::Std(child) => child.kill(),
            Handle::Native {
                status: Some(_), ..
            } => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "child already reaped",
            )),
            Handle::Native { pid, .. } => {
                if unsafe { libc::kill(*pid, libc::SIGKILL) } == 0 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            }
        }
    }
    pub(crate) fn id(&self) -> u32 {
        match &self.handle {
            Handle::Std(child) => child.id(),
            Handle::Native { pid, .. } => *pid as u32,
        }
    }
    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.wait_inner(libc::WNOHANG)
    }
    pub(crate) fn wait(&mut self) -> io::Result<ExitStatus> {
        drop(self.stdin.take());
        self.wait_inner(0)?
            .ok_or_else(|| io::Error::other("waitpid returned no status"))
    }
    fn wait_inner(&mut self, flags: libc::c_int) -> io::Result<Option<ExitStatus>> {
        match &mut self.handle {
            Handle::Std(child) => {
                if flags == 0 {
                    child.wait().map(Some)
                } else {
                    child.try_wait()
                }
            }
            Handle::Native { pid, status } => {
                if status.is_some() {
                    return Ok(*status);
                }
                loop {
                    let mut raw = 0;
                    let rc = unsafe { libc::waitpid(*pid, &mut raw, flags) };
                    if rc == *pid {
                        *status = Some(ExitStatus::from_raw(raw));
                        return Ok(*status);
                    }
                    if rc == 0 {
                        return Ok(None);
                    }
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
            }
        }
    }
}

pub(crate) struct PipeIo<'a> {
    pub(crate) gated_stdin: bool,
    pub(crate) stdout: &'a File,
    pub(crate) stderr: &'a File,
    pub(crate) exit: RawFd,
    pub(crate) failure: RawFd,
}

type AddChdir =
    unsafe extern "C" fn(*mut libc::posix_spawn_file_actions_t, *const libc::c_char) -> libc::c_int;
#[cfg(not(target_os = "macos"))]
type CloseFrom =
    unsafe extern "C" fn(*mut libc::posix_spawn_file_actions_t, libc::c_int) -> libc::c_int;
#[derive(Clone, Copy)]
struct Functions {
    chdir: AddChdir,
    #[cfg(not(target_os = "macos"))]
    closefrom: CloseFrom,
}
type SpawnBackend = Result<Functions, String>;
static BACKEND: OnceLock<SpawnBackend> = OnceLock::new();
static FALLBACK_SPAWNS: AtomicU64 = AtomicU64::new(0);

// Resolve optional libc extensions at runtime so release binaries retain their
// old glibc baseline instead of acquiring GLIBC_2.29/GLIBC_2.34 symbol versions.
fn symbol(name: &CStr) -> *mut libc::c_void {
    unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) }
}
fn discover() -> SpawnBackend {
    if !cfg!(any(
        target_os = "macos",
        all(target_os = "linux", target_env = "gnu")
    )) {
        return Err("unsupported Unix libc".into());
    }
    #[cfg(target_os = "macos")]
    let chdir = {
        let modern = symbol(c"posix_spawn_file_actions_addchdir");
        if modern.is_null() {
            symbol(c"posix_spawn_file_actions_addchdir_np")
        } else {
            modern
        }
    };
    #[cfg(not(target_os = "macos"))]
    let chdir = symbol(c"posix_spawn_file_actions_addchdir_np");
    if chdir.is_null() {
        return Err("posix_spawn_file_actions_addchdir_np".into());
    }
    #[cfg(not(target_os = "macos"))]
    let closefrom = symbol(c"posix_spawn_file_actions_addclosefrom_np");
    #[cfg(not(target_os = "macos"))]
    if closefrom.is_null() {
        return Err("posix_spawn_file_actions_addclosefrom_np".into());
    }
    let functions = Functions {
        chdir: unsafe { std::mem::transmute::<*mut libc::c_void, AddChdir>(chdir) },
        #[cfg(not(target_os = "macos"))]
        closefrom: unsafe { std::mem::transmute::<*mut libc::c_void, CloseFrom>(closefrom) },
    };
    // Older libc may export the file actions yet reject SETSID. Probe before
    // accepting work, not by retrying a user's command after an ambiguous error.
    let null = File::options()
        .read(true)
        .write(true)
        .open("/dev/null")
        .map_err(|error| format!("capability probe open /dev/null: {error}"))?;
    let mut command = Command::new("/usr/bin/true");
    command.env_clear().current_dir("/");
    let mut child = spawn_native(
        &command,
        true,
        false,
        &PipeIo {
            gated_stdin: false,
            stdout: &null,
            stderr: &null,
            exit: null.as_raw_fd(),
            failure: null.as_raw_fd(),
        },
        functions,
    )
    .map_err(|error| format!("POSIX_SPAWN_SETSID probe: {error}"))?;
    if !child
        .wait()
        .map_err(|error| format!("POSIX_SPAWN_SETSID wait: {error}"))?
        .success()
    {
        return Err("POSIX_SPAWN_SETSID probe exited unsuccessfully".into());
    }
    Ok(functions)
}

pub(crate) fn initialize() {
    BACKEND.get_or_init(|| {
        let backend = discover();
        if let Err(reason) = &backend {
            crate::slog_warn!("bash_spawn=fork_fallback missing_capability={reason}");
        } else {
            crate::slog_info!("bash_spawn=posix_spawn");
        }
        backend
    });
}

#[cfg(test)]
thread_local! { pub(crate) static FORCE_FALLBACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
fn missing_capability() -> Option<String> {
    #[cfg(test)]
    if FORCE_FALLBACK.get() {
        return Some("test-forced missing capability".into());
    }
    initialize();
    BACKEND.get().unwrap().as_ref().err().cloned()
}
pub(crate) fn health() -> serde_json::Value {
    let missing = missing_capability();
    serde_json::json!({
        "backend": if missing.is_some() { "fork_fallback" } else { "posix_spawn" },
        "missing_capability": missing,
        "fallback_spawns": FALLBACK_SPAWNS.load(Ordering::Relaxed),
        "pty_backend": "fork",
    })
}

/// None means the caller must use its existing pre_exec composition unchanged.
pub(crate) fn try_spawn(
    command: &Command,
    cleared: bool,
    disclaim: bool,
    io: &PipeIo<'_>,
) -> Option<io::Result<Child>> {
    if missing_capability().is_some() {
        FALLBACK_SPAWNS.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    Some(spawn_native(
        command,
        cleared,
        disclaim,
        io,
        *BACKEND.get().unwrap().as_ref().unwrap(),
    ))
}

fn check(rc: libc::c_int) -> io::Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(rc))
    }
}
fn cstring(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "command or environment contains NUL",
        )
    })
}
struct Attributes(libc::posix_spawnattr_t);
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe {
            libc::posix_spawnattr_destroy(&mut self.0);
        }
    }
}
struct Actions(libc::posix_spawn_file_actions_t);
impl Drop for Actions {
    fn drop(&mut self) {
        unsafe {
            libc::posix_spawn_file_actions_destroy(&mut self.0);
        }
    }
}

fn high_copy(fd: RawFd) -> io::Result<File> {
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) };
    if copy < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(copy) })
    }
}
fn input(gated: bool) -> io::Result<(File, Option<File>)> {
    if !gated {
        return Ok((File::open("/dev/null")?, None));
    }
    // Use the same CLOEXEC pipe implementation as std::Command's piped stdin.
    // The spawn allowlist also closes both originals in the child after dup2.
    let (read, write) = std::io::pipe()?;
    Ok((
        File::from(OwnedFd::from(read)),
        Some(File::from(OwnedFd::from(write))),
    ))
}

fn spawn_native(
    command: &Command,
    cleared: bool,
    disclaim: bool,
    io: &PipeIo<'_>,
    functions: Functions,
) -> io::Result<Child> {
    let mut environment = if cleared {
        BTreeMap::new()
    } else {
        std::env::vars_os().collect::<BTreeMap<_, _>>()
    };
    for (key, value) in command.get_envs() {
        if let Some(value) = value {
            environment.insert(key.to_owned(), value.to_owned());
        } else {
            environment.remove(key);
        }
    }
    let cwd = command
        .get_current_dir()
        .map(Path::to_owned)
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)?;
    let cwd = if cwd.is_absolute() {
        cwd
    } else {
        std::env::current_dir()?.join(cwd)
    };
    let program = command.get_program();
    // Resolve only after applying the complete child environment; posix_spawnp
    // would instead search the daemon's PATH. Preserve the requested argv[0].
    let path = if Path::new(program).is_absolute() {
        Path::new(program).to_owned()
    } else {
        which::which_in(program, environment.get(OsStr::new("PATH")), &cwd)
            .map_err(|error| io::Error::new(io::ErrorKind::NotFound, error))?
    };
    let path = cstring(path.as_os_str())?;
    let cwd = cstring(cwd.as_os_str())?;
    let argv_storage = std::iter::once(program)
        .chain(command.get_args())
        .map(cstring)
        .collect::<io::Result<Vec<_>>>()?;
    let env_storage = environment
        .into_iter()
        .map(|(key, value)| {
            let mut entry = OsString::from(key);
            entry.push("=");
            entry.push(value);
            cstring(&entry)
        })
        .collect::<io::Result<Vec<_>>>()?;
    let pointers = |values: &[CString]| {
        values
            .iter()
            .map(|value| value.as_ptr().cast_mut())
            .chain(std::iter::once(std::ptr::null_mut()))
            .collect::<Vec<_>>()
    };
    let argv = pointers(&argv_storage);
    let env = pointers(&env_storage);
    unsafe {
        let mut attr = std::mem::zeroed();
        check(libc::posix_spawnattr_init(&mut attr))?;
        let mut attr = Attributes(attr);
        // Darwin's sys/spawn.h exposes SETSID even though libc does not name it.
        #[cfg(target_os = "macos")]
        const POSIX_SPAWN_SETSID: libc::c_int = 0x0400;
        #[cfg(target_os = "macos")]
        let flags =
            POSIX_SPAWN_SETSID | libc::POSIX_SPAWN_CLOEXEC_DEFAULT | libc::POSIX_SPAWN_SETSIGDEF;
        #[cfg(target_os = "linux")]
        let flags = libc::POSIX_SPAWN_SETSID | libc::POSIX_SPAWN_SETSIGDEF;
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let flags = libc::POSIX_SPAWN_SETSIGDEF;
        check(libc::posix_spawnattr_setflags(&mut attr.0, flags as i16))?;
        let mut defaults = std::mem::zeroed();
        libc::sigemptyset(&mut defaults);
        libc::sigaddset(&mut defaults, libc::SIGPIPE);
        check(libc::posix_spawnattr_setsigdefault(&mut attr.0, &defaults))?;
        // Pipe commands inherit the parent's signal mask, as std::Command does.
        crate::privacy_spawn::apply_spawn_attribute(&mut attr.0, disclaim)?;
        let mut actions = std::mem::zeroed();
        check(libc::posix_spawn_file_actions_init(&mut actions))?;
        let mut actions = Actions(actions);
        let (stdin, parent_stdin) = input(io.gated_stdin)?;
        // Copy every source above the targets before constructing actions: even
        // when the daemon has closed stdio, no dup2 can overwrite another source.
        let sources = [
            stdin.as_raw_fd(),
            io.stdout.as_raw_fd(),
            io.stderr.as_raw_fd(),
            io.exit,
            io.failure,
        ]
        .into_iter()
        .map(high_copy)
        .collect::<io::Result<Vec<_>>>()?;
        for (target, source) in sources.iter().enumerate() {
            check(libc::posix_spawn_file_actions_adddup2(
                &mut actions.0,
                source.as_raw_fd(),
                target as RawFd,
            ))?;
        }
        check((functions.chdir)(&mut actions.0, cwd.as_ptr()))?;
        #[cfg(not(target_os = "macos"))]
        check((functions.closefrom)(&mut actions.0, 5))?;
        let mut pid = 0;
        check(libc::posix_spawn(
            &mut pid,
            path.as_ptr(),
            &actions.0,
            &attr.0,
            argv.as_ptr(),
            env.as_ptr(),
        ))?;
        Ok(Child {
            handle: Handle::Native { pid, status: None },
            stdin: parent_stdin,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn spawn(command: &Command, root: &Path, gated: bool) -> (Child, [File; 4]) {
        let files =
            ["out", "err", "exit", "failure"].map(|name| File::create(root.join(name)).unwrap());
        let child = try_spawn(
            command,
            true,
            false,
            &PipeIo {
                gated_stdin: gated,
                stdout: &files[0],
                stderr: &files[1],
                exit: files[2].as_raw_fd(),
                failure: files[3].as_raw_fd(),
            },
        )
        .unwrap_or_else(|| panic!("modern libc required for native tests: {}", health()))
        .unwrap();
        (child, files)
    }

    #[test]
    fn native_spawn_cwd_env_arg0_markers_and_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("shell");
        std::os::unix::fs::symlink("/bin/sh", &script).unwrap();
        let mut command = Command::new("shell");
        command.current_dir(dir.path()).env_clear().env("PATH", dir.path()).env("TOKEN", "child-value")
            .args(["-c", "read release; printf '%s:%s:%s:%s' \"$0\" \"$TOKEN\" \"$PWD\" \"$release\"; printf exit >&3; printf fail >&4", "requested-zero"]);
        let (mut child, _files) = spawn(&command, dir.path(), true);
        let pid = child.id() as i32;
        assert_eq!(unsafe { libc::getsid(pid) }, pid);
        assert_eq!(unsafe { libc::getpgid(pid) }, pid);
        assert!(child.try_wait().unwrap().is_none());
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"released\n")
            .unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(child.try_wait().unwrap().unwrap().code(), Some(0));
        assert!(
            child.kill().is_err(),
            "never signal a pid once it has been reaped"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out")).unwrap(),
            format!(
                "requested-zero:child-value:{}:released",
                dir.path().display()
            )
        );
        assert_eq!(std::fs::read(dir.path().join("exit")).unwrap(), b"exit");
        assert_eq!(std::fs::read(dir.path().join("failure")).unwrap(), b"fail");
    }

    #[test]
    fn native_spawn_reports_exec_errors() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let not_executable = dir.path().join("not-executable");
        std::fs::write(&not_executable, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&not_executable, std::fs::Permissions::from_mode(0o644)).unwrap();
        for (program, errno) in [
            (dir.path().join("absent"), libc::ENOENT),
            (not_executable, libc::EACCES),
        ] {
            let mut command = Command::new(program);
            command.current_dir(dir.path());
            let file = File::create(dir.path().join("out")).unwrap();
            let error = try_spawn(
                &command,
                true,
                false,
                &PipeIo {
                    gated_stdin: false,
                    stdout: &file,
                    stderr: &file,
                    exit: file.as_raw_fd(),
                    failure: file.as_raw_fd(),
                },
            )
            .unwrap()
            .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(errno));
        }
    }

    #[test]
    fn native_spawn_fd_probe_child() {
        let Some(fd) = std::env::var_os("AFT_NATIVE_FD_PROBE") else {
            return;
        };
        let fd = fd.to_str().unwrap().parse::<i32>().unwrap();
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) },
            -1,
            "daemon fd leaked into child"
        );
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
        assert_ne!(
            unsafe { libc::fcntl(3, libc::F_GETFD) },
            -1,
            "exit marker missing"
        );
        assert_ne!(
            unsafe { libc::fcntl(4, libc::F_GETFD) },
            -1,
            "failure marker missing"
        );
    }

    #[test]
    fn native_spawn_closes_non_cloexec_daemon_fd() {
        let dir = tempfile::tempdir().unwrap();
        let file = File::create(dir.path().join("secret")).unwrap();
        let raw = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 200) };
        assert!(raw >= 200);
        let _held = unsafe { File::from_raw_fd(raw) };
        assert_eq!(
            unsafe { libc::fcntl(raw, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .current_dir(dir.path())
            .env_clear()
            .env("AFT_NATIVE_FD_PROBE", raw.to_string())
            .args([
                "--exact",
                "posix_spawn::tests::native_spawn_fd_probe_child",
                "--nocapture",
            ]);
        let (mut child, _files) = spawn(&command, dir.path(), false);
        assert!(
            child.wait().unwrap().success(),
            "{}",
            std::fs::read_to_string(dir.path().join("out")).unwrap()
        );
    }
}
