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
    FileEditSummary, FileQuery, IndexStatus, MessageFilters, MessageHit, SearchExplain,
    SearchFilters, SearchHit, SessionMeta, SessionRecord,
};

/// RAII application root shared by native frontends and language bindings.
///
/// Opening an instance applies the configured SQLite contention and performance
/// policy exactly once. Dropping it closes the owned database connection.
pub struct SessionSearch {
    config: Config,
    db: Db,
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

    /// Index lifecycle operations.
    pub const fn index(&self) -> IndexService<'_> {
        IndexService::new(&self.config, &self.db)
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
    }
}
