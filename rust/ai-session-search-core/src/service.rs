//! Typed application services shared by CLI, MCP, and language bindings.
//!
//! Services own operation boundaries, while [`Db`] remains the storage layer.
//! Adapters must not duplicate SQL, filtering, pagination, or lifecycle policy.

use std::collections::HashMap;
use std::fs;

use anyhow::Result;

use crate::config::{Config, ScoringConfig};
use crate::db::Db;
use crate::indexer::{self, AutoReindexOutcome};
use crate::models::{
    DiagnosticStatus, FileCrossRef, FileEditSummary, FileQuery, FileVersion, IndexStatus,
    MessageFilters, MessageHit, SearchExplain, SearchFilters, SearchHit, SessionMeta,
    SessionRecord,
};

/// RAII application root shared by native frontends and language bindings.
///
/// Opening an instance applies the configured SQLite contention and performance
/// policy exactly once. Dropping it closes the owned database connection.
pub struct SessionSearch {
    config: Config,
    db: Db,
}

#[cfg(test)]
mod analysis_service_tests {
    use super::*;
    use crate::models::{Message, MessageKind, Provider, Role};
    use crate::util::minimal_record;

    #[test]
    fn analysis_service_reuses_indexed_correction_planning_and_role_queries() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().into_owned());
        config.index.cache_dir = Some(dir.path().join("cache").to_string_lossy().into_owned());
        let app = SessionSearch::open(config).unwrap();
        let mut parsed = minimal_record(
            Provider::Claude,
            std::path::Path::new("/fixture/session.jsonl"),
            String::new(),
        );
        parsed.session.id = "claude:analysis".into();
        parsed.session.provider_session_id = "analysis".into();
        parsed.messages = vec![
            Message {
                seq: 0,
                role: Role::User,
                ts: None,
                tool_name: None,
                kind: MessageKind::Conversation,
                tool_call_id: None,
                is_compaction: false,
                content: "actually, that is wrong".into(),
            },
            Message {
                seq: 1,
                role: Role::Slash,
                ts: None,
                tool_name: None,
                kind: MessageKind::Conversation,
                tool_call_id: None,
                is_compaction: false,
                content: "/plan verify migration".into(),
            },
        ];
        app.database().upsert_session(&parsed, 0, 0).unwrap();
        let mut other = minimal_record(
            Provider::Codex,
            std::path::Path::new("/fixture/codex.jsonl"),
            String::new(),
        );
        other.session.id = "codex:analysis".into();
        other.session.provider_session_id = "analysis".into();
        other.messages = vec![Message {
            seq: 0,
            role: Role::User,
            ts: None,
            tool_name: None,
            kind: MessageKind::Conversation,
            tool_call_id: None,
            is_compaction: false,
            content: "unrelated provider message".into(),
        }];
        app.database().upsert_session(&other, 0, 0).unwrap();

        let analysis = app.analysis();
        let filters = MessageFilters::default();
        assert_eq!(analysis.corrections(&filters).unwrap().len(), 1);
        let planning = analysis.planning(&filters, &["^/plan$".into()]).unwrap();
        assert_eq!(planning.len(), 1);
        assert_eq!(planning[0].command, "/plan");
        let roles = analysis.role_statistics(&filters).unwrap();
        assert_eq!(roles.len(), 2);
        assert_eq!(roles.iter().map(|row| row.count).sum::<i64>(), 3);
        let provider_roles = analysis
            .role_statistics(&MessageFilters {
                provider: Some(Provider::Claude),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(provider_roles.iter().map(|row| row.count).sum::<i64>(), 2);
    }
}

impl SessionSearch {
    /// Load platform configuration and open the configured index.
    pub fn load() -> Result<Self> {
        Self::open(Config::load()?)
    }

    /// Open an index using an explicit configuration.
    pub fn open(config: Config) -> Result<Self> {
        fs::create_dir_all(config.cache_dir())?;
        let mut db = Db::open_with_busy_timeout(&config.db_path(), config.index.busy_timeout_ms)?;
        db.apply_performance_config(&config.performance);
        Ok(Self { config, db })
    }

    /// Effective configuration used by every service handle.
    pub const fn config(&self) -> &Config {
        &self.config
    }

