//! Typed application services shared by CLI, MCP, and language bindings.
//!
//! Services own operation boundaries, while [`Db`] remains the storage layer.
//! Adapters must not duplicate SQL, filtering, pagination, or lifecycle policy.

use std::collections::HashMap;
use std::fs;
use std::num::NonZeroUsize;

use anyhow::{anyhow, bail, Result};

use crate::config::{Config, IndexRefresh, ScoringConfig};
use crate::db::{Db, SchemaState, MIN_READABLE_SCHEMA_VERSION};
use crate::indexer::{self, AutoReindexOutcome, IndexCoordinator};
use crate::message_search::{
    ContextWindow, ExecutionOrder, LineWindow, MatchWindow, MessageResponsePlan,
    MessageRetrievalPlan, MessageSearchOrigins, MessageSearchPlan, MessageSearchRequest,
    MessageSearchResponse, PageInfo, ReceiptLevel, ResolvedExtent, ResolvedMessagePredicates,
    ResolvedMessagePresentation, SearchSurface, ValueOrigin,
};
use crate::models::{
    DiagnosticStatus, FileCrossRef, FileEditSummary, FileQuery, FileVersion, IndexStatus,
    MessageFilters, MessageHit, SearchExplain, SearchFilters, SearchHit, SessionMeta,
    SessionRecord,
};

/// The Python API currently defaults message searches to 50 results when the caller omits a
/// limit. Keep this named until the Python adapter supplies the value explicitly.
const CURRENT_PYTHON_MESSAGE_SEARCH_LIMIT: usize = 50;

/// Narrow an asymmetric context window to a total-message ceiling while retaining its requested
/// before/after proportion. Integer ties round toward `before`; the returned total equals the
/// ceiling whenever narrowing is necessary.
fn narrow_context_proportionally(context: ContextWindow, maximum: NonZeroUsize) -> ContextWindow {
    let before = context.before() as u128;
    let after = context.after() as u128;
    let total = before + after;
    let maximum = maximum.get() as u128;
    if total <= maximum {
        return context;
    }
    let narrowed_before = (before * maximum + total / 2) / total;
    ContextWindow::new(
        narrowed_before as usize,
        (maximum - narrowed_before) as usize,
    )
}

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

    #[test]
    fn analysis_run_matches_paged_documents_reference_on_indexed_sessions() {
        use crate::analysis_pipeline::{
            AnalysisPolicySpec, ClassificationRuleSpec, ClassificationTarget, PhraseTextMode,
            PhraseVocabularyPolicySpec,
        };

        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().into_owned());
        config.index.cache_dir = Some(dir.path().join("cache").to_string_lossy().into_owned());
        let app = SessionSearch::open(config).unwrap();
        // Multi-message sessions with junction-spanning phrases and a code fence that
        // opens in one message and closes in the next.
        for (id, provider, texts) in [
            (
                "claude:multi",
                Provider::Claude,
                vec!["use tdd across chunks", "tdd again across chunks"],
            ),
            (
                "codex:fence",
                Provider::Codex,
                vec!["prose one\n```", "code hidden\n```\nprose two"],
            ),
            ("gemini-cli:empty", Provider::GeminiCli, vec![]),
        ] {
            let mut parsed = minimal_record(
                Provider::Claude,
                std::path::Path::new("/fixture/session.jsonl"),
                String::new(),
            );
            parsed.session.provider = provider;
            parsed.session.id = id.into();
            parsed.session.provider_session_id = id.into();
            parsed.messages = texts
                .iter()
                .enumerate()
                .map(|(seq, text)| Message {
                    seq: seq as i64,
                    role: Role::User,
                    ts: None,
                    tool_name: None,
                    kind: MessageKind::Conversation,
                    tool_call_id: None,
                    is_compaction: false,
                    content: (*text).into(),
                })
                .collect();
            app.database().upsert_session(&parsed, 0, 0).unwrap();
        }
        let policy = AnalysisPolicySpec {
            classification_rules: vec![ClassificationRuleSpec {
                dimension: "technique".into(),
                label: "tdd".into(),
                target: ClassificationTarget::UserText,
                pattern: "(?i)\\btdd\\b".into(),
                weight: 1,
            }],
            relationship_rules: vec![],
            phrase_vocabulary: Some(PhraseVocabularyPolicySpec {
                widths: vec![2],
                max_unique_phrases: 1000,
                min_document_tokens: 0,
                excluded_tokens: vec![],
                exclude_numeric_tokens: true,
                text_mode: PhraseTextMode::ProseOnly,
            }),
            max_classification_chars: None,
        }
        .compile()
        .unwrap();
        let filters = SearchFilters {
            provider: None,
            path_prefix: None,
            exclude_path_prefixes: Vec::new(),
            exclude_session_ids: Vec::new(),
            since: None,
            until: None,
            limit: 0,
            warnings_only: false,
        };

        let streamed = app.analysis().run(&filters, &policy).unwrap();

        // Reference: the public paged-documents API still materializes joined text; the
        // streaming run must be indistinguishable from analyzing those documents.
        let limited = SearchFilters {
            limit: 2,
            ..filters
        };
        let mut documents = Vec::new();
        let mut cursor = None;
        loop {
            let page = app.analysis().documents(&limited, cursor.as_ref()).unwrap();
            documents.extend(page.documents);
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        let reference = policy.analyze(documents).unwrap();

        assert_eq!(
            serde_json::to_value(&streamed).unwrap(),
            serde_json::to_value(&reference).unwrap()
        );
        assert!(!streamed.vocabulary.is_empty());
        assert!(streamed.sessions["claude:multi"].has_user_text);
        assert!(!streamed.sessions["gemini-cli:empty"].has_user_text);
    }

    #[test]
    fn analysis_run_streams_one_snapshot_and_resolves_across_pages() {
        use crate::analysis_pipeline::{
            AnalysisPolicy, ClassificationRuleSpec, ClassificationTarget, RelationshipKind,
            RelationshipResolution, RelationshipRuleSpec,
        };

        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().into_owned());
        config.index.cache_dir = Some(dir.path().join("cache").to_string_lossy().into_owned());
        let app = SessionSearch::open(config).unwrap();
        for (id, provider, title, text) in [
            ("claude:root", Provider::Claude, "Root", "use TDD"),
            ("codex:root", Provider::Codex, "Root", ""),
            (
                "gemini-cli:child",
                Provider::GeminiCli,
                "Branch of Root",
                "use TDD",
            ),
        ] {
            let mut parsed = minimal_record(
                provider,
                std::path::Path::new("/fixture/session.jsonl"),
                String::new(),
            );
            parsed.session.id = id.into();
            parsed.session.provider_session_id = id.into();
            parsed.session.title = Some(title.into());
            if !text.is_empty() {
                parsed.messages.push(Message {
                    seq: 0,
                    role: Role::User,
                    ts: None,
                    tool_name: None,
                    kind: MessageKind::Conversation,
                    tool_call_id: None,
                    is_compaction: false,
                    content: text.into(),
                });
            }
            app.database().upsert_session(&parsed, 0, 0).unwrap();
        }
        let policy = AnalysisPolicy::compile(
            vec![ClassificationRuleSpec {
                dimension: "technique".into(),
                label: "tdd".into(),
                target: ClassificationTarget::UserText,
                pattern: "(?i)\\btdd\\b".into(),
                weight: 1,
            }],
            vec![RelationshipRuleSpec {
                id: "branch_of".into(),
                kind: RelationshipKind::Branch,
                pattern: "^Branch of (?P<parent>.+)$".into(),
            }],
        )
        .unwrap();
        let filters = SearchFilters {
            provider: None,
            path_prefix: None,
            exclude_path_prefixes: Vec::new(),
            exclude_session_ids: Vec::new(),
            since: None,
            until: None,
            limit: 0,
            warnings_only: false,
        };
        let result = app
            .analysis()
            .run_with_session_batch_size(&filters, std::num::NonZeroUsize::new(1).unwrap(), &policy)
            .unwrap();
        let automatic_result = app.analysis().run(&filters, &policy).unwrap();

        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            serde_json::to_value(&automatic_result).unwrap()
        );

        assert_eq!(result.sessions.len(), 3);
        assert_eq!(result.sessions["gemini-cli:child"].score, 1);
        assert_eq!(
            result.sessions["gemini-cli:child"].relationship_hints[0].resolution,
            RelationshipResolution::Ambiguous {
                session_ids: vec!["claude:root".into(), "codex:root".into()]
            }
        );

        let limited = SearchFilters {
            limit: 2,
            ..filters
        };
        assert_eq!(
            app.analysis()
                .run_with_session_batch_size(
                    &limited,
                    std::num::NonZeroUsize::new(1).unwrap(),
                    &policy,
                )
                .unwrap()
                .sessions
                .len(),
            2
        );
    }
}

