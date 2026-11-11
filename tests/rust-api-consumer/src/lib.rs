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

use ai_session_search::{
    AnalysisPolicySpec, AnalysisPublicationFormat, AnalysisPublicationPlan,
    AnalysisPublicationReceipt, AnalysisRequest, AnalysisSessionSelection, CapabilityReceipt,
    ClassificationRuleSpec, ClassificationTarget, ExportFormat, ExportPublicationPlan, FileQuery,
    InspectionOptions, MessageClassificationMatch, MessageClassificationQuery,
    MessageClassificationReport, MessageFilters, MessageMatchViewMarkers, MessageQuery,
    MessageSearchMode, MessageSearchRequest, MessageTarget, PhraseTextMode,
    PhraseVocabularyPolicySpec, ReceiptedAnalysis, SearchFilters, SessionKind, SessionSearch,
    SkillCapabilityInput, SkillCapabilityOutput, SkillRunQuery, SkillSelector,
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

// Compile the Rust examples in the repository README and the library guide. They live outside the
// published crate, so this unpublished crate owns the check.
#[cfg(doctest)]
#[doc = include_str!("../../../README.md")]
struct RepositoryReadmeExamples;

#[cfg(doctest)]
#[doc = include_str!("../../../docs/development/library-api.md")]
struct LibraryGuideExamples;
