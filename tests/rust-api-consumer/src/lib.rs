// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

//! Compile-only coverage for the supported public Rust API.
//!
//! Hit fields can grow; construct hits through their constructor.
//! ```compile_fail
//! use ai_session_search::models::{MessageHit, MessageKind, Provider, Role};
//! let hit = MessageHit {
//!     provenance: None, session_id: "claude:example".into(), provider: Provider::Claude,
//!     seq: 0, role: Role::User, kind: MessageKind::Conversation, ts: None,
//!     tool_name: None, tool_call_id: None, fuzzy_score: None, content: "prompt".into(),
//! };
//! ```
//!
//! Correlation authorities can grow; downstream matches need a wildcard arm.
//! ```compile_fail
//! use ai_session_search::models::MessageCorrelationAuthority;
//! fn is_local(authority: MessageCorrelationAuthority) -> bool {
//!     match authority {
//!         MessageCorrelationAuthority::Anthropic | MessageCorrelationAuthority::OpenAi
//!         | MessageCorrelationAuthority::Cursor | MessageCorrelationAuthority::Google
//!         | MessageCorrelationAuthority::Pi | MessageCorrelationAuthority::PrimeAgent => false,
//!         MessageCorrelationAuthority::SourceLocal => true,
//!     }
//! }
//! ```

use ai_session_search::sql_query::{QueryResult, ResolvedDbQueryArgs};
use ai_session_search::{
    AnalysisPolicySpec, AnalysisPublicationFormat, AnalysisPublicationPlan,
    AnalysisPublicationReceipt, AnalysisRequest, AnalysisSessionSelection, CapabilityReceipt,
    ClassificationRuleSpec, ClassificationTarget, CorrectionCategorySpec, ExportFormat,
    ExportPublicationPlan, FileQuery, InspectionOptions, MessageClassificationDefinition,
    MessageClassificationMatch, MessageClassificationQuery, MessageClassificationReport,
    MessageFilters, MessageMatchViewMarkers, MessageQuery, MessageSearchMode, MessageSearchRequest,
    MessageTarget, PhraseTextMode, PhraseVocabularyPolicySpec, ReceiptedAnalysis, SearchFilters,
    SessionKind, SessionSearch, SkillCapabilityInput, SkillCapabilityOutput, SkillRunQuery,
    SkillSelector,
};

const EXAMPLE_CLASSIFICATION_WINDOW_CHARS: usize = 4_096;

/// Construct provenance and hits without depending on exhaustive public field lists.
pub fn provenance_hit() -> ai_session_search::models::MessageHit {
    use ai_session_search::models::{
        ContentPartAuthorship, ContentPartOrigin, MessageAuthorship, MessageContentPart,
        MessageCorrelationAuthority, MessageCorrelationIdentity, MessageHit, MessageKind,
        MessageProvenance, Provider, Role,
    };
    let mut provenance = MessageProvenance::default();
    provenance.authorship = MessageAuthorship::Mixed;
    provenance.content_parts = vec![
        MessageContentPart::new(
            0,
            0,
            1,
            ContentPartAuthorship::Human,
            ContentPartOrigin::DirectInput,
        ),
        MessageContentPart::new(
            1,
            1,
            2,
            ContentPartAuthorship::Generated,
            ContentPartOrigin::ToolPayload,
        ),
    ];
    provenance.correlation_identity = Some(MessageCorrelationIdentity::new(
        MessageCorrelationAuthority::SourceLocal,
        "claude:example",
        "event",
    ));
    provenance.validate("ab").unwrap();
    let mut hit = MessageHit::new(
        "claude:example",
        Provider::Claude,
        0,
        Role::Tool,
        MessageKind::ToolResult,
        "ab",
    );
    hit.provenance = Some(Box::new(provenance));
    let _ = match MessageCorrelationAuthority::SourceLocal {
        MessageCorrelationAuthority::SourceLocal => "source_local",
        _ => "other",
    };
    hit
}