impl SessionSearch {
    /// Load platform configuration and open the configured index.
    pub fn load() -> Result<Self> {
        Self::open(Config::load()?)
    }

    /// Open an index using an explicit configuration.
    ///
    /// `existing-only` opens a SQLite-enforced read-only handle and never creates directories,
    /// installs schema objects, refreshes sources, or changes journal policy. SQLite may create or
    /// update empty `-wal`/`-shm` coordination sidecars when opening a WAL database whose sidecars
    /// are absent; it does not modify the durable database contents. Other refresh modes
    /// may create a new current-schema database and later perform the configured incremental
    /// refresh. Newer, hybrid, and incomplete current schemas fail before an ordinary query.
    pub fn open(config: Config) -> Result<Self> {
        let schema_state = IndexCoordinator::new(&config).inspect_schema()?;
        match schema_state {
            SchemaState::Missing if config.index.refresh == IndexRefresh::ExistingOnly => {
                anyhow::bail!(
                    "existing-only index {} does not exist; run `aise reindex --full` without --index-refresh existing-only",
                    config.db_path().display()
                )
            }
            SchemaState::Older { current, .. }
                if config.index.refresh == IndexRefresh::ExistingOnly
                    && current < MIN_READABLE_SCHEMA_VERSION =>
            {
                anyhow::bail!(
                    "existing-only index {} uses unreadable schema generation {current}; run `aise reindex --full` without --index-refresh existing-only",
                    config.db_path().display()
                )
            }
            SchemaState::Missing | SchemaState::Current | SchemaState::Older { .. } => {}
            SchemaState::Newer { current, supported } => anyhow::bail!(
                "index {} uses schema generation {current}, newer than this aise build supports ({supported}); upgrade aise before opening it",
                config.db_path().display()
            ),
            SchemaState::RepairableLayout { reason }
                if config.index.refresh == IndexRefresh::ExistingOnly =>
            {
                anyhow::bail!(
                    "existing-only index {} needs a one-time message-search index rebuild ({reason}); run a writable aise command (without --index-refresh existing-only) to self-heal it in place",
                    config.db_path().display()
                )
            }
            SchemaState::RepairableLayout { .. } => {
                // Elect one cross-process writer, re-inspect under the lock (a peer process may have
                // already healed it), and rebuild the derived message-search indexes online from the
                // intact base rows via the atomic exclusive migration. Then fall through to the
                // normal open below against the now-current schema.
                let heal_threads = NonZeroUsize::new(config.resolve_threads())
                    .expect("Config::resolve_threads always returns at least one");
                let heal_busy_timeout = config.index.busy_timeout_ms;
                crate::indexer::with_index_update_lock(&config, || {
                    if matches!(
                        IndexCoordinator::new(&config).inspect_schema()?,
                        SchemaState::RepairableLayout { .. }
                    ) {
                        let db = Db::open_with_threads(
                            &config.db_path(),
                            heal_busy_timeout,
                            heal_threads,
                        )?;
                        db.migrate_message_search_schema_exclusive()?;
                    }
                    Ok(())
                })?;
            }
            SchemaState::RecoveryRequired { reason } => anyhow::bail!(
                "index {} requires offline recovery: {reason}; stop AISE processes, then run `aise reindex --full`",
                config.db_path().display()
            ),
        }
        if config.index.refresh != IndexRefresh::ExistingOnly {
            fs::create_dir_all(config.cache_dir())?;
        }
        let worker_threads = NonZeroUsize::new(config.resolve_threads())
            .expect("Config::resolve_threads always returns at least one");
        let mut db = if config.index.refresh == IndexRefresh::ExistingOnly {
            Db::open_existing_read_only_with_threads(
                &config.db_path(),
                config.index.busy_timeout_ms,
                worker_threads,
            )?
        } else {
            Db::open_with_threads(
                &config.db_path(),
                config.index.busy_timeout_ms,
                worker_threads,
            )?
        };
        db.set_implicit_index_maintenance(config.index.refresh != IndexRefresh::ExistingOnly);
        Ok(Self { config, db })
    }

