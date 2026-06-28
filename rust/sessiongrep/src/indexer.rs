use anyhow::Result;

use crate::config::Config;
use crate::db::Db;
use crate::models::{Provider, SourceFile};
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
        // Incremental tail-parse fast path: when we hold a checkpoint for this file, it only
        // grew (offset within it → not truncated), and its head bytes are unchanged (not
        // rewritten/rotated), parse + append ONLY the appended bytes instead of re-reading the
        // whole (possibly multi-hundred-MB) file. Each provider reuses its own `parse_reader`
        // over the appended slice; on any doubt it returns `FullParse` and we re-read below.
        if !full {
            let outcome = match source.provider {
                Provider::Claude => {
                    try_tail(source, &source_path, db, |r, p| claude.parse_reader(r, p))?
                }
                Provider::Codex => {
                    try_tail(source, &source_path, db, |r, p| codex.parse_reader(r, p))?
                }
                Provider::Cursor => {
                    try_tail(source, &source_path, db, |r, p| cursor.parse_reader(r, p))?
                }
                Provider::Antigravity => try_tail(source, &source_path, db, |r, p| {
                    antigravity.parse_reader(r, p)
                })?,
                Provider::Pi => try_tail(source, &source_path, db, |r, p| pi.parse_reader(r, p))?,
            };
            match outcome {
                TailOutcome::Appended => {
                    updated += 1;
                    if let Some(cb) = progress.as_deref_mut() {
                        cb(i + 1, total, updated);
                    }
                    continue;
                }
                TailOutcome::NothingNew => continue,
                TailOutcome::FullParse => {}
            }
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
        // Record/refresh the tail checkpoint so the next reindex of this grown file can append
        // incrementally from the end of what we just parsed (instead of re-reading it all).
        let offset = crate::tail::complete_prefix_offset(&source.path)?;
        let fingerprint = crate::tail::prefix_fingerprint(&source.path)?;
        db.set_file_checkpoint(source.provider, &source_path, offset, &fingerprint)?;
        updated += 1;
        if let Some(cb) = progress.as_deref_mut() {
            cb(i + 1, total, updated);
        }
    }

    // Fold the WAL back into the main DB after writing, so the `-wal` file does not accumulate
    // across the per-command auto-reindex. Cheap when nothing was written (skip then).
    if updated > 0 {
        // A full reindex deletes+reinserts every row, fragmenting the FTS5 index into many
        // unmerged segments (≈2x on-disk bloat, measured). Merge them back — but ONLY on a full
        // reindex, never on the per-command incremental path, since `optimize` rewrites the whole
        // index. Incremental appends rely on FTS5's own automerge to stay reasonably compact.
        if full {
            db.optimize_fts()?;
        }
        db.checkpoint_truncate()?;
    }

    Ok((total, updated))
}

/// Outcome of an incremental tail-parse attempt for one file.
enum TailOutcome {
    /// New rows were parsed from the appended bytes and appended to the index.
    Appended,
    /// The file grew only by a partially-written (unterminated) line; nothing complete to index
    /// yet — skip it (the next reindex re-checks cheaply once the line is flushed).
    NothingNew,
    /// The fast path is not safe (no checkpoint, truncation, or a rewritten head); the caller
    /// must perform a full parse.
    FullParse,
}

/// Try to incrementally append only the bytes appended to a session file since its last
/// checkpoint, reusing that provider's real parser (`parse_slice`) over the appended slice
/// ([`crate::tail`]). The preconditions (a stored checkpoint, no truncation, an unchanged file
/// head) make this a pure optimization: on any doubt it returns [`TailOutcome::FullParse`] and
/// the caller re-reads the whole file, so correctness never depends on the fast path.
fn try_tail<F>(
    source: &SourceFile,
    source_path: &str,
    db: &Db,
    parse_slice: F,
) -> Result<TailOutcome>
where
    F: Fn(std::io::Cursor<Vec<u8>>, &std::path::Path) -> Result<crate::models::ParsedSession>,
{
    let Some((offset, stored_fingerprint)) = db.file_checkpoint(source.provider, source_path)?
    else {
        return Ok(TailOutcome::FullParse);
    };
    // Truncation / copytruncate: the file is now shorter than where we parsed to → re-read whole.
    if offset <= 0 || source.size_bytes < offset {
        return Ok(TailOutcome::FullParse);
    }
    // Rewrite / rotation: the head bytes changed → the stored offset is meaningless → re-read.
    if !crate::tail::fingerprint_matches(&source.path, &stored_fingerprint)? {
        return Ok(TailOutcome::FullParse);
    }
    match crate::tail::tail_parse(&source.path, offset, parse_slice) {
        Ok(Some(tail)) => {
            db.append_tail(&tail, source.mtime_ns, source.size_bytes)?;
            Ok(TailOutcome::Appended)
        }
        Ok(None) => Ok(TailOutcome::NothingNew),
        // The tail fast path is a pure optimization, so ANY failure parsing the appended slice
        // degrades to a full re-read rather than aborting the reindex (this module's "on any doubt
        // → FullParse" contract). The error is usually a `parse_slice` UTF-8 failure: a tool-output
        // line with embedded binary, or the bounded overlap window beginning mid-character. The
        // full parse re-reads from byte 0 (a clean char boundary) and is itself panic/error-safe
        // via `minimal_record`, so a single bad file never breaks the whole reindex — and thus
        // every read command, which auto-reindexes first.
        Err(_) => Ok(TailOutcome::FullParse),
    }
}