/// Compile the supported construction pattern for public configuration structs.
///
/// They are `#[non_exhaustive]`, so a downstream crate starts from the shipped defaults and sets
/// what it wants rather than writing a struct literal. That is what makes a new setting a minor
/// release instead of a breaking one -- `[ui]` alone gained ten fields in the cycle before 1.0.0.
/// A struct expression here fails to compile with E0639, which is this file's job to prove.
pub fn tui_config_with_one_override() -> ai_session_search::config::UiConfig {
    let mut ui = ai_session_search::config::UiConfig::default();
    ui.preview_body_lines = 20;
    ui
}

/// Compile representative service composition as an external Rust consumer.
///
/// This function is intentionally not executed: downstream compilation verifies that callers can
/// use public types without gaining access to storage, CLI, MCP, or PyO3 implementation details.
pub fn exercise_public_api(
    app: &SessionSearch,
    recovery_destination: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut sessions = SearchFilters::default();
    // A downstream consumer can select session classes and follow the spawn link through public
    // types: `SessionKind` and the parent's own session id.
    sessions.session_kinds = Some(vec![SessionKind::User, SessionKind::Subagent]);
    sessions.limit = 20;
    let _ = app.catalog().list_sessions(&sessions)?;
    let _ = app.index().status()?;
    let message_filters = MessageFilters::default();
    message_filters.validate("")?;
    let summary_options = InspectionOptions::default();
    let _ = summary_options;
    let mode = "regex".parse::<MessageSearchMode>()?;
    assert_eq!(mode.as_str(), "regex");
    let message_request =
        MessageSearchRequest::builder(MessageQuery::literal("request")?, MessageTarget::content())
            .build()?;
    let message_response = app.messages().search(message_request)?;
    let _ = message_response.hits().first().and_then(|hit| {
        hit.match_evidence()
            .map(|evidence| match &evidence.markers {
                MessageMatchViewMarkers::Characters { ranges, .. } => ranges.len(),
                // Marker kinds are non-exhaustive: a kind added later carries no ranges here.
                _ => 0,
            })
    });
    let analysis = app.analysis();
    let page = analysis.documents(&sessions, None)?;
    let _ = page.next_cursor.as_ref().map(|cursor| cursor.as_str());
    let skill_report = analysis.run_skill(&SkillRunQuery::new(
        SkillSelector::name("corrections")?,
        SkillCapabilityInput::MessageClassification({
            let mut value = MessageClassificationQuery::default();
            value.filters = message_filters.clone();
            value
        }),
    ))?;
    let SkillCapabilityOutput::MessageClassification(classification) = skill_report.output else {
        return Err("message-classification query returned another capability output".into());
    };
    let receipt: CapabilityReceipt = classification.receipt;
    let report: MessageClassificationReport = classification.report;
    let _ = receipt.sha256.as_str();
    let _ = report
        .matches
        .first()
        .map(|hit: &MessageClassificationMatch| {
            (hit.policy_name.as_str(), hit.matched_text.as_str())
        });
    let _ = analysis.planning(&message_filters, &[])?;
    let _ = analysis.role_statistics(&message_filters)?;
    let policy = {
        let mut value = AnalysisPolicySpec::default();
        value.classification_rules = vec![ClassificationRuleSpec::new(
            "workflow",
            "testing",
            ClassificationTarget::UserText,
            "(?i)\\btest",
            1,
        )];
        value.phrase_vocabulary = Some({
            let mut value = PhraseVocabularyPolicySpec::new(vec![1], 1_000);
            value.text_mode = PhraseTextMode::ProseOnly;
            value
        });
        value.max_classification_chars = Some(EXAMPLE_CLASSIFICATION_WINDOW_CHARS);
        value
    }
    .compile()?;
    let analysis_request = AnalysisRequest::new(
        {
            let mut value = sessions.clone();
            value.limit = 0;
            value
        },
        AnalysisSessionSelection::AllEligible,
    )?;
    let analyzed = analysis.run(&analysis_request, &policy)?;
    let _ = analyzed.result.session_graph();
    let _ = app.files().search(&FileQuery::default())?;
    let _ = app
        .files()
        .reconstruct_versions("example.rs", &FileQuery::default());
    let _ = app
        .files()
        .publish_versions("example.rs", &FileQuery::default(), recovery_destination);
    let _ = app.sources().inventory();
    let _ = app.index().refresh()?;
    let _ = app.index().reindex(false)?;
    let _ = app.maintenance().diagnostics()?;
    let _ = app.maintenance().compact()?;
    let format = "markdown".parse::<ExportFormat>()?;
    let _ = app.exports().render_full("provider:session", format)?;
    let _ = ExportPublicationPlan::new(recovery_destination, format)?;
    Ok(())
}