    /// Open with explicit maintenance authority even when implicit refresh is configured as
    /// `existing-only`. CLI `reindex` and `compact` are explicit write requests; ordinary query
    /// commands continue to receive a SQLite-enforced read-only handle. The stored configuration
    /// retains the user's selected refresh policy.
    pub(crate) fn open_for_maintenance(mut config: Config) -> Result<Self> {
        let refresh = config.index.refresh;
        config.index.refresh = IndexRefresh::BeforeQuery;
        let mut app = Self::open(config)?;
        app.config.index.refresh = refresh;
        Ok(app)
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
        MessageService::new(&self.config, &self.db, SearchSurface::Rust)
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

    /// Crate-internal storage access for operations not yet represented by a service.
    ///
    /// External consumers (Python bindings, integration tests, downstream crates) must use
    /// the typed services above; raw [`Db`] access is not part of the supported API.
    pub(crate) const fn database(&self) -> &Db {
        &self.db
    }

    /// Install a frontend-specific progress sink.
    pub fn set_progress_reporter(&mut self, reporter: impl Fn(&str) + Send + Sync + 'static) {
        self.db.set_progress_reporter(reporter);
    }
}

#[cfg(test)]
mod execution_runtime_tests {
    use super::*;
    use crate::models::{Message, MessageFilters, MessageKind, MessageSearchMode, Provider, Role};
    use crate::util::minimal_record;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[test]
    fn ordinary_open_refuses_newer_schema_before_initialization_can_mutate_it() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("index.db");
        drop(Db::open(&db_path).unwrap());
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "user_version", crate::db::SCHEMA_VERSION + 1)
            .unwrap();
        let schema_before: String = conn
            .query_row(
                "select group_concat(coalesce(sql, ''), char(10))
                   from (select sql from sqlite_schema order by type, name)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);

        let mut config = Config::default();
        config.index.db_path = Some(db_path.to_string_lossy().into_owned());
        let error = SessionSearch::open(config)
            .err()
            .expect("newer schema must be rejected")
            .to_string();
        assert!(error.contains("newer than this aise build"), "{error}");

        let conn = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let schema_after: String = conn
            .query_row(
                "select group_concat(coalesce(sql, ''), char(10))
                   from (select sql from sqlite_schema order by type, name)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(schema_after, schema_before);
    }

    #[test]
    fn applications_own_independent_worker_counts() {
        let root = tempfile::tempdir().unwrap();
        let mut one_config = Config::default();
        one_config.index.db_path = Some(root.path().join("one.db").to_string_lossy().into_owned());
        one_config.performance.threads = 1;
        let mut two_config = Config::default();
        two_config.index.db_path = Some(root.path().join("two.db").to_string_lossy().into_owned());
        two_config.performance.threads = 2;

        let one = SessionSearch::open(one_config).unwrap();
        let two = SessionSearch::open(two_config).unwrap();

        assert_eq!(one.database().worker_threads(), 1);
        assert_eq!(two.database().worker_threads(), 2);
    }

