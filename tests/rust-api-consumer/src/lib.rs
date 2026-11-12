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

//! CLI dispatch adapters are crate-private; public compatibility wrappers remain callable.
//! ```compile_fail
//! let _ = ai_session_search::files::run_command;
//! ```
//! ```compile_fail
//! let _ = ai_session_search::messages::run_command;
//! ```
//! ```compile_fail
//! let _ = ai_session_search::skills::run_with_harness_home;
//! ```
//! ```compile_fail
//! let _ = ai_session_search::sql_query::run_command;
//! ```
//! ```compile_fail
//! let _ = ai_session_search::analytics::run_vocab;
//! ```
//! ```compile_fail
//! let _ = ai_session_search::analytics::run_repeats;
//! ```
//! ```compile_fail
//! let _ = ai_session_search::analytics::run_stats;
//! ```
//! ```compile_fail
//! let _ = ai_session_search::analytics::run_planning;
//! ```
//! ```compile_fail
//! let _ = ai_session_search::mcp_schema_budget::run;
//! ```
//! ```compile_fail
//! let _ = ai_session_search::sql_query::schema_path;
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
    let _ = app.index().reindex_report(false)?;
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
    let filters = SearchFilters::default()
        .with_provider(Provider::Claude)
        .with_since(since)
        .with_limit(5);
    let sessions = catalog.list_sessions(&filters)?;
    let hits = catalog.search("tests", &filters)?;
    let _ = catalog.search_sessions("tests", &filters, None, &app.config().search.scoring)?;
    let message_filters = MessageFilters::default()
        .with_role(Role::User)
        .with_kinds(vec![MessageKind::Conversation]);
    let mut newest = Vec::new();
    let mut inspection = None;
    if let Some(session) = sessions.first() {
        newest = messages.read_session(
            &message_filters
                .clone()
                .with_session_id(session.id.clone())
                .with_limit(10),
            MessageOrder::NewestFirst,
        )?;
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

/// Build every documented request type with chainable setters instead of `let mut` plus field
/// assignment, which `#[non_exhaustive]` forces on struct literals.
pub fn requests_built_with_setters() -> (
    SearchFilters,
    MessageFilters,
    FileQuery,
    InspectionOptions,
    MessageClassificationQuery,
    ai_session_search::ConfigOverrides,
) {
    (
        SearchFilters::default()
            .with_path_prefix("/r")
            .with_session_kinds(vec![SessionKind::User])
            .with_limit(5),
        MessageFilters::default()
            .with_session_id("claude:consumer-a")
            .with_seq_from(1)
            .with_limit(10)
            .with_offset(2),
        FileQuery::default()
            .with_pattern("*.py")
            .with_min_edits(2)
            .with_limit(3),
        InspectionOptions::default()
            .with_preview_chars(80)
            .with_include_time_profile(true),
        MessageClassificationQuery::default()
            .with_filters(MessageFilters::default().with_limit(50))
            .with_include_unattributed_input(true),
        ai_session_search::ConfigOverrides::default()
            .with_database_path("/tmp/index.db")
            .with_threads(2),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_session_search::ServiceError;

    fn line(value: &str) -> String {
        format!("{value}\n")
    }

    /// Two Claude sessions under `/r`. A's first user turn is what the custom category matches,
    /// and A ends with a queued message whose sender is not recorded; B writes and then edits
    /// `/r/app.py`, so it has two file versions, and ends with a harness notice. "please run the
    /// tests before you commit" is typed in both sessions and queued once more.
    fn transcripts() -> [(&'static str, String); 2] {
        let a = [
            r#"{"type":"user","sessionId":"consumer-a","timestamp":"2026-06-01T10:00:00Z","cwd":"/r","uuid":"a0","message":{"role":"user","content":[{"type":"text","text":"you skipped the migration step. please run the tests before you commit"}]}}"#,
            r#"{"type":"assistant","sessionId":"consumer-a","timestamp":"2026-06-01T10:00:05Z","cwd":"/r","uuid":"a1","message":{"role":"assistant","content":[{"type":"text","text":"running the tests now"}]}}"#,
            // Queued while the agent worked, with no record of who sent it.
            r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-06-01T10:00:06Z","sessionId":"consumer-a","content":"please run the tests before you commit"}"#,
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
            // Tests run in parallel and the clock can repeat within one process, so a counter
            // keeps each fixture's directory its own.
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "aise-api-consumer-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
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

    /// The typed SQL call returns the rows `aise db query` returns, applies the configured row
    /// limit, cuts long cells on request, and is refused under `allowed-roots` like the CLI.
    #[test]
    fn sql_service_runs_read_only_queries_under_the_configured_scope() {
        use ai_session_search::{SqlQuery, SqlSchemaQuery, SqlService};
        let fixture = Fixture::new();
        let app = fixture.indexed();

        let rows = app
            .sql()
            .query(&SqlQuery::new("select id from sessions order by id").with_limit(1))
            .unwrap();
        assert_eq!(rows.columns, ["id"]);
        assert_eq!(rows.rows.len(), 1);
        assert_eq!(rows.rows[0]["id"], "claude:consumer-a");
        assert!(rows.truncated, "a second session remains past the limit");

        let page = app
            .sql()
            .query(
                &SqlQuery::new("select id from sessions order by id")
                    .with_offset(1)
                    .with_max_cell_chars(8),
            )
            .unwrap();
        assert_eq!(page.rows[0]["id"], "claude:c... [truncated]");
        assert_eq!(page.truncated_cell_char_limit, Some(8));
        assert!(!page.truncated);

        let error = app
            .sql()
            .query(&SqlQuery::new("delete from sessions"))
            .unwrap_err();
        assert!(format!("{error:#}").contains("not authorized"), "{error:#}");

        let tables = app.sql().schema(&SqlSchemaQuery::default()).unwrap();
        assert!(tables.rows.iter().any(|row| row["name"] == "sessions"));
        let columns = app
            .sql()
            .schema(&SqlSchemaQuery::default().with_table("sessions"))
            .unwrap();
        assert!(columns.rows.iter().any(|row| row["name"] == "provider"));

        let mut restricted = fixture.config();
        restricted.search.scope.mode = ai_session_search::config::SearchScopeMode::AllowedRoots;
        let error = SqlService::new(&restricted)
            .query(&SqlQuery::new("select 1"))
            .unwrap_err();
        assert!(error.to_string().contains("allowed-roots"), "{error:#}");
        assert!(SqlService::new(&restricted)
            .schema(&SqlSchemaQuery::default())
            .is_ok());
    }

    /// A never-refreshed index is not mistaken for an empty history: typed reads fail with
    /// `IndexNotRefreshed` until `prepare_for_read` applies the configured policy, which for the
    /// default `auto` indexes the sources synchronously.
    #[test]
    fn a_never_refreshed_index_is_distinguishable_from_no_matches() {
        use ai_session_search::AutoReindexOutcome;
        let fixture = Fixture::new();
        let app = SessionSearch::open(fixture.config()).unwrap();
        let error = app
            .catalog()
            .list_sessions(&SearchFilters::default())
            .unwrap_err();
        assert!(matches!(
            service_error(&error),
            ServiceError::IndexNotRefreshed { .. }
        ));
        assert_eq!(service_error(&error).code(), "index-not-refreshed");
        let error = app
            .messages()
            .search(
                MessageSearchRequest::builder(
                    MessageQuery::literal("tests").unwrap(),
                    MessageTarget::content(),
                )
                .build()
                .unwrap(),
            )
            .unwrap_err();
        assert_eq!(service_error(&error).code(), "index-not-refreshed");

        let outcome = app.index().prepare_for_read().unwrap();
        assert!(
            matches!(
                outcome,
                Some(AutoReindexOutcome::Updated {
                    sessions_updated: 2,
                    ..
                })
            ),
            "{outcome:?}"
        );
        assert_eq!(
            app.catalog()
                .list_sessions(&SearchFilters::default())
                .unwrap()
                .len(),
            2
        );
        // A refreshed index that matches nothing is an ordinary empty result.
        let none = app
            .catalog()
            .list_sessions(&SearchFilters::default().with_path_prefix("/elsewhere"))
            .unwrap();
        assert!(none.is_empty());
    }

    /// Origins name a library override as the `ConfigOverrides` field that set it, not as a
    /// command-line flag the program never received.
    #[test]
    fn config_origins_name_library_overrides_by_field() {
        use ai_session_search::config::ConfigEnvironment;
        use ai_session_search::{Config, ConfigOverrides};
        let fixture = Fixture::new();
        let config_file = fixture.root.join("config.toml");
        std::fs::write(&config_file, "").unwrap();
        let resolved = Config::resolve_with_environment(
            ConfigOverrides::default()
                .with_config_path(&config_file)
                .with_database_path(fixture.root.join("index.db"))
                .with_threads(2),
            ConfigEnvironment::default(),
        )
        .unwrap();
        assert_eq!(resolved.origins.config, "ConfigOverrides.config_path");
        assert_eq!(resolved.origins.database, "ConfigOverrides.database_path");
        assert_eq!(resolved.origins.threads, "ConfigOverrides.threads");
        assert_eq!(resolved.origins.cache, "typed/platform default");
    }

    /// Inspection follow-ups come as library calls beside the `aise` command strings, and each
    /// one runs.
    #[test]
    fn inspection_follow_ups_are_runnable_library_calls() {
        use ai_session_search::{InspectionFollowUp, MessageOrder};
        let fixture = Fixture::new();
        let app = fixture.indexed();
        let inspection = app
            .catalog()
            .inspect("claude:consumer-b", InspectionOptions::default())
            .unwrap();
        assert_eq!(inspection.follow_ups.len(), inspection.next_commands.len());
        for follow_up in &inspection.follow_ups {
            match follow_up {
                InspectionFollowUp::TranscriptWindow {
                    session_id, lines, ..
                } => {
                    assert_eq!(*lines, -40);
                    app.catalog().transcript(session_id, *lines).unwrap();
                }
                InspectionFollowUp::MessageContext {
                    session_id,
                    seq,
                    before,
                    after,
                    ..
                } => {
                    assert!(!app
                        .messages()
                        .context(session_id, *seq, *before, *after)
                        .unwrap()
                        .is_empty());
                }
                InspectionFollowUp::SessionMessages { session_id, .. } => {
                    let filters = MessageFilters::default().with_session_id(session_id.clone());
                    assert!(!app
                        .messages()
                        .read_session(&filters, MessageOrder::OldestFirst)
                        .unwrap()
                        .is_empty());
                }
                InspectionFollowUp::FileCrossReference { session_id, .. } => {
                    let query = FileQuery::default().with_session_id(session_id.clone());
                    assert!(!app.files().cross_reference(&query).unwrap().is_empty());
                }
                other => panic!("unexpected follow-up {other:?}"),
            }
        }
        assert!(matches!(
            inspection.follow_ups.first(),
            Some(InspectionFollowUp::TranscriptWindow { .. })
        ));
    }

    /// A completed batch traversal reports the same ordered digest as the materialized response,
    /// without the caller asking for a full receipt.
    #[test]
    fn batch_completion_reports_the_materialized_ordered_digest() {
        use ai_session_search::RequestedExtent;
        let fixture = Fixture::new();
        let app = fixture.indexed();
        let request = || {
            MessageSearchRequest::builder(
                MessageQuery::literal("tests").unwrap(),
                MessageTarget::content(),
            )
            .extent(RequestedExtent::all_results())
            .build()
            .unwrap()
        };
        let materialized = app.messages().search(request()).unwrap();
        let mut batches = app
            .message_search_batches(request(), std::num::NonZeroUsize::new(1).unwrap())
            .unwrap();
        let mut rows = 0;
        while let Some(batch) = batches.next_batch().unwrap() {
            rows += batch.results().len();
        }
        assert_eq!(rows, materialized.results().len());
        let completion = batches.completion().unwrap();
        assert_eq!(
            completion.ordered_digest(),
            Some(materialized.ordered_digest().as_str())
        );
    }

    /// Transcript windows use the CLI's signed convention: first N, last N, or all for 0.
    #[test]
    fn transcript_window_reads_first_last_or_all_lines() {
        use ai_session_search::{TranscriptEdge, TranscriptWindow};
        let fixture = Fixture::new();
        let app = fixture.indexed();
        let all: TranscriptWindow = app.catalog().transcript("claude:consumer-b", 0).unwrap();
        assert!(all.complete && all.total_lines == 2, "{all:?}");
        assert_eq!(
            (all.edge, all.lines_label.as_str()),
            (TranscriptEdge::All, "all")
        );
        assert_eq!(all.session.id, "claude:consumer-b");

        let head = app.catalog().transcript("claude:consumer-b", 1).unwrap();
        assert_eq!(
            (head.edge, head.lines_returned, head.complete),
            (TranscriptEdge::Head, 1, false)
        );
        assert_eq!(head.text, all.text.lines().next().unwrap());
        assert!(head.lines_label.starts_with("first 1 (truncated"));

        let tail = app.catalog().transcript("claude:consumer-b", -1).unwrap();
        assert_eq!(tail.text, all.text.lines().last().unwrap());
        assert_eq!(tail.edge, TranscriptEdge::Tail);

        let error = app.catalog().transcript("claude:consumer-", 0).unwrap_err();
        assert_eq!(service_error(&error).code(), "session-prefix-ambiguous");
    }

    /// The resume command is a named struct; a session the provider cannot reopen has no argv
    /// and says why.
    #[test]
    fn resume_command_names_argv_cwd_or_why_not() {
        use ai_session_search::ResumeCommand;
        let fixture = Fixture::new();
        let app = fixture.indexed();
        let plan: ResumeCommand = app.catalog().resume_command("claude:consumer-a").unwrap();
        assert_eq!(plan.session_id, "claude:consumer-a");
        match &plan.argv {
            Some(argv) => {
                assert_eq!(argv, &["claude", "--resume", "consumer-a"]);
                assert!(plan.unavailable_reason.is_none());
            }
            // The test machine may not have the `claude` CLI on PATH; the reason says so.
            None => {
                let reason = plan.unavailable_reason.as_deref().unwrap();
                assert!(reason.contains("`claude` is not on your PATH"), "{reason}");
                assert!(plan.cwd.is_none());
            }
        }
        // `/r` does not exist on the test machine, so no working directory is offered.
        assert!(plan.cwd.is_none());
        let error = app.catalog().resume_command("claude:nope").unwrap_err();
        assert_eq!(service_error(&error).code(), "session-not-found");
    }

    /// Recurring phrases come back typed, with the count of queued messages left out because
    /// nobody is recorded as their sender.
    #[test]
    fn repeats_return_groups_and_the_unattributed_count() {
        use ai_session_search::{RepeatsQuery, RepeatsReport};
        let fixture = Fixture::new();
        let app = fixture.indexed();
        let query = RepeatsQuery::default()
            .with_phrase_min_words(4)
            .with_max_examples_per_group(0);
        let report: RepeatsReport = app.analysis().repeats(&query).unwrap();
        let group = report
            .groups
            .iter()
            .find(|group| group.repeat == "please run the tests before")
            .unwrap_or_else(|| panic!("{report:#?}"));
        assert_eq!((group.matches, group.sessions), (2, 2));
        let first = &group.examples[0];
        assert_eq!(
            (first.session_id.as_str(), first.seq),
            ("claude:consumer-a", 0)
        );
        assert!(first.timestamp.is_some());
        assert!(first.preview.starts_with("you skipped the migration step"));
        assert_eq!(report.unattributed_input_count, 1);
        assert!(!report.unattributed_input_included);

        let included = app
            .analysis()
            .repeats(&query.clone().with_include_unattributed_input(true))
            .unwrap();
        let group = included
            .groups
            .iter()
            .find(|group| group.repeat == "please run the tests before")
            .unwrap();
        assert_eq!(group.matches, 3);
        assert_eq!(included.unattributed_input_count, 1);
        assert!(included.unattributed_input_included);

        let error = app
            .analysis()
            .repeats(&RepeatsQuery::default().with_min_matches(0))
            .unwrap_err();
        assert_eq!(service_error(&error).code(), "invalid-argument");
        let error = app
            .analysis()
            .repeats(
                &RepeatsQuery::default()
                    .with_filters(MessageFilters::default().with_session_id("claude:nope")),
            )
            .unwrap_err();
        assert_eq!(service_error(&error).code(), "session-not-found");
    }

    /// Term frequency is a typed call; it is refused under an allowed-roots access scope because
    /// index-wide counts cannot be limited to the allowed sessions.
    #[test]
    fn vocabulary_returns_typed_term_counts() {
        use ai_session_search::{TermFrequency, VocabularyQuery};
        let fixture = Fixture::new();
        let app = fixture.indexed();
        let terms: Vec<TermFrequency> = app
            .analysis()
            .vocabulary(&VocabularyQuery::default().with_prefix("TEST").with_limit(0))
            .unwrap();
        let tests = terms.iter().find(|row| row.term == "tests").unwrap();
        assert_eq!((tests.docs, tests.count), (4, 4), "{terms:?}");
        let one = app
            .analysis()
            .vocabulary(&VocabularyQuery::default().with_limit(1))
            .unwrap();
        assert_eq!(one.len(), 1);
        let trigrams = app
            .analysis()
            .vocabulary(
                &VocabularyQuery::default()
                    .with_trigram(true)
                    .with_prefix("tes"),
            )
            .unwrap();
        assert!(trigrams
            .iter()
            .all(|row| row.term.starts_with("tes") && row.term.chars().count() == 3));
        drop(app);

        let mut restricted = fixture.config();
        restricted.search.scope.mode = ai_session_search::config::SearchScopeMode::AllowedRoots;
        restricted.search.scope.roots = vec![fixture.root.to_string_lossy().into_owned()];
        let app = SessionSearch::open(restricted).unwrap();
        let error = app
            .analysis()
            .vocabulary(&VocabularyQuery::default())
            .unwrap_err();
        assert!(error.to_string().contains("allowed-roots"), "{error:#}");
    }

    #[test]
    fn setters_set_the_named_fields_and_keep_the_other_defaults() {
        let (sessions, messages, files, inspection, classification, overrides) =
            requests_built_with_setters();
        assert_eq!(sessions.path_prefix.as_deref(), Some("/r"));
        assert_eq!(sessions.session_kinds, Some(vec![SessionKind::User]));
        assert_eq!((sessions.limit, sessions.provider), (5, None));
        assert_eq!(messages.session_id.as_deref(), Some("claude:consumer-a"));
        assert_eq!(
            (messages.seq_from, messages.limit, messages.offset),
            (Some(1), 10, 2)
        );
        assert_eq!(files.pattern.as_deref(), Some("*.py"));
        assert_eq!(
            (files.min_edits, files.limit, files.offset),
            (Some(2), 3, 0)
        );
        assert_eq!(inspection.preview_chars, 80);
        assert!(inspection.include_time_profile);
        assert_eq!(classification.filters.limit, 50);
        assert!(classification.include_unattributed_input);
        assert_eq!(
            overrides.database_path.as_deref(),
            Some(std::path::Path::new("/tmp/index.db"))
        );
        assert_eq!((overrides.threads, overrides.config_path), (Some(2), None));
    }

    #[test]
    fn session_requests_distinguish_configured_limits_from_explicit_all_and_legacy_fields() {
        use ai_session_search::SessionQuery;
        let fixture = Fixture::new();
        std::fs::write(
            fixture.root.join("projects/p/consumer-c.jsonl"),
            transcripts()[0].1.replace("consumer-a", "consumer-c"),
        )
        .unwrap();
        let mut config = fixture.config();
        config.search.default_limit = 2;
        let app = SessionSearch::open(config).unwrap();
        app.index().reindex_report(true).unwrap();
        let omitted = SessionQuery::default();
        assert_eq!(app.catalog().list_query(&omitted).unwrap().len(), 2);
        assert_eq!(
            app.catalog().search_query("tests", &omitted).unwrap().len(),
            2
        );
        let all = SessionQuery::default().with_limit(0);
        assert_eq!(app.catalog().list_query(&all).unwrap().len(), 3);
        assert_eq!(app.catalog().search_query("tests", &all).unwrap().len(), 3);
        let mut legacy = SearchFilters::default();
        assert_eq!(legacy.limit, 0);
        legacy.limit = 1;
        assert_eq!(app.catalog().list_sessions(&legacy).unwrap().len(), 1);
        assert_eq!(app.catalog().search("tests", &legacy).unwrap().len(), 1);
        legacy.limit = 0;
        assert_eq!(app.catalog().list_sessions(&legacy).unwrap().len(), 3);
        let legacy_request = SessionQuery::from_filters(legacy);
        assert_eq!(app.catalog().list_query(&legacy_request).unwrap().len(), 3);
        let configured = legacy_request.with_configured_limit();
        assert_eq!(app.catalog().list_query(&configured).unwrap().len(), 2);
    }

    #[test]
    #[allow(deprecated)] // These callable compatibility paths must survive this release.
    fn api_hygiene_cli_arguments_and_row_traits_are_externally_callable() {
        use ai_session_search::{files, messages, skills, sql_query};
        fn args<T: clap::Args>(tokens: &[&str]) -> T {
            let matches = T::augment_args(clap::Command::new("consumer"))
                .try_get_matches_from(tokens)
                .unwrap();
            T::from_arg_matches(&matches).unwrap()
        }
        let fixture = Fixture::new();
        let config = fixture.config();
        let db = ai_session_search::db::Db::open(&fixture.root.join("direct.db")).unwrap();
        files::run(
            &db,
            &files::FilesCmd::Search(args::<files::FilesSearchArgs>(&[
                "consumer", "*.py", "--format", "json",
            ])),
        )
        .unwrap();
        messages::run(
            &db,
            &messages::MessagesCmd::Search(Box::new(args::<messages::MessageSearchArgs>(&[
                "consumer", "needle", "--format", "json",
            ]))),
            &config,
        )
        .unwrap();
        let _skill_command = skills::SkillsCmd::List(args::<skills::SkillsListArgs>(&[
            "consumer", "--format", "json",
        ]));
        let _skill_adapter: fn(
            &ai_session_search::Config,
            skills::SkillsCmd,
            &std::path::Path,
        ) -> ai_session_search::anyhow::Result<()> = skills::run;
        let query = args::<sql_query::DbQueryArgs>(&["consumer", "select 1"]);
        assert_eq!(query.resolve(&config.db).sql, "select 1");
        let _sql_adapter = sql_query::run;
        assert_eq!(
            <skills::SkillCategoryRow as ai_session_search::render::Row>::headers(),
            &["order", "category", "regex"]
        );
        let _cells: fn(&skills::SkillCategoryRow) -> Vec<String> =
            <skills::SkillCategoryRow as ai_session_search::render::Row>::cells;
    }

    #[test]
    fn purpose_definitions_can_be_constructed_without_deserializing_private_fields() {
        use ai_session_search::config::{PurposeDefinition, SearchOperation};
        let mut purpose = PurposeDefinition::new(
            std::num::NonZeroU32::new(2).unwrap(),
            SearchOperation::MessageSearch,
        );
        assert_eq!(purpose.version.get(), 2);
        assert_eq!(purpose.operation, SearchOperation::MessageSearch);
        assert!(purpose.preferences.default_limit.is_none());
        purpose.preferences.default_limit = std::num::NonZeroUsize::new(3);
        let mut config = ai_session_search::Config::default();
        config.search.purposes.insert("review".into(), purpose);
        assert_eq!(
            config.search.purposes["review"]
                .preferences
                .default_limit
                .unwrap()
                .get(),
            3
        );
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
    #[test]
    fn message_search_names_parse_in_the_core_api() {
        use ai_session_search::{DetailLevel, MessageSearchInclude, ReceiptLevel};
        for (name, expected) in [
            ("compact", DetailLevel::Compact),
            ("full", DetailLevel::Full),
        ] {
            assert_eq!(name.parse::<DetailLevel>().unwrap(), expected);
        }
        for (name, expected) in [
            ("none", ReceiptLevel::None),
            ("summary", ReceiptLevel::Summary),
            ("full", ReceiptLevel::Full),
        ] {
            assert_eq!(name.parse::<ReceiptLevel>().unwrap(), expected);
        }
        for (name, expected) in [
            (
                "normalized_session_metadata",
                MessageSearchInclude::NormalizedSessionMetadata,
            ),
            ("parsed_references", MessageSearchInclude::ParsedReferences),
            (
                "raw_provider_metadata",
                MessageSearchInclude::RawProviderMetadata,
            ),
            (
                "runtime_diagnostics",
                MessageSearchInclude::RuntimeDiagnostics,
            ),
        ] {
            assert_eq!(name.parse::<MessageSearchInclude>().unwrap(), expected);
        }
        assert_eq!(
            "FULL".parse::<DetailLevel>().unwrap_err(),
            "detail must be compact or full; got \"FULL\""
        );
        assert!("bad"
            .parse::<ReceiptLevel>()
            .unwrap_err()
            .contains("receipt_level"));
        assert!("raw-metadata"
            .parse::<MessageSearchInclude>()
            .unwrap_err()
            .contains("accepted values"));
    }
    #[test]
    #[allow(deprecated)] // Preserve callable compatibility paths while steering new consumers to services.
    fn compatibility_storage_and_provider_helpers_remain_callable() {
        use ai_session_search::{db::Db, message_search::MessageSearchResponse, util};
        let _ = Db::counts_by_provider;
        let _ = Db::clear_all;
        let _ = Db::is_file_current;
        let _ = Db::replace_session;
        let _ = Db::messages_fts_count;
        let _ = Db::explain_message_search;
        let _ = Db::file_edits_for;
        let _ = Db::file_edits_for_session_id;
        let _ = Db::file_edit_count;
        let _ = ai_session_search::MessageSearchBatch::into_parts;
        let _ = MessageSearchResponse::into_hits;
        let _ = MessageSearchResponse::into_rows;
        let _ = util::to_messages;
        let _ = util::to_messages_with_tools::<util::RawMessage>;
        let _ = util::to_messages_with_tools_in_scope::<util::RawMessage>;
        let _ = ai_session_search::indexer::auto_reindex;
        let _ = ai_session_search::dates::parse_bound;
        let _ = ai_session_search::dates::parse_span;
        let _ = ai_session_search::search_scope::EffectiveAccessScope::is_unrestricted;
        let _ = ai_session_search::search_scope::EffectiveAccessScope::allows_workspace_path;
        let _ = ai_session_search::mcp_schema_budget::LEDGER_STAGES;
        let _ = ai_session_search::mcp_schema_budget::ceiling_for;
        let _ = ai_session_search::mcp_schema_budget::codex_strips_descriptions;
        let _ = ai_session_search::trigram_index::ensure_schema;
        let _ = ai_session_search::trigram_index::schema_is_compatible;
        let _ = ai_session_search::trigram_index::base_max_id;
        let _ = ai_session_search::trigram_index::build;
        let _ = ai_session_search::trigram_index::build_in_current_transaction;
        let _ = ai_session_search::trigram_index::candidates;
        assert_eq!(
            ai_session_search::trigram::trigram_prefilter_all(["needle"]),
            Some("(\"dle\" AND \"edl\" AND \"eed\" AND \"nee\")".into())
        );
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

#[test]
#[allow(deprecated)]
fn legacy_user_role_authorship_preserves_provider_evidence() {
    use ai_session_search::{
        models::{MessageAuthorship, MessageKind},
        util::{
            apply_user_role_authorship, to_messages_with_tools, RawMessage,
            UserRoleAuthorshipEvidence,
        },
    };
    for (evidence, expected) in [
        (
            UserRoleAuthorshipEvidence::HumanInputEvent,
            MessageAuthorship::Human,
        ),
        (
            UserRoleAuthorshipEvidence::AgentDelegationEvent,
            MessageAuthorship::Agent,
        ),
        (
            UserRoleAuthorshipEvidence::Unverified,
            MessageAuthorship::Unknown,
        ),
    ] {
        let raw = || RawMessage::message("user", "prompt".into(), None, None);
        let mut messages = to_messages_with_tools(vec![raw(), raw(), raw(), raw(), raw()]);
        messages[1].role = ai_session_search::models::Role::Slash;
        messages[2].provenance.authorship = MessageAuthorship::Agent;
        messages[3].kind = MessageKind::HarnessNotice;
        messages[4].role = ai_session_search::models::Role::Assistant;
        let before = messages.clone();
        apply_user_role_authorship(&mut messages, evidence);
        assert_eq!(messages[0].provenance.authorship, expected);
        assert_eq!(messages[1].provenance.authorship, expected);
        for index in 2..messages.len() {
            assert_eq!(
                messages[index].provenance.authorship,
                before[index].provenance.authorship
            );
        }
        let replacement = to_messages_with_tools(vec![raw().with_authorship(expected)]);
        assert_eq!(
            messages[0].provenance.authorship,
            replacement[0].provenance.authorship
        );
    }
}

#[test]
#[allow(deprecated)]
fn published_trigram_helpers_accept_a_rusqlite_0_40_connection() {
    use ai_session_search::{runtime::ExecutionRuntime, trigram_index};
    let connection = rusqlite::Connection::open_in_memory().unwrap();
    trigram_index::ensure_schema(&connection).unwrap();
    assert!(trigram_index::schema_is_compatible(&connection).unwrap());
    assert_eq!(trigram_index::base_max_id(&connection).unwrap(), 0);
    let _build: fn(
        &rusqlite::Connection,
        &ExecutionRuntime,
    ) -> ai_session_search::anyhow::Result<i64> = trigram_index::build;
    let _transaction: fn(
        &rusqlite::Connection,
        &ExecutionRuntime,
    ) -> ai_session_search::anyhow::Result<i64> = trigram_index::build_in_current_transaction;
    assert!(trigram_index::candidates(&connection, &[])
        .unwrap()
        .is_empty());
}
