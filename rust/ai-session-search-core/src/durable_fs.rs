//! Small, shared filesystem durability and collision primitives.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Report whether a directory entry exists without following symbolic links.
pub(crate) fn entry_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Sync a directory where the platform exposes directory handles.
#[cfg(unix)]
pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
pub(crate) fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Sync the directory entry containing `path`.
pub(crate) fn sync_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) => sync_directory(parent),
        None => Ok(()),
    }
}

/// RAII-owned same-parent directory staging with atomic no-replace publication.
///
/// Every staged entry is a single relative path component created with `create_new`. Publishing
/// syncs the staged directory, atomically renames it to an absent destination, and syncs the
/// parent. Dropping an unpublished transaction removes its staging directory.
pub(crate) struct StagedDirectory {
    path: PathBuf,
    committed: bool,
}

impl StagedDirectory {
    pub(crate) fn begin(parent: &Path, label: &str) -> Result<Self> {
        let mut components = Path::new(label).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            bail!("staging label must be one normal path component");
        }
        let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is earlier than the Unix epoch")?
            .as_nanos();
        let path = parent.join(format!(
            ".ai-session-search-{label}-stage-{}-{nonce}-{sequence}",
            std::process::id(),
        ));
        fs::create_dir(&path)
            .with_context(|| format!("failed to create staging directory {}", path.display()))?;
        Ok(Self {
            path,
            committed: false,
        })
    }

    pub(crate) fn write(&self, name: &Path, content: &[u8]) -> Result<()> {
        let mut components = name.components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            bail!("staged entry name must be one normal relative path component");
        }
        let path = self.path.join(name);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("failed to create staged artifact {}", path.display()))?;
        file.write_all(content)
            .with_context(|| format!("failed to write staged artifact {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync staged artifact {}", path.display()))
    }

    pub(crate) fn publish(mut self, destination: &Path) -> Result<()> {
        sync_directory(&self.path)
            .with_context(|| format!("failed to sync staging directory {}", self.path.display()))?;
        rename_noreplace(&self.path, destination).with_context(|| {
            format!(
                "failed to atomically publish {} as {}",
                self.path.display(),
                destination.display()
            )
        })?;
        self.path = destination.to_path_buf();
        sync_parent(destination)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for StagedDirectory {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// Atomically rename `source` to an absent `destination` without replacing any entry.
#[cfg(target_vendor = "apple")]
pub(crate) fn rename_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination path contains NUL")
    })?;
    // SAFETY: both pointers are valid NUL-terminated strings for the duration of the call.
    let result =
        unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn rename_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination path contains NUL")
    })?;
    // SAFETY: both pointers are valid NUL-terminated strings and AT_FDCWD scopes them to cwd.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
pub(crate) fn rename_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::MoveFileW;

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: both buffers are NUL-terminated and remain alive for the duration of the call.
    let result = unsafe { MoveFileW(source.as_ptr(), destination.as_ptr()) };
    if result != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux", windows)))]
pub(crate) fn rename_noreplace(_source: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace rename is unsupported on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_noreplace_claims_absent_path_and_rejects_existing_entry() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let destination = dir.path().join("destination");
        fs::write(&source, b"first").unwrap();
        rename_noreplace(&source, &destination).unwrap();
        assert!(!source.exists());
        assert_eq!(fs::read(&destination).unwrap(), b"first");

        fs::write(&source, b"second").unwrap();
        let error = rename_noreplace(&source, &destination).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&source).unwrap(), b"second");
        assert_eq!(fs::read(&destination).unwrap(), b"first");
    }

    #[test]
    fn staged_directory_rejects_traversal_and_cleans_unpublished_content() {
        let dir = tempfile::tempdir().unwrap();
        {
            let staging = StagedDirectory::begin(dir.path(), "test").unwrap();
            staging
                .write(Path::new("artifact.txt"), b"complete")
                .unwrap();
            assert!(staging.write(Path::new("../escape"), b"no").is_err());
            assert_eq!(dir.path().read_dir().unwrap().count(), 1);
        }
        assert_eq!(dir.path().read_dir().unwrap().count(), 0);
        assert!(!dir.path().join("escape").exists());
    }
}