    #[test]
    fn existing_only_search_never_builds_the_lazy_trigram_index() {
        let root = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.db_path = Some(root.path().join("index.db").to_string_lossy().into_owned());
        config.index.cache_dir = Some(root.path().join("cache").to_string_lossy().into_owned());
        let mut parsed = minimal_record(
            Provider::Claude,
            std::path::Path::new("/fixture/session.jsonl"),
            String::new(),
        );
        parsed.session.id = "claude:existing-only".into();
        parsed.session.provider_session_id = "existing-only".into();
        parsed.messages = vec![Message {
            seq: 0,
            role: Role::User,
            ts: None,
            tool_name: None,
            kind: MessageKind::Conversation,
            tool_call_id: None,
            is_compaction: false,
            content: "request failed with ECONNRESET".into(),
        }];
        let writer = SessionSearch::open(config.clone()).unwrap();
        writer.database().upsert_session(&parsed, 0, 0).unwrap();
        drop(writer);
        let conn = rusqlite::Connection::open_with_flags(
            config.db_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let schema_before: String = conn
            .query_row(
                "select group_concat(coalesce(sql, ''), char(10))
                   from (select sql from sqlite_schema order by type, name)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);

        config.index.refresh = IndexRefresh::ExistingOnly;
        let mut app = SessionSearch::open(config).unwrap();
        let write_error = app
            .database()
            .upsert_session(&parsed, 1, 1)
            .expect_err("existing-only database authority must reject writes")
            .to_string();
        assert!(
            write_error.contains("readonly") || write_error.contains("read-only"),
            "{write_error}"
        );

        let progress_calls = Arc::new(AtomicU32::new(0));
        let observed_calls = Arc::clone(&progress_calls);
        app.set_progress_reporter(move |_message| {
            observed_calls.fetch_add(1, Ordering::Relaxed);
        });
        let hits = app
            .messages()
            .search_legacy(
                "ECONNRESET",
                &MessageFilters {
                    match_mode: MessageSearchMode::Regex,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(
            hits.len(),
            1,
            "existing-only still scans the unindexed delta"
        );
        assert_eq!(
            progress_calls.load(Ordering::Relaxed),
            0,
            "existing-only must not start implicit trigram maintenance"
        );
        assert!(
            app.database()
                .vocabulary(true, 0)
                .unwrap()
                .iter()
                .any(|(term, _, _)| term == "eco" || term == "con" || term == "res"),
            "v4 trigram vocabulary remains readable without legacy maintenance"
        );
        drop(app);
        let conn = rusqlite::Connection::open_with_flags(
            root.path().join("index.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let schema_after: String = conn
            .query_row(
                "select group_concat(coalesce(sql, ''), char(10))
                   from (select sql from sqlite_schema order by type, name)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(schema_after, schema_before);
    }

    #[test]
    fn existing_only_open_refuses_missing_index_without_creating_paths() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("missing/index.db");
        let cache_path = root.path().join("missing/cache");
        let mut config = Config::default();
        config.index.db_path = Some(db_path.to_string_lossy().into_owned());
        config.index.cache_dir = Some(cache_path.to_string_lossy().into_owned());
        config.index.refresh = IndexRefresh::ExistingOnly;

        let error = SessionSearch::open(config)
            .err()
            .expect("missing existing-only index must fail")
            .to_string();

        assert!(error.contains("does not exist"), "{error}");
        assert!(error.contains("aise reindex --full"), "{error}");
        assert!(!db_path.exists());
        assert!(!cache_path.exists());
        assert!(!root.path().join("missing").exists());
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

const DEFAULT_ANALYSIS_SESSION_BATCH_SIZE: std::num::NonZeroUsize =
    std::num::NonZeroUsize::new(50).expect("analysis session batch constant is nonzero");

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
    ///
    /// # Complexity
    ///
    /// Time is proportional to filtered user-message bytes times the configured correction
    /// patterns. Memory is proportional to returned matches; `filters.limit = 0` is unbounded.
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
    ///
    /// # Complexity
    ///
    /// Time is proportional to filtered user-message rows times the combined command patterns.
    /// Memory is proportional to distinct matched commands, sessions, and projects.
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
    ///
    /// # Complexity
    ///
    /// Time and returned memory are proportional to the page's selected sessions plus their
    /// joined user-message text. Keyset traversal avoids work proportional to prior page offsets.
    pub fn documents(
        &self,
        filters: &SearchFilters,
        cursor: Option<&crate::models::AnalysisCursor>,
    ) -> Result<crate::models::AnalysisDocumentPage> {
        self.db.analysis_documents(filters, cursor)
    }

    /// Run a compiled provider-neutral policy over one snapshot of the indexed corpus.
    ///
    /// `filters.limit` bounds the total number of sessions (`0` means all). Internal keyset
    /// traversal is automatic and does not alter analysis or publication results. User-message
    /// text streams through per-message, so memory is bounded by the policy's explicit bounds
    /// plus one message — a single session's aggregate user text is never materialized (except
    /// when a `user_text`/`any` classification rule runs without `max_classification_chars`).
    ///
    /// # Complexity
    ///
    /// Time is proportional to streamed user-message bytes times applicable policy rules, plus
    /// phrase aggregation. Memory is bounded by policy limits plus one message, except an
    /// explicitly unbounded `user_text`/`any` classification rule retains its matched text.
    pub fn run(
        &self,
        filters: &SearchFilters,
        policy: &crate::analysis_pipeline::AnalysisPolicy,
    ) -> Result<crate::analysis_pipeline::AnalysisResult> {
        self.run_with_session_batch_size(filters, DEFAULT_ANALYSIS_SESSION_BATCH_SIZE, policy)
    }

    /// Run with an explicit internal session batch size for invariant tests and internal tuning.
    /// This value controls database traversal only and must never change returned results.
    pub(crate) fn run_with_session_batch_size(
        &self,
        filters: &SearchFilters,
        session_batch_size: std::num::NonZeroUsize,
        policy: &crate::analysis_pipeline::AnalysisPolicy,
    ) -> Result<crate::analysis_pipeline::AnalysisResult> {
        let mut accumulator = policy.accumulator();
        self.db.visit_analysis_sessions(
            filters,
            session_batch_size,
            |session, message_count, user_message_count, chunks| {
                accumulator.push_session_text_stream(
                    session,
                    message_count,
                    user_message_count,
                    chunks,
                )
            },
        )?;
        accumulator.finish()
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

    /// Select sessions and stream their rendered documents into one immutable bundle.
    ///
    /// This retains no full-corpus transcript collection: memory is bounded by the selected
    /// session metadata plus the largest individual rendered document.
    /// Time and output bytes are proportional to the selected transcripts; `filters.limit = 0`
    /// intentionally selects the complete filtered corpus.
    pub fn publish_bundle(
        &self,
        filters: &SearchFilters,
        plan: &crate::export::ExportPublicationPlan,
    ) -> Result<crate::export::ExportPublicationReceipt> {
        let sessions = self.db.list_recent(filters)?;
        let format = plan.format();
        let documents = sessions.into_iter().map(|session| {
            self.render_full(&session.id, format)
                .map(|document| (session.id, document))
        });
        plan.publish(documents)
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
            let before_bytes = self.db.storage_allocation()?.total_bytes;
            self.db.optimize_fts()?;
            self.db.vacuum()?;
            self.db.checkpoint_truncate()?;
            Ok(CompactOutcome {
                before_bytes,
                after_bytes: self.db.storage_allocation()?.total_bytes,
            })
        })
    }
}

#[derive(Clone, Copy)]
/// Explicit refresh, reindex, and status operations sharing the application's elected writer.
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
        let outcome = indexer::explicit_reindex_and_migrate(self.config, self.db, full, None)?;
        Ok((outcome.files_seen, outcome.sessions_updated))
    }

    /// Report parser/schema freshness and only repairs applicable to discoverable sources.
    pub fn status(&self) -> Result<IndexStatus> {
        crate::diagnostics::index_status(self.config, self.db)
    }
}

#[derive(Clone, Copy)]
/// Typed session listing, search, resolution, and compact inspection operations.
pub struct CatalogService<'db> {
    db: &'db Db,
}

impl<'db> CatalogService<'db> {
    pub const fn new(db: &'db Db) -> Self {
        Self { db }
    }

    /// List filtered sessions in database order.
    ///
    /// # Complexity
    ///
    /// Returned memory is proportional to the selected rows. A zero limit intentionally returns
    /// the complete filtered corpus; nonzero limits bound result materialization.
    pub fn list_sessions(&self, filters: &SearchFilters) -> Result<Vec<SessionRecord>> {
        self.db.list_recent(filters)
    }

    /// Search session text with FTS candidate selection and configured relevance scoring.
    ///
    /// # Complexity
    ///
    /// Work is proportional to FTS candidates plus any fuzzy re-ranking over those candidates;
    /// returned memory is proportional to selected hits. A zero limit scans/returns the complete
    /// filtered match set by explicit caller request.
    pub fn search_sessions(
        &self,
        query: &str,
        filters: &SearchFilters,
        current_repo: Option<&str>,
        scoring: &ScoringConfig,
    ) -> Result<Vec<SearchHit>> {
        self.db.search(query, filters, current_repo, scoring)
    }

    /// Resolve one canonical session ID or unique prefix; ambiguous errors list candidates.
    pub fn resolve_session(&self, id_or_prefix: &str) -> Result<SessionRecord> {
        self.db.resolve_session_record(id_or_prefix)
    }

    /// Build a bounded evidence summary for one resolved session without filesystem writes.
    pub fn inspect(
        &self,
        id_or_prefix: &str,
        options: crate::inspect::InspectionOptions,
    ) -> Result<crate::inspect::SessionInspection> {
        crate::inspect::inspect_session(self.db, id_or_prefix, options)
    }
}

#[derive(Clone, Copy)]
/// Canonical message search, explain, metadata, and context operations used by all adapters.
pub struct MessageService<'db> {
    config: &'db Config,
    db: &'db Db,
    surface: SearchSurface,
}

impl<'db> MessageService<'db> {
    pub const fn new(config: &'db Config, db: &'db Db, surface: SearchSurface) -> Self {
        Self {
            config,
            db,
            surface,
        }
    }

    /// Search individual messages using the selected exact, fuzzy, or regex mode.
    ///
    /// # Complexity
    ///
    /// Schema-v4 exact/regex uses SQLite trigram candidates when a safe literal exists, then
    /// authoritative verification. Fuzzy content/tool-argument work is bounded by the configured
    /// candidate budget; fuzzy tool-name work is bounded by 10,000 distinct names plus the page.
    /// Regex without a safe literal may scan the filtered corpus. Exact/regex output is unbounded
    /// only when `filters.limit == 0`; fuzzy validation rejects an unbounded page.
    pub fn search_legacy(&self, query: &str, filters: &MessageFilters) -> Result<Vec<MessageHit>> {
        self.db.search_messages(query, filters)
    }

