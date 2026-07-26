//! Platform-faithful path encoding for durable JSON records.
//!
//! A path is not text on either Unix or Windows: Unix paths are arbitrary bytes and Windows paths
//! are arbitrary UTF-16 code units, and neither is guaranteed to be valid UTF-8. Any record that
//! has to survive a process restart -- a transaction receipt, an install manifest -- therefore
//! cannot store a path as a `String` without risking a lossy round trip that renames or, worse,
//! points at a different file than the one it recorded.
//!
//! Shared rather than duplicated because the receipt and the manifest describe the SAME paths, and
//! two encodings of one path would let them disagree about which file an install wrote.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// A filesystem path recorded in its native encoding.
///
/// Tagged by platform rather than normalized, so a record written on one system fails loudly when
/// read on another instead of silently resolving to a different path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "encoding", content = "units", rename_all = "snake_case")]
pub(crate) enum EncodedPath {
    UnixBytes(Vec<u8>),
    WindowsWide(Vec<u16>),
    Utf8(String),
}

impl EncodedPath {
    /// Record `path` exactly as this platform represents it.
    pub(crate) fn from_path(path: &Path) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            return Self::UnixBytes(path.as_os_str().as_bytes().to_vec());
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt as _;
            return Self::WindowsWide(path.as_os_str().encode_wide().collect());
        }
        #[allow(unreachable_code)]
        Self::Utf8(path.to_string_lossy().into_owned())
    }

    /// Rebuild the path, refusing a record written for a different platform.
    ///
    /// # Errors
    ///
    /// Returns an error when the record holds an encoding this host cannot represent. Reinterpreting
    /// it would produce a path that looks plausible and names the wrong file.
    pub(crate) fn to_path_buf(&self) -> Result<PathBuf> {
        match self {
            Self::UnixBytes(bytes) => {
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStringExt as _;
                    Ok(PathBuf::from(OsString::from_vec(bytes.clone())))
                }
                #[cfg(not(unix))]
                bail!("record contains a Unix path on a non-Unix host")
            }
            Self::WindowsWide(_units) => {
                #[cfg(windows)]
                {
                    use std::os::windows::ffi::OsStringExt as _;
                    return Ok(PathBuf::from(OsString::from_wide(_units)));
                }
                #[cfg(not(windows))]
                bail!("record contains a Windows path on a non-Windows host")
            }
            Self::Utf8(text) => Ok(PathBuf::from(text)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_round_trips_through_its_native_encoding() {
        for original in ["/tmp/plain", "/tmp/with space/and-dash"] {
            let encoded = EncodedPath::from_path(Path::new(original));
            assert_eq!(encoded.to_path_buf().unwrap(), PathBuf::from(original));
        }
    }

    /// The reason this type exists: a path that is not valid UTF-8 must survive intact.
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_path_survives_a_json_round_trip() {
        use std::os::unix::ffi::OsStrExt as _;

        let raw = std::ffi::OsStr::from_bytes(b"/tmp/inv\xffalid");
        let original = PathBuf::from(raw);
        assert!(
            original.to_str().is_none(),
            "this fixture must not be valid UTF-8, or it proves nothing"
        );

        let json = serde_json::to_string(&EncodedPath::from_path(&original)).unwrap();
        let decoded: EncodedPath = serde_json::from_str(&json).unwrap();
        assert_eq!(
            decoded.to_path_buf().unwrap(),
            original,
            "to_string_lossy would have replaced the invalid byte and named a different file"
        );
    }
}
