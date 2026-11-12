// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

//! Typed failures a library caller must tell apart without reading message text.

use std::ops::RangeInclusive;
use std::path::PathBuf;

/// A failure a caller can act on by kind: look a session up again, choose a file version inside
/// the valid range, pick another destination, refresh the index, or fix an argument.
///
/// Every service method returns `anyhow::Result`; these failures travel inside that
/// `anyhow::Error`, so existing callers keep compiling and a caller that needs the kind
/// downcasts before converting the error into anything else:
///
/// ```
/// # fn demo(app: &ai_session_search::SessionSearch) {
/// use ai_session_search::ServiceError;
/// match app.catalog().resolve_session("claude:does-not-exist") {
///     Ok(session) => println!("{}", session.id),
///     Err(error) => match error.downcast_ref::<ServiceError>() {
///         Some(ServiceError::SessionNotFound { .. }) => println!("no such session"),
///         Some(ServiceError::SessionPrefixAmbiguous { candidates, .. }) => {
///             println!("pick one of {candidates:?}")
///         }
///         _ => println!("{error:#}"),
///     },
/// }
/// # }
/// ```
///
/// Converting the `anyhow::Error` into `Box<dyn std::error::Error>` with `?` first keeps the
/// message but loses the downcast, so classify before that conversion.
///
/// [`Display`](std::fmt::Display) is the sentence the command line prints for the same failure.
/// Use [`ServiceError::code`] for a stable machine-readable kind. The enum and each variant are
/// `#[non_exhaustive]`: match variants with `{ .. }` and keep a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ServiceError {
    /// No indexed session in the caller's access scope has this id or id prefix.
    #[error("{message}")]
    #[non_exhaustive]
    SessionNotFound {
        /// The id or prefix the caller passed.
        id_or_prefix: String,
        /// The sentence [`Display`](std::fmt::Display) prints.
        message: String,
    },
    /// The prefix matches more than one session.
    #[error("{message}")]
    #[non_exhaustive]
    SessionPrefixAmbiguous {
        /// The prefix the caller passed.
        prefix: String,
        /// Every matching canonical session id, in index order. The message shows at most
        /// eight; this list is complete.
        candidates: Vec<String>,
        /// The sentence [`Display`](std::fmt::Display) prints.
        message: String,
    },
    /// No recorded edit matches this file name or path under the query's filters.
    #[error("{message}")]
    #[non_exhaustive]
    FileNotFound {
        /// The file name or path the caller passed.
        file: String,
        /// The sentence [`Display`](std::fmt::Display) prints.
        message: String,
    },
    /// The requested file version does not exist. Versions are numbered from 1.
    #[error("{message}")]
    #[non_exhaustive]
    FileVersionOutOfRange {
        /// The file name or path the caller passed.
        file: String,
        /// The version the caller asked for.
        requested: usize,
        /// Versions that exist, `1..=N`.
        valid: RangeInclusive<usize>,
        /// The sentence [`Display`](std::fmt::Display) prints.
        message: String,
    },
    /// A publication or export refuses to replace something already at its destination.
    #[error("{message}")]
    #[non_exhaustive]
    DestinationExists {
        /// The destination that already exists.
        path: PathBuf,
        /// The sentence [`Display`](std::fmt::Display) prints.
        message: String,
    },
    /// The index database does not exist and the configured refresh policy
    /// (`existing-only`) forbids creating it.
    #[error("{message}")]
    #[non_exhaustive]
    IndexMissing {
        /// The database path that was looked for.
        path: PathBuf,
        /// The sentence [`Display`](std::fmt::Display) prints.
        message: String,
    },
    /// The index holds no sessions and no refresh has ever completed, so an empty result would
    /// say nothing about the session history. Call
    /// [`IndexService::prepare_for_read`](crate::service::IndexService::prepare_for_read) or
    /// [`IndexService::refresh`](crate::service::IndexService::refresh) first.
    #[error("{message}")]
    #[non_exhaustive]
    IndexNotRefreshed {
        /// The database path.
        path: PathBuf,
        /// The sentence [`Display`](std::fmt::Display) prints.
        message: String,
    },
    /// An argument or a combination of arguments can never be satisfied.
    #[error("{message}")]
    #[non_exhaustive]
    InvalidArgument {
        /// The sentence [`Display`](std::fmt::Display) prints, naming the argument.
        message: String,
    },
}

