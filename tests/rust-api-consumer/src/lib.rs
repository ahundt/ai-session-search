//! Compile-only coverage for the supported public Rust API.

use ai_session_search::export::ExportFormat;
use ai_session_search::models::{FileQuery, MessageFilters, MessageSearchMode, SearchFilters};
use ai_session_search::service::SessionSearch;

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
    let _ = app.catalog().index_status()?;
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
