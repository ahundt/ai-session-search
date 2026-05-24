use anyhow::Result;

use crate::config::Config;
use crate::db::Db;
use crate::models::Provider;
use crate::providers::{
    antigravity::AntigravityAdapter, claude::ClaudeAdapter, codex::CodexAdapter,
    cursor::CursorAdapter,
};
use crate::util::normalize_path;

/// Incrementally (or fully) reindex all enabled providers into `db`.
///
/// Returns `(files_seen, sessions_updated)`. When `full` is true the database is
/// cleared first and every discovered file is re-parsed. Otherwise each file is
/// skipped when its `(mtime_ns, size_bytes)` already matches what's recorded in
/// `files_seen`, making repeated calls cheap.
///
/// When `progress` is provided it's invoked with `(index, total, updated)` after
/// each updated file so callers can render progress; the CLI uses this and the
/// MCP server passes `None`.
pub fn reindex(
    config: &Config,
    db: &Db,
    full: bool,
    progress: Option<&mut dyn FnMut(usize, usize, usize)>,
) -> Result<(usize, usize)> {
    if full {
        db.clear_all()?;
    }

    let claude = ClaudeAdapter::new(config.claude_paths());
    let codex = CodexAdapter::new(config.codex_paths(), config.codex_home());
    let cursor = CursorAdapter::new(config.cursor_paths());
    let antigravity = AntigravityAdapter::new(config.antigravity_paths());

    let mut sources = Vec::new();
    if config.providers.claude.enabled {
        sources.extend(claude.discover());
    }
    if config.providers.codex.enabled {
        sources.extend(codex.discover());
    }
    if config.providers.cursor.enabled {
        sources.extend(cursor.discover());
    }
    if config.providers.antigravity.enabled {
        sources.extend(antigravity.discover());
    }

    let total = sources.len();
    let mut updated = 0usize;
    let mut progress = progress;
    for (i, source) in sources.iter().enumerate() {
        let source_path = normalize_path(&source.path);
        if !full
            && db.is_file_current(
                source.provider,
                &source_path,
                source.mtime_ns,
                source.size_bytes,
            )?
        {
            continue;
        }
        let parsed = match source.provider {
            Provider::Claude => claude.parse(source),
            Provider::Codex => codex.parse(source),
            Provider::Cursor => cursor.parse(source),
            Provider::Antigravity => antigravity.parse(source),
        };
        db.upsert_session(&parsed, source.mtime_ns, source.size_bytes)?;
        updated += 1;
        if let Some(cb) = progress.as_deref_mut() {
            cb(i + 1, total, updated);
        }
    }

    Ok((total, updated))
}
