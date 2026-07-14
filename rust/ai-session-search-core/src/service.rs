//! Typed application services shared by CLI, MCP, and language bindings.
//!
//! Services own operation boundaries, while [`Db`] remains the storage layer.
//! Adapters must not duplicate SQL, filtering, pagination, or lifecycle policy.

use std::collections::HashMap;
use std::fs;
use std::num::NonZeroUsize;

use anyhow::Result;

use crate::config::{Config, IndexRefresh, ScoringConfig};
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
    pub fn open(config: Config) -> Result<Self> {
        fs::create_dir_all(config.cache_dir())?;
        let worker_threads = NonZeroUsize::new(config.resolve_threads())
            .expect("Config::resolve_threads always returns at least one");
        let mut db = Db::open_with_threads(
            &config.db_path(),
            config.index.busy_timeout_ms,
            worker_threads,
        )?;
        db.apply_performance_config(&config.performance);
        db.set_implicit_index_maintenance(config.index.refresh != IndexRefresh::ExistingOnly);
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
        config.index.refresh = IndexRefresh::ExistingOnly;
        let mut app = SessionSearch::open(config).unwrap();
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
        app.database().upsert_session(&parsed, 0, 0).unwrap();

        let progress_calls = Arc::new(AtomicU32::new(0));
        let observed_calls = Arc::clone(&progress_calls);
        app.set_progress_reporter(move |_message| {
            observed_calls.fetch_add(1, Ordering::Relaxed);
        });
        let hits = app
            .messages()
            .search(
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
        indexer::with_index_update_lock(self.config, || {
            let schema_backfill_required = self.db.needs_backfill()?;
            let effective_full = full || schema_backfill_required;
            let outcome = indexer::reindex(self.config, self.db, effective_full, None)?;
            if effective_full {
                self.db.purge_injected_messages()?;
                self.db.mark_schema_current()?;
            }
            self.db.mark_auto_reindex_complete()?;
            Ok(outcome)
        })
    }

    /// Report parser/schema freshness and only repairs applicable to discoverable sources.
    pub fn status(&self) -> Result<IndexStatus> {
        crate::diagnostics::index_status(self.config, self.db)
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

    /// Search individual messages using the selected exact, fuzzy, or regex mode.
    ///
    /// # Complexity
    ///
    /// Exact search uses FTS candidates. Fuzzy and regex work is proportional to their filtered
    /// candidate corpus; regex may scan that entire corpus when no selective literal prefilter is
    /// available. Returned content and memory are bounded only when `filters.limit` is nonzero.
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
}

#[derive(Clone, Copy)]
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

    pub fn cross_reference(&self, query: &FileQuery) -> Result<Vec<FileCrossRef>> {
        self.db.file_cross_ref(query)
    }

    /// Return causally ordered edits for one selected file.
    ///
    /// # Complexity
    ///
    /// Time and memory are proportional to matching versions and their stored edit payloads.
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
    fn explicit_reindex_owns_lock_and_completes_required_schema_backfill() {
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
        assert!(app.database().needs_backfill().unwrap());

        assert_eq!(app.index().reindex(false).unwrap(), (0, 0));

        assert!(!app.database().needs_backfill().unwrap());
        assert!(app.index().status().unwrap().parser_health.schema_current);
        assert!(indexer::index_update_lock_path(&app.config().db_path()).exists());
    }
}
