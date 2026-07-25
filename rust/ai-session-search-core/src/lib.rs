#![recursion_limit = "256"]

pub mod analysis_pipeline;
pub mod analysis_publication;
pub mod analytics;
mod background_refresh;
mod cli;
pub mod config;
pub mod dates;
pub mod db;
pub mod diagnostics;
mod durable_fs;
mod executable_alias;
pub mod export;
pub mod files;
mod fts;
pub mod indexer;
pub mod inspect;
pub(crate) mod integrations;
pub mod mcp_server;
pub mod message_search;
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
pub use export::{ExportFormat, ExportPublicationPlan};
pub use inspect::{EvidenceWindow, InspectionOptions};
pub use message_search::{
    ContextWindow, FuzzyQuery, JsonPointer, LineWindow, LiteralQuery, MatchWindow,
    MessagePredicates, MessageQuery, MessageSearchError, MessageSearchRequest,
    MessageSearchRequestBuilder, MessageTarget, PurposeSelection, ReceiptLevel, RequestedExtent,
    RequestedTimeRange, SequenceRange, ValidatedRegex,
};
pub use models::{FileQuery, MessageFilters, MessageSearchMode, SearchFilters, SessionKind};
pub use search_scope::{
    AccessRoot, AccessRootOrigin, AccessRootSource, EffectiveAccessScope, TrustedAccessInputs,
};
pub use service::SessionSearch;
