#![recursion_limit = "256"]
// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

pub mod analysis_pipeline;
pub mod analysis_publication;
pub mod analytics;
mod background_refresh;
mod cli;
pub mod config;
pub(crate) mod corrections;
pub mod dates;
pub mod db;
pub mod diagnostics;
mod durable_fs;
pub(crate) mod durable_path;
mod executable_alias;
pub mod export;
pub mod files;
mod fts;
pub mod hashing;
pub mod indexer;
pub mod inspect;
pub(crate) mod integrations;
pub mod mcp_schema_budget;
pub mod mcp_server;
pub(crate) mod message_classification;
pub mod message_search;
mod message_search_batches;
pub mod messages;
pub mod migration;
pub mod models;
mod text_file_transaction;
// Safety guard (plan H8): the provider parse path must never `.unwrap()` on
// non-test code — a single malformed session file would abort the whole reindex.
// Errors there must flow through `minimal_record` (util.rs) instead. Scoped to
// `not(test)` so the providers' test fixtures may still use `.unwrap()` freely.
#[cfg_attr(not(test), warn(clippy::unwrap_used))]
pub mod providers;
pub mod refs;
pub mod render;
pub mod runtime;
pub mod search_scope;
pub mod service;
pub mod skill_catalog;
pub(crate) mod skill_manifest;
pub mod skill_run;
pub mod skills;
pub mod source;
mod sql_functions;
pub mod sql_query;
pub mod tail;
pub mod trigram;
pub mod trigram_index;
mod tui;
pub(crate) mod update;
pub mod util;

// Curated application API. Module paths remain stable for existing consumers, while new callers
// can discover and import the supported service/query/publication surface from the crate root
// without traversing storage, CLI, MCP, provider, or PyO3 implementation modules.
pub use analysis_pipeline::{
    AnalysisPolicySpec, AnalysisResult, ClassificationRuleSpec, ClassificationTarget,
    PhraseTextMode, PhraseVocabularyPolicySpec,
};
pub use analysis_publication::{
    AnalysisPublicationFormat, AnalysisPublicationPlan, AnalysisPublicationReceipt,
};
pub use cli::run_from as run_cli_from;
pub use corrections::{CapabilityReceipt, MessageClassificationReport};
pub use export::{ExportFormat, ExportPublicationPlan};
pub use inspect::{EvidenceWindow, InspectionOptions};
pub use message_search::{
    AdditionalFieldText, ContextWindow, CoordinateUnit, DetailLevel, FieldViewBudget,
    FieldViewExtent, FuzzyQuery, JsonPointer, LineWindow, LiteralQuery, MatchViewBudget,
    MatchWindow, MessageFieldView, MessageMatchEvidence, MessageMatchViewMarkers,
    MessagePredicates, MessageQuery, MessageResultRef, MessageSearchError, MessageSearchHit,
    MessageSearchInclude, MessageSearchIncludedData, MessageSearchOmission, MessageSearchParameter,
    MessageSearchParameterDomain, MessageSearchParameterRegistry, MessageSearchParameterSpec,
    MessageSearchRequest, MessageSearchRequestBuilder, MessageSearchResponse, MessageSearchRule,
    MessageSearchRuntimeDiagnostics, MessageSearchSpecification, MessageTarget, PageSide,
    ProviderScope, PurposeSelection, ReceiptLevel, RequestedExtent, RequestedTimeRange,
    ResolvedMessageSearchRequest, ResolvedQueryMode, ResolvedRequestExtent,
    ResolvedRequestPresentation, ResultSetExtent, SequenceRange, ValidatedRegex, ValueOriginKind,
    ViewCharRange,
};
pub use message_search_batches::{
    MessageSearchBatch, MessageSearchBatches, MessageSearchCompletion,
};
pub use models::{AnalysisRequest, AnalysisSessionSelection};
pub use models::{
    FileQuery, MessageClassificationMatch, MessageFilters, MessageSearchMode, SearchFilters,
    SessionKind,
};
pub use search_scope::{
    AccessRoot, AccessRootOrigin, AccessRootSource, EffectiveAccessScope, TrustedAccessInputs,
};
pub use service::SessionSearch;
pub use service::{AnalysisReceipt, ReceiptedAnalysis};
pub use skill_catalog::{SkillName, SkillNameSelector, SkillPathSelector, SkillSelector};
pub use skill_run::{
    CapabilityExecutionSource, MessageClassificationDefinition, MessageClassificationQuery,
    MessageClassificationResult, ResolvedSkillReceipt, SelectedSkillLocation, SkillCapabilityInput,
    SkillCapabilityOutput, SkillRunQuery, SkillRunReport,
};

/// Execute the canonical CLI after recording that this process entered through the Python binding.
///
/// This is an embedding boundary rather than a second dispatcher: command parsing and execution
/// remain in [`run_cli_from`], while detached children can re-enter through the installed Python
/// console script without guessing from platform-specific interpreter paths.
#[doc(hidden)]
pub fn run_cli_from_python<I, T>(args: I) -> anyhow::Result<i32>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    update::mark_python_binding_process();
    cli::run_from(args)
}