/// Compile immutable analysis rendering and publication as an external consumer.
pub fn publish_analysis(
    analysis: &ReceiptedAnalysis,
    destination: &std::path::Path,
) -> Result<AnalysisPublicationReceipt, Box<dyn std::error::Error + Send + Sync>> {
    let plan = AnalysisPublicationPlan::new(
        destination,
        [
            AnalysisPublicationFormat::Json,
            AnalysisPublicationFormat::Markdown,
        ],
    )?;
    let artifacts = plan.render(analysis)?;
    for artifact in &artifacts {
        assert!(!artifact.name().is_empty());
        assert_eq!(artifact.bytes(), artifact.content().len());
        assert_eq!(artifact.sha256().len(), 64);
    }
    Ok(plan.publish(analysis)?)
}

/// Build the inputs a caller supplies itself, which `#[non_exhaustive]` keeps out of struct
/// literals: a transcript to parse, edits to replay, and config publication options.
pub fn caller_built_inputs(
    transcript: &std::path::Path,
    config_destination: &std::path::Path,
) -> Option<String> {
    use ai_session_search::migration::ConfigPublishOptions;
    use ai_session_search::models::{EditOp, FileEdit, Provider, SourceFile};
    use ai_session_search::providers::claude::ClaudeAdapter;

    let source = SourceFile::new(Provider::Claude, transcript, 0, 0);
    let parsed = ClaudeAdapter::new(Vec::new()).parse(&source);
    assert_eq!(parsed.session.provider, Provider::Claude);

    let mut write = FileEdit::new(0, "Write", "src/app.py");
    write.new_content = Some("print('one')\n".into());
    let mut edit = FileEdit::new(1, "Edit", "src/app.py");
    edit.edits.push(EditOp::new("one", "two"));
    assert_eq!(edit.file_name, "app.py");

    let mut options = ConfigPublishOptions::new(config_destination);
    options.replace_existing = true;
    assert!(options.rollback_copy.is_none());

    ai_session_search::files::reconstruct(&[write, edit], 2)
}

/// Build the query inputs that have no other construction path and run each one: read-only SQL
/// through `sql_query::query_path`, and a typed custom correction category through `run_skill`.
///
/// Returns the SQL result and the categories the custom definition matched, in report order.
pub fn caller_built_queries(
    app: &SessionSearch,
    sql: &str,
    category: &str,
    pattern: &str,
) -> Result<(QueryResult, Vec<String>), Box<dyn std::error::Error + Send + Sync>> {
    let args = ResolvedDbQueryArgs::new(sql, 10);
    let rows = ai_session_search::sql_query::query_path(
        &app.config().db_path(),
        app.config().index.busy_timeout_ms,
        &args,
    )?;

    let mut query = SkillRunQuery::new(
        SkillSelector::name("corrections")?,
        SkillCapabilityInput::MessageClassification(MessageClassificationQuery::default()),
    );
    query.definition = Some(MessageClassificationDefinition::new(vec![
        CorrectionCategorySpec::new(category, [pattern]),
    ]));
    let SkillCapabilityOutput::MessageClassification(result) =
        app.analysis().run_skill(&query)?.output
    else {
        return Err("message-classification query returned another capability output".into());
    };
    let categories = result
        .report
        .matches
        .into_iter()
        .map(|hit| hit.category)
        .collect();
    Ok((rows, categories))
}

