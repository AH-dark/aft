//! A kernel-enforced marker that tells the gh shim it runs inside AFT's own
//! worker jail.
//!
//! The shim must refuse to fall back to the operator's gh for a jailed worker,
//! even when the worker clears its environment (`env -i`) or unsets its ticket.
//! Generic "am I sandboxed" signals cannot do this: Linux `no_new_privs` and a
//! macOS Seatbelt policy are also set by containers, CI runners, systemd units,
//! bubblewrap or another tool's sandbox. A standalone user there must keep gh
//! passthrough.
//!
//! The marker is a canary file at a fixed path under the account's default AFT
//! storage. Only the worker-jail profile denies reading it. The path comes from
//! the password database (and `$HOME`), never from a variable a worker could
//! point somewhere readable. It is not secret: a same-uid shim can find it, and
//! so can the worker. Security rests on the kernel deny alone. The jail denies
//! reads and writes of the canary directory, so the worker can neither make the
//! canary readable nor delete, rename or replace it.
//!
//! Head shells (the operator's own agent) never deny the canary; their profiles
//! grant reading it, so a sandboxed head keeps v19 passthrough.

use std::path::{Path, PathBuf};

/// Directory, relative to an account home, that holds only the canary. It sits
/// in AFT's default storage root, `~/.local/share/cortexkit/aft`, even when the
/// installation is configured to use a different storage root.
pub const CANARY_DIR_FROM_HOME: &str = ".local/share/cortexkit/aft/gh-shim-worker-jail";
/// File name of the canary inside [`CANARY_DIR_FROM_HOME`].
pub const CANARY_FILE: &str = "canary";
#[cfg(unix)]
const CANARY_CONTENTS: &[u8] =
    b"AFT worker-jail canary. Worker shells cannot read this file; the gh shim uses that to recognize them.\n";

/// The canary directory for one account home.
pub fn dir_for_home(home: &Path) -> PathBuf {
    home.join(CANARY_DIR_FROM_HOME)
}

/// The canary file for one account home.
pub fn path_for_home(home: &Path) -> PathBuf {
    dir_for_home(home).join(CANARY_FILE)
}

/// Create the canary, or repair one whose contents or permissions changed, and
/// return its canonical directory. Runs outside any jail, before a profile is
/// built. Any failure means the jail cannot be fenced.
#[cfg(unix)]
pub fn prepare(home: &Path) -> Result<PathBuf, String> {
    use std::fs;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let dir = dir_for_home(home);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|error| {
            format!(
                "cannot create worker-jail canary {}: {error}",
                dir.display()
            )
        })?;
    let metadata = fs::symlink_metadata(&dir).map_err(|error| {
        format!(
            "cannot inspect worker-jail canary {}: {error}",
            dir.display()
        )
    })?;
    if !metadata.is_dir() {
        return Err(format!(
            "worker-jail canary directory is not a real directory: {}",
            dir.display()
        ));
    }
    // A worker on a backend without chmod mediation (Landlock) may have
    // changed these modes in an earlier run; put them back each launch.
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("cannot restore canary directory mode: {error}"))?;
    let file = dir.join(CANARY_FILE);
    let intact = fs::symlink_metadata(&file).is_ok_and(|metadata| metadata.is_file())
        && fs::read(&file).is_ok_and(|bytes| bytes == CANARY_CONTENTS);
    if !intact {
        let staging = dir.join(format!(".{CANARY_FILE}.{}", std::process::id()));
        let _ = fs::remove_file(&staging);
        fs::write(&staging, CANARY_CONTENTS)
            .and_then(|()| fs::rename(&staging, &file))
            .map_err(|error| {
                format!(
                    "cannot write worker-jail canary {}: {error}",
                    file.display()
                )
            })?;
    }
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("cannot restore canary mode: {error}"))?;
    if fs::read(&file).ok().as_deref() != Some(CANARY_CONTENTS) {
        return Err(format!(
            "worker-jail canary did not verify after writing: {}",
            file.display()
        ));
    }
    dir.canonicalize()
        .map_err(|error| format!("cannot canonicalize worker-jail canary: {error}"))
}

/// What the current process may do with one canary path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CanaryAccess {
    /// The kernel policy over this process denies reading the canary.
    Denied,
    /// The canary exists and this process can read it.
    Readable,
    /// No canary exists at this path.
    Missing,
}

/// Probe one canary path from inside the current process's policy.
///
/// Linux: Landlock does not mediate stat, so a jailed process sees the file
/// exist while `open` fails with EACCES. A stat that itself fails with EACCES
/// means an ancestor's mode was changed (Landlock cannot stop chmod); that is
/// treated as denied so a worker cannot hide the canary that way.
///
/// macOS: ask Seatbelt whether this process may read the file's data. The
/// query opens nothing and, with SANDBOX_CHECK_NO_REPORT, logs no violation.
pub fn access(path: &Path) -> CanaryAccess {
    #[cfg(target_os = "linux")]
    {
        linux_access(path)
    }
    #[cfg(target_os = "macos")]
    {
        macos_access(path)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = path;
        CanaryAccess::Missing
    }
}

#[cfg(target_os = "linux")]
fn linux_access(path: &Path) -> CanaryAccess {
    match std::fs::metadata(path) {
        Err(error) if error.raw_os_error() == Some(libc::EACCES) => CanaryAccess::Denied,
        Err(_) => CanaryAccess::Missing,
        Ok(metadata) if !metadata.is_file() => CanaryAccess::Missing,
        Ok(_) => match std::fs::File::open(path) {
            Ok(_) => CanaryAccess::Readable,
            Err(error) if error.raw_os_error() == Some(libc::EACCES) => CanaryAccess::Denied,
            Err(_) => CanaryAccess::Missing,
        },
    }
}

