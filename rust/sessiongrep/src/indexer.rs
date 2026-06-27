use anyhow::Result;

use crate::config::Config;
use crate::db::Db;
use crate::models::Provider;
use crate::providers::{
    antigravity::AntigravityAdapter, claude::ClaudeAdapter, codex::CodexAdapter,
    cursor::CursorAdapter, pi::PiAdapter,
};
use crate::util::normalize_path;

/// Incrementally (or fully) reindex all enabled providers into `db`.
///
/// Returns `(files_seen, sessions_updated)`. When `full` is true every discovered file
/// is re-parsed (bypassing the `(mtime_ns, size_bytes)` skip); otherwise a file is
/// skipped when it already matches what's recorded in `files_seen`, making repeated
/// calls cheap.
///
/// DURABLE ARCHIVE: reindex never deletes. A session whose source file has been removed
/// (e.g. a CLI harness clearing old sessions) is simply not re-visited, so its indexed
/// history is retained — the database is the system of record once data is captured.
/// Re-parsing an existing file upserts in place (idempotent). An explicit full wipe is
/// [`Db::clear_all`] (not part of reindex) or deleting the index file.
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

    let claude = ClaudeAdapter::new(config.claude_paths());
    let codex = CodexAdapter::new(config.codex_paths(), config.codex_home());
    let cursor = CursorAdapter::new(config.cursor_paths());
    let antigravity = AntigravityAdapter::new(config.antigravity_paths());
    let pi = PiAdapter::new(config.pi_paths());

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
    if config.providers.pi.enabled {
        sources.extend(pi.discover());
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
        let mut parsed = match source.provider {
            Provider::Claude => claude.parse(source),
            Provider::Codex => codex.parse(source),
            Provider::Cursor => cursor.parse(source),
            Provider::Antigravity => antigravity.parse(source),
            Provider::Pi => pi.parse(source),
        };
        // Guarantee every session has a date: fall back to the file mtime when the parser
        // found no timestamps (covers all providers + the parse-failure minimal_record).
        crate::util::backfill_session_dates(&mut parsed.session, source.mtime_ns);
        db.upsert_session(&parsed, source.mtime_ns, source.size_bytes)?;
        updated += 1;
        if let Some(cb) = progress.as_deref_mut() {
            cb(i + 1, total, updated);
        }
    }

    // Fold the WAL back into the main DB after writing, so the `-wal` file does not accumulate
    // across the per-command auto-reindex. Cheap when nothing was written (skip then).
    if updated > 0 {
        db.checkpoint_truncate()?;
    }

    Ok((total, updated))
}