    /// Session catalog operations.
    pub const fn catalog(&self) -> CatalogService<'_> {
        CatalogService::new(&self.db)
    }

    /// Message search and context operations.
    pub const fn messages(&self) -> MessageService<'_> {
        MessageService::new(&self.db)
    }

    /// File history operations.
    pub const fn files(&self) -> FileService<'_> {
        FileService::new(&self.db)
    }

    /// Destination-independent session export operations.
    pub const fn exports(&self) -> ExportService<'_> {
        ExportService::new(&self.db)
    }

    /// Index lifecycle operations.
    pub const fn index(&self) -> IndexService<'_> {
        IndexService::new(&self.config, &self.db)
    }

    /// Index diagnostics and destructive maintenance operations.
    pub const fn maintenance(&self) -> MaintenanceService<'_> {
        MaintenanceService::new(&self.config, &self.db)
    }

    /// Effective provider roots and discovery status.
    pub const fn sources(&self) -> SourceService<'_> {
        SourceService::new(&self.config)
    }

    /// Indexed correction, planning, and statistics operations.
    pub const fn analysis(&self) -> AnalysisService<'_> {
        AnalysisService::new(&self.config, &self.db)
    }

    /// Advanced storage access for operations not yet represented by a service.
    pub const fn database(&self) -> &Db {
        &self.db
    }

    /// Install a frontend-specific progress sink.
    pub fn set_progress_reporter(&mut self, reporter: impl Fn(&str) + Send + Sync + 'static) {
        self.db.set_progress_reporter(reporter);
    }
}

/// Typed indexed analysis shared by native adapters.
///
/// This service returns data and performs no terminal or filesystem I/O. It deliberately keeps
/// legacy recovery-directory counters out of the canonical API: statistics describe the shared
/// index and honor the same structural message filters as CLI search.
#[derive(Clone, Copy)]
pub struct AnalysisService<'app> {
    config: &'app Config,
    db: &'app Db,
}

impl<'app> AnalysisService<'app> {
    /// Create an analysis service with configuration-backed correction and planning policy.
    pub const fn new(config: &'app Config, db: &'app Db) -> Self {
        Self { config, db }
    }

    /// Find categorized user corrections using configured patterns.
    ///
    /// # Errors
    ///
    /// Returns an error when a configured pattern is invalid or the index query fails.
    pub fn corrections(
        &self,
        filters: &MessageFilters,
    ) -> Result<Vec<crate::models::CorrectionMatch>> {
        let patterns = crate::analytics::compile_patterns(self.config)?;
        self.db.find_corrections(&patterns, filters)
    }

    /// Aggregate slash-command usage with configured and request-specific token patterns.
    ///
    /// # Errors
    ///
    /// Returns an error when a configured/request pattern is invalid or the index query fails.
    pub fn planning(
        &self,
        filters: &MessageFilters,
        command_patterns: &[String],
    ) -> Result<Vec<crate::models::PlanningCount>> {
        let command_filters =
            crate::analytics::compile_planning_filters(self.config, command_patterns)?;
        self.db.planning_usage(filters, &command_filters)
    }

    /// Count indexed messages by normalized role, ordered by role.
    ///
    /// # Errors
    ///
    /// Returns an error when the index query fails.
    pub fn role_statistics(
        &self,
        filters: &MessageFilters,
    ) -> Result<Vec<crate::analytics::RoleStat>> {
        let rows: Vec<_> = self
            .db
            .message_role_counts(filters)?
            .into_iter()
            .map(|(role, count)| crate::analytics::RoleStat { role, count })
            .collect();
        let limit = if filters.limit == 0 {
            usize::MAX
        } else {
            filters.limit
        };
        Ok(rows.into_iter().skip(filters.offset).take(limit).collect())
    }

    /// Return one bounded keyset page of provider-normalized session text for outward analysis.
    pub fn documents(
        &self,
        filters: &SearchFilters,
        cursor: Option<&crate::models::AnalysisCursor>,
    ) -> Result<crate::models::AnalysisDocumentPage> {
        self.db.analysis_documents(filters, cursor)
    }
}

/// Read-only session rendering over the shared catalog database.
#[derive(Clone, Copy)]
pub struct ExportService<'db> {
    db: &'db Db,
}

impl<'db> ExportService<'db> {
    /// Create an export service over an existing database connection.
    pub const fn new(db: &'db Db) -> Self {
        Self { db }
    }

