// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

//! Small, shared filesystem durability and collision primitives.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use fd_lock::RwLock;

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Report whether a directory entry exists without following symbolic links.
pub(crate) fn entry_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Open a non-following regular file suitable for an advisory inter-process lock.
pub(crate) fn open_file_lock(path: &Path) -> io::Result<RwLock<File>> {
    let parent = file_parent(path);
    fs::create_dir_all(parent)?;
    open_file_lock_with(path, true)
}

/// Open an existing advisory lock without creating its parent or the lock file.
///
/// Read-only status paths use this to coordinate with writers after the first transaction while
/// remaining non-mutating for a fresh or read-only configuration directory.
pub(crate) fn open_existing_file_lock(path: &Path) -> io::Result<Option<RwLock<File>>> {
    match open_file_lock_with(path, false) {
        Ok(lock) => Ok(Some(lock)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn open_file_lock_with(path: &Path, create: bool) -> io::Result<RwLock<File>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "lock path exists and is not a regular file",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound && create => {}
        Err(error) => return Err(error),
    }

    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(create)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "opened lock path is not a regular file",
        ));
    }
    Ok(RwLock::new(file))
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
    sync_directory(file_parent(path))
}

/// Flush one file through a handle opened with write access.
///
/// Windows `FlushFileBuffers` rejects a read-only handle with `ERROR_ACCESS_DENIED`; Unix accepts
/// one, which allowed read-only `File::open(...).sync_all()` calls to escape local CI.
pub(crate) fn sync_file(path: &Path) -> io::Result<()> {
    OpenOptions::new().write(true).open(path)?.sync_all()
}

fn file_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// Whether an atomic file publication must claim an absent path or may replace a regular file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AtomicWriteMode {
    CreateNew,
    Replace,
}

/// RAII-owned same-parent file staging.
///
/// Dropping an unpublished value removes its staging file. Publication disarms cleanup immediately
/// after the atomic rename, before the parent sync, because a sync failure must never delete bytes
/// that are already visible at the destination.
pub(crate) struct StagedFile {
    pub(crate) path: PathBuf,
    published: bool,
}

