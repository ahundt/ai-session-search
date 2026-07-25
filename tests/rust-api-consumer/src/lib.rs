//! Compile-only coverage for the supported public Rust API.

use ai_session_search::{
    AnalysisPolicySpec, AnalysisPublicationFormat, AnalysisPublicationPlan,
    AnalysisPublicationReceipt, AnalysisResult, ClassificationRuleSpec, ClassificationTarget,
    ExportFormat, ExportPublicationPlan, FileQuery, InspectionOptions, MessageFilters,
    MessageQuery, MessageSearchMode, MessageSearchRequest, MessageTarget, PhraseTextMode,
    PhraseVocabularyPolicySpec, SearchFilters, SessionKind, SessionSearch,
};

const EXAMPLE_CLASSIFICATION_WINDOW_CHARS: usize = 4_096;

/// Compile representative service composition as an external Rust consumer.
///
/// This function is intentionally not executed: downstream compilation verifies that callers can
/// use public types without gaining access to storage, CLI, MCP, or PyO3 implementation details.
pub fn exercise_public_api(
    app: &SessionSearch,
    recovery_destination: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let sessions = SearchFilters {
        provider: None,
        path_prefix: None,
        exclude_path_prefixes: Vec::new(),
        exclude_session_ids: Vec::new(),
        // A downstream consumer can select session classes and follow the spawn link, both
        // through public types: `SessionKind` and the parent's own session id.
        session_kinds: Some(vec![SessionKind::User, SessionKind::Subagent]),
        parent_session_id: None,
        since: None,
        until: None,
        limit: 20,
        warnings_only: false,
    };
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
    let _ = app.messages().search(message_request)?;
    let analysis = app.analysis();
    let page = analysis.documents(&sessions, None)?;
    let _ = page.next_cursor.as_ref().map(|cursor| cursor.as_str());
    let _ = analysis.corrections(&message_filters)?;
    let _ = analysis.planning(&message_filters, &[])?;
    let _ = analysis.role_statistics(&message_filters)?;
    let policy = AnalysisPolicySpec {
        classification_rules: vec![ClassificationRuleSpec {
            dimension: "workflow".into(),
            label: "testing".into(),
            target: ClassificationTarget::UserText,
            pattern: "(?i)\\btest".into(),
            weight: 1,
        }],
        relationship_rules: Vec::new(),
        phrase_vocabulary: Some(PhraseVocabularyPolicySpec {
            widths: vec![1],
            max_unique_phrases: 1_000,
            min_document_tokens: 0,
            excluded_tokens: Vec::new(),
            exclude_numeric_tokens: false,
            text_mode: PhraseTextMode::ProseOnly,
        }),
        max_classification_chars: Some(EXAMPLE_CLASSIFICATION_WINDOW_CHARS),
    }
    .compile()?;
    let analyzed = analysis.run(&sessions, &policy)?;
    let _ = analyzed.session_graph();
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
    result: &AnalysisResult,
    destination: &std::path::Path,
) -> Result<AnalysisPublicationReceipt, Box<dyn std::error::Error + Send + Sync>> {
    let plan = AnalysisPublicationPlan::new(
        destination,
        [
            AnalysisPublicationFormat::Json,
            AnalysisPublicationFormat::Markdown,
        ],
    )?;
    let artifacts = plan.render(result)?;
    for artifact in &artifacts {
        assert!(!artifact.name().is_empty());
        assert_eq!(artifact.bytes(), artifact.content().len());
        assert_eq!(artifact.sha256().len(), 64);
    }
    Ok(plan.publish(result)?)
}