    /// Resolve an exact session ID or unambiguous prefix and render its complete transcript.
    ///
    /// The returned document is held in memory and performs no terminal or filesystem I/O.
    /// Resolution errors include candidate context for ambiguous prefixes.
    ///
    /// # Errors
    ///
    /// Returns an error when the identifier is missing or ambiguous, database access fails, or
    /// structured serialization fails.
    pub fn render_full(
        &self,
        id_or_prefix: &str,
        format: crate::export::ExportFormat,
    ) -> Result<crate::export::ExportDocument> {
        let session = self.db.resolve_session(id_or_prefix)?;
        crate::export::render_full(&session, format)
    }
}

#[derive(Clone, Copy)]
/// Read-only discovery operations bound to a validated configuration.
pub struct SourceService<'config> {
    config: &'config Config,
}

impl<'config> SourceService<'config> {
    /// Create a source service over `config`.
    pub const fn new(config: &'config Config) -> Self {
        Self { config }
    }

    /// Discover enabled sources and return status for every supported provider.
    pub fn inventory(&self) -> Vec<crate::source::ProviderSourceStatus> {
        crate::source::inventory(self.config)
    }
}

/// Measurements from one completed compaction transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactOutcome {
    pub before_bytes: u64,
    pub after_bytes: u64,
}

impl CompactOutcome {
    pub const fn reclaimed_bytes(self) -> u64 {
        self.before_bytes.saturating_sub(self.after_bytes)
    }
}

/// Diagnostics and writer-exclusive index maintenance.
#[derive(Clone, Copy)]
pub struct MaintenanceService<'app> {
    config: &'app Config,
    db: &'app Db,
}

impl<'app> MaintenanceService<'app> {
    pub const fn new(config: &'app Config, db: &'app Db) -> Self {
        Self { config, db }
    }

    pub fn diagnostics(&self) -> Result<DiagnosticStatus> {
        crate::diagnostics::collect(self.config, self.db)
    }

    /// Merge FTS segments, rebuild the database, and checkpoint the WAL while
    /// holding the same process lock used by index writers.
    pub fn compact(&self) -> Result<CompactOutcome> {
        indexer::with_index_update_lock(self.config, || {
            let before_bytes = file_size(&self.config.db_path());
            self.db.optimize_fts()?;
            self.db.vacuum()?;
            self.db.checkpoint_truncate()?;
            Ok(CompactOutcome {
                before_bytes,
                after_bytes: file_size(&self.config.db_path()),
            })
        })
    }
}

fn file_size(path: &std::path::Path) -> u64 {
    fs::metadata(path).map_or(0, |metadata| metadata.len())
}

#[derive(Clone, Copy)]
pub struct IndexService<'app> {
    config: &'app Config,
    db: &'app Db,
}

impl<'app> IndexService<'app> {
    pub const fn new(config: &'app Config, db: &'app Db) -> Self {
        Self { config, db }
    }

    pub fn refresh(&self) -> Result<AutoReindexOutcome> {
        indexer::refresh_index_opportunistically(self.config, self.db, None)
    }

    pub fn reindex(&self, full: bool) -> Result<(usize, usize)> {
        indexer::reindex(self.config, self.db, full, None)
    }
}

#[derive(Clone, Copy)]
pub struct CatalogService<'db> {
    db: &'db Db,
}

impl<'db> CatalogService<'db> {
    pub const fn new(db: &'db Db) -> Self {
        Self { db }
    }

    pub fn list_sessions(&self, filters: &SearchFilters) -> Result<Vec<SessionRecord>> {
        self.db.list_recent(filters)
    }

    pub fn search_sessions(
        &self,
        query: &str,
        filters: &SearchFilters,
        current_repo: Option<&str>,
        scoring: &ScoringConfig,
    ) -> Result<Vec<SearchHit>> {
        self.db.search(query, filters, current_repo, scoring)
    }

    pub fn index_status(&self) -> Result<IndexStatus> {
        self.db.index_status()
    }

    pub fn resolve_session(&self, id_or_prefix: &str) -> Result<SessionRecord> {
        self.db.resolve_session_record(id_or_prefix)
    }

    pub fn inspect(
        &self,
        id_or_prefix: &str,
        options: crate::inspect::InspectionOptions,
    ) -> Result<crate::inspect::SessionInspection> {
        crate::inspect::inspect_session(self.db, id_or_prefix, options)
    }
}