impl ServiceError {
    /// Stable kebab-case name of the failure kind, for logs and protocol adapters.
    ///
    /// The values are `session-not-found`, `session-prefix-ambiguous`, `file-not-found`,
    /// `file-version-out-of-range`, `destination-exists`, `index-missing`,
    /// `index-not-refreshed`, and `invalid-argument`. A value is never reused for another kind.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::SessionNotFound { .. } => "session-not-found",
            Self::SessionPrefixAmbiguous { .. } => "session-prefix-ambiguous",
            Self::FileNotFound { .. } => "file-not-found",
            Self::FileVersionOutOfRange { .. } => "file-version-out-of-range",
            Self::DestinationExists { .. } => "destination-exists",
            Self::IndexMissing { .. } => "index-missing",
            Self::IndexNotRefreshed { .. } => "index-not-refreshed",
            Self::InvalidArgument { .. } => "invalid-argument",
        }
    }

    pub(crate) fn session_not_found(id_or_prefix: &str) -> Self {
        Self::SessionNotFound {
            id_or_prefix: id_or_prefix.to_string(),
            message: format!(
                "no session matches '{id_or_prefix}' — run `aise list` to see recent session \
                 ids, or `aise search <keywords>` to find one"
            ),
        }
    }

    pub(crate) fn session_prefix_ambiguous(prefix: &str, candidates: Vec<String>) -> Self {
        let shown = candidates
            .iter()
            .take(8)
            .map(String::as_str)
            .collect::<Vec<_>>();
        let more = candidates.len().saturating_sub(shown.len());
        let suffix = if more > 0 {
            format!(" (+{more} more)")
        } else {
            String::new()
        };
        let message = format!(
            "session prefix '{prefix}' is ambiguous — {} sessions match: {}{}. \
             Pass a longer prefix or the full id.",
            candidates.len(),
            shown.join(", "),
            suffix
        );
        Self::SessionPrefixAmbiguous {
            prefix: prefix.to_string(),
            candidates,
            message,
        }
    }

    pub(crate) fn file_not_found(file: &str) -> Self {
        Self::FileNotFound {
            file: file.to_string(),
            message: format!("no file edits found for '{file}'"),
        }
    }

    pub(crate) fn file_version_out_of_range(file: &str, requested: usize, versions: usize) -> Self {
        Self::FileVersionOutOfRange {
            file: file.to_string(),
            requested,
            valid: 1..=versions,
            message: format!(
                "version {requested} is out of range for '{file}'; expected 1..={versions}"
            ),
        }
    }

    pub(crate) fn destination_exists(path: &std::path::Path, message: String) -> Self {
        Self::DestinationExists {
            path: path.to_path_buf(),
            message,
        }
    }

    pub(crate) fn index_missing(path: &std::path::Path, message: String) -> Self {
        Self::IndexMissing {
            path: path.to_path_buf(),
            message,
        }
    }

    pub(crate) fn invalid_argument(message: impl Into<String>) -> Self {
        Self::InvalidArgument {
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ServiceError;

    #[test]
    fn display_is_the_message_and_codes_are_distinct() {
        let errors = [
            ServiceError::session_not_found("x"),
            ServiceError::session_prefix_ambiguous("c", vec!["c:1".into(), "c:2".into()]),
            ServiceError::file_not_found("a.py"),
            ServiceError::file_version_out_of_range("a.py", 9, 3),
            ServiceError::destination_exists(
                std::path::Path::new("/d"),
                "export bundle destination already exists: /d".into(),
            ),
            ServiceError::index_missing(std::path::Path::new("/i.db"), "missing".into()),
            ServiceError::invalid_argument("bad"),
        ];
        let codes = errors
            .iter()
            .map(ServiceError::code)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(codes.len(), errors.len());
        assert_eq!(
            errors.len(),
            7,
            "one sample per kind except index-not-refreshed"
        );
        assert_eq!(
            errors[3].to_string(),
            "version 9 is out of range for 'a.py'; expected 1..=3"
        );
        assert_eq!(
            errors[4].to_string(),
            "export bundle destination already exists: /d"
        );
        let many = (0..10).map(|n| format!("c:{n}")).collect::<Vec<_>>();
        let ambiguous = ServiceError::session_prefix_ambiguous("c", many);
        assert!(ambiguous.to_string().contains("10 sessions match"));
        assert!(ambiguous
            .to_string()
            .ends_with("(+2 more). Pass a longer prefix or the full id."));
        let ServiceError::SessionPrefixAmbiguous { candidates, .. } = ambiguous else {
            unreachable!()
        };
        assert_eq!(candidates.len(), 10);
    }
}