    pub fn plan(&self, request: MessageSearchRequest) -> Result<MessageSearchPlan> {
        let purpose = request
            .purpose()
            .map(|selection| {
                let definition = self
                    .config
                    .search
                    .purposes
                    .get(selection.name())
                    .ok_or_else(|| {
                        anyhow!("unknown message-search purpose {:?}", selection.name())
                    })?;
                if selection
                    .version()
                    .is_some_and(|version| version != definition.version)
                {
                    bail!(
                        "purpose {:?} version {} is unavailable; configured version is {}",
                        selection.name(),
                        selection.version().unwrap(),
                        definition.version
                    );
                }
                Ok((selection.name().to_string(), definition))
            })
            .transpose()?;
        let purpose_origin = || {
            purpose
                .as_ref()
                .map(|(name, definition)| ValueOrigin::Purpose {
                    name: name.clone(),
                    version: definition.version,
                })
        };
        let purpose_preferences = purpose
            .as_ref()
            .map(|(_, definition)| &definition.preferences);

        let (requested_limit, offset, explicit_all) = match request.extent() {
            crate::message_search::RequestedExtent::Page { limit, offset } => {
                (limit, offset, false)
            }
            crate::message_search::RequestedExtent::AllResults => (None, 0, true),
        };
        let (mut limit, mut limit_origin) = if let Some(limit) = requested_limit {
            (Some(limit), ValueOrigin::Explicit)
        } else if let Some(limit) = purpose_preferences.and_then(|value| value.default_limit) {
            (Some(limit), purpose_origin().unwrap())
        } else if let Some(limit) = self.config.search.message_search.default_limit {
            (Some(limit), ValueOrigin::OperationConfig)
        } else {
            let surface_limit = match self.surface {
                SearchSurface::Mcp => NonZeroUsize::new(self.config.mcp.search_messages_limit),
                SearchSurface::Python => NonZeroUsize::new(CURRENT_PYTHON_MESSAGE_SEARCH_LIMIT),
                SearchSurface::Rust | SearchSurface::Cli => None,
            };
            match surface_limit {
                Some(limit) => (
                    Some(limit),
                    ValueOrigin::SurfaceConfig {
                        surface: self.surface,
                    },
                ),
                None => (None, ValueOrigin::TypedDefault),
            }
        };
        if let (Some(current), Some(maximum)) =
            (limit, self.config.search.budgets.max_results_per_page)
        {
            if current > maximum {
                limit = Some(maximum);
                limit_origin = ValueOrigin::PolicyCeiling;
            }
        }
        let extent = if explicit_all {
            ResolvedExtent::AllResults { offset: 0 }
        } else if let Some(limit) = limit {
            ResolvedExtent::Page { limit, offset }
        } else if matches!(
            request.query(),
            crate::message_search::MessageQuery::Fuzzy(_)
        ) {
            bail!("fuzzy search requires a positive page size from the request or configuration");
        } else {
            ResolvedExtent::AllResults { offset }
        };

        let (mut context, mut context_before_origin, mut context_after_origin) =
            if let Some(context) = request.context() {
                (context, ValueOrigin::Explicit, ValueOrigin::Explicit)
            } else {
                let (before, before_origin) = if let Some(value) =
                    purpose_preferences.and_then(|preferences| preferences.context_before)
                {
                    (value, purpose_origin().unwrap())
                } else if let Some(value) =
                    self.config.search.message_search.context.messages_before
                {
                    (value, ValueOrigin::OperationConfig)
                } else {
                    (0, ValueOrigin::TypedDefault)
                };
                let (after, after_origin) = if let Some(value) =
                    purpose_preferences.and_then(|preferences| preferences.context_after)
                {
                    (value, purpose_origin().unwrap())
                } else if let Some(value) = self.config.search.message_search.context.messages_after
                {
                    (value, ValueOrigin::OperationConfig)
                } else {
                    (0, ValueOrigin::TypedDefault)
                };
                (
                    ContextWindow::new(before, after),
                    before_origin,
                    after_origin,
                )
            };
        if let Some(maximum) = self.config.search.budgets.max_context_messages {
            let narrowed = narrow_context_proportionally(context, maximum);
            if narrowed.before() != context.before() {
                context_before_origin = ValueOrigin::PolicyCeiling;
            }
            if narrowed.after() != context.after() {
                context_after_origin = ValueOrigin::PolicyCeiling;
            }
            context = narrowed;
        }

        let (include_refs, include_refs_origin) =
            if let Some(value) = request.presentation().include_refs() {
                (value, ValueOrigin::Explicit)
            } else if let Some(value) =
                purpose_preferences.and_then(|preferences| preferences.include_refs)
            {
                (value, purpose_origin().unwrap())
            } else {
                (false, ValueOrigin::TypedDefault)
            };
        let (message_lines, message_lines_origin) =
            if let Some(value) = request.presentation().message_lines() {
                (value, ValueOrigin::Explicit)
            } else if let Some(value) =
                purpose_preferences.and_then(|preferences| preferences.lines_per_message)
            {
                (LineWindow::from_signed(value)?, purpose_origin().unwrap())
            } else {
                let value = match self.surface {
                    SearchSurface::Cli => self.config.cli.lines_per_message,
                    SearchSurface::Mcp => self.config.mcp.lines_per_message,
                    SearchSurface::Rust | SearchSurface::Python => 0,
                };
                (
                    LineWindow::from_signed(value)?,
                    ValueOrigin::SurfaceConfig {
                        surface: self.surface,
                    },
                )
            };
        let (receipt, receipt_origin) = if let Some(value) = request.receipt_level() {
            (value, ValueOrigin::Explicit)
        } else if let Some(value) =
            purpose_preferences.and_then(|preferences| preferences.receipt_level)
        {
            (value, purpose_origin().unwrap())
        } else {
            (ReceiptLevel::None, ValueOrigin::TypedDefault)
        };

        let predicates = request.predicates();
        let session_id = predicates
            .session()
            .map(|value| {
                self.catalog()
                    .resolve_session(value)
                    .map(|session| session.id)
            })
            .transpose()?;
        let normalize = |value: &str| crate::util::normalize_path_prefix(value);
        let resolved_predicates = ResolvedMessagePredicates {
            role: predicates.role(),
            kind: predicates.kind(),
            provider: predicates.provider(),
            session_id,
            workspace_path_prefix: predicates.workspace_path_prefix().map(normalize),
            transcript_path_prefix: predicates.transcript_path_prefix().map(normalize),
            exclude_workspace_path_prefixes: predicates
                .exclude_workspace_path_prefixes()
                .map(normalize)
                .collect(),
            exclude_transcript_path_prefixes: predicates
                .exclude_transcript_path_prefixes()
                .map(normalize)
                .collect(),
            exclude_session_ids: predicates
                .exclude_session_ids()
                .map(str::to_string)
                .collect(),
            time: predicates.time(),
            sequence: predicates.sequence(),
            tool_name_contains: predicates.tool_name_contains().map(str::to_string),
            include_compaction: predicates.include_compaction(),
        };
        let ordering = if matches!(
            request.query(),
            crate::message_search::MessageQuery::Fuzzy(_)
        ) {
            ExecutionOrder::FuzzyRelevance
        } else {
            ExecutionOrder::SessionSequence
        };
        Ok(MessageSearchPlan {
            retrieval: MessageRetrievalPlan {
                query: request.query().clone(),
                target: request.target().clone(),
                predicates: resolved_predicates,
                match_window: request.match_window(),
                ordering,
                extent,
            },
            response: MessageResponsePlan {
                context,
                presentation: ResolvedMessagePresentation {
                    include_refs,
                    message_lines,
                },
            },
            receipt,
            origins: MessageSearchOrigins {
                limit: limit_origin,
                context_before: context_before_origin,
                context_after: context_after_origin,
                include_refs: include_refs_origin,
                message_lines: message_lines_origin,
                receipt_level: receipt_origin,
                ordering: ValueOrigin::Derived,
            },
        })
    }