#[derive(Clone, Copy)]
pub struct MessageService<'db> {
    db: &'db Db,
}

impl<'db> MessageService<'db> {
    pub const fn new(db: &'db Db) -> Self {
        Self { db }
    }

    pub fn search(&self, query: &str, filters: &MessageFilters) -> Result<Vec<MessageHit>> {
        self.db.search_messages(query, filters)
    }

    pub fn search_with_explain(
        &self,
        query: &str,
        filters: &MessageFilters,
        include_explain: bool,
    ) -> Result<(Vec<MessageHit>, Option<SearchExplain>)> {
        self.db
            .search_messages_with_explain(query, filters, include_explain)
    }

    pub fn session_metadata(&self, session_ids: &[String]) -> Result<HashMap<String, SessionMeta>> {
        self.db.session_metadata(session_ids)
    }

    pub fn context(
        &self,
        session_id: &str,
        seq: i64,
        before: i64,
        after: i64,
    ) -> Result<Vec<MessageHit>> {
        self.db.message_context(session_id, seq, before, after)
    }
}

#[derive(Clone, Copy)]
pub struct FileService<'db> {
    db: &'db Db,
}

impl<'db> FileService<'db> {
    pub const fn new(db: &'db Db) -> Self {
        Self { db }
    }

    pub fn search(&self, query: &FileQuery) -> Result<Vec<FileEditSummary>> {
        self.db.file_search(query)
    }

    pub fn cross_reference(&self, query: &FileQuery) -> Result<Vec<FileCrossRef>> {
        self.db.file_cross_ref(query)
    }

    pub fn history(&self, file: &str, query: &FileQuery) -> Result<Vec<FileVersion>> {
        crate::files::history(self.db, file, query)
    }

    pub fn reconstruct(
        &self,
        file: &str,
        query: &FileQuery,
        version: Option<usize>,
    ) -> Result<crate::files::ReconstructedFile> {
        crate::files::reconstruct_query(self.db, file, query, version)
    }

    /// Collision-safely write a reconstructed file through the shared recovery policy.
    pub fn restore(
        &self,
        reconstructed: &crate::files::ReconstructedFile,
        output_dir: Option<&std::path::Path>,
    ) -> Result<std::path::PathBuf> {
        crate::files::restore_reconstructed(reconstructed, output_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn services_share_one_database_and_return_typed_empty_results() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.mark_schema_current().unwrap();

        let messages = MessageService::new(&db)
            .search("missing", &MessageFilters::default())
            .unwrap();
        let files = FileService::new(&db).search(&FileQuery::default()).unwrap();
        let status = CatalogService::new(&db).index_status().unwrap();

        assert!(messages.is_empty());
        assert!(files.is_empty());
        assert!(status.parser_health.schema_current);
        assert_eq!(status.parser_health.indexed_sessions, 0);
    }

    #[test]
    fn application_owns_config_database_and_service_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().to_string());
        config.index.cache_dir = Some(dir.path().join("cache").to_string_lossy().to_string());
        config.providers.claude.enabled = false;
        config.providers.claude_desktop.enabled = false;
        config.providers.codex.enabled = false;
        config.providers.cursor.enabled = false;
        config.providers.antigravity.enabled = false;
        config.providers.pi.enabled = false;
        config.providers.aistudio.enabled = false;
        config.providers.gemini_cli.enabled = false;

        let app = SessionSearch::open(config).unwrap();
        let outcome = app.index().refresh().unwrap();

        assert!(matches!(outcome, AutoReindexOutcome::Updated { .. }));
        assert!(app.config().db_path().exists());
        assert!(app
            .catalog()
            .list_sessions(&SearchFilters {
                provider: None,
                path_prefix: None,
                exclude_path_prefixes: Vec::new(),
                exclude_session_ids: Vec::new(),
                since: None,
                until: None,
                limit: 1,
                warnings_only: false,
            })
            .unwrap()
            .is_empty());

        let diagnostics = app.maintenance().diagnostics().unwrap();
        assert_eq!(diagnostics.db_path, app.config().db_path());
        let compacted = app.maintenance().compact().unwrap();
        assert!(compacted.after_bytes > 0);
        assert_eq!(
            compacted.reclaimed_bytes(),
            compacted.before_bytes.saturating_sub(compacted.after_bytes)
        );
    }
}
