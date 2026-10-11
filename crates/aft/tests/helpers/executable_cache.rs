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
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&pending)?;
        file.write_all(bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o755))?;
        }
        #[cfg(target_os = "linux")]
        file.sync_all()?;
        #[cfg(not(target_os = "linux"))]
        file.sync_all()?;
    }

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