    pub fn search(&self, request: MessageSearchRequest) -> Result<MessageSearchResponse> {
        let plan = self.plan(request)?;
        let include_explain = plan.receipt != ReceiptLevel::None;
        let (mut hits, planner) = self
            .db
            .search_message_plan(&plan.retrieval, include_explain)?;
        let (next_offset, extent) = match plan.retrieval.extent {
            ResolvedExtent::Page { limit, offset } => {
                let has_more = hits.len() > limit.get();
                hits.truncate(limit.get());
                (
                    has_more.then_some(offset.saturating_add(limit.get())),
                    plan.retrieval.extent,
                )
            }
            ResolvedExtent::AllResults { .. } => (None, plan.retrieval.extent),
        };
        if plan.retrieval.match_window == Some(MatchWindow::Latest) {
            hits.reverse();
        }
        let context_windows = if plan.response.context.before() == 0
            && plan.response.context.after() == 0
        {
            Vec::new()
        } else {
            let before = i64::try_from(plan.response.context.before())
                .map_err(|_| anyhow!("resolved context_before exceeds SQLite's signed range"))?;
            let after = i64::try_from(plan.response.context.after())
                .map_err(|_| anyhow!("resolved context_after exceeds SQLite's signed range"))?;
            hits.iter()
                .map(|hit| {
                    self.db
                        .message_context(&hit.session_id, hit.seq, before, after)
                })
                .collect::<Result<Vec<_>>>()?
        };
        let origins = (plan.receipt != ReceiptLevel::None).then(|| plan.origins.clone());
        Ok(MessageSearchResponse::new(
            hits,
            context_windows,
            PageInfo::new(extent, next_offset, plan.retrieval.ordering),
            plan.response.context,
            plan.response.presentation,
            planner,
            origins,
        ))
    }

    fn catalog(&self) -> CatalogService<'db> {
        CatalogService::new(self.db)
    }

    /// Run the same search as [`MessageService::search`] and optionally return its actual planner
    /// receipt. The receipt distinguishes prefilter skips from candidate-source saturation.
    pub fn search_with_explain(
        &self,
        query: &str,
        filters: &MessageFilters,
        include_explain: bool,
    ) -> Result<(Vec<MessageHit>, Option<SearchExplain>)> {
        self.db
            .search_messages_with_explain(query, filters, include_explain)
    }

    /// Load compact metadata for the requested canonical IDs; absent IDs are omitted.
    pub fn session_metadata(&self, session_ids: &[String]) -> Result<HashMap<String, SessionMeta>> {
        self.db.session_metadata(session_ids)
    }

    /// Return a bounded sequence window around one message.
    ///
    /// # Complexity
    ///
    /// The `(session_id, seq)` index makes database work and memory proportional to
    /// `before + after + 1`, subject to messages available in the session.
    pub fn context(
        &self,
        session_id: &str,
        seq: i64,
        before: i64,
        after: i64,
    ) -> Result<Vec<MessageHit>> {
        self.db.message_context(session_id, seq, before, after)
    }

    /// Read one session's messages, selecting the oldest or newest `filters.limit` by `order`,
    /// always returned in chronological (seq-ascending) order. Wraps
    /// [`Db::read_session_messages`]; `filters.session_id` must be set.
    pub fn read_session(
        &self,
        filters: &MessageFilters,
        order: crate::db::MessageOrder,
    ) -> Result<Vec<MessageHit>> {
        self.db.read_session_messages(filters, order)
    }
}

#[derive(Clone, Copy)]
/// Read-only file activity, causal history, reconstruction, and safe publication operations.
pub struct FileService<'db> {
    db: &'db Db,
}

impl<'db> FileService<'db> {
    pub const fn new(db: &'db Db) -> Self {
        Self { db }
    }

    /// Aggregate matching file-edit rows.
    ///
    /// # Complexity
    ///
    /// Work is proportional to filtered edit rows; returned memory is proportional to grouped
    /// files and is unbounded only when the query explicitly uses a zero limit.
    pub fn search(&self, query: &FileQuery) -> Result<Vec<FileEditSummary>> {
        self.db.file_search(query)
    }

    /// Return deterministic file/session edit-count pages using `query.limit` and `query.offset`.
    pub fn cross_reference(&self, query: &FileQuery) -> Result<Vec<FileCrossRef>> {
        self.db.file_cross_ref(query)
    }

    /// Return causally ordered edits for one selected file.
    ///
    /// # Complexity
    ///
    /// Reconstruction work is proportional to matching versions and their stored edit payloads;
    /// returned memory is bounded by `query.limit` unless it is explicitly zero. `query.offset`
    /// skips versions after stable session/version ordering.
    pub fn history(&self, file: &str, query: &FileQuery) -> Result<Vec<FileVersion>> {
        crate::files::history(self.db, file, query)
    }

    /// Replay edits through one requested version.
    ///
    /// # Complexity
    ///
    /// Time is proportional to replayed edit operations and content bytes; peak memory includes
    /// the reconstructed file plus the selected edit history.
    pub fn reconstruct(
        &self,
        file: &str,
        query: &FileQuery,
        version: Option<usize>,
    ) -> Result<crate::files::ReconstructedFile> {
        crate::files::reconstruct_query(self.db, file, query, version)
    }

    /// Lazily reconstruct every causally ordered version with a complete replay path.
    pub fn reconstruct_versions(
        &self,
        file: &str,
        query: &FileQuery,
    ) -> Result<crate::files::ReconstructedFileVersions> {
        crate::files::reconstruct_versions_query(self.db, file, query)
    }

