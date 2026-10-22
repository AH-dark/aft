//! Immutable executable files shared by tests that need a named fixture.

#![allow(dead_code)]

use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const CACHE_ROOT: &str = "aft-test-executables";

fn cache_root() -> PathBuf {
    std::env::temp_dir().join(CACHE_ROOT)
}

fn cache_path(root: &Path, bytes: &[u8], name: &str) -> PathBuf {
    let mut digest = Sha256::new();
    digest.update(b"mode:755\0");
    digest.update(bytes);
    root.join(format!("{:x}", digest.finalize())).join(name)
}

/// Return the immutable cache path for these executable bytes.
///
/// Publishing by hard link is atomic and refuses to replace an existing path,
/// so concurrent test processes either share one inode or verify the winner.
pub fn cached_executable(bytes: &[u8]) -> io::Result<PathBuf> {
    cached_executable_named(
        bytes,
        if cfg!(windows) {
            "executable.exe"
        } else {
            "executable"
        },
    )
}

/// Return the content-addressed executable under a caller-required basename.
pub fn cached_executable_named(bytes: &[u8], name: &str) -> io::Result<PathBuf> {
    cached_executable_named_in(&cache_root(), bytes, name)
}

/// Cache an executable inside a fixture root that must remain visible to a sandbox.
pub fn cached_executable_named_in(root: &Path, bytes: &[u8], name: &str) -> io::Result<PathBuf> {
    if Path::new(name).file_name().and_then(|value| value.to_str()) != Some(name) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cached executable name must be one path component",
        ));
    }
    let path = cache_path(root, bytes, name);
    if path.exists() {
        verify_cached(&path, bytes)?;
        return Ok(path);
    }

    let cache_root = root;
    fs::create_dir_all(path.parent().expect("cache path has a parent"))?;
    let pending_dir = tempfile::Builder::new()
        .prefix(".pending-")
        .tempdir_in(cache_root)?;
    let pending = pending_dir.path().join("executable");
    write_executable_with_mode(&pending, bytes, 0o755)?;
    #[cfg(target_os = "linux")]
    OpenOptions::new().read(true).open(&pending)?.sync_all()?;
    // Windows requires a writable handle for FlushFileBuffers.
    #[cfg(not(target_os = "linux"))]
    OpenOptions::new().write(true).open(&pending)?.sync_all()?;

    match fs::hard_link(&pending, &path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    verify_cached(&path, bytes)?;
    Ok(path)
}

/// Make a named fixture resolve to an already cached executable without copying it.
pub fn link_cached_path(source: &Path, destination: &Path) -> io::Result<PathBuf> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(source, destination)?;
    #[cfg(windows)]
    fs::hard_link(source, destination)?;
    Ok(destination.to_path_buf())
}

/// Make a named fixture resolve to the shared executable without copying it.
pub fn link_cached_executable(destination: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    let cached = cached_executable(bytes)?;
    link_cached_path(&cached, destination)
}

/// Executing an executable while any process still has it open for writing can fail
/// with `ETXTBSY` (text file busy) on Linux. A short-lived writer process keeps test-thread forks
/// from inheriting the staged file's write descriptor before it is renamed.
pub fn write_executable(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_executable_with_mode(path, bytes, 0o755)
}

/// Publish a staged executable with the requested Unix permission bits.
pub fn write_executable_with_mode(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    static NEXT_TEMPORARY_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    let (temporary_path, file) = loop {
        let temporary_path = parent.join(format!(
            ".{file_name}.tmp.{}.{}",
            std::process::id(),
            NEXT_TEMPORARY_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
        {
            Ok(file) => break (temporary_path, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    drop(file);

    let write_result = write_staged_bytes(&temporary_path, bytes);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = fs::set_permissions(&temporary_path, fs::Permissions::from_mode(mode)) {
            let _ = fs::remove_file(&temporary_path);
            return Err(error);
        }
    }
    #[cfg(not(unix))]
    let _ = mode;
    if let Err(error) = fs::rename(&temporary_path, path) {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }
    Ok(())
}

#[cfg(unix)]
fn write_staged_bytes(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::process::{Command, Stdio};

    let mut writer = Command::new("/bin/sh")
        .args(["-c", "/bin/cat > \"$1\"", "aft-test-executable-writer"])
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let write_result = writer
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("test executable writer has no stdin"))
        .and_then(|mut stdin| stdin.write_all(bytes));
    let status = writer.wait()?;
    write_result?;
    if !status.success() {
        return Err(io::Error::other("test executable writer process failed"));
    }
    Ok(())
}

#[cfg(not(unix))]
fn write_staged_bytes(path: &Path, bytes: &[u8]) -> io::Result<()> {
    OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)?
        .write_all(bytes)
}

fn verify_cached(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || fs::read(path)? != bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "content-addressed test executable is invalid: {}",
                path.display()
            ),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o777 != 0o755 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "cached test executable is not mode 0755: {}",
                    path.display()
                ),
            ));
        }
    }
    Ok(())
}