/// Name every documented service input and result from the crate root alone.
///
/// This compiles only while each type is re-exported at the root; no path here reaches into
/// `config`, `db`, `models`, `files`, `inspect`, `indexer`, `export`, or `source`.
#[allow(clippy::type_complexity)]
pub fn root_reexports_name_service_types(
    app: &SessionSearch,
) -> ai_session_search::anyhow::Result<(
    Vec<ai_session_search::SessionRecord>,
    Vec<ai_session_search::SearchHit>,
    Vec<ai_session_search::MessageHit>,
    Option<ai_session_search::SessionInspection>,
)> {
    use ai_session_search::chrono::{TimeZone, Utc};
    use ai_session_search::{
        AnalysisService, AutoReindexOutcome, CatalogService, CompactOutcome, Config,
        ConfigOverrides, DiagnosticStatus, ExplicitReindexOutcome, ExportDocument, ExportService,
        FileCrossRef, FileEditSummary, FileService, FileVersion, IndexRefresh, IndexService,
        IndexStatus, MaintenanceService, MessageKind, MessageOrder, MessageService, PlanningCount,
        Provider, ProviderSourceStatus, ReconstructedFile, RecoveryPublicationReceipt,
        ResolvedConfig, Role, RoleStat, ScoringConfig, SessionMeta, SourceService,
    };
    let since = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let _: Option<fn(ConfigOverrides) -> ai_session_search::anyhow::Result<ResolvedConfig>> =
        Some(Config::resolve);
    let _: Option<IndexRefresh> = Some(IndexRefresh::BeforeQuery);
    let _: &ScoringConfig = &app.config().search.scoring;
    let catalog: CatalogService<'_> = app.catalog();
    let messages: MessageService<'_> = app.messages();
    let files: FileService<'_> = app.files();
    let index: IndexService<'_> = app.index();
    let _: MaintenanceService<'_> = app.maintenance();
    let analysis: AnalysisService<'_> = app.analysis();
    let _: ExportService<'_> = app.exports();
    let sources: SourceService<'_> = app.sources();
    let _: Vec<ProviderSourceStatus> = sources.inventory();
    let _: IndexStatus = index.status()?;
    type R<T> = ai_session_search::anyhow::Result<T>;
    let _ = |m: MaintenanceService<'_>| -> R<DiagnosticStatus> { m.diagnostics() };
    let _ = |m: MaintenanceService<'_>| -> R<CompactOutcome> { m.compact() };
    let _ = |i: IndexService<'_>| -> R<AutoReindexOutcome> { i.refresh() };
    let _ = |i: IndexService<'_>| -> R<ExplicitReindexOutcome> { i.reindex_report(false) };
    let mut filters = SearchFilters::default();
    filters.provider = Some(Provider::Claude);
    filters.since = Some(since);
    filters.limit = 5;
    let sessions = catalog.list_sessions(&filters)?;
    let hits = catalog.search_sessions("tests", &filters, None, &app.config().search.scoring)?;
    let mut message_filters = MessageFilters::default();
    message_filters.role = Some(Role::User);
    message_filters.kinds = Some(vec![MessageKind::Conversation]);
    let mut newest = Vec::new();
    let mut inspection = None;
    if let Some(session) = sessions.first() {
        let mut one_session = message_filters.clone();
        one_session.session_id = Some(session.id.clone());
        one_session.limit = 10;
        newest = messages.read_session(&one_session, MessageOrder::NewestFirst)?;
        inspection = Some(catalog.inspect(&session.id, InspectionOptions::default())?);
        let _: std::collections::HashMap<String, SessionMeta> =
            messages.session_metadata(std::slice::from_ref(&session.id))?;
        let _: ExportDocument = app
            .exports()
            .render_full(&session.id, ExportFormat::Markdown)?;
    }
    let _: Vec<FileEditSummary> = files.search(&FileQuery::default())?;
    let _: Vec<FileCrossRef> = files.cross_reference(&FileQuery::default())?;
    let q = FileQuery::default();
    let _ = |f: FileService<'_>| -> R<Vec<FileVersion>> { f.history("a.py", &q) };
    let _ = |f: FileService<'_>| -> R<ReconstructedFile> { f.reconstruct("a.py", &q, None) };
    let _ = |f: FileService<'_>, to: &std::path::Path| -> R<RecoveryPublicationReceipt> {
        f.publish_versions("a.py", &q, to)
    };
    let _: Vec<PlanningCount> = analysis.planning(&message_filters, &[])?;
    let _: Vec<RoleStat> = analysis.role_statistics(&message_filters)?;
    Ok((sessions, hits, newest, inspection))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_session_search::ServiceError;

    fn line(value: &str) -> String {
        format!("{value}\n")
    }

    /// Two Claude sessions under `/r`. A's first user turn is what the custom category matches;
    /// B writes and then edits `/r/app.py`, so it has two file versions, and ends with a harness
    /// notice.
    fn transcripts() -> [(&'static str, String); 2] {
        let a = [
            r#"{"type":"user","sessionId":"consumer-a","timestamp":"2026-06-01T10:00:00Z","cwd":"/r","uuid":"a0","message":{"role":"user","content":[{"type":"text","text":"you skipped the migration step. please run the tests before you commit"}]}}"#,
            r#"{"type":"assistant","sessionId":"consumer-a","timestamp":"2026-06-01T10:00:05Z","cwd":"/r","uuid":"a1","message":{"role":"assistant","content":[{"type":"text","text":"running the tests now"}]}}"#,
        ]
        .map(line)
        .concat();
        let b = [
            r#"{"type":"user","sessionId":"consumer-b","timestamp":"2026-06-02T10:00:00Z","cwd":"/r","uuid":"b0","message":{"role":"user","content":[{"type":"text","text":"please run the tests before you commit"}]}}"#,
            r#"{"type":"assistant","sessionId":"consumer-b","timestamp":"2026-06-02T10:00:01Z","cwd":"/r","uuid":"b1","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Write","input":{"file_path":"/r/app.py","content":"one\n"}}]}}"#,
            r#"{"type":"user","sessionId":"consumer-b","timestamp":"2026-06-02T10:00:02Z","cwd":"/r","uuid":"b2","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
            r#"{"type":"assistant","sessionId":"consumer-b","timestamp":"2026-06-02T10:00:03Z","cwd":"/r","uuid":"b3","message":{"role":"assistant","content":[{"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"/r/app.py","old_string":"one","new_string":"two"}}]}}"#,
            r#"{"type":"user","sessionId":"consumer-b","timestamp":"2026-06-02T10:00:04Z","cwd":"/r","uuid":"b4","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","content":"ok"}]}}"#,
            r#"{"type":"user","sessionId":"consumer-b","timestamp":"2026-06-02T10:00:05Z","cwd":"/r","uuid":"b5","isMeta":true,"message":{"role":"user","content":"Stop hook feedback: zebra notice"}}"#,
        ]
        .map(line)
        .concat();
        [("consumer-a", a), ("consumer-b", b)]
    }

    /// A temporary root holding the two transcripts and a configuration that reads only them.
    struct Fixture {
        root: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "aise-api-consumer-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let project = root.join("projects/p");
            std::fs::create_dir_all(&project).unwrap();
            for (id, text) in transcripts() {
                std::fs::write(project.join(format!("{id}.jsonl")), text).unwrap();
            }
            Self { root }
        }

        fn config(&self) -> ai_session_search::config::Config {
            let mut config = ai_session_search::config::Config::default();
            let providers = &mut config.providers;
            for provider in [
                &mut providers.claude_desktop.enabled,
                &mut providers.codex.enabled,
                &mut providers.cursor.enabled,
                &mut providers.antigravity.enabled,
                &mut providers.pi.enabled,
                &mut providers.prime_agent.enabled,
                &mut providers.aistudio.enabled,
                &mut providers.gemini_cli.enabled,
            ] {
                *provider = false;
            }
            providers.claude.enabled = true;
            providers.claude.paths =
                vec![self.root.join("projects").to_string_lossy().into_owned()];
            config.skills.search_paths = Vec::new();
            config.index.db_path = Some(self.root.join("index.db").to_string_lossy().into_owned());
            config.index.cache_dir = Some(self.root.join("cache").to_string_lossy().into_owned());
            config
        }

        /// Open and fully index the fixture.
        fn indexed(&self) -> SessionSearch {
            let app = SessionSearch::open(self.config()).unwrap();
            app.index().reindex_report(true).unwrap();
            app
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn service_error(error: &ai_session_search::anyhow::Error) -> &ServiceError {
        error
            .downcast_ref::<ServiceError>()
            .unwrap_or_else(|| panic!("not a ServiceError: {error:#}"))
    }

    #[test]
    fn caller_built_queries_run_against_a_temporary_index() {
        let fixture = Fixture::new();
        let app = fixture.indexed();
        let (rows, categories) = caller_built_queries(
            &app,
            "select count(*) as sessions from sessions",
            "skipped-step",
            r"\bskipped\b",
        )
        .unwrap();

        assert_eq!(rows.columns, ["sessions"]);
        assert_eq!(rows.rows.len(), 1);
        assert_eq!(rows.rows[0]["sessions"], 2);
        assert!(!rows.truncated);
        assert_eq!(categories, ["skipped-step"]);
    }

    /// Each failure a caller must branch on arrives as a `ServiceError` with a stable code.
    #[test]
    fn typed_errors_downcast_from_service_results() {
        let fixture = Fixture::new();
        let app = fixture.indexed();

        let error = app.catalog().resolve_session("claude:nope").unwrap_err();
        assert!(matches!(
            service_error(&error),
            ServiceError::SessionNotFound { id_or_prefix, .. } if id_or_prefix == "claude:nope"
        ));
        assert_eq!(service_error(&error).code(), "session-not-found");
        assert!(error
            .to_string()
            .starts_with("no session matches 'claude:nope'"));

        let error = app
            .catalog()
            .resolve_session("claude:consumer-")
            .unwrap_err();
        let ServiceError::SessionPrefixAmbiguous { candidates, .. } = service_error(&error) else {
            panic!("{error:#}")
        };
        assert_eq!(candidates, &["claude:consumer-a", "claude:consumer-b"]);

        // An unknown session in a context read is the same not-found failure, not an empty list.
        let error = app.messages().context("claude:nope", 0, 1, 1).unwrap_err();
        assert_eq!(service_error(&error).code(), "session-not-found");

        let error = app
            .files()
            .history("missing.py", &FileQuery::default())
            .unwrap_err();
        assert!(matches!(
            service_error(&error),
            ServiceError::FileNotFound { file, .. } if file == "missing.py"
        ));

        let error = app
            .files()
            .reconstruct("app.py", &FileQuery::default(), Some(9))
            .unwrap_err();
        let ServiceError::FileVersionOutOfRange {
            requested, valid, ..
        } = service_error(&error)
        else {
            panic!("{error:#}")
        };
        assert_eq!((*requested, valid.clone()), (9, 1..=2));

        let existing = fixture.root.join("projects");
        let error = app
            .files()
            .publish_versions("app.py", &FileQuery::default(), &existing)
            .unwrap_err();
        assert!(matches!(
            service_error(&error),
            ServiceError::DestinationExists { path, .. } if path == &existing
        ));

        let mut contradictory = SearchFilters::default();
        contradictory.parent_session_id = Some("claude:consumer-a".into());
        contradictory.session_kinds = Some(vec![SessionKind::User]);
        let error = app.catalog().list_sessions(&contradictory).unwrap_err();
        assert_eq!(service_error(&error).code(), "invalid-argument");
        drop(app);

        let mut missing = fixture.config();
        missing.index.db_path = Some(
            fixture
                .root
                .join("absent.db")
                .to_string_lossy()
                .into_owned(),
        );
        missing.index.refresh = ai_session_search::config::IndexRefresh::ExistingOnly;
        let error = SessionSearch::open(missing).err().unwrap();
        assert_eq!(service_error(&error).code(), "index-missing");
    }

    #[test]
    fn root_reexports_reach_every_service_on_an_indexed_fixture() {
        let fixture = Fixture::new();
        let app = fixture.indexed();
        let (sessions, hits, newest, inspection) = root_reexports_name_service_types(&app).unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(hits.len(), 2);
        assert!(!newest.is_empty() && newest.len() <= 10);
        assert!(inspection.is_some());
    }
}

// Compile the Rust examples in the repository README and the library guide. They live outside the
// published crate, so this unpublished crate owns the check.
#[cfg(doctest)]
#[doc = include_str!("../../../README.md")]
struct RepositoryReadmeExamples;

#[cfg(doctest)]
#[doc = include_str!("../../../docs/development/library-api.md")]
struct LibraryGuideExamples;
