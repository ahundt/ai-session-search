//! Small, shared filesystem durability and collision primitives.

use std::fs::{self, File};
use std::io;
use std::path::Path;

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
}