#[cfg(target_os = "macos")]
mod seatbelt {
    unsafe extern "C" {
        pub fn sandbox_check(
            pid: libc::pid_t,
            operation: *const libc::c_char,
            filter: libc::c_int,
            ...
        ) -> libc::c_int;
        /// Exported by libsystem_sandbox; ORed into the filter type.
        pub static SANDBOX_CHECK_NO_REPORT: libc::c_int;
    }
    /// `SANDBOX_FILTER_PATH` in libsystem_sandbox's filter-type enumeration.
    pub const SANDBOX_FILTER_PATH: libc::c_int = 1;
}

#[cfg(target_os = "macos")]
fn macos_access(path: &Path) -> CanaryAccess {
    use std::os::unix::ffi::OsStrExt;
    // Seatbelt matches the resolved path. The canary's own metadata may be
    // denied, so resolve its parent directory and append the file name.
    let resolved = path
        .parent()
        .and_then(|parent| parent.canonicalize().ok())
        .zip(path.file_name())
        .map(|(parent, name)| parent.join(name))
        .unwrap_or_else(|| path.to_path_buf());
    let Ok(c_path) = std::ffi::CString::new(resolved.as_os_str().as_bytes()) else {
        return CanaryAccess::Missing;
    };
    // SAFETY: both strings are NUL-terminated and live across the call; the
    // path filter takes exactly one const char* variadic argument.
    let denied = unsafe {
        seatbelt::sandbox_check(
            libc::getpid(),
            c"file-read-data".as_ptr(),
            seatbelt::SANDBOX_FILTER_PATH | seatbelt::SANDBOX_CHECK_NO_REPORT,
            c_path.as_ptr(),
        ) != 0
    };
    if denied {
        return CanaryAccess::Denied;
    }
    if std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        CanaryAccess::Readable
    } else {
        CanaryAccess::Missing
    }
}

/// Whether this process runs inside AFT's worker jail, judged only by kernel
/// state and the account's password-database home.
///
/// Linux also requires `no_new_privs`, which every AFT jail sets: without it a
/// denied canary is only a mode change outside any jail.
pub fn inside_worker_jail() -> bool {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: PR_GET_NO_NEW_PRIVS takes no pointer arguments.
        if unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) } != 1 {
            return false;
        }
    }
    candidate_homes()
        .iter()
        .any(|home| access(&path_for_home(home)) == CanaryAccess::Denied)
}

/// Homes whose canary the shim checks. The password database cannot be
/// changed by a worker; `$HOME` is only an extra place to look, so unsetting
/// or redirecting it can never hide the account's canary.
fn candidate_homes() -> Vec<PathBuf> {
    #[cfg(test)]
    if let Some(homes) = TEST_HOMES.with(|slot| slot.borrow().clone()) {
        return homes;
    }
    let mut homes = Vec::new();
    #[cfg(unix)]
    if let Ok(home) = crate::production_storage::account_home() {
        homes.push(home);
    }
    if let Some(home) = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
    {
        if !homes.contains(&home) {
            homes.push(home);
        }
    }
    homes
}

#[cfg(test)]
thread_local! {
    static TEST_HOMES: std::cell::RefCell<Option<Vec<PathBuf>>> =
        const { std::cell::RefCell::new(None) };
}

/// Point this thread's shim checks at test homes instead of the account's.
#[cfg(test)]
pub(crate) fn with_test_homes<R>(homes: Vec<PathBuf>, run: impl FnOnce() -> R) -> R {
    let previous = TEST_HOMES.with(|slot| slot.replace(Some(homes)));
    struct Restore(Option<Vec<PathBuf>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_HOMES.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(previous);
    run()
}

/// Launcher check, after the jail is applied and before the shell starts: the
/// jail must deny this canary. A jail that cannot is refused, never run
/// unfenced.
pub fn verify_denied_after_apply(path: &Path) -> Result<(), String> {
    match access(path) {
        CanaryAccess::Denied => Ok(()),
        other => Err(format!(
            "the worker jail does not deny its gh-shim canary {} ({other:?}); refusing to start an unfenced worker shell",
            path.display()
        )),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn prepare_creates_and_repairs_the_canary() {
        let home = tempfile::tempdir().unwrap();
        let dir = prepare(home.path()).unwrap();
        assert_eq!(dir, dir_for_home(home.path()).canonicalize().unwrap());
        let file = dir.join(CANARY_FILE);
        assert_eq!(std::fs::read(&file).unwrap(), CANARY_CONTENTS);
        std::fs::write(&file, b"tampered").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        prepare(home.path()).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), CANARY_CONTENTS);
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn prepare_refuses_a_canary_directory_replaced_by_a_symlink() {
        let home = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let dir = dir_for_home(home.path());
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), &dir).unwrap();
        assert!(prepare(home.path()).is_err());
    }

    #[test]
    fn an_unconfined_process_sees_a_readable_or_missing_canary() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(access(&path_for_home(home.path())), CanaryAccess::Missing);
        prepare(home.path()).unwrap();
        assert_eq!(access(&path_for_home(home.path())), CanaryAccess::Readable);
        with_test_homes(vec![home.path().to_path_buf()], || {
            assert!(!inside_worker_jail());
        });
        assert!(verify_denied_after_apply(&path_for_home(home.path())).is_err());
    }
}