impl StagedFile {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            published: false,
        }
    }

    pub(crate) fn publish_new(mut self, destination: &Path) -> Result<()> {
        rename_noreplace(&self.path, destination).with_context(|| {
            format!(
                "failed to atomically claim new destination {} from {}",
                destination.display(),
                self.path.display()
            )
        })?;
        self.published = true;
        sync_parent(destination).with_context(|| {
            format!(
                "published {} but failed to sync its parent directory; the complete destination remains present and should be verified before retrying",
                destination.display()
            )
        })
    }

    pub(crate) fn publish_replace(mut self, destination: &Path) -> Result<()> {
        rename_replace(&self.path, destination).with_context(|| {
            format!(
                "failed to atomically publish {} as {}",
                self.path.display(),
                destination.display()
            )
        })?;
        self.published = true;
        sync_parent(destination).with_context(|| {
            format!(
                "published {} but failed to sync its parent directory; the complete destination remains present and should be verified before retrying",
                destination.display()
            )
        })
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Durably publish one complete file without ever truncating the destination in place.
pub(crate) fn atomic_write_file(
    destination: &Path,
    content: &[u8],
    mode: AtomicWriteMode,
) -> Result<()> {
    let parent = file_parent(destination);
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create directory {}", parent.display()))?;

    let existing_permissions = match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!(
                "refusing to replace symbolic link destination {}",
                destination.display()
            )
        }
        Ok(metadata) if !metadata.file_type().is_file() => {
            bail!(
                "destination must be absent or a regular file: {}",
                destination.display()
            )
        }
        Ok(_) if mode == AtomicWriteMode::CreateNew => {
            bail!("destination already exists: {}", destination.display())
        }
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to inspect destination {}", destination.display())
            })
        }
    };

    let file_name = destination
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("destination must name a file: {}", destination.display())
        })?;
    let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is earlier than the Unix epoch")?
        .as_nanos();
    let staging_path = parent.join(format!(
        ".{}.ai-session-search-stage-{}-{nonce}-{sequence}",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    let staging = StagedFile::new(staging_path);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&staging.path)
        .with_context(|| format!("failed to create staging file {}", staging.path.display()))?;
    if let Some(permissions) = existing_permissions.as_ref() {
        file.set_permissions(permissions.clone()).with_context(|| {
            format!(
                "failed to preserve permissions on staging file {}",
                staging.path.display()
            )
        })?;
    }
    file.write_all(content)
        .with_context(|| format!("failed to write staging file {}", staging.path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync staging file {}", staging.path.display()))?;
    drop(file);

    match (mode, existing_permissions.is_some()) {
        (AtomicWriteMode::Replace, true) => staging.publish_replace(destination),
        _ => staging.publish_new(destination),
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

    /// Stage one file at `name`, a relative path under the staging root.
    ///
    /// Nested paths are allowed and their parent directories are created, because a skill
    /// directory has `corrections/policy.toml` inside it. EVERY component must be
    /// [`Component::Normal`]: that is what keeps the write inside the staging root, so `..`, an
    /// absolute path, and a Windows prefix are all refused rather than escaping it. `create_new`
    /// still refuses to overwrite, so two entries cannot collide silently.
    pub(crate) fn write(&self, name: &Path, content: &[u8]) -> Result<()> {
        let mut components = name.components().peekable();
        if components.peek().is_none() {
            bail!("staged entry name must not be empty");
        }
        if !components.all(|component| matches!(component, Component::Normal(_))) {
            bail!(
                "staged entry name must be a relative path of normal components, got {}",
                name.display()
            );
        }
        let path = self.path.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create staged directory {}", parent.display())
            })?;
        }
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

    pub(crate) fn publish(self, destination: &Path) -> Result<()> {
        self.publish_with_parent_sync(destination, sync_parent)
    }

    fn publish_with_parent_sync<F>(mut self, destination: &Path, parent_sync: F) -> Result<()>
    where
        F: FnOnce(&Path) -> io::Result<()>,
    {
        sync_directory(&self.path)
            .with_context(|| format!("failed to sync staging directory {}", self.path.display()))?;
        rename_noreplace(&self.path, destination).with_context(|| {
            format!(
                "failed to atomically publish {} as {}",
                self.path.display(),
                destination.display()
            )
        })?;
        // The complete directory is now externally visible. Disarm Drop before syncing the
        // parent: a durability-confirmation failure must not delete a successfully published
        // destination or race with another process that observes it.
        self.committed = true;
        parent_sync(destination).with_context(|| {
            format!(
                "published {} but failed to sync its parent directory; the complete destination remains present and should be verified before retrying",
                destination.display()
            )
        })
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

#[cfg(not(windows))]
fn rename_replace(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn rename_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

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
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_file_create_and_replace_are_complete() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("config.toml");

        atomic_write_file(&destination, b"first", AtomicWriteMode::CreateNew).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"first");

        let error =
            atomic_write_file(&destination, b"lost", AtomicWriteMode::CreateNew).unwrap_err();
        assert!(error.to_string().contains("already exists"));
        assert_eq!(fs::read(&destination).unwrap(), b"first");

        atomic_write_file(&destination, b"second", AtomicWriteMode::Replace).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"second");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_file_replace_preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("config.toml");
        fs::write(&destination, b"first").unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o640)).unwrap();

        atomic_write_file(&destination, b"second", AtomicWriteMode::Replace).unwrap();

        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn atomic_file_rejects_nonregular_destinations() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("config.toml");
        fs::create_dir(&destination).unwrap();

        let error =
            atomic_write_file(&destination, b"content", AtomicWriteMode::Replace).unwrap_err();
        assert!(error.to_string().contains("regular file"));
        assert!(destination.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_file_rejects_symlink_destinations_without_touching_targets() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.toml");
        let destination = dir.path().join("config.toml");
        fs::write(&target, b"preserve").unwrap();
        symlink(&target, &destination).unwrap();

        let error =
            atomic_write_file(&destination, b"replacement", AtomicWriteMode::Replace).unwrap_err();
        assert!(error.to_string().contains("symbolic link"));
        assert_eq!(fs::read(&target).unwrap(), b"preserve");
        assert!(fs::symlink_metadata(&destination)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn staged_file_drop_preserves_destination_and_removes_staging_file() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("config.toml");
        let staging = dir.path().join(".config.toml.injected-stage");
        fs::write(&destination, b"preserve").unwrap();
        fs::write(&staging, b"partial replacement").unwrap();

        drop(StagedFile::new(staging.clone()));

        assert_eq!(fs::read(&destination).unwrap(), b"preserve");
        assert!(!staging.exists());
    }

    #[test]
    fn bare_filename_uses_current_directory_as_parent() {
        assert_eq!(file_parent(Path::new("config.toml")), Path::new("."));
    }

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
            // Nested paths are allowed -- a skill directory holds `corrections/policy.toml` -- but
            // every component must be Normal, which is what keeps the write inside the staging root.
            staging
                .write(Path::new("nested/deeper/artifact.txt"), b"nested")
                .unwrap();
            assert!(staging.write(Path::new("../escape"), b"no").is_err());
            assert!(staging.write(Path::new("a/../../escape"), b"no").is_err());
            assert!(staging.write(Path::new("/absolute"), b"no").is_err());
            assert!(staging.write(Path::new(""), b"no").is_err());
            assert_eq!(dir.path().read_dir().unwrap().count(), 1);
        }
        assert_eq!(dir.path().read_dir().unwrap().count(), 0);
        assert!(!dir.path().join("escape").exists());
    }

    #[test]
    fn parent_sync_failure_never_removes_the_published_directory() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("published");
        let staging = StagedDirectory::begin(dir.path(), "test").unwrap();
        staging
            .write(Path::new("artifact.txt"), b"complete")
            .unwrap();

        let error = staging
            .publish_with_parent_sync(&destination, |_| {
                Err(io::Error::other("injected parent sync failure"))
            })
            .unwrap_err();

        assert!(error.to_string().contains("complete destination remains"));
        assert_eq!(
            fs::read(destination.join("artifact.txt")).unwrap(),
            b"complete"
        );
    }
}