/// Return whether an application error means an ordinary downstream reader closed its pipe.
///
/// Inspecting the short error chain is `O(chain depth)` time and `O(1)` memory. Native and
/// language-bound entrypoints share this classifier so wrappers cannot turn successful shell
/// pipelines into failures merely by erasing the underlying I/O error type.
#[doc(hidden)]
pub fn is_broken_pipe_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::BrokenPipe)
            || cause
                .downcast_ref::<serde_json::Error>()
                .is_some_and(|error| error.io_error_kind() == Some(std::io::ErrorKind::BrokenPipe))
    })
}

/// Return whether an application error means a filesystem or the index database ran out of space.
///
/// Inspecting the short error chain is `O(chain depth)` time and `O(1)` memory. One classifier
/// covers both ways a write reports exhaustion, and both are already platform-independent, so this
/// needs no per-platform branch: `std::io::ErrorKind::StorageFull` is what the standard library
/// maps `ENOSPC` to on Unix and `ERROR_DISK_FULL`/`ERROR_HANDLE_DISK_FULL` to on Windows, and
/// SQLite reports `SQLITE_FULL` identically everywhere. Matching a raw platform error number here
/// instead would need one arm per target and would silently classify nothing on the targets it
/// missed.
///
/// Callers use this to separate a condition that clears by itself, sometimes after days, from one
/// that needs a correction before the next attempt can succeed.
#[doc(hidden)]
pub fn is_storage_full_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::StorageFull)
            || cause
                .downcast_ref::<serde_json::Error>()
                .is_some_and(|error| error.io_error_kind() == Some(std::io::ErrorKind::StorageFull))
            || cause
                .downcast_ref::<rusqlite::Error>()
                .and_then(rusqlite::Error::sqlite_error_code)
                .is_some_and(|code| code == rusqlite::ErrorCode::DiskFull)
    })
}

/// The sentence a caller needs after an operation stopped because storage is full.
///
/// An operating system reports a full disk as one clause with no advice. The two facts a reader
/// cannot derive from it are what survived and what to do next, so both are stated here once.
const STORAGE_FULL_GUIDANCE: &str =
    "There is no free space left for this write. No partial file replaced a complete one: file \
     writes publish by atomic rename and database writes roll back, so already-indexed history \
     stays searchable. Free space, then rerun; `aise doctor` reports index readiness.";

/// Render an application error for a person, adding recovery guidance the error itself lacks.
///
/// Every entry point that ends a run formats through this, so what a reader is told does not depend
/// on whether they reached aise through the executable or through the Python binding. Cost is one
/// formatting pass plus `O(chain depth)` classification.
#[doc(hidden)]
pub fn error_message_with_recovery(error: &anyhow::Error) -> String {
    let rendered = format!("{error:#}");
    if is_storage_full_error(error) {
        return format!("{rendered}\n{STORAGE_FULL_GUIDANCE}");
    }
    rendered
}

#[cfg(test)]
mod storage_pressure_tests {
    /// Both sources of a full-disk failure classify the same way on every supported platform.
    ///
    /// A staged file write reports it as `std::io::ErrorKind::StorageFull`, and the index database
    /// reports it as SQLite's `SQLITE_FULL`; a caller deciding whether to wait or to ask for a
    /// correction has to treat them alike. Neither spelling is platform-specific, which is why one
    /// code path serves Unix and Windows.
    #[test]
    fn storage_exhaustion_is_recognized_from_the_filesystem_and_from_sqlite() {
        let staged = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::StorageFull))
            .context("failed to write staging file /tmp/index.db.stage");
        assert!(super::is_storage_full_error(&staged), "{staged:#}");

        let database = anyhow::Error::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
            Some("database or disk is full".to_string()),
        ))
        .context("could not commit the index transaction");
        assert!(super::is_storage_full_error(&database), "{database:#}");
    }

    /// What a reader is told is what the entry points print, because both format through here.
    ///
    /// A full disk arrives as one clause — `No space left on device (os error 28)`, or the Windows
    /// equivalent — and a reader acting on it needs two things that clause never carries: whether
    /// their indexed history survived, and what to do. Testing the formatter rather than each
    /// entry point is what makes this checkable without a full disk on three platforms; the
    /// executable and the Python binding both call it and add no wording of their own.
    #[test]
    fn a_storage_failure_is_rendered_with_what_survived_and_what_to_do() {
        let rendered = super::error_message_with_recovery(
            &anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::StorageFull))
                .context("failed to write staging file"),
        );
        assert!(
            rendered.contains("failed to write staging file"),
            "{rendered}"
        );
        assert!(rendered.contains("no free space"), "{rendered}");
        assert!(rendered.contains("stays searchable"), "{rendered}");
        assert!(rendered.contains("aise doctor"), "{rendered}");
    }

    /// An unrelated failure is rendered exactly as before, with nothing appended.
    #[test]
    fn an_unrelated_failure_gains_no_storage_guidance() {
        let error = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            .context("failed to write staging file");
        assert_eq!(
            super::error_message_with_recovery(&error),
            format!("{error:#}")
        );
    }

    /// Failures that need a correction stay outside the wait-and-retry classification.
    #[test]
    fn other_failures_are_not_reported_as_storage_exhaustion() {
        for error in [
            anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::NotFound)),
            anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            anyhow::Error::new(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                Some("database is locked".to_string()),
            )),
        ] {
            assert!(!super::is_storage_full_error(&error), "{error:#}");
        }
    }
}
