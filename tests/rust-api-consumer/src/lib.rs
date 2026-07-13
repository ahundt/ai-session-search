//! Compile-only coverage for the supported public Rust API.

use std::num::NonZeroUsize;

use ai_session_search::analysis_pipeline::{
    AnalysisPolicy, ClassificationRuleSpec, ClassificationTarget, PhraseTextMode,
    PhraseVocabularySpec,
};
use ai_session_search::export::ExportFormat;
use ai_session_search::models::{FileQuery, MessageFilters, MessageSearchMode, SearchFilters};
use ai_session_search::service::SessionSearch;

const EXAMPLE_ANALYSIS_PAGE_SIZE: usize = 10;
const EXAMPLE_MAX_UNIQUE_PHRASES: usize = 1_000;

/// Compile representative service composition as an external Rust consumer.
///
/// This function is intentionally not executed: downstream compilation verifies that callers can
/// use public types without gaining access to storage, CLI, MCP, or PyO3 implementation details.
pub fn exercise_public_api(
    app: &SessionSearch,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let sessions = SearchFilters {
        provider: None,
        path_prefix: None,
        exclude_path_prefixes: Vec::new(),
        exclude_session_ids: Vec::new(),
        since: None,
        until: None,
        limit: 20,
        warnings_only: false,
    };
    let _ = app.catalog().list_sessions(&sessions)?;
    let _ = app.index().status()?;
    let message_filters = MessageFilters::default();
    message_filters.validate("")?;
    let mode = "regex".parse::<MessageSearchMode>()?;
    assert_eq!(mode.as_str(), "regex");
    let _ = app.messages().search("request", &message_filters)?;
    let analysis = app.analysis();
    let page = analysis.documents(&sessions, None)?;
    let _ = page.next_cursor.as_ref().map(|cursor| cursor.as_str());
    let _ = analysis.corrections(&message_filters)?;
    let _ = analysis.planning(&message_filters, &[])?;
    let _ = analysis.role_statistics(&message_filters)?;
    let page_size = NonZeroUsize::new(EXAMPLE_ANALYSIS_PAGE_SIZE)
        .ok_or_else(|| std::io::Error::other("analysis page size must be nonzero"))?;
    let max_phrases = NonZeroUsize::new(EXAMPLE_MAX_UNIQUE_PHRASES)
        .ok_or_else(|| std::io::Error::other("phrase bound must be nonzero"))?;
    let phrase_vocabulary =
        PhraseVocabularySpec::new([NonZeroUsize::MIN], max_phrases, 0, Vec::new(), false)?
            .with_text_mode(PhraseTextMode::ProseOnly);
    let policy = AnalysisPolicy::compile(
        vec![ClassificationRuleSpec {
            dimension: "workflow".into(),
            label: "testing".into(),
            target: ClassificationTarget::UserText,
            pattern: "(?i)\\btest".into(),
            weight: 1,
        }],
        Vec::new(),
    )?
    .with_phrase_vocabulary(phrase_vocabulary)
    .with_max_classification_chars(page_size);
    let analyzed = analysis.run(&sessions, page_size, &policy)?;
    let _ = analyzed.session_graph();
    let _ = app.files().search(&FileQuery::default())?;
    let _ = app.sources().inventory();
    let _ = app.index().refresh()?;
    let _ = app.index().reindex(false)?;
    let _ = app.maintenance().diagnostics()?;
    let _ = app.maintenance().compact()?;
    let format = "markdown".parse::<ExportFormat>()?;
    let _ = app.exports().render_full("provider:session", format)?;
    Ok(())
}