    /// Atomically publish every reconstructable version to a new non-replacing directory.
    pub fn publish_versions(
        &self,
        file: &str,
        query: &FileQuery,
        destination: &std::path::Path,
    ) -> Result<crate::files::RecoveryPublicationReceipt> {
        let versions = self.reconstruct_versions(file, query)?;
        crate::files::publish_reconstructed_versions(versions, destination)
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
mod message_search_service_tests {
    use std::num::{NonZeroU32, NonZeroUsize};

    use super::*;
    use crate::config::{MessagePurposePreferences, PurposeDefinition, SearchOperation};
    use crate::message_search::{
        MessageQuery, MessageSearchRequestBuilder, MessageTarget, PurposeSelection,
        RequestedExtent, ResolvedExtent,
    };
    use crate::models::{Message, MessageKind, Provider, Role};
    use crate::util::minimal_record;

    fn disposable_db() -> (tempfile::TempDir, Db) {
        let directory = tempfile::tempdir().unwrap();
        let db = Db::open(&directory.path().join("message-search.db")).unwrap();
        db.mark_schema_current().unwrap();
        (directory, db)
    }

    fn literal_request() -> MessageSearchRequestBuilder {
        MessageSearchRequest::builder(
            MessageQuery::literal("needle").unwrap(),
            MessageTarget::content(),
        )
    }

    fn limit_of(plan: &MessageSearchPlan) -> Option<usize> {
        match plan.extent() {
            ResolvedExtent::Page { limit, .. } => Some(limit.get()),
            ResolvedExtent::AllResults { .. } => None,
        }
    }

    fn insert_session(db: &Db, id: &str, workspace: &str, transcript: &str, contents: &[&str]) {
        let mut parsed = minimal_record(
            Provider::Claude,
            std::path::Path::new(transcript),
            String::new(),
        );
        parsed.session.id = id.into();
        parsed.session.provider_session_id = id.replace(':', "-");
        parsed.session.cwd = Some(workspace.into());
        parsed.session.repo_root = Some(format!("{workspace}/repo"));
        parsed.session.source_path = transcript.into();
        parsed.messages = contents
            .iter()
            .enumerate()
            .map(|(sequence, content)| Message {
                seq: sequence as i64,
                role: Role::User,
                ts: None,
                tool_name: None,
                kind: MessageKind::Conversation,
                tool_call_id: None,
                is_compaction: false,
                content: (*content).into(),
            })
            .collect();
        db.upsert_session(&parsed, 0, 0).unwrap();
    }

    #[test]
    fn omitted_limits_preserve_each_surface_default_and_fuzzy_stays_bounded() {
        let (_directory, db) = disposable_db();
        let config = Config::default();
        let request = literal_request()
            .extent(RequestedExtent::page(None, 7).unwrap())
            .build()
            .unwrap();

        for surface in [SearchSurface::Rust, SearchSurface::Cli] {
            let plan = MessageService::new(&config, &db, surface)
                .plan(request.clone())
                .unwrap();
            assert_eq!(plan.extent(), ResolvedExtent::AllResults { offset: 7 });
            assert_eq!(plan.origins().limit(), &ValueOrigin::TypedDefault);
        }

        let mcp = MessageService::new(&config, &db, SearchSurface::Mcp)
            .plan(request.clone())
            .unwrap();
        assert_eq!(limit_of(&mcp), Some(config.mcp.search_messages_limit));
        assert_eq!(
            mcp.origins().limit(),
            &ValueOrigin::SurfaceConfig {
                surface: SearchSurface::Mcp,
            }
        );

        let python = MessageService::new(&config, &db, SearchSurface::Python)
            .plan(request)
            .unwrap();
        assert_eq!(limit_of(&python), Some(CURRENT_PYTHON_MESSAGE_SEARCH_LIMIT));

        let fuzzy = MessageSearchRequest::builder(
            MessageQuery::fuzzy("needle").unwrap(),
            MessageTarget::content(),
        )
        .build()
        .unwrap();
        assert!(MessageService::new(&config, &db, SearchSurface::Rust)
            .plan(fuzzy.clone())
            .unwrap_err()
            .to_string()
            .contains("requires a positive page size"));
        assert_eq!(
            limit_of(
                &MessageService::new(&config, &db, SearchSurface::Mcp)
                    .plan(fuzzy)
                    .unwrap()
            ),
            Some(config.mcp.search_messages_limit)
        );
    }

    #[test]
    fn planner_precedence_and_receipt_origins_are_explicit_and_policy_bounded() {
        let (_directory, db) = disposable_db();
        let mut config = Config::default();
        config.search.message_search.default_limit = NonZeroUsize::new(8);
        config.search.message_search.context.messages_before = Some(2);
        config.search.purposes.insert(
            "focused-review".into(),
            PurposeDefinition {
                version: NonZeroU32::new(1).unwrap(),
                operation: SearchOperation::MessageSearch,
                preferences: MessagePurposePreferences {
                    default_limit: NonZeroUsize::new(6),
                    context_before: Some(3),
                    context_after: Some(4),
                    receipt_level: Some(ReceiptLevel::Summary),
                    include_refs: Some(true),
                    lines_per_message: Some(-5),
                },
            },
        );
        let purpose = PurposeSelection::new("focused-review", NonZeroU32::new(1)).unwrap();
        let service = MessageService::new(&config, &db, SearchSurface::Mcp);

        let purpose_plan = service
            .plan(literal_request().purpose(purpose.clone()).build().unwrap())
            .unwrap();
        assert_eq!(limit_of(&purpose_plan), Some(6));
        assert!(matches!(
            purpose_plan.origins().limit(),
            ValueOrigin::Purpose { name, version }
                if name == "focused-review" && version.get() == 1
        ));
        assert_eq!(purpose_plan.context(), ContextWindow::new(3, 4));
        assert!(purpose_plan.presentation().include_refs());
        assert_eq!(
            purpose_plan.presentation().message_lines(),
            LineWindow::Tail(NonZeroUsize::new(5).unwrap())
        );
        assert_eq!(purpose_plan.receipt_level(), ReceiptLevel::Summary);

        let explicit = service
            .plan(
                literal_request()
                    .purpose(purpose)
                    .extent(RequestedExtent::page(Some(3), 0).unwrap())
                    .context(ContextWindow::new(9, 10))
                    .include_refs(false)
                    .message_lines(LineWindow::Head(NonZeroUsize::new(2).unwrap()))
                    .receipt_level(ReceiptLevel::Full)
                    .build()
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(limit_of(&explicit), Some(3));
        assert_eq!(explicit.origins().limit(), &ValueOrigin::Explicit);
        assert_eq!(explicit.origins().context_before(), &ValueOrigin::Explicit);
        assert_eq!(explicit.origins().receipt_level(), &ValueOrigin::Explicit);

        config.search.budgets.max_results_per_page = NonZeroUsize::new(4);
        let bounded = MessageService::new(&config, &db, SearchSurface::Mcp)
            .plan(literal_request().build().unwrap())
            .unwrap();
        assert_eq!(limit_of(&bounded), Some(4));
        assert_eq!(bounded.origins().limit(), &ValueOrigin::PolicyCeiling);
        assert_eq!(bounded.context(), ContextWindow::new(2, 0));
        assert_eq!(
            bounded.origins().context_before(),
            &ValueOrigin::OperationConfig
        );

        config.search.budgets.max_context_messages = NonZeroUsize::new(4);
        let bounded_context = MessageService::new(&config, &db, SearchSurface::Mcp)
            .plan(
                literal_request()
                    .context(ContextWindow::new(9, 10))
                    .build()
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(bounded_context.context(), ContextWindow::new(2, 2));
        assert_eq!(
            bounded_context.origins().context_before(),
            &ValueOrigin::PolicyCeiling
        );
        assert_eq!(
            bounded_context.origins().context_after(),
            &ValueOrigin::PolicyCeiling
        );
    }

    #[test]
    fn planner_resolves_unique_session_prefix_and_rejects_ambiguous_prefix() {
        let (_directory, db) = disposable_db();
        insert_session(
            &db,
            "claude:abcdef",
            "/workspace/a",
            "/transcripts/a.jsonl",
            &["needle"],
        );
        insert_session(
            &db,
            "claude:abcxyz",
            "/workspace/b",
            "/transcripts/b.jsonl",
            &["needle"],
        );
        let config = Config::default();
        let service = MessageService::new(&config, &db, SearchSurface::Rust);

        let unique = service
            .plan(
                literal_request()
                    .session_id("claude:abcd")
                    .unwrap()
                    .build()
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(
            unique.retrieval.predicates.session_id.as_deref(),
            Some("claude:abcdef")
        );

        assert!(service
            .plan(
                literal_request()
                    .session_id("claude:abc")
                    .unwrap()
                    .build()
                    .unwrap(),
            )
            .unwrap_err()
            .to_string()
            .contains("ambiguous"));
    }

    #[test]
    fn latest_literal_and_regex_select_newest_matches_then_present_chronologically() {
        let (_directory, db) = disposable_db();
        insert_session(
            &db,
            "claude:latest-window",
            "/workspace/latest",
            "/transcripts/latest.jsonl",
            &["needle zero", "unrelated", "needle two", "needle three"],
        );
        let config = Config::default();
        let service = MessageService::new(&config, &db, SearchSurface::Rust);

        for query in [
            MessageQuery::literal("needle").unwrap(),
            MessageQuery::regex("needle (zero|two|three)").unwrap(),
        ] {
            let response = service
                .search(
                    MessageSearchRequest::builder(query, MessageTarget::content())
                        .session_id("claude:latest")
                        .unwrap()
                        .match_window(MatchWindow::Latest)
                        .extent(RequestedExtent::page(Some(2), 0).unwrap())
                        .build()
                        .unwrap(),
                )
                .unwrap();
            assert_eq!(
                response
                    .hits()
                    .iter()
                    .map(|hit| hit.seq)
                    .collect::<Vec<_>>(),
                vec![2, 3]
            );
            assert_eq!(response.page().next_offset(), Some(2));
        }
    }

    #[test]
    fn typed_path_domains_are_separate_while_legacy_path_remains_broad() {
        let (_directory, db) = disposable_db();
        insert_session(
            &db,
            "claude:workspace-domain",
            "/domains/shared/workspace",
            "/elsewhere/workspace.jsonl",
            &["needle"],
        );
        insert_session(
            &db,
            "claude:transcript-domain",
            "/elsewhere/transcript",
            "/domains/shared/transcript/session.jsonl",
            &["needle"],
        );
        let config = Config::default();
        let service = MessageService::new(&config, &db, SearchSurface::Rust);

        let workspace = service
            .search(
                literal_request()
                    .workspace_path_prefix("/domains/shared")
                    .unwrap()
                    .build()
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(workspace.hits().len(), 1);
        assert_eq!(workspace.hits()[0].session_id, "claude:workspace-domain");

        let transcript = service
            .search(
                literal_request()
                    .transcript_path_prefix("/domains/shared")
                    .unwrap()
                    .build()
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(transcript.hits().len(), 1);
        assert_eq!(transcript.hits()[0].session_id, "claude:transcript-domain");

        let excluded_workspace = service
            .search(
                literal_request()
                    .exclude_workspace_path_prefix("/domains/shared")
                    .unwrap()
                    .build()
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(excluded_workspace.hits().len(), 1);
        assert_eq!(
            excluded_workspace.hits()[0].session_id,
            "claude:transcript-domain"
        );

        let excluded_transcript = service
            .search(
                literal_request()
                    .exclude_transcript_path_prefix("/domains/shared")
                    .unwrap()
                    .build()
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(excluded_transcript.hits().len(), 1);
        assert_eq!(
            excluded_transcript.hits()[0].session_id,
            "claude:workspace-domain"
        );

        let legacy = service
            .search_legacy(
                "needle",
                &MessageFilters {
                    path_prefix: Some("/domains/shared".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(legacy.len(), 2);
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

        let config = Config::default();
        let messages = MessageService::new(&config, &db, SearchSurface::Rust)
            .search_legacy("missing", &MessageFilters::default())
            .unwrap();
        let files = FileService::new(&db).search(&FileQuery::default()).unwrap();
        let mut config = Config::default();
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().to_string());
        let app = SessionSearch::open(config).unwrap();
        app.database().mark_schema_current().unwrap();
        let status = app.index().status().unwrap();

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

    #[test]
    fn explicit_reindex_owns_lock_on_current_schema() {
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
        assert!(!app.database().needs_backfill().unwrap());

        assert_eq!(app.index().reindex(false).unwrap(), (0, 0));

        assert!(!app.database().needs_backfill().unwrap());
        assert!(app.index().status().unwrap().parser_health.schema_current);
        assert!(indexer::index_update_lock_path(&app.config().db_path()).exists());
    }

    #[test]
    fn public_full_reindex_promotes_v3_to_current_search_schema() {
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

        drop(Db::open(&config.db_path()).unwrap());
        let conn = rusqlite::Connection::open(config.db_path()).unwrap();
        conn.execute_batch(
            "drop trigger messages_ai;
                 drop trigger messages_ad;
                 drop trigger messages_au;
                 drop table messages_trigram_terms;
                 drop table messages_trigram_vocab;
                 drop table messages_trigram;
                 pragma user_version=3;",
        )
        .unwrap();
        crate::fts::install_released_message_word_index(&conn).unwrap();
        crate::trigram_index::ensure_schema(&conn).unwrap();
        drop(conn);

        let app = SessionSearch::open(config).unwrap();

        assert_eq!(app.index().reindex(true).unwrap(), (0, 0));
        assert_eq!(
            app.database().schema_version().unwrap(),
            crate::db::SCHEMA_VERSION
        );
        let conn = rusqlite::Connection::open_with_flags(
            app.config().db_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let layout: (bool, bool, bool) = conn
            .query_row(
                "select exists(select 1 from sqlite_schema where name='messages_trigram'),
                        exists(select 1 from sqlite_schema where name='messages_trigram_vocab'),
                        not exists(select 1 from sqlite_schema where name='trigram_postings')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(layout, (true, true, true));
    }
}
