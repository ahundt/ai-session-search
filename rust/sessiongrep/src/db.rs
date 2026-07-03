use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as NucleoConfig, Matcher as NucleoMatcher, Utf32Str};
use rayon::prelude::*;
use rusqlite::{params, Connection, ErrorCode, OptionalExtension};

use crate::models::{
    CorrectionMatch, EditOp, FileCrossRef, FileEdit, FileEditSummary, FileQuery, MessageFilters,
    MessageHit, ParsedSession, PlanningCount, Provider, Role, SearchExplain, SearchFilters,
    SearchHit, SessionRecord, SessionWithTranscript,
};
use crate::util::snippet_from_match;

/// On-disk index generation (NOT the package version). This release INTRODUCES index versioning:
/// the upstream session-only release never set SQLite's `pragma user_version`, so any pre-existing
/// index reads as `0`. [`Db::needs_backfill`] compares this constant against `user_version` to
/// trigger a one-time full reindex after an upgrade, without re-parsing on every run. Bump by
/// exactly 1 in a future release whenever a schema/parse change requires existing indexes to be
/// re-parsed; an upgrading user then reindexes once.
///
///   1: message-level index — the first versioned schema, layered over the upstream session-only
///      schema (`sessions` + `transcripts` + `sessions_fts`). It adds the per-message `messages`
///      table (normalized role / `tool_name` / ts / compaction across all providers) with its
///      `messages_fts` word index and the custom, parallel-built [`crate::trigram_index`]
///      substring/regex prefilter (`trigram_postings` / `trigram_meta`), plus the `file_edits`
///      table behind file-version recovery (`files …`). The parser excludes harness-injected
///      output from the `user` role (claude `<local-command-*>`, codex `<environment_context>`). An
///      upstream index is at `user_version = 0 < 1`, so the first run does a single full reindex to
///      populate the message-level schema, then stamps `user_version = 1`; the trigram base then
///      builds lazily on first regex use (no per-row trigram work during reindex).
pub const SCHEMA_VERSION: i64 = 1;

/// Minimum number of FTS candidate sessions to retrieve before fuzzy re-ranking. The
/// candidate set is re-scored in [`Db::search`], so it must be wider than the final
/// `--limit` (a strong fuzzy match can rank low under raw FTS `rank`), and it must never
/// collapse to 0 when a caller requests "unlimited" (limit == 0).
/// Default lower bound on the FTS candidate-set size (see [`crate::config::ScoringConfig`],
/// whose `fts_candidate_floor` defaults to this).
pub const FTS_CANDIDATE_FLOOR: usize = 200;

/// Corpus-size threshold below which a regex `messages search` skips the trigram prefilter and
/// scans the structurally-filtered rows directly. The prefilter's win is amortized over a large
/// corpus; once a role/session/time/tool filter narrows the scan to a small slice, a direct regex
/// pass over that slice beats intersecting it against the whole-corpus trigram index. Heuristic:
/// the real corpus is ~628k messages, while `role='user'` is ~7.7k — 50k cleanly separates the
/// full-corpus (prefilter) case from filtered slices (direct scan). See [`Db::search_messages`].
const TRIGRAM_PREFILTER_MIN_CORPUS: i64 = 50_000;

/// How many newer-than-base messages may accumulate before the custom trigram base index is
/// rebuilt (in parallel). Below this, messages with `id > base_max` form the "delta" and are
/// regex-verified by a direct scan rather than via the index — bounded by the SAME magnitude as
/// [`TRIGRAM_PREFILTER_MIN_CORPUS`] so the un-indexed delta a query may direct-scan stays in the
/// range a direct scan already handles cheaply. See [`Db::ensure_trigram_base`].
const TRIGRAM_BASE_REBUILD_DELTA: i64 = TRIGRAM_PREFILTER_MIN_CORPUS;

/// Default SQLite busy timeout for normal CLI/MCP use. This is intentionally a short wait, not
/// an indefinite block: concurrent agent sessions should ride out brief write bursts, while real
/// stuck maintenance still surfaces as an actionable error.
pub const DEFAULT_BUSY_TIMEOUT_MS: u64 = 5_000;

/// Automatic read-command refreshes use a separate stale-read fallback timeout: wait long enough
/// for ordinary writer handoffs, then serve the existing index if another process is still writing.
pub const DEFAULT_AUTO_REINDEX_BUSY_TIMEOUT_MS: u64 = 10_000;

/// Shared cross-process window after a successful automatic refresh where later read commands skip
/// auto-reindex and stay read-only. This replaces the old MCP-only in-process throttle.
pub const DEFAULT_AUTO_REINDEX_INTERVAL_MS: u64 = 1_500;

/// Caller-injected sink for human-facing progress notices (see [`Db::set_progress_reporter`]).
type ProgressReporter = Box<dyn Fn(&str)>;

const AUTO_REINDEX_COMPLETED_MS_KEY: &str = "auto_reindex_completed_ms";

struct TrigramRebuild {
    base_max: i64,
    rebuilt: bool,
}

fn elapsed_ms(now_ms: i64, earlier_ms: i64) -> u64 {
    now_ms.saturating_sub(earlier_ms).max(0) as u64
}

pub struct Db {
    conn: Connection,
    /// Corpus-size threshold for the regex prefilter (default [`TRIGRAM_PREFILTER_MIN_CORPUS`],
    /// overridable via `[performance] regex_prefilter_min_corpus`).
    prefilter_min_corpus: i64,
    /// Un-indexed delta size before the trigram base is rebuilt (default
    /// [`TRIGRAM_BASE_REBUILD_DELTA`], overridable via `[performance] trigram_rebuild_delta`).
    trigram_rebuild_delta: i64,
    /// Optional sink for human-facing progress notices (e.g. the one-time lazy index build). The
    /// library NEVER writes to stderr/stdout itself — the caller injects how (or whether) to report:
    /// the CLI sets an `eprintln` sink, the MCP server leaves it unset (silent, so nothing can
    /// pollute its stdio JSON-RPC channel). Mirrors the indexer's `progress` callback.
    progress: Option<ProgressReporter>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_busy_timeout(path, DEFAULT_BUSY_TIMEOUT_MS)
    }

    pub fn open_with_busy_timeout(path: &Path, busy_timeout_ms: u64) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_millis(busy_timeout_ms))?;
        let db = Self {
            conn,
            prefilter_min_corpus: TRIGRAM_PREFILTER_MIN_CORPUS,
            trigram_rebuild_delta: TRIGRAM_BASE_REBUILD_DELTA,
            progress: None,
        };
        db.init()?;
        Ok(db)
    }

    pub fn set_busy_timeout_ms(&self, busy_timeout_ms: u64) -> Result<()> {
        self.conn
            .busy_timeout(Duration::from_millis(busy_timeout_ms))?;
        Ok(())
    }

    pub fn busy_timeout_ms(&self) -> Result<u64> {
        let timeout: i64 = self
            .conn
            .query_row("pragma busy_timeout", [], |row| row.get(0))?;
        Ok(timeout.max(0) as u64)
    }

    pub fn with_busy_timeout_ms<T>(
        &self,
        busy_timeout_ms: u64,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let original = self.busy_timeout_ms()?;
        self.set_busy_timeout_ms(busy_timeout_ms)?;
        let result = f();
        let restore = self.set_busy_timeout_ms(original);
        match (result, restore) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(err), Ok(())) => Err(err),
            (Ok(_), Err(err)) => Err(err),
            (Err(err), Err(_restore_err)) => Err(err),
        }
    }

    fn with_immediate_transaction<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        self.conn.execute_batch("begin immediate")?;
        let result = f();
        match result {
            Ok(value) => {
                self.conn.execute_batch("commit")?;
                Ok(value)
            }
            Err(err) => {
                let _ = self.conn.execute_batch("rollback");
                Err(err)
            }
        }
    }

    pub fn is_sqlite_busy_error(err: &anyhow::Error) -> bool {
        err.chain().any(|source| {
            source
                .downcast_ref::<rusqlite::Error>()
                .and_then(rusqlite::Error::sqlite_error_code)
                .is_some_and(|code| {
                    matches!(code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
                })
        })
    }

    pub fn auto_reindex_is_fresh(&self, interval_ms: u64) -> Result<bool> {
        self.auto_reindex_is_fresh_at(Utc::now().timestamp_millis(), interval_ms)
    }

    fn auto_reindex_is_fresh_at(&self, now_ms: i64, interval_ms: u64) -> Result<bool> {
        Ok(self
            .index_metadata_i64(AUTO_REINDEX_COMPLETED_MS_KEY)?
            .is_some_and(|completed_ms| elapsed_ms(now_ms, completed_ms) < interval_ms))
    }

    pub fn mark_auto_reindex_complete(&self) -> Result<()> {
        self.mark_auto_reindex_complete_at(Utc::now().timestamp_millis())
    }

    pub fn auto_reindex_completed_at(&self) -> Result<Option<DateTime<Utc>>> {
        Ok(self
            .index_metadata_i64(AUTO_REINDEX_COMPLETED_MS_KEY)?
            .and_then(|value| Utc.timestamp_millis_opt(value).single()))
    }

    fn mark_auto_reindex_complete_at(&self, now_ms: i64) -> Result<()> {
        self.set_index_metadata_i64(AUTO_REINDEX_COMPLETED_MS_KEY, now_ms)
    }

    fn index_metadata_i64(&self, key: &str) -> Result<Option<i64>> {
        self.conn
            .query_row(
                "select value from index_metadata where key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    fn set_index_metadata_i64(&self, key: &str, value: i64) -> Result<()> {
        self.conn.execute(
            "insert into index_metadata (key, value) values (?1, ?2)
             on conflict(key) do update set value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Apply user performance overrides ([`crate::config::PerformanceConfig`]) to this connection.
    /// A field value of `0` keeps the built-in default. Call once after [`Db::open`].
    pub fn apply_performance_config(&mut self, perf: &crate::config::PerformanceConfig) {
        if perf.regex_prefilter_min_corpus > 0 {
            self.prefilter_min_corpus = perf.regex_prefilter_min_corpus as i64;
        }
        if perf.trigram_rebuild_delta > 0 {
            self.trigram_rebuild_delta = perf.trigram_rebuild_delta as i64;
        }
    }

    /// Inject a sink for human-facing progress notices (e.g. the one-time lazy trigram-index build).
    /// Lets a terminal frontend report progress without the library hardcoding stderr; leave it unset
    /// for silent operation (the MCP server, tests). Call once after [`Db::open`].
    pub fn set_progress_reporter(&mut self, reporter: impl Fn(&str) + 'static) {
        self.progress = Some(Box::new(reporter));
    }

    /// Emit a progress notice to the injected sink, if any (no-op otherwise).
    fn report_progress(&self, message: &str) {
        if let Some(reporter) = &self.progress {
            reporter(message);
        }
    }

    fn init(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            pragma journal_mode = wal;
            pragma foreign_keys = on;
            -- Read-perf tuning for the analytics/search workload over a multi-GB index:
            -- queries like `corrections` fetch thousands of message rows by rowid (the
            -- role/ts index doesn't cover `content`), which is random I/O across the file.
            -- A larger page cache + memory-mapped reads + in-memory temp store cut that
            -- cost; synchronous=normal is the documented-safe durability level under WAL.
            pragma synchronous = normal;
            pragma temp_store = memory;
            pragma cache_size = -65536;
            pragma mmap_size = 268435456;
            create table if not exists sessions (
                id text primary key,
                provider text not null,
                provider_session_id text not null,
                title text,
                summary text,
                cwd text,
                repo_root text,
                created_at text,
                updated_at text,
                last_message_at text,
                preview_text text not null,
                source_path text not null,
                message_count integer,
                parse_version text not null,
                raw_metadata_json text,
                parse_warning text,
                discovery_source text not null
            );
            create table if not exists transcripts (
                session_id text primary key references sessions(id) on delete cascade,
                transcript_text text not null
            );
            create table if not exists files_seen (
                provider text not null,
                source_path text not null,
                mtime_ns integer not null,
                size_bytes integer not null,
                last_indexed_at text not null,
                content_hash text,
                -- Incremental tail-parse checkpoint (§7): byte offset (at a newline boundary)
                -- up to which the file is parsed, and a fingerprint of the file's leading bytes
                -- used to detect rewrite/rotation. NULL = no checkpoint → always a full parse.
                tail_byte_offset integer,
                prefix_fingerprint text,
                primary key(provider, source_path)
            );
            create table if not exists index_metadata (
                key text primary key,
                value integer not null
            );
            create index if not exists idx_sessions_provider on sessions(provider);
            create index if not exists idx_sessions_updated_at on sessions(updated_at desc);
            create index if not exists idx_sessions_provider_id on sessions(provider_session_id);
            create table if not exists messages (
                id integer primary key,
                session_id text not null references sessions(id) on delete cascade,
                provider text not null,
                seq integer not null,
                role text not null,
                ts text,
                tool_name text,
                is_compaction integer not null default 0,
                content text not null
            );
            -- Bare ts index for date-range message filters that span all roles; the
            -- composites below lead with role/session_id and so cannot serve a bare
            -- `ts between ? and ?` scan.
            create index if not exists idx_messages_ts on messages(ts);
            -- Composite indexes serve the hot filter + ORDER BY combinations straight from
            -- the index (no temp B-tree sort) and, by leftmost-prefix, subsume a bare
            -- (session_id) or (role) index: (session_id, seq) covers `where session_id=?`
            -- [+ `order by seq`] (message search / get / context); (role, ts) covers
            -- `where role=?` [+ `order by ts`] (corrections / planning / stats). Older
            -- branch builds created standalone (session_id) and (role) indexes before these
            -- composites existed — drop them so every index converges on this final shape;
            -- they were pure write-amplification (an upstream index never had them).
            drop index if exists idx_messages_session;
            drop index if exists idx_messages_role;
            create index if not exists idx_messages_session_seq on messages(session_id, seq);
            create index if not exists idx_messages_role_ts on messages(role, ts);
            create table if not exists file_edits (
                id integer primary key,
                session_id text not null references sessions(id) on delete cascade,
                provider text not null,
                seq integer not null,
                ts text,
                tool text not null,
                file_path text not null,
                file_name text not null,
                new_content text,
                edits_json text
            );
            create index if not exists idx_file_edits_session on file_edits(session_id);
            create index if not exists idx_file_edits_path on file_edits(file_path);
            create index if not exists idx_file_edits_name on file_edits(file_name);
            ",
        )?;
        // Migrate: drop old contentless FTS table if present, then create regular FTS table
        let fts_sql: Option<String> = self
            .conn
            .query_row(
                "select sql from sqlite_master where type='table' and name='sessions_fts'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if fts_sql.as_ref().is_some_and(|sql| sql.contains("content=")) {
            self.conn.execute_batch("drop table sessions_fts")?;
        }
        self.conn.execute_batch(
            "create virtual table if not exists sessions_fts using fts5(
                title, summary, preview_text, transcript_text
            )",
        )?;
        // External-content FTS over message bodies. Message search no longer lets FTS tokenization
        // define literal semantics, but this index is still kept current for vocabulary,
        // compatibility, and any future explicit word-search surface. The insert/delete/update
        // triggers are the full canonical FTS5 external-content set, written in the
        // `'delete'`-command form that AFTER triggers require (it passes the OLD content so
        // the right tokens are removed; a plain `delete from` here risks index corruption).
        // So every mutation path stays in sync: the delete+reinsert reindex path
        // (upsert_session) and any future in-place `update messages set content = ...`.
        self.conn.execute_batch(
            "create virtual table if not exists messages_fts
                using fts5(content, content='messages', content_rowid='id');
             create trigger if not exists messages_ai after insert on messages begin
                 insert into messages_fts(rowid, content) values (new.id, new.content);
             end;
             create trigger if not exists messages_ad after delete on messages begin
                 insert into messages_fts(messages_fts, rowid, content)
                 values ('delete', old.id, old.content);
             end;
             create trigger if not exists messages_au after update on messages begin
                 insert into messages_fts(messages_fts, rowid, content)
                 values ('delete', old.id, old.content);
                 insert into messages_fts(rowid, content) values (new.id, new.content);
             end;",
        )?;
        // Backfill messages_fts if messages exist but its index is empty — e.g. an index
        // file from before messages_fts existed, or one whose FTS shadow was cleared. FTS5
        // triggers only maintain the index for mutations made AFTER they exist, so the
        // `'rebuild'` command is the canonical way to (re)populate an external-content index
        // from its content table. Without this, message search (which queries messages_fts)
        // would silently return nothing for such an index.
        //
        // NOTE: `count(*) from messages_fts` reflects the external CONTENT table (messages),
        // not the index, so it can't detect an empty index. The `_docsize` shadow holds one
        // row per INDEXED document, so its count is the true index population.
        let messages_count: i64 =
            self.conn
                .query_row("select count(*) from messages", [], |row| row.get(0))?;
        let indexed_messages: i64 =
            self.conn
                .query_row("select count(*) from messages_fts_docsize", [], |row| {
                    row.get(0)
                })?;
        if messages_count > 0 && indexed_messages == 0 {
            self.conn
                .execute_batch("insert into messages_fts(messages_fts) values('rebuild')")?;
        }
        // Literal/regex PREFILTER over message content (the Google Code Search trigram technique):
        // turns substring and regex-literal anchors into indexed candidate queries that exact
        // literal or Rust regex verification checks afterward. This is the custom, parallel-built
        // [`crate::trigram_index`] — NOT an FTS5 virtual table — because FTS5's trigram tokenizer
        // builds single-threaded inside the one SQLite writer, which is ~80% of a cold build
        // (measured ~145 s for 1.8 GB of content). The custom index tokenizes with Rayon and
        // bulk-loads compact delta-varint postings: ~5x faster build, same on-disk size, sub-3 ms
        // candidate queries. It is built LAZILY on first eligible message content search
        // ([`Db::ensure_trigram_base`]), so `reindex` does NO trigram work and
        // `list`/`show`/`paths`/`resume` never pay for it.
        crate::trigram_index::ensure_schema(&self.conn)?;
        // Defensive cleanup: no released version shipped an FTS5 `messages_trigram`, but an
        // in-development index may have one — drop it (+ its sync triggers and fts5vocab view) so
        // the custom index is the sole prefilter. A no-op (`if exists`) on every released index.
        self.conn.execute_batch(
            "drop trigger if exists messages_tri_ai;
             drop trigger if exists messages_tri_ad;
             drop trigger if exists messages_tri_au;
             drop table if exists messages_trigram_vocab;
             drop table if exists messages_trigram;",
        )?;
        // Zero-storage word-term-frequency view (fts5vocab 'row' → term,doc,cnt) for `vocab`.
        // (Trigram vocab is served from the custom index's `trigram_postings.df` column instead.)
        self.conn.execute_batch(
            "create virtual table if not exists messages_vocab
                 using fts5vocab('messages_fts', 'row');",
        )?;
        // Auto-populate FTS if sessions exist but FTS is empty (e.g. after schema upgrade)
        let sessions_count: i64 =
            self.conn
                .query_row("select count(*) from sessions", [], |row| row.get(0))?;
        let fts_count: i64 =
            self.conn
                .query_row("select count(*) from sessions_fts", [], |row| row.get(0))?;
        if sessions_count > 0 && fts_count == 0 {
            self.conn.execute(
                "insert into sessions_fts (rowid, title, summary, preview_text, transcript_text)
                 select s.rowid, s.title, s.summary, s.preview_text, coalesce(t.transcript_text, '')
                 from sessions s
                 left join transcripts t on t.session_id = s.id",
                [],
            )?;
        }
        // Evolve an existing `files_seen` (a rebuildable cache) to carry the tail-parse
        // checkpoint columns. `create table if not exists` won't add columns to a table that
        // already exists, so add them idempotently; NULL on existing rows means "no checkpoint"
        // which the indexer treats as a full parse — always safe.
        self.ensure_column("files_seen", "tail_byte_offset", "tail_byte_offset integer")?;
        self.ensure_column(
            "files_seen",
            "prefix_fingerprint",
            "prefix_fingerprint text",
        )?;
        Ok(())
    }

    /// Add `column_decl` to `table` if the column is not already present (idempotent
    /// schema evolution). Used for the `files_seen` cache columns; a no-op once the
    /// column exists, so it is safe to call on every `open`.
    fn ensure_column(&self, table: &str, column: &str, column_decl: &str) -> Result<()> {
        let present = self
            .conn
            .prepare(&format!("pragma table_info({table})"))?
            .query_map([], |row| row.get::<_, String>(1))?
            .filter_map(Result::ok)
            .any(|name| name == column);
        if !present {
            self.conn
                .execute_batch(&format!("alter table {table} add column {column_decl}"))?;
        }
        Ok(())
    }

    /// Ensure the custom [`crate::trigram_index`] base is built and current enough to serve as the
    /// regex/substring prefilter, returning its `base_max_id`. Builds (in parallel) when the base
    /// is empty or the un-indexed delta (`max(id) - base_max`) exceeds
    /// [`TRIGRAM_BASE_REBUILD_DELTA`]; otherwise the recent delta is left for the caller to
    /// direct-scan (`id > base_max`). This makes the one-time parallel build lazy — paid on first
    /// regex use, never by `list`/`show`/`paths`/`resume` — and keeps incremental reindex free of
    /// trigram work (no triggers): new messages just accumulate in the delta until a rebuild.
    pub fn ensure_trigram_base(&self) -> Result<i64> {
        let base_max = crate::trigram_index::base_max_id(&self.conn)?;
        let max_id: i64 =
            self.conn
                .query_row("select coalesce(max(id), 0) from messages", [], |row| {
                    row.get(0)
                })?;
        if (base_max == 0 && max_id > 0) || (max_id - base_max) > self.trigram_rebuild_delta {
            return match self.rebuild_trigram_base_with_writer_lock() {
                Ok(rebuild) => Ok(rebuild.base_max),
                Err(err) if Self::is_sqlite_busy_error(&err) => {
                    self.report_progress(
                        "substring/regex search index is already being updated; scanning the unindexed delta directly",
                    );
                    Ok(base_max)
                }
                Err(err) => Err(err),
            };
        }
        Ok(base_max)
    }

    fn rebuild_trigram_base_with_writer_lock(&self) -> Result<TrigramRebuild> {
        self.with_immediate_transaction(|| {
            let base_max = crate::trigram_index::base_max_id(&self.conn)?;
            let max_id: i64 =
                self.conn
                    .query_row("select coalesce(max(id), 0) from messages", [], |row| {
                        row.get(0)
                    })?;
            if !((base_max == 0 && max_id > 0) || (max_id - base_max) > self.trigram_rebuild_delta)
            {
                return Ok(TrigramRebuild {
                    base_max,
                    rebuilt: false,
                });
            }

            // The one-time parallel build can take tens of seconds on a large corpus; notify via
            // the injected progress sink (the CLI prints it; the MCP server stays silent) so a first
            // regex/substring search isn't an unexplained pause. Holding BEGIN IMMEDIATE here is a
            // deliberate maintenance lock: readers keep working in WAL mode, while competing
            // writers/builders wait or fall back according to their configured busy timeout.
            let count: i64 = self
                .conn
                .query_row("select count(*) from messages", [], |row| row.get(0))?;
            self.report_progress(&format!(
                "building substring/regex search index in parallel (one-time over {count} messages)…"
            ));
            let base_max = crate::trigram_index::build_in_current_transaction(&self.conn)?;
            Ok(TrigramRebuild {
                base_max,
                rebuilt: true,
            })
        })
        .and_then(|rebuild| {
            if rebuild.rebuilt {
                // Fold the large build out of the WAL so the -wal file doesn't retain the index size.
                self.checkpoint_truncate()?;
            }
            Ok(rebuild)
        })
    }

    /// Stage a regex prefilter's candidate row ids into the per-connection temp table
    /// `_trigram_cand`: the base candidates (`id <= base_max`) PLUS the un-indexed delta
    /// (`id > base_max`), which the caller's Rust regex then re-verifies. The caller joins
    /// `_trigram_cand` to restrict the scan. Temp tables are per-connection, so this is safe for
    /// the one-connection-per-command CLI and the single-connection MCP server.
    fn stage_candidates(
        &self,
        base_max: i64,
        candidates: &std::collections::HashSet<i64>,
    ) -> Result<()> {
        self.conn.execute_batch(
            "create temp table if not exists _trigram_cand (id integer primary key);
             delete from _trigram_cand;",
        )?;
        // Insert all candidates in ONE transaction: an unselective pattern can yield tens of
        // thousands of ids, and per-statement auto-commits would add needless overhead. The temp
        // table lives in memory (temp_store=memory), so this is a single in-memory batch.
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare("insert or ignore into _trigram_cand(id) values (?1)")?;
            for id in candidates {
                stmt.execute([id])?;
            }
        }
        tx.execute(
            "insert or ignore into _trigram_cand(id) select id from messages where id > ?1",
            [base_max],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Run a TRUNCATE WAL checkpoint: fold the write-ahead log back into the main database file
    /// and shrink `-wal` to zero. Cheap when the WAL is small (a no-op-ish), worth calling after
    /// large writes (the trigram rebuild, a big reindex) so the `-wal` file does not accumulate
    /// gigabytes. Best-effort: a concurrent reader can leave it partial, which is harmless.
    pub fn checkpoint_truncate(&self) -> Result<()> {
        // `wal_checkpoint` returns a row (busy, log, checkpointed); ignore it.
        self.conn
            .query_row("pragma wal_checkpoint(truncate)", [], |_| Ok(()))
            .or_else(|err| match err {
                rusqlite::Error::QueryReturnedNoRows => Ok(()),
                other => Err(other),
            })?;
        Ok(())
    }

    /// Merge each FTS5 index's b-tree segments into one (the `'optimize'` command). A full reindex
    /// deletes and reinserts every message, which leaves `messages_fts` with many unmerged segments
    /// — measured to roughly DOUBLE its on-disk size (≈1.0 GB → ≈2.0 GB on a 637k-message corpus)
    /// and to slow queries. `'optimize'` merges them, freeing the redundant pages (reused by later
    /// writes, or returned to the OS by a one-time `VACUUM`). Call ONLY after a full reindex: it
    /// rewrites the whole index, so it must never run on the per-command incremental path. Cheap for
    /// the tiny `sessions_fts`; the cost is in `messages_fts`, amortized over a rare full rebuild.
    pub fn optimize_fts(&self) -> Result<()> {
        self.conn
            .execute_batch("insert into messages_fts(messages_fts) values('optimize');")?;
        self.conn
            .execute_batch("insert into sessions_fts(sessions_fts) values('optimize');")?;
        Ok(())
    }

    /// Reclaim free pages to the OS by rewriting the database file (`VACUUM`). Run AFTER
    /// [`Db::optimize_fts`]: VACUUM repacks page bytes but does NOT merge FTS5 segments, so optimize
    /// must logically compact the index first (the documented OPTIMIZE → VACUUM order). VACUUM takes
    /// an exclusive lock and needs up to ~2x the database size in free disk while it runs, and cannot
    /// run inside a transaction — `execute_batch` runs it in autocommit.
    pub fn vacuum(&self) -> Result<()> {
        self.conn.execute_batch("vacuum")?;
        Ok(())
    }

    /// True when the on-disk `user_version` is behind [`SCHEMA_VERSION`], i.e. a new
    /// schema generation has shipped and a one-time full reindex is needed to backfill
    /// new tables/columns (the old rows were skipped by incremental indexing).
    pub fn needs_backfill(&self) -> Result<bool> {
        let version: i64 = self
            .conn
            .query_row("pragma user_version", [], |row| row.get(0))?;
        Ok(version < SCHEMA_VERSION)
    }

    /// Stamp the on-disk `user_version` to [`SCHEMA_VERSION`] after a full reindex, so
    /// subsequent runs take the fast incremental path.
    pub fn mark_schema_current(&self) -> Result<()> {
        self.conn
            .execute_batch(&format!("pragma user_version = {SCHEMA_VERSION}"))?;
        Ok(())
    }

    /// Explicit, total wipe of all indexed data. NOT used by [`crate::indexer::reindex`],
    /// which is a durable archive (it never deletes sessions whose source files were
    /// removed). This is the deliberate "start over" reset for embedders / corruption
    /// recovery; the user-facing equivalent is deleting the index file.
    pub fn clear_all(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            delete from sessions_fts;
            delete from transcripts;
            delete from messages;
            delete from file_edits;
            delete from sessions;
            delete from files_seen;
            ",
        )?;
        Ok(())
    }

    pub fn clear_trigram_base(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            delete from trigram_postings;
            delete from trigram_meta;
            ",
        )?;
        Ok(())
    }

    pub fn is_file_current(
        &self,
        provider: Provider,
        path: &str,
        mtime_ns: i64,
        size: i64,
    ) -> Result<bool> {
        let result = self
            .conn
            .query_row(
                "select mtime_ns, size_bytes from files_seen where provider = ?1 and source_path = ?2",
                params![provider.as_str(), path],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        Ok(
            matches!(result, Some((stored_mtime, stored_size)) if stored_mtime == mtime_ns && stored_size == size),
        )
    }

    pub fn source_parse_version_is_current(
        &self,
        provider: Provider,
        path: &str,
        parse_version: &str,
    ) -> Result<bool> {
        let stored: Option<String> = self
            .conn
            .query_row(
                "select parse_version from sessions where provider = ?1 and source_path = ?2",
                params![provider.as_str(), path],
                |row| row.get(0),
            )
            .optional()?;
        Ok(stored.as_deref() == Some(parse_version))
    }

    pub fn upsert_session(
        &self,
        parsed: &ParsedSession,
        mtime_ns: i64,
        size_bytes: i64,
    ) -> Result<()> {
        self.upsert_session_with_mode(parsed, mtime_ns, size_bytes, true)
    }

    /// Persist a fully re-parsed session and force message/file rows to be replaced, even
    /// when the new parse appears to be an append-only growth of the old rows. Use this
    /// for explicit full reindex/backfill paths so parser/schema fixes repair existing
    /// rows instead of preserving a stale prefix for performance.
    pub fn replace_session(
        &self,
        parsed: &ParsedSession,
        mtime_ns: i64,
        size_bytes: i64,
    ) -> Result<()> {
        self.upsert_session_with_mode(parsed, mtime_ns, size_bytes, false)
    }

    fn upsert_session_with_mode(
        &self,
        parsed: &ParsedSession,
        mtime_ns: i64,
        size_bytes: i64,
        allow_append_optimization: bool,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let session = &parsed.session;
        tx.execute(
            "
            insert into sessions (
                id, provider, provider_session_id, title, summary, cwd, repo_root, created_at,
                updated_at, last_message_at, preview_text, source_path, message_count, parse_version,
                raw_metadata_json, parse_warning, discovery_source
            ) values (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                ?9, ?10, ?11, ?12, ?13, ?14,
                ?15, ?16, ?17
            )
            on conflict(id) do update set
                provider = excluded.provider,
                provider_session_id = excluded.provider_session_id,
                title = excluded.title,
                summary = excluded.summary,
                cwd = excluded.cwd,
                repo_root = excluded.repo_root,
                created_at = excluded.created_at,
                updated_at = excluded.updated_at,
                last_message_at = excluded.last_message_at,
                preview_text = excluded.preview_text,
                source_path = excluded.source_path,
                message_count = excluded.message_count,
                parse_version = excluded.parse_version,
                raw_metadata_json = excluded.raw_metadata_json,
                parse_warning = excluded.parse_warning,
                discovery_source = excluded.discovery_source
            ",
            params![
                session.id,
                session.provider.as_str(),
                session.provider_session_id,
                session.title,
                session.summary,
                session.cwd,
                session.repo_root,
                session.created_at.map(|value| value.to_rfc3339()),
                session.updated_at.map(|value| value.to_rfc3339()),
                session.last_message_at.map(|value| value.to_rfc3339()),
                session.preview_text,
                session.source_path,
                session.message_count,
                session.parse_version,
                session.raw_metadata_json,
                session.parse_warning,
                session.discovery_source,
            ],
        )?;
        tx.execute(
            "
            insert into transcripts (session_id, transcript_text)
            values (?1, ?2)
            on conflict(session_id) do update set transcript_text = excluded.transcript_text
            ",
            params![session.id, parsed.transcript_text],
        )?;
        // Update FTS index: delete old entry then insert new one
        tx.execute(
            "insert or replace into sessions_fts (rowid, title, summary, preview_text, transcript_text)
             values (
                 (select rowid from sessions where id = ?1),
                 ?2, ?3, ?4, ?5
             )",
            params![
                session.id,
                session.title,
                session.summary,
                session.preview_text,
                parsed.transcript_text,
            ],
        )?;
        tx.execute(
            "
            insert into files_seen (provider, source_path, mtime_ns, size_bytes, last_indexed_at, content_hash)
            values (?1, ?2, ?3, ?4, ?5, null)
            on conflict(provider, source_path) do update set
                mtime_ns = excluded.mtime_ns,
                size_bytes = excluded.size_bytes,
                last_indexed_at = excluded.last_indexed_at
            ",
            params![
                session.provider.as_str(),
                session.source_path,
                mtime_ns,
                size_bytes,
                Utc::now().to_rfc3339(),
            ],
        )?;
        // Re-sync per-message rows. Session logs are APPEND-ONLY, so when a re-parse only GREW
        // the message list and the existing rows are an unchanged prefix, insert just the new
        // tail instead of deleting and re-inserting the whole session. Re-inserting every message
        // also re-runs the messages_fts triggers over the entire session, so a
        // delete+insert re-indexed multi-hundred-MB sessions on EVERY incremental reindex — the
        // dominant reindex cost. The boundary check (the last existing message still matches the
        // parse at that seq) guards against in-place rewrites; on any mismatch or shrink we fall
        // back to a full replace. Messages carry seq = parse index, so the appended tail's seqs
        // never collide with the retained prefix.
        let existing_count: i64 = tx.query_row(
            "select count(*) from messages where session_id = ?1",
            params![session.id],
            |row| row.get(0),
        )?;
        let parsed_count = parsed.messages.len() as i64;
        let append_from: Option<usize> =
            if allow_append_optimization && existing_count > 0 && parsed_count > existing_count {
                let boundary = &parsed.messages[(existing_count - 1) as usize];
                let existing_boundary: Option<String> = tx
                    .query_row(
                        "select content from messages where session_id = ?1 and seq = ?2",
                        params![session.id, boundary.seq],
                        |row| row.get(0),
                    )
                    .optional()?;
                (existing_boundary.as_deref() == Some(boundary.content.as_str()))
                    .then_some(existing_count as usize)
            } else {
                None
            };
        let new_messages = match append_from {
            Some(start) => &parsed.messages[start..],
            None => {
                tx.execute(
                    "delete from messages where session_id = ?1",
                    params![session.id],
                )?;
                &parsed.messages[..]
            }
        };
        // Persist messages with their parse-order seq (0..N).
        insert_messages(&tx, session, new_messages.iter().map(|m| (m.seq, m)))?;
        // Re-sync file-edit rows (idempotent, same as messages). `edits` are stored as a
        // JSON array of [old, new] pairs; `new_content` holds full content for Write only.
        tx.execute(
            "delete from file_edits where session_id = ?1",
            params![session.id],
        )?;
        insert_file_edits(&tx, session, parsed.file_edits.iter().map(|e| (e.seq, e)))?;
        tx.commit()?;
        Ok(())
    }

    /// Delete `user`-role messages that are harness-injected output, not prompts — content
    /// leading with `<local-command-stdout>` / `-stderr` / `-caveat` (claude) or
    /// `<environment_context>` (codex). The current parser already excludes these from re-parsed
    /// files, but sessions whose source file was deleted are never re-visited (durable archive), so
    /// their already-indexed injected rows persist; this one-time data purge reaches them. Returns
    /// the number of rows deleted. The `messages_fts` delete trigger keeps the word index in sync;
    /// the custom trigram base is rebuilt lazily on next use. Run during the schema migration (see
    /// cli.rs).
    pub fn purge_injected_messages(&self) -> Result<usize> {
        let deleted = self.conn.execute(
            "delete from messages where role = 'user' and (\
                 content like '<local-command-stdout>%' \
              or content like '<local-command-stderr>%' \
              or content like '<local-command-caveat>%' \
              or content like '<environment_context>%')",
            [],
        )?;
        Ok(deleted)
    }

    /// The stored incremental tail-parse checkpoint for a file: `(tail_byte_offset,
    /// prefix_fingerprint)`. `None` when there is no row or the checkpoint columns are NULL
    /// (an upstream/older index, or a file never parsed on this generation) — the caller then
    /// performs a full parse. See [`crate::tail`] and plan §7.
    pub fn file_checkpoint(
        &self,
        provider: Provider,
        source_path: &str,
    ) -> Result<Option<(i64, String)>> {
        let row = self
            .conn
            .query_row(
                "select tail_byte_offset, prefix_fingerprint from files_seen
                 where provider = ?1 and source_path = ?2",
                params![provider.as_str(), source_path],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .optional()?;
        Ok(match row {
            Some((Some(offset), Some(fingerprint))) => Some((offset, fingerprint)),
            _ => None,
        })
    }

    /// Record/refresh a file's tail-parse checkpoint after a FULL parse, so the next reindex of
    /// the grown file can incrementally append from here. Must run after the `files_seen` row
    /// exists (i.e. after [`Db::upsert_session`]). Updating it on every full parse is what keeps a
    /// stale offset from causing the next tail parse to re-append already-indexed rows.
    pub fn set_file_checkpoint(
        &self,
        provider: Provider,
        source_path: &str,
        tail_byte_offset: i64,
        prefix_fingerprint: &str,
    ) -> Result<()> {
        self.conn.execute(
            "update files_seen set tail_byte_offset = ?3, prefix_fingerprint = ?4
             where provider = ?1 and source_path = ?2",
            params![
                provider.as_str(),
                source_path,
                tail_byte_offset,
                prefix_fingerprint
            ],
        )?;
        Ok(())
    }

    /// Append ONLY the new rows from an incremental tail parse to an already-indexed session, in
    /// one transaction (SQLite makes the checkpoint update atomic with the data). New messages /
    /// file-edits are re-sequenced to continue after the rows already stored, so their seqs match
    /// what a full parse would assign. Immutable session fields (created_at, summary/first-user)
    /// are preserved; updated_at/last_message_at advance only forward; title/preview refresh from
    /// the tail's latest view; cwd fills in if it was NULL; message_count becomes the true count.
    /// The new conversation text is appended to the transcript blob and the session FTS is rebuilt
    /// from the now-current row. The messages_fts/trigram triggers index the new message rows
    /// automatically. See [`crate::tail`] and plan §7.
    pub fn append_tail(
        &self,
        tail: &crate::tail::TailParse,
        mtime_ns: i64,
        size_bytes: i64,
    ) -> Result<()> {
        let session = &tail.session;
        let tx = self.conn.unchecked_transaction()?;

        // New messages, re-sequenced after the existing rows (seqs are 0..N parse-order).
        let existing_count: i64 = tx.query_row(
            "select count(*) from messages where session_id = ?1",
            params![session.id],
            |row| row.get(0),
        )?;
        // Append messages re-sequenced after the existing rows (seqs are 0..N parse-order).
        insert_messages(
            &tx,
            session,
            tail.new_messages
                .iter()
                .enumerate()
                .map(|(i, m)| (existing_count + i as i64, m)),
        )?;

        // New file edits, re-sequenced after the existing ones.
        let existing_edit_seq: i64 = tx.query_row(
            "select coalesce(max(seq), -1) from file_edits where session_id = ?1",
            params![session.id],
            |row| row.get(0),
        )?;
        insert_file_edits(
            &tx,
            session,
            tail.new_file_edits
                .iter()
                .enumerate()
                .map(|(i, e)| (existing_edit_seq + 1 + i as i64, e)),
        )?;

        // Advance volatile session metadata; updated_at/last_message_at only move forward (RFC3339
        // sorts lexically), title/preview take the tail's newest view, cwd fills if it was NULL.
        let new_count = existing_count + tail.new_messages.len() as i64;
        tx.execute(
            "update sessions set
                updated_at = case when ?2 is not null and ?2 > coalesce(updated_at, '') then ?2
                                  else updated_at end,
                last_message_at = case when ?3 is not null and ?3 > coalesce(last_message_at, '') then ?3
                                       else last_message_at end,
                title = ?4,
                preview_text = ?5,
                cwd = coalesce(cwd, ?6),
                message_count = ?7
             where id = ?1",
            params![
                session.id,
                session.updated_at.map(|value| value.to_rfc3339()),
                session.last_message_at.map(|value| value.to_rfc3339()),
                session.title,
                session.preview_text,
                session.cwd,
                new_count,
            ],
        )?;

        // Append the new conversation text to the transcript blob, then rebuild this session's FTS
        // row from the now-current sessions + transcripts rows (no drift from the live values).
        if !tail.new_transcript.is_empty() {
            tx.execute(
                "update transcripts set transcript_text =
                    case when transcript_text = '' then ?2
                         else transcript_text || char(10) || char(10) || ?2 end
                 where session_id = ?1",
                params![session.id, tail.new_transcript],
            )?;
        }
        tx.execute(
            "insert or replace into sessions_fts (rowid, title, summary, preview_text, transcript_text)
             select s.rowid, s.title, s.summary, s.preview_text, coalesce(t.transcript_text, '')
             from sessions s left join transcripts t on t.session_id = s.id
             where s.id = ?1",
            params![session.id],
        )?;

        // Persist the checkpoint + refresh files_seen mtime/size in the same transaction.
        tx.execute(
            "update files_seen set
                mtime_ns = ?3, size_bytes = ?4, last_indexed_at = ?5,
                tail_byte_offset = ?6, prefix_fingerprint = ?7
             where provider = ?1 and source_path = ?2",
            params![
                session.provider.as_str(),
                session.source_path,
                mtime_ns,
                size_bytes,
                Utc::now().to_rfc3339(),
                tail.new_tail_offset,
                tail.new_fingerprint,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Total persisted message rows. Basis for migration detection (empty → reindex) and tests.
    pub fn message_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("select count(*) from messages", [], |row| row.get(0))?)
    }

    /// Indexed document rows in the message FTS index. For external-content FTS5,
    /// `count(*) from messages_fts` reflects the `messages` content table even when
    /// the token index is empty; `_docsize` holds one row per indexed document and is
    /// the value that can actually assert trigger/rebuild sync (== `message_count`).
    pub fn messages_fts_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("select count(*) from messages_fts_docsize", [], |row| {
                row.get(0)
            })?)
    }

    /// Messages grouped by role, ordered by role, honoring the session/date filters.
    /// Basis for `stats` and tests. `MessageFilters::default()` counts everything.
    pub fn message_role_counts(&self, filters: &MessageFilters) -> Result<Vec<(String, i64)>> {
        use rusqlite::types::Value;

        let mut sql = String::from("select role, count(*) from messages where 1 = 1");
        let mut args: Vec<Value> = Vec::new();
        if let Some(session) = &filters.session {
            sql.push_str(" and session_id like ?");
            args.push(Value::Text(format!("%{session}%")));
        }
        push_path_prefix(
            &mut sql,
            &mut args,
            "session_id",
            filters.path_prefix.as_deref(),
        );
        push_ts_window(&mut sql, &mut args, "ts", filters.since, filters.until);
        sql.push_str(" group by role order by role");

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Term-frequency vocabulary over the message index via `fts5vocab`. Returns
    /// `(term, doc_count, total_count)` ordered by total occurrences (desc). `trigram=true` reads
    /// the substring (3-gram) index — useful for substring statistics; otherwise the word-token
    /// index (real words). `limit == 0` = all. Zero extra storage: `fts5vocab` is a read-only
    /// view over the FTS index, ordered most-frequent-first. `limit == 0` = all.
    ///
    /// `trigram=false` reads the word-token index (`messages_vocab` fts5vocab over `messages_fts`),
    /// reporting a true per-occurrence count. `trigram=true` reads the custom trigram index's
    /// `trigram_postings.df` (document frequency = number of messages containing the 3-gram); the
    /// custom index stores no per-occurrence count, so doc and count are both the document
    /// frequency. The trigram base builds lazily, so it is ensured before reading.
    pub fn vocabulary(&self, trigram: bool, limit: usize) -> Result<Vec<(String, i64, i64)>> {
        let lim: i64 = if limit == 0 { -1 } else { limit as i64 };
        // Both indexes return the same (term, doc_or_df, count_or_df) shape ordered most-frequent
        // first; only the source table/columns differ. The trigram base builds lazily, so ensure it
        // before reading. One query + one row-extractor for both arms.
        let sql = if trigram {
            self.ensure_trigram_base()?;
            "select tg, df, df from trigram_postings order by df desc, tg limit ?1"
        } else {
            "select term, doc, cnt from messages_vocab order by cnt desc, term limit ?1"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map([lim], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Message-level search. A literal `query` is an exact case-insensitive substring match:
    /// punctuation and infix text are significant (`/goal`, `C++`, `--path`, and `handled` inside
    /// `mishandled` all match literally). The custom trigram index may stage a superset of
    /// candidate rows for speed, but Rust/SQLite literal verification defines correctness.
    /// When `filters.regex` is set it is applied as a Rust regex (linear-time) over the rows
    /// matching the structured filters. `limit == 0` = unlimited.
    pub fn search_messages(
        &self,
        query: &str,
        filters: &MessageFilters,
    ) -> Result<Vec<MessageHit>> {
        Ok(self.search_messages_with_explain(query, filters, false)?.0)
    }

    /// Like [`Db::search_messages`], optionally returning the exact planner diagnostics used by
    /// this search. This keeps MCP `explain`, CLI `--explain`, and the search path on one shared
    /// FTS/trigram decision instead of running the planner twice.
    pub fn search_messages_with_explain(
        &self,
        query: &str,
        filters: &MessageFilters,
        include_explain: bool,
    ) -> Result<(Vec<MessageHit>, Option<SearchExplain>)> {
        use rusqlite::types::Value;

        let fuzzy_query = filters
            .fuzzy_query
            .as_deref()
            .filter(|value| !value.is_empty());
        let content_modes = [
            !query.is_empty(),
            filters.regex.is_some(),
            fuzzy_query.is_some(),
        ]
        .into_iter()
        .filter(|enabled| *enabled)
        .count();
        if content_modes > 1 {
            return Err(anyhow!(
                "provide only one content search mode: query (exact literal), --regex, or --fuzzy"
            ));
        }

        let mut sql = String::from(
            "select m.session_id, m.provider, m.seq, m.role, m.ts, m.tool_name, m.content \
             from messages m where 1 = 1",
        );
        let mut args: Vec<Value> = Vec::new();
        append_message_filters(&mut sql, &mut args, filters);
        if let Some(fuzzy_query) = fuzzy_query {
            sql.push_str(" order by m.session_id, m.seq");
            let hits = self.query_message_hits(&sql, &args)?;
            let corpus = hits.len() as i64;
            let mut hits = fuzzy_rank_message_hits(fuzzy_query, hits, filters.limit);
            let explain = include_explain.then(|| SearchExplain {
                prefilter: None,
                candidates: Some(hits.len() as i64),
                prefilter_skipped: Some("nucleo fuzzy scorer".to_string()),
                corpus,
            });
            return Ok((std::mem::take(&mut hits), explain));
        }

        let literal_query = filters.regex.is_none() && !query.is_empty();
        if literal_query {
            sql.push_str(" and instr(lower(m.content), lower(?)) > 0");
            args.push(Value::Text(query.to_string()));
        }
        let (use_trigram_candidates, explain) = self.prepare_content_prefilter(
            filters.regex.as_deref().or(literal_query.then_some(query)),
            filters,
            include_explain,
        )?;
        if use_trigram_candidates {
            sql.push_str(" and m.id in (select id from _trigram_cand)");
        }
        sql.push_str(" order by m.session_id, m.seq");
        // When regex is active the limit is applied after matching (in Rust), so only
        // push a SQL LIMIT for the non-regex path.
        if filters.limit > 0 && filters.regex.is_none() {
            sql.push_str(" limit ?");
            args.push(Value::Integer(filters.limit as i64));
        }

        let compiled = match &filters.regex {
            Some(pattern) => {
                Some(regex::Regex::new(pattern).map_err(|err| anyhow!("invalid --regex: {err}"))?)
            }
            None => None,
        };

        let mut stmt = self.conn.prepare(&sql)?;
        let raw_hits =
            stmt.query_map(rusqlite::params_from_iter(args.iter()), row_to_message_hit)?;
        let mut hits = Vec::new();
        for hit in raw_hits {
            let hit = hit?;
            if let Some(re) = &compiled {
                if !re.is_match(&hit.content) {
                    continue;
                }
            }
            hits.push(hit);
            if filters.limit > 0 && hits.len() >= filters.limit {
                break;
            }
        }
        Ok((hits, explain))
    }

    fn query_message_hits(
        &self,
        sql: &str,
        args: &[rusqlite::types::Value],
    ) -> Result<Vec<MessageHit>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), row_to_message_hit)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Count the messages matching the structural filters (role / provider / session / path / time /
    /// tool / no-compaction) — the corpus that literal or regex content matching then
    /// scans. Shared by [`Db::search_messages`]'s prefilter gate and [`Db::explain_message_search`]
    /// so both see the exact same denominator (predicates via [`append_message_filters`]).
    fn filtered_corpus_count(&self, filters: &MessageFilters) -> Result<i64> {
        use rusqlite::types::Value;
        let mut sql = String::from("select count(*) from messages m where 1 = 1");
        let mut args: Vec<Value> = Vec::new();
        append_message_filters(&mut sql, &mut args, filters);
        Ok(self
            .conn
            .query_row(&sql, rusqlite::params_from_iter(args.iter()), |row| {
                row.get(0)
            })?)
    }

    fn corpus_count(&self, filters: &MessageFilters, cached: Option<i64>) -> Result<i64> {
        cached.map_or_else(|| self.filtered_corpus_count(filters), Ok)
    }

    fn staged_candidate_count(&self, filters: &MessageFilters) -> Result<i64> {
        use rusqlite::types::Value;
        let mut sql = String::from("select count(*) from messages m where 1 = 1");
        let mut args: Vec<Value> = Vec::new();
        append_message_filters(&mut sql, &mut args, filters);
        sql.push_str(" and m.id in (select id from _trigram_cand)");
        Ok(self
            .conn
            .query_row(&sql, rusqlite::params_from_iter(args.iter()), |row| {
                row.get(0)
            })?)
    }

    /// Prepare the trigram acceleration path and, when requested, return diagnostics for the
    /// same decision. The prefilter is a superset: it may include false positives, but the
    /// caller's literal/regex verifier remains authoritative. Returning `(false, explain)` is
    /// still correct: it means either no usable anchor exists or the structured filters already
    /// made a direct scan cheaper than intersecting the whole-corpus trigram index.
    fn prepare_content_prefilter(
        &self,
        pattern: Option<&str>,
        filters: &MessageFilters,
        include_explain: bool,
    ) -> Result<(bool, Option<SearchExplain>)> {
        let Some(pattern) = pattern else {
            let explain = include_explain
                .then(|| {
                    self.filtered_corpus_count(filters)
                        .map(|corpus| SearchExplain {
                            prefilter: None,
                            candidates: None,
                            prefilter_skipped: None,
                            corpus,
                        })
                })
                .transpose()?;
            return Ok((false, explain));
        };

        let corpus = if filters.narrows_corpus() || include_explain {
            Some(self.filtered_corpus_count(filters)?)
        } else {
            None
        };
        let Some(groups) = crate::trigram::trigram_prefilter_groups(pattern) else {
            let explain = if include_explain {
                Some(SearchExplain {
                    prefilter: None,
                    candidates: None,
                    prefilter_skipped: None,
                    corpus: self.corpus_count(filters, corpus)?,
                })
            } else {
                None
            };
            return Ok((false, explain));
        };

        // Corpus-size gate: only query the trigram index when the structurally-filtered corpus is
        // large enough to benefit. A role/session/path/ts/tool filter can restrict the scan to a
        // small slice, where a direct literal/regex scan beats intersecting it against the
        // whole-corpus trigram index. Regression-free: the prefilter is a superset and the final
        // literal/regex verifier remains authoritative.
        let use_prefilter = !filters.narrows_corpus()
            || self.corpus_count(filters, corpus)? >= self.prefilter_min_corpus;
        let prefilter = include_explain.then(|| crate::trigram::render_prefilter_groups(&groups));
        if !use_prefilter {
            let explain = if include_explain {
                Some(SearchExplain {
                    prefilter,
                    candidates: None,
                    prefilter_skipped: Some(format!(
                        "structured filters reduced the corpus below regex_prefilter_min_corpus ({})",
                        self.prefilter_min_corpus
                    )),
                    corpus: self.corpus_count(filters, corpus)?,
                })
            } else {
                None
            };
            return Ok((false, explain));
        }

        // Custom parallel-built trigram index (base) + un-indexed delta; the final literal/regex
        // verifier checks every candidate, so this is a SUPERSET filter.
        let base_max = self.ensure_trigram_base()?;
        let candidates = crate::trigram_index::candidates(&self.conn, &groups)?;
        self.stage_candidates(base_max, &candidates)?;
        let explain = if include_explain {
            Some(SearchExplain {
                prefilter,
                candidates: Some(self.staged_candidate_count(filters)?),
                prefilter_skipped: None,
                corpus: self.corpus_count(filters, corpus)?,
            })
        } else {
            None
        };
        Ok((true, explain))
    }

    /// Explain the actual message-search plan for the regex stored in `filters`. Returns the
    /// corpus size under the structural filters, the trigram prefilter when a usable anchor exists,
    /// and either the candidate-row count that search will verify or the reason the prefilter was
    /// skipped. Candidates close to corpus = a non-selective prefilter = a slow query. Uses the
    /// SAME predicates and threshold gate as [`Db::search_messages`] so diagnostics cannot drift.
    pub fn explain_message_search(&self, filters: &MessageFilters) -> Result<SearchExplain> {
        let (_, explain) =
            self.prepare_content_prefilter(filters.regex.as_deref(), filters, true)?;
        explain.context("message search explanation was not produced")
    }

    /// Fetch the messages surrounding a `(session_id, seq)` anchor — `before` rows
    /// before and `after` rows after — for `messages search --context`. Ordered by
    /// seq; served directly by the `(session_id, seq)` index.
    pub fn message_context(
        &self,
        session_id: &str,
        seq: i64,
        before: i64,
        after: i64,
    ) -> Result<Vec<MessageHit>> {
        let mut stmt = self.conn.prepare(
            "select session_id, provider, seq, role, ts, tool_name, content from messages
             where session_id = ?1 and seq between ?2 and ?3 order by seq",
        )?;
        let rows = stmt.query_map(params![session_id, seq - before, seq + after], |row| {
            Ok(MessageHit {
                session_id: row.get(0)?,
                provider: Provider::from_db_str(&row.get::<_, String>(1)?),
                seq: row.get(2)?,
                role: Role::from_db_str(&row.get::<_, String>(3)?),
                ts: row.get::<_, Option<String>>(4)?.and_then(|value| {
                    chrono::DateTime::parse_from_rfc3339(&value)
                        .ok()
                        .map(|dt| dt.with_timezone(&Utc))
                }),
                tool_name: row.get(5)?,
                fuzzy_score: None,
                content: row.get(6)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Fetch compact session metadata for a set of session ids in ONE query, keyed by
    /// id. Used by the MCP `search_messages` serializer to enrich each hit with its
    /// session context without an N+1 per-hit lookup. Unknown ids are simply absent from the map.
    pub fn session_metadata(
        &self,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, crate::models::SessionMeta>> {
        use crate::models::SessionMeta;
        use std::collections::HashMap;
        let mut map = HashMap::new();
        if ids.is_empty() {
            return Ok(map);
        }
        let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "select id, provider_session_id, cwd, repo_root, title, updated_at, last_message_at, \
             message_count, parse_warning from sessions where id in ({placeholders})"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                SessionMeta {
                    provider_session_id: row.get(1)?,
                    cwd: row.get(2)?,
                    repo_root: row.get(3)?,
                    title: row.get(4)?,
                    updated_at: row
                        .get::<_, Option<String>>(5)?
                        .as_deref()
                        .and_then(crate::util::parse_datetime),
                    last_message_at: row
                        .get::<_, Option<String>>(6)?
                        .as_deref()
                        .and_then(crate::util::parse_datetime),
                    message_count: row.get(7)?,
                    parse_warning: row.get(8)?,
                },
            ))
        })?;
        for row in rows {
            let (id, meta) = row?;
            map.insert(id, meta);
        }
        Ok(map)
    }

    /// Scan user messages and tag each against the ordered `patterns` (first match wins,
    /// so `other` must be last). Streams rows; only matches are materialized.
    /// `filters.limit == 0` means unlimited.
    ///
    /// Corrections are intrinsically scoped to `role = 'user'` — the user's own prompts, a small
    /// slice of the corpus (≈7.7k rows / ~10 MB on the real index vs 628k total). A direct regex
    /// scan of that slice is milliseconds. We deliberately do NOT route this through the trigram
    /// prefilter: the correction keywords ("wrong", "stop", "actually", …) have very common
    /// trigrams, so a prefilter `MATCH` would scan a large fraction of the multi-GB trigram index
    /// (95% of which is tool output the `role='user'` filter then discards) AND trigger its lazy
    /// build — measured ~21 s, strictly slower than scanning the user rows outright. The structural
    /// `role='user'` filter is the selective one here, so we lean on it alone.
    pub fn find_corrections(
        &self,
        patterns: &[(String, regex::Regex)],
        filters: &MessageFilters,
    ) -> Result<Vec<CorrectionMatch>> {
        use rusqlite::types::Value;

        let mut sql = String::from(
            "select session_id, provider, ts, content from messages where role = 'user'",
        );
        let mut args: Vec<Value> = Vec::new();
        if let Some(session) = &filters.session {
            sql.push_str(" and session_id like ?");
            args.push(Value::Text(format!("%{session}%")));
        }
        push_path_prefix(
            &mut sql,
            &mut args,
            "session_id",
            filters.path_prefix.as_deref(),
        );
        push_ts_window(&mut sql, &mut args, "ts", filters.since, filters.until);
        sql.push_str(" order by ts desc");

        let mut stmt = self.conn.prepare(&sql)?;
        // Materialize the user-row slice BEFORE going parallel: rusqlite's `Connection`/`Statement`
        // are not `Sync`, so the parallel classification below must own its rows. This is the same
        // ~13 MB the sequential scan already streamed (role='user' is a small slice), so collecting
        // it up front is cheap relative to the regex work that follows.
        let rows: Vec<(String, String, Option<String>, String)> = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // Classify one row against the ordered patterns. Borrow `content` only for the regex
        // search, then MOVE the owned fields into the result — no per-match clone of
        // session_id/content. This single closure is shared by both the sequential and parallel
        // paths below (DRY); regex matching is the CPU-bound cost (~98% of one core: ~13 MB × the
        // category regexes) and each row is independent.
        let classify = |(session_id, provider, ts, content): (
            String,
            String,
            Option<String>,
            String,
        )|
         -> Option<CorrectionMatch> {
            let (category, matched_pattern) = patterns.iter().find_map(|(cat, re)| {
                re.find(&content)
                    .map(|m| (cat.clone(), m.as_str().to_string()))
            })?;
            let ts = ts.as_deref().and_then(|value| {
                chrono::DateTime::parse_from_rfc3339(value)
                    .ok()
                    .map(|dt| dt.with_timezone(&Utc))
            });
            Some(CorrectionMatch {
                session_id,
                provider: Provider::from_db_str(&provider),
                ts,
                category,
                matched_pattern,
                content,
            })
        };

        // Run sequentially when the configured pool is single-threaded (threads=1) to avoid Rayon's
        // split/join overhead; otherwise classify in parallel. `regex::Regex` is `Sync`, so sharing
        // `patterns` read-only across workers is safe. Both paths preserve the SQL `order by ts
        // desc` (Rayon's `collect` is order-preserving), so output is identical — verified by
        // `find_corrections_parallel_matches_sequential`.
        use rayon::prelude::*;
        let mut out: Vec<CorrectionMatch> = if rayon::current_num_threads() <= 1 {
            rows.into_iter().filter_map(classify).collect()
        } else {
            rows.into_par_iter().filter_map(classify).collect()
        };
        // `limit == 0` means unlimited; otherwise keep the first N in ts-desc order — identical to
        // the sequential early-break, which stopped after N matches in that same order.
        if filters.limit > 0 {
            out.truncate(filters.limit);
        }
        Ok(out)
    }

    /// Aggregate slash-command frequency: count, distinct sessions, distinct projects
    /// (session repo_root, falling back to cwd). Sorted by count desc then command.
    /// Count slash-command usage. When `command_filters` is non-empty, only commands whose
    /// token matches one of the (already-compiled) patterns are counted; empty = count all.
    pub fn planning_usage(
        &self,
        filters: &MessageFilters,
        command_filters: &[regex::Regex],
    ) -> Result<Vec<PlanningCount>> {
        use rusqlite::types::Value;
        use std::collections::{HashMap, HashSet};

        let mut sql = String::from(
            "select m.session_id, s.repo_root, s.cwd, m.content from messages m \
             join sessions s on s.id = m.session_id where 1 = 1",
        );
        let mut args: Vec<Value> = Vec::new();
        let mut filters = filters.clone();
        filters.role = Some(Role::Slash);
        append_message_filters(&mut sql, &mut args, &filters);

        let mut stmt = self.conn.prepare(&sql)?;
        let raw = stmt.query_map(rusqlite::params_from_iter(args.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;

        // command -> (count, distinct sessions, distinct projects)
        let mut agg: HashMap<String, (i64, HashSet<String>, HashSet<String>)> = HashMap::new();
        for row in raw {
            let (session_id, repo_root, cwd, content) = row?;
            if let Some(command) = crate::util::slash_command_token(&content) {
                if !command_filters.is_empty()
                    && !command_filters.iter().any(|re| re.is_match(&command))
                {
                    continue;
                }
                let project = repo_root.or(cwd).unwrap_or_default();
                let entry = agg.entry(command).or_default();
                entry.0 += 1;
                entry.1.insert(session_id);
                if !project.is_empty() {
                    entry.2.insert(project);
                }
            }
        }
        let mut counts: Vec<PlanningCount> = agg
            .into_iter()
            .map(|(command, (count, sessions, projects))| PlanningCount {
                command,
                count,
                unique_sessions: sessions.len() as i64,
                unique_projects: projects.len() as i64,
            })
            .collect();
        counts.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| a.command.cmp(&b.command))
        });
        if filters.limit > 0 {
            counts.truncate(filters.limit);
        }
        Ok(counts)
    }

    /// Aggregate file-edit activity per file (`files search`). Honors an optional glob
    /// `pattern`, session scope, date window, and min/max edit-count thresholds.
    pub fn file_search(&self, query: &FileQuery) -> Result<Vec<FileEditSummary>> {
        use rusqlite::types::Value;

        let mut sql = String::from(
            "select file_path, file_name, count(*) as edits, \
             count(distinct session_id) as sessions, max(ts) as last_edited \
             from file_edits where 1 = 1",
        );
        let mut args: Vec<Value> = Vec::new();
        if let Some(pattern) = &query.pattern {
            let (col, like) = glob_clause(pattern);
            sql.push_str(&format!(" and {col} like ? escape '\\'"));
            args.push(Value::Text(like));
        }
        push_file_session_filter(
            &mut sql,
            &mut args,
            query.session_id.as_deref(),
            query.session.as_deref(),
        );
        push_ts_window(&mut sql, &mut args, "ts", query.since, query.until);
        sql.push_str(" group by file_path");
        let mut having: Vec<&str> = Vec::new();
        if let Some(min) = query.min_edits {
            having.push("count(*) >= ?");
            args.push(Value::Integer(min));
        }
        if let Some(max) = query.max_edits {
            having.push("count(*) <= ?");
            args.push(Value::Integer(max));
        }
        if !having.is_empty() {
            sql.push_str(" having ");
            sql.push_str(&having.join(" and "));
        }
        sql.push_str(" order by edits desc, last_edited desc");
        if query.limit > 0 {
            sql.push_str(" limit ?");
            args.push(Value::Integer(query.limit as i64));
        }

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), |row| {
                Ok(FileEditSummary {
                    file_path: row.get(0)?,
                    file_name: row.get(1)?,
                    edits: row.get(2)?,
                    sessions: row.get(3)?,
                    last_edited: row
                        .get::<_, Option<String>>(4)?
                        .as_deref()
                        .and_then(crate::util::parse_datetime),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// File ↔ session linkage with per-pair edit counts (`files cross-ref`).
    pub fn file_cross_ref(&self, query: &FileQuery) -> Result<Vec<FileCrossRef>> {
        use rusqlite::types::Value;

        let mut sql = String::from(
            "select file_path, session_id, provider, count(*) as edits \
             from file_edits where 1 = 1",
        );
        let mut args: Vec<Value> = Vec::new();
        if let Some(pattern) = &query.pattern {
            let (col, like) = glob_clause(pattern);
            sql.push_str(&format!(" and {col} like ? escape '\\'"));
            args.push(Value::Text(like));
        }
        push_file_session_filter(
            &mut sql,
            &mut args,
            query.session_id.as_deref(),
            query.session.as_deref(),
        );
        push_ts_window(&mut sql, &mut args, "ts", query.since, query.until);
        sql.push_str(" group by file_path, session_id order by file_path, edits desc");
        if query.limit > 0 {
            sql.push_str(" limit ?");
            args.push(Value::Integer(query.limit as i64));
        }

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), |row| {
                let provider: String = row.get(2)?;
                Ok(FileCrossRef {
                    file_path: row.get(0)?,
                    session_id: row.get(1)?,
                    provider: Provider::from_db_str(&provider),
                    edits: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Ordered raw edits for one file (`files history`/`extract`). Matches by exact
    /// basename, exact path, or path suffix (`%/file`), optionally scoped to a session.
    /// Results are ordered by `(session_id, seq)` so callers can number versions per
    /// session and replay deltas deterministically.
    pub fn file_edits_for(
        &self,
        file: &str,
        session: Option<&str>,
    ) -> Result<Vec<(String, Provider, FileEdit)>> {
        self.file_edits_for_scoped(file, None, session)
    }

    pub fn file_edits_for_session_id(
        &self,
        file: &str,
        session_id: &str,
    ) -> Result<Vec<(String, Provider, FileEdit)>> {
        self.file_edits_for_scoped(file, Some(session_id), None)
    }

    fn file_edits_for_scoped(
        &self,
        file: &str,
        session_id: Option<&str>,
        session: Option<&str>,
    ) -> Result<Vec<(String, Provider, FileEdit)>> {
        use rusqlite::types::Value;

        let mut sql = String::from(
            "select session_id, provider, seq, ts, tool, file_path, file_name, new_content, edits_json \
             from file_edits where (file_name = ? or file_path = ? or file_path like ?)",
        );
        let mut args: Vec<Value> = vec![
            Value::Text(file.to_string()),
            Value::Text(file.to_string()),
            Value::Text(format!("%/{file}")),
        ];
        push_file_session_filter(&mut sql, &mut args, session_id, session);
        sql.push_str(" order by session_id, seq");

        let mut stmt = self.conn.prepare(&sql)?;
        let raw = stmt.query_map(rusqlite::params_from_iter(args.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in raw {
            let (
                session_id,
                provider,
                seq,
                ts,
                tool,
                file_path,
                file_name,
                new_content,
                edits_json,
            ) = row?;
            // Surface a corrupt/truncated edits_json instead of silently yielding no edits (which
            // would make `files extract` show an edit row with no diffs and no signal).
            let edits: Vec<EditOp> = match edits_json.as_deref() {
                Some(json) => serde_json::from_str(json).with_context(|| {
                    format!("corrupt edits_json for {file_path} in session {session_id}")
                })?,
                None => Vec::new(),
            };
            out.push((
                session_id,
                Provider::from_db_str(&provider),
                FileEdit {
                    seq,
                    ts: ts.as_deref().and_then(crate::util::parse_datetime),
                    tool,
                    file_path,
                    file_name,
                    new_content,
                    edits,
                },
            ));
        }
        Ok(out)
    }

    /// Total persisted file-edit rows. Basis for migration detection and tests.
    pub fn file_edit_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("select count(*) from file_edits", [], |row| row.get(0))?)
    }

    pub fn list_recent(&self, filters: &SearchFilters) -> Result<Vec<SessionRecord>> {
        let mut results = self.load_sessions(filters)?;
        results.sort_by_key(|session| std::cmp::Reverse(session.session.updated_at));
        results.truncate(filters.limit);
        Ok(results.into_iter().map(|item| item.session).collect())
    }

    pub fn search(
        &self,
        query: &str,
        filters: &SearchFilters,
        current_repo: Option<&str>,
        scoring: &crate::config::ScoringConfig,
    ) -> Result<Vec<SearchHit>> {
        // Try FTS first for efficient candidate retrieval
        let fts_ids = self.fts_candidate_ids(
            query,
            filters.limit * scoring.fts_candidate_multiplier,
            scoring.fts_candidate_floor,
        )?;
        let candidates = if fts_ids.is_empty() {
            // Fallback: load all sessions for fuzzy-only matching
            self.load_sessions(filters)?
        } else {
            // Load only FTS-matched sessions (still apply filters)
            self.load_sessions_by_ids(&fts_ids, filters)?
        };

        let matcher = SkimMatcherV2::default().smart_case();
        let query_lower = query.to_ascii_lowercase();
        let tokens: Vec<&str> = query_lower.split_whitespace().collect();
        let mut hits = Vec::new();

        for record in candidates {
            let title = record.session.title.as_deref().unwrap_or_default();
            let summary = record.session.summary.as_deref().unwrap_or_default();
            let cwd = record.session.cwd.as_deref().unwrap_or_default();
            let repo_root = record.session.repo_root.as_deref().unwrap_or_default();
            let preview = record.session.preview_text.as_str();
            let transcript = record.transcript_text.as_str();
            let haystacks = [
                ("title", title),
                ("summary", summary),
                ("cwd", cwd),
                ("repo", repo_root),
                ("preview", preview),
                ("transcript", transcript),
            ];

            let mut score = 0i64;
            let mut best_source = "fuzzy".to_string();
            let mut best_source_score = i64::MIN;
            let mut best_snippet = snippet_from_match(preview, query, 160);

            let mut total_tokens_matched = 0usize;
            for (source, value) in haystacks {
                let lowered = value.to_ascii_lowercase();
                let mut source_score = 0i64;
                if lowered.contains(&query_lower) {
                    source_score += match source {
                        "title" => scoring.title_score,
                        "summary" => scoring.summary_score,
                        "cwd" | "repo" => scoring.path_score,
                        "preview" => scoring.preview_score,
                        _ => scoring.other_score,
                    };
                }
                let mut tokens_hit = 0usize;
                for token in &tokens {
                    if !token.is_empty() && lowered.contains(token) {
                        source_score += scoring.token_bonus;
                        tokens_hit += 1;
                    }
                }
                total_tokens_matched = total_tokens_matched.max(tokens_hit);
                if matches!(source, "title" | "cwd" | "repo" | "preview") {
                    source_score += matcher.fuzzy_match(value, query).unwrap_or_default();
                }

                score += source_score;
                if source_score > best_source_score {
                    best_source_score = source_score;
                    best_source = source.to_string();
                    best_snippet = snippet_from_match(value, query, 160);
                }
            }
            // Bonus when all query tokens matched somewhere
            if tokens.len() > 1 && total_tokens_matched == tokens.len() {
                score += scoring.all_tokens_bonus;
            }

            if let Some(updated_at) = record.session.updated_at {
                let age_days = (Utc::now() - updated_at)
                    .num_days()
                    .clamp(0, scoring.recency_max_days);
                score += (scoring.recency_max_days - age_days) * scoring.recency_weight;
            }
            if let (Some(current_repo), Some(repo_root)) =
                (current_repo, record.session.repo_root.as_deref())
            {
                if current_repo == repo_root {
                    score += scoring.current_repo_bonus;
                    if best_source == "fuzzy" {
                        best_source = "repo".to_string();
                        best_snippet = snippet_from_match(repo_root, query, 160);
                    }
                }
            }
            if score > 0 {
                hits.push(SearchHit {
                    session: record.session,
                    score,
                    match_source: best_source,
                    match_snippet: best_snippet,
                });
            }
        }

        hits.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| b.session.updated_at.cmp(&a.session.updated_at))
        });
        hits.truncate(filters.limit);
        Ok(hits)
    }

    /// Query FTS5 index for candidate session IDs matching the query. The returned ids are
    /// re-ranked by the fuzzy scorer in [`Db::search`], so we retrieve a generous candidate
    /// set (never fewer than [`FTS_CANDIDATE_FLOOR`]) rather than exactly the caller's limit:
    /// a high-fuzzy-score session that ranks low under raw FTS `rank` must still be loaded.
    fn fts_candidate_ids(&self, query: &str, limit: usize, floor: usize) -> Result<Vec<String>> {
        // Phrase-quote each token (neutralizing FTS5 operators like * OR NEAR) and OR them.
        // Drop tokens with no searchable characters: a punctuation-only token (e.g. "***")
        // tokenizes to an empty phrase, which is a MATCH syntax error in strict FTS5 builds.
        let fts_query: String = query
            .split_whitespace()
            .filter(|token| token.chars().any(char::is_alphanumeric))
            .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR ");
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }
        // Never issue `LIMIT 0` (zero rows): a caller passing limit==0 means "unlimited".
        let cap = limit.max(floor);
        let mut stmt = self.conn.prepare(
            "select s.id
             from sessions_fts f
             join sessions s on s.rowid = f.rowid
             where sessions_fts match ?1
             order by rank
             limit ?2",
        )?;
        let rows = stmt.query_map(params![fts_query, cap as i64], |row| {
            row.get::<_, String>(0)
        })?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row?);
        }
        Ok(ids)
    }

    /// Load specific sessions by ID, applying search filters.
    fn load_sessions_by_ids(
        &self,
        ids: &[String],
        filters: &SearchFilters,
    ) -> Result<Vec<SessionWithTranscript>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let mut sql = format!(
            "
            select
                s.id, s.provider, s.provider_session_id, s.title, s.summary, s.cwd, s.repo_root,
                s.created_at, s.updated_at, s.last_message_at, s.preview_text, s.source_path,
                s.message_count, s.parse_version, s.raw_metadata_json, s.parse_warning, s.discovery_source,
                coalesce(t.transcript_text, '')
            from sessions s
            left join transcripts t on t.session_id = s.id
            where s.id in ({placeholders})
            "
        );
        let mut params_vec: Vec<String> = ids.to_vec();
        if let Some(provider) = filters.provider {
            sql.push_str(" and s.provider = ? ");
            params_vec.push(provider.as_str().to_string());
        }
        if let Some(path_prefix) = &filters.path_prefix {
            push_session_path_prefix(&mut sql, &mut params_vec, path_prefix);
        }
        push_session_exclusions(&mut sql, &mut params_vec, filters);
        push_session_time_window(&mut sql, &mut params_vec, filters.since, filters.until);
        if filters.warnings_only {
            sql.push_str(" and s.parse_warning is not null and s.parse_warning != '' ");
        }
        sql.push_str(" order by s.updated_at desc");

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(params_vec.iter()),
            row_to_session_with_transcript,
        )?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    pub fn resolve_session(&self, value: &str) -> Result<SessionWithTranscript> {
        let mut stmt = self.conn.prepare(RESOLVE_SESSION_SQL)?;

        let pattern = format!("{value}%");
        let rows = stmt.query_map(params![value, pattern], row_to_session_with_transcript)?;
        let mut matches = Vec::new();
        for row in rows {
            matches.push(row?);
        }
        unique_session_match(value, matches, |session| &session.session.id)
    }

    pub fn resolve_session_record(&self, value: &str) -> Result<SessionRecord> {
        let mut stmt = self.conn.prepare(RESOLVE_SESSION_RECORD_SQL)?;

        let pattern = format!("{value}%");
        let rows = stmt.query_map(params![value, pattern], row_to_session_record)?;
        let mut matches = Vec::new();
        for row in rows {
            matches.push(row?);
        }
        unique_session_match(value, matches, |session| &session.id)
    }

    pub fn count_parse_warnings(&self) -> Result<i64> {
        self.conn
            .query_row(
                "select count(*) from sessions where parse_warning is not null and parse_warning != ''",
                [],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn counts_by_provider(&self) -> Result<HashMap<String, i64>> {
        let mut stmt = self
            .conn
            .prepare("select provider, count(*) from sessions group by provider")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (provider, count) = row?;
            out.insert(provider, count);
        }
        Ok(out)
    }

    fn load_sessions(&self, filters: &SearchFilters) -> Result<Vec<SessionWithTranscript>> {
        let mut sql = String::from(
            "
            select
                s.id, s.provider, s.provider_session_id, s.title, s.summary, s.cwd, s.repo_root,
                s.created_at, s.updated_at, s.last_message_at, s.preview_text, s.source_path,
                s.message_count, s.parse_version, s.raw_metadata_json, s.parse_warning, s.discovery_source,
                coalesce(t.transcript_text, '')
            from sessions s
            left join transcripts t on t.session_id = s.id
            where 1 = 1
            ",
        );
        let mut params_vec: Vec<String> = Vec::new();
        if let Some(provider) = filters.provider {
            sql.push_str(" and s.provider = ? ");
            params_vec.push(provider.as_str().to_string());
        }
        if let Some(path_prefix) = &filters.path_prefix {
            push_session_path_prefix(&mut sql, &mut params_vec, path_prefix);
        }
        push_session_exclusions(&mut sql, &mut params_vec, filters);
        push_session_time_window(&mut sql, &mut params_vec, filters.since, filters.until);
        if filters.warnings_only {
            sql.push_str(" and s.parse_warning is not null and s.parse_warning != '' ");
        }
        sql.push_str(" order by s.updated_at desc");

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(params_vec.iter()),
            row_to_session_with_transcript,
        )?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }
}

/// RFC3339 text for an inclusive UPPER date bound. Period-end bounds from `dates.rs`
/// are second-granular (a month resolves to `…T23:59:59`), but stored timestamps can
/// carry sub-second fractions (`…T23:59:59.123+00:00`). The SQL compare is lexicographic
/// over the rfc3339 strings, and a bare `…59+00:00` sorts *before* `…59.123…` (because
/// '+' < '.'), so a plain `<= until` would wrongly drop a row in the final second.
/// Extending the bound to the last nanosecond of its second makes `<=` cover the whole
/// second for any stored sub-second precision (`+00:00`, not `Z`, to match stored text).
fn until_bound_text(until: chrono::DateTime<Utc>) -> String {
    use chrono::Timelike;
    until
        .with_nanosecond(999_999_999)
        .unwrap_or(until)
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, false)
}

/// Append the `path_prefix` predicate — restrict to messages whose session is rooted at the
/// prefix — onto a query whose message rows expose `session_id` as `id_col` (e.g. `m.session_id`
/// or a bare `session_id`). The `sessions` table is tiny relative to `messages`, so a subquery is
/// cheap and needs no dedicated index. Mirrors the session-level `path_prefix` semantics in
/// `list_recent`/`search` (exact directory or a child path, with LIKE metacharacters escaped) so
/// `--path` behaves identically across the session, message-search, and analytics surfaces. Shared by
/// [`append_message_filters`] and the bespoke-SQL analytics queries (corrections / planning /
/// stats) so none can silently ignore `--path`. No-op when `path_prefix` is None.
fn push_path_prefix(
    sql: &mut String,
    args: &mut Vec<rusqlite::types::Value>,
    id_col: &str,
    path_prefix: Option<&str>,
) {
    use std::fmt::Write as _;
    if let Some(prefix) = path_prefix {
        let _ = write!(sql, " and {id_col} in (select id from sessions where ");
        push_path_condition(sql, args, prefix);
        sql.push(')');
    }
}

fn push_path_condition(sql: &mut String, args: &mut Vec<rusqlite::types::Value>, prefix: &str) {
    use rusqlite::types::Value;
    sql.push_str(
        "(coalesce(cwd, '') = ? or coalesce(cwd, '') like ? escape '\\' \
          or coalesce(repo_root, '') = ? or coalesce(repo_root, '') like ? escape '\\' \
          or coalesce(source_path, '') = ? or coalesce(source_path, '') like ? escape '\\')",
    );
    let (exact, child_pattern) = path_prefix_patterns(prefix);
    args.push(Value::Text(exact.clone()));
    args.push(Value::Text(child_pattern.clone()));
    args.push(Value::Text(exact.clone()));
    args.push(Value::Text(child_pattern.clone()));
    args.push(Value::Text(exact));
    args.push(Value::Text(child_pattern));
}

fn push_exclude_path_prefixes(
    sql: &mut String,
    args: &mut Vec<rusqlite::types::Value>,
    id_col: &str,
    prefixes: &[String],
) {
    use std::fmt::Write as _;
    if prefixes.is_empty() {
        return;
    }
    let _ = write!(sql, " and {id_col} not in (select id from sessions where ");
    for (i, prefix) in prefixes.iter().enumerate() {
        if i > 0 {
            sql.push_str(" or ");
        }
        push_path_condition(sql, args, prefix);
    }
    sql.push(')');
}

/// Append the structural message predicates shared by [`Db::search_messages`] and
/// [`Db::explain_message_search`] — role, provider, session, tool name, the date
/// window, and the compaction filter — all ANDed onto an existing WHERE using the
/// `m` table alias. Centralizing this guarantees the `explain` candidate count is
/// computed over exactly the rows `search_messages` scans (no filter drift between
/// the two as filters are added).
fn append_message_filters(
    sql: &mut String,
    args: &mut Vec<rusqlite::types::Value>,
    filters: &MessageFilters,
) {
    use rusqlite::types::Value;
    if let Some(role) = filters.role {
        sql.push_str(" and m.role = ?");
        args.push(Value::Text(role.as_str().to_string()));
    }
    if let Some(provider) = filters.provider {
        sql.push_str(" and m.provider = ?");
        args.push(Value::Text(provider.as_str().to_string()));
    }
    if let Some(session_id) = &filters.session_id {
        sql.push_str(" and m.session_id = ?");
        args.push(Value::Text(session_id.clone()));
    }
    if let Some(session) = &filters.session {
        sql.push_str(" and m.session_id like ?");
        args.push(Value::Text(format!("%{session}%")));
    }
    push_path_prefix(sql, args, "m.session_id", filters.path_prefix.as_deref());
    push_exclude_path_prefixes(sql, args, "m.session_id", &filters.exclude_path_prefixes);
    for session_id in &filters.exclude_session_ids {
        sql.push_str(" and m.session_id <> ?");
        args.push(Value::Text(session_id.clone()));
    }
    if let Some(tool) = &filters.tool {
        // NULL tool_name (non-tool rows) is correctly excluded by instr(NULL,..) = NULL.
        sql.push_str(" and instr(lower(m.tool_name), lower(?)) > 0");
        args.push(Value::Text(tool.clone()));
    }
    if let Some(seq_from) = filters.seq_from {
        sql.push_str(" and m.seq >= ?");
        args.push(Value::Integer(seq_from));
    }
    if let Some(seq_to) = filters.seq_to {
        sql.push_str(" and m.seq <= ?");
        args.push(Value::Integer(seq_to));
    }
    push_ts_window(sql, args, "m.ts", filters.since, filters.until);
    if filters.no_compaction {
        sql.push_str(" and m.is_compaction = 0");
    }
}

/// Insert message rows for `session`, taking each row's `seq` from the caller (parse-order on a
/// full upsert, or post-existing-count on an incremental append). Shared by `upsert_session` and
/// `append_tail` so the `insert into messages` statement + 8-field bind live in ONE place.
fn insert_messages<'a>(
    tx: &rusqlite::Transaction<'_>,
    session: &SessionRecord,
    rows: impl Iterator<Item = (i64, &'a crate::models::Message)>,
) -> Result<()> {
    let mut stmt = tx.prepare(
        "insert into messages
            (session_id, provider, seq, role, ts, tool_name, is_compaction, content)
         values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    for (seq, message) in rows {
        stmt.execute(params![
            session.id,
            session.provider.as_str(),
            seq,
            message.role.as_str(),
            message.ts.map(|ts| ts.to_rfc3339()),
            message.tool_name,
            message.is_compaction as i64,
            message.content,
        ])?;
    }
    Ok(())
}

/// Insert file-edit rows for `session`, with the caller-supplied `seq`. Shared by `upsert_session`
/// and `append_tail`. `edits` serialize to a JSON `[old, new]` array (NULL when empty); the same
/// shape both call sites previously duplicated.
fn insert_file_edits<'a>(
    tx: &rusqlite::Transaction<'_>,
    session: &SessionRecord,
    rows: impl Iterator<Item = (i64, &'a FileEdit)>,
) -> Result<()> {
    let mut stmt = tx.prepare(
        "insert into file_edits
            (session_id, provider, seq, ts, tool, file_path, file_name, new_content, edits_json)
         values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    for (seq, edit) in rows {
        let edits_json = if edit.edits.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&edit.edits)?)
        };
        stmt.execute(params![
            session.id,
            session.provider.as_str(),
            seq,
            edit.ts.map(|ts| ts.to_rfc3339()),
            edit.tool,
            edit.file_path,
            edit.file_name,
            edit.new_content,
            edits_json,
        ])?;
    }
    Ok(())
}

/// Append the inclusive timestamp-window clauses and push their rfc3339 args,
/// centralizing the date filter shared by every time-scoped query (messages,
/// corrections, planning, files). `col` lets callers target `ts` or a table-qualified
/// `m.ts`. Args are pushed since-then-until to match the SQL order. The upper bound
/// covers the whole final second (see [`until_bound_text`]).
/// Unknown (`NULL`) timestamps do not match a date window. Providers/indexing paths
/// that need date-filterable rows must persist a fallback timestamp instead of letting
/// every undated row leak through every date filter.
fn push_ts_window(
    sql: &mut String,
    args: &mut Vec<rusqlite::types::Value>,
    col: &str,
    since: Option<chrono::DateTime<Utc>>,
    until: Option<chrono::DateTime<Utc>>,
) {
    use rusqlite::types::Value;
    use std::fmt::Write as _;
    // `write!` into the existing String avoids a throwaway `format!` allocation; writing to a
    // String is infallible, so the `Result` is discarded.
    if let Some(since) = since {
        let _ = write!(sql, " and {col} >= ?");
        args.push(Value::Text(since.to_rfc3339()));
    }
    if let Some(until) = until {
        let _ = write!(sql, " and {col} <= ?");
        args.push(Value::Text(until_bound_text(until)));
    }
}

fn push_file_session_filter(
    sql: &mut String,
    args: &mut Vec<rusqlite::types::Value>,
    session_id: Option<&str>,
    session: Option<&str>,
) {
    use rusqlite::types::Value;
    if let Some(session_id) = session_id {
        sql.push_str(" and session_id = ?");
        args.push(Value::Text(session_id.to_string()));
    } else if let Some(session) = session {
        sql.push_str(" and session_id like ?");
        args.push(Value::Text(format!("%{session}%")));
    }
}

fn path_prefix_patterns(prefix: &str) -> (String, String) {
    let exact = prefix.trim_end_matches('/').to_string();
    let mut escaped = String::with_capacity(exact.len() + 3);
    for ch in exact.chars() {
        match ch {
            '%' | '_' | '\\' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            other => escaped.push(other),
        }
    }
    escaped.push('/');
    escaped.push('%');
    (exact, escaped)
}

fn push_session_path_prefix(sql: &mut String, args: &mut Vec<String>, path_prefix: &str) {
    let (exact, child_pattern) = path_prefix_patterns(path_prefix);
    sql.push_str(
        " and ((coalesce(s.cwd, '') = ? or coalesce(s.cwd, '') like ? escape '\\') \
         or (coalesce(s.repo_root, '') = ? or coalesce(s.repo_root, '') like ? escape '\\') \
         or (coalesce(s.source_path, '') = ? or coalesce(s.source_path, '') like ? escape '\\')) ",
    );
    args.push(exact.clone());
    args.push(child_pattern.clone());
    args.push(exact.clone());
    args.push(child_pattern.clone());
    args.push(exact);
    args.push(child_pattern);
}

fn push_session_exclusions(sql: &mut String, args: &mut Vec<String>, filters: &SearchFilters) {
    for session_id in &filters.exclude_session_ids {
        sql.push_str(" and s.id <> ? ");
        args.push(session_id.clone());
    }
    for prefix in &filters.exclude_path_prefixes {
        let (exact, child_pattern) = path_prefix_patterns(prefix);
        sql.push_str(
            " and not ((coalesce(s.cwd, '') = ? or coalesce(s.cwd, '') like ? escape '\\') \
             or (coalesce(s.repo_root, '') = ? or coalesce(s.repo_root, '') like ? escape '\\') \
             or (coalesce(s.source_path, '') = ? or coalesce(s.source_path, '') like ? escape '\\')) ",
        );
        args.push(exact.clone());
        args.push(child_pattern.clone());
        args.push(exact.clone());
        args.push(child_pattern.clone());
        args.push(exact);
        args.push(child_pattern);
    }
}

fn push_session_time_window(
    sql: &mut String,
    args: &mut Vec<String>,
    since: Option<chrono::DateTime<Utc>>,
    until: Option<chrono::DateTime<Utc>>,
) {
    if let Some(since) = since {
        sql.push_str(" and coalesce(s.updated_at, s.created_at) >= ? ");
        args.push(since.to_rfc3339());
    }
    if let Some(until) = until {
        sql.push_str(" and coalesce(s.updated_at, s.created_at) <= ? ");
        args.push(until_bound_text(until));
    }
}

fn row_to_message_hit(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageHit> {
    let ts: Option<String> = row.get(4)?;
    Ok(MessageHit {
        session_id: row.get(0)?,
        provider: Provider::from_db_str(&row.get::<_, String>(1)?),
        seq: row.get(2)?,
        role: Role::from_db_str(&row.get::<_, String>(3)?),
        ts: ts.and_then(|value| {
            chrono::DateTime::parse_from_rfc3339(&value)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        }),
        tool_name: row.get(5)?,
        fuzzy_score: None,
        content: row.get(6)?,
    })
}

fn fuzzy_rank_message_hits(query: &str, hits: Vec<MessageHit>, limit: usize) -> Vec<MessageHit> {
    let pattern = Pattern::new(
        query,
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );
    let query_lower = query.to_lowercase();
    let mut scored: Vec<(MessageHit, bool)> = hits
        .into_par_iter()
        .map_init(
            || (NucleoMatcher::new(NucleoConfig::DEFAULT), Vec::new()),
            |(matcher, utf32_buf), mut hit| {
                let score = {
                    let haystack = Utf32Str::new(&hit.content, utf32_buf);
                    pattern.score(haystack, matcher)
                };
                score.map(|score| {
                    hit.fuzzy_score = Some(score);
                    let exact_phrase = hit.content.to_lowercase().contains(&query_lower);
                    (hit, exact_phrase)
                })
            },
        )
        .filter_map(std::convert::identity)
        .collect();
    scored.sort_by(|a, b| {
        b.0.fuzzy_score
            .unwrap_or_default()
            .cmp(&a.0.fuzzy_score.unwrap_or_default())
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.0.session_id.cmp(&b.0.session_id))
            .then_with(|| a.0.seq.cmp(&b.0.seq))
    });
    if limit > 0 {
        scored.truncate(limit);
    }
    scored.into_iter().map(|(hit, _)| hit).collect()
}

/// Translate a shell-style glob into an `(column, LIKE-pattern)` pair for the
/// `file_edits` table. A pattern without `/` matches the basename (`file_name`);
/// one containing `/` matches anywhere in the absolute `file_path` (leading `%`).
/// `*`→`%`, `?`→`_`; literal `%`/`_`/`\` are backslash-escaped (use `escape '\'`).
fn glob_clause(pattern: &str) -> (&'static str, String) {
    let mut like = String::with_capacity(pattern.len() + 1);
    for ch in pattern.chars() {
        match ch {
            '*' => like.push('%'),
            '?' => like.push('_'),
            '%' | '_' | '\\' => {
                like.push('\\');
                like.push(ch);
            }
            other => like.push(other),
        }
    }
    if pattern.contains('/') {
        ("file_path", format!("%{like}"))
    } else {
        ("file_name", like)
    }
}

macro_rules! session_record_columns {
    () => {
        "s.id, s.provider, s.provider_session_id, s.title, s.summary, s.cwd, s.repo_root, \
         s.created_at, s.updated_at, s.last_message_at, s.preview_text, s.source_path, \
         s.message_count, s.parse_version, s.raw_metadata_json, s.parse_warning, s.discovery_source"
    };
}

macro_rules! session_id_match_sql {
    () => {
        "s.id = ?1 or s.provider_session_id = ?1 or s.id like ?2 or s.provider_session_id like ?2"
    };
}

const RESOLVE_SESSION_SQL: &str = concat!(
    "select ",
    session_record_columns!(),
    ", coalesce(t.transcript_text, '') \
     from sessions s \
     left join transcripts t on t.session_id = s.id \
     where ",
    session_id_match_sql!()
);

const RESOLVE_SESSION_RECORD_SQL: &str = concat!(
    "select ",
    session_record_columns!(),
    " from sessions s where ",
    session_id_match_sql!()
);

fn unique_session_match<T>(
    value: &str,
    mut matches: Vec<T>,
    id_of: impl Fn(&T) -> &str,
) -> Result<T> {
    match matches.len() {
        0 => Err(anyhow!(
            "no session matches '{value}' — run `sessiongrep list` to see recent session \
             ids, or `sessiongrep search <keywords>` to find one"
        )),
        1 => Ok(matches.remove(0)),
        _ => {
            let shown: Vec<&str> = matches.iter().take(8).map(id_of).collect();
            let more = matches.len().saturating_sub(shown.len());
            let suffix = if more > 0 {
                format!(" (+{more} more)")
            } else {
                String::new()
            };
            Err(anyhow!(
                "session prefix '{value}' is ambiguous — {} sessions match: {}{}. \
                 Pass a longer prefix or the full id.",
                matches.len(),
                shown.join(", "),
                suffix
            ))
        }
    }
}

fn row_to_session_with_transcript(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<SessionWithTranscript> {
    Ok(SessionWithTranscript {
        session: row_to_session_record(row)?,
        transcript_text: row.get(17)?,
    })
}

fn row_to_session_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRecord> {
    let provider: String = row.get(1)?;
    Ok(SessionRecord {
        id: row.get(0)?,
        provider: provider
            .parse()
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
        provider_session_id: row.get(2)?,
        title: row.get(3)?,
        summary: row.get(4)?,
        cwd: row.get(5)?,
        repo_root: row.get(6)?,
        created_at: row
            .get::<_, Option<String>>(7)?
            .as_deref()
            .and_then(crate::util::parse_datetime),
        updated_at: row
            .get::<_, Option<String>>(8)?
            .as_deref()
            .and_then(crate::util::parse_datetime),
        last_message_at: row
            .get::<_, Option<String>>(9)?
            .as_deref()
            .and_then(crate::util::parse_datetime),
        preview_text: row.get(10)?,
        source_path: row.get(11)?,
        message_count: row.get(12)?,
        parse_version: row.get(13)?,
        raw_metadata_json: row.get(14)?,
        parse_warning: row.get(15)?,
        discovery_source: row.get(16)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_BUSY_TIMEOUT_MS: u64 = 250;
    const TEST_NO_WAIT_BUSY_TIMEOUT_MS: u64 = 0;

    #[test]
    fn glob_clause_maps_basename_and_path() {
        // No slash → basename match; `*`→`%`, `?`→`_`.
        assert_eq!(glob_clause("*.rs"), ("file_name", "%.rs".to_string()));
        assert_eq!(glob_clause("db.rs"), ("file_name", "db.rs".to_string()));
        // Slash present → full-path match, anchored with a leading `%`.
        assert_eq!(
            glob_clause("src/*.rs"),
            ("file_path", "%src/%.rs".to_string())
        );
        // LIKE specials are escaped so they match literally.
        assert_eq!(glob_clause("a_b%c"), ("file_name", "a\\_b\\%c".to_string()));
    }

    #[test]
    fn planning_usage_optionally_filters_by_command_patterns() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute(
                "insert into sessions (id, provider, provider_session_id, preview_text, \
                 source_path, parse_version, discovery_source) \
                 values ('s1','claude','s1','','/p','1','test')",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "insert into sessions (id, provider, provider_session_id, preview_text, \
                 source_path, parse_version, discovery_source) \
                 values ('s2','codex','s2','','/p2','1','test')",
                [],
            )
            .unwrap();
        let slash = |id: i64, session_id: &str, provider: &str, seq: i64, content: &str| {
            db.conn
                .execute(
                    "insert into messages (id, session_id, provider, seq, role, content) \
                     values (?1,?2,?3,?4,'slash',?5)",
                    params![id, session_id, provider, seq, content],
                )
                .unwrap();
        };
        slash(1, "s1", "claude", 0, "/ar:plannew make a plan");
        slash(2, "s1", "claude", 1, "/help");
        slash(3, "s1", "claude", 2, "/ar:plannew refine it");
        slash(4, "s2", "codex", 0, "/goal ship the fix");

        // No filter (config default) → every slash command is counted.
        let all = db.planning_usage(&MessageFilters::default(), &[]).unwrap();
        assert_eq!(all.len(), 3, "all distinct commands counted");

        // A configured filter restricts to matching commands.
        let only = db
            .planning_usage(
                &MessageFilters::default(),
                &[regex::Regex::new("plannew").unwrap()],
            )
            .unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].command, "/ar:plannew");
        assert_eq!(only[0].count, 2);

        // Shared MessageFilters must apply here too; planning is not a special search path.
        let codex = db
            .planning_usage(
                &MessageFilters {
                    provider: Some(Provider::Codex),
                    ..Default::default()
                },
                &[],
            )
            .unwrap();
        assert_eq!(codex.len(), 1);
        assert_eq!(codex[0].command, "/goal");
    }

    #[test]
    fn search_messages_uses_exact_literal_substring_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute(
                "insert into sessions (id, provider, provider_session_id, preview_text, \
                 source_path, parse_version, discovery_source) \
                 values ('s1','claude','s1','','/p','1','test')",
                [],
            )
            .unwrap();
        let insert = |id: i64, seq: i64, content: &str| {
            db.conn
                .execute(
                    "insert into messages (id, session_id, provider, seq, role, content) \
                     values (?1,'s1','claude',?2,'user',?3)",
                    params![id, seq, content],
                )
                .unwrap();
        };
        insert(1, 0, "please handle the error");
        insert(2, 1, "the handler crashed");
        insert(3, 2, "we mishandled the input");
        insert(4, 3, "error handling code here");
        insert(5, 4, "use a => b arrow");
        insert(6, 5, "literal /goal command");
        insert(7, 6, "plain goal token");
        insert(8, 7, "compile C++ today");
        insert(9, 8, "flag --path passed");

        let seqs = |query: &str| -> Vec<i64> {
            let mut v: Vec<i64> = db
                .search_messages(query, &MessageFilters::default())
                .unwrap()
                .into_iter()
                .map(|h| h.seq)
                .collect();
            v.sort();
            v
        };
        assert_eq!(seqs("handle"), vec![0, 1, 2]);
        assert_eq!(seqs("handled"), vec![2]);
        // A multi-word query is a contiguous phrase.
        assert_eq!(seqs("error handling"), vec![3]);
        assert_eq!(seqs("=>"), vec![4]);
        assert_eq!(seqs("/goal"), vec![5]);
        assert_eq!(seqs("goal"), vec![5, 6]);
        assert_eq!(seqs("C++"), vec![7]);
        assert_eq!(seqs("--path"), vec![8]);
        // Empty query lists everything (structured filters only).
        assert_eq!(seqs("").len(), 9);
        // --regex still matches arbitrary patterns over the rows (scan path).
        let re = db
            .search_messages(
                "",
                &MessageFilters {
                    regex: Some("h.ndler".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(re.into_iter().map(|h| h.seq).collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn fuzzy_message_search_ranks_approximate_matches_without_changing_literal_search() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute(
                "insert into sessions (id, provider, provider_session_id, preview_text, \
                 source_path, parse_version, discovery_source) \
                 values ('s1','claude','s1','','/p','1','test')",
                [],
            )
            .unwrap();
        for (seq, content) in [
            (
                0,
                "please avoid magic values and keep settings configurable",
            ),
            (1, "hard-coded timeout should move into config"),
            (2, "unrelated transcript text"),
            (3, "magic numbers should move into named values"),
        ] {
            db.conn
                .execute(
                    "insert into messages (session_id, provider, seq, role, content) \
                     values ('s1','claude',?1,'user',?2)",
                    params![seq, content],
                )
                .unwrap();
        }

        let literal = db
            .search_messages("magic config", &MessageFilters::default())
            .unwrap();
        assert!(literal.is_empty(), "literal search remains exact");

        let fuzzy = db
            .search_messages(
                "",
                &MessageFilters {
                    fuzzy_query: Some("magic config".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(fuzzy.iter().map(|hit| hit.seq).collect::<Vec<_>>(), vec![0]);
        assert!(fuzzy[0].fuzzy_score.is_some());

        let fuzzy_phrase = db
            .search_messages(
                "",
                &MessageFilters {
                    fuzzy_query: Some("magic values".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(fuzzy_phrase[0].seq, 0, "exact phrase wins fuzzy ties");
    }

    #[test]
    fn search_messages_path_prefix_scopes_by_session_root_and_metadata_enriches() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let session = |id: &str, cwd: &str, repo: &str, title: &str| {
            db.conn
                .execute(
                    "insert into sessions (id, provider, provider_session_id, title, cwd, \
                     repo_root, preview_text, source_path, parse_version, discovery_source) \
                     values (?1,'claude',?1,?2,?3,?4,'','/p','1','test')",
                    params![id, title, cwd, repo],
                )
                .unwrap();
        };
        session("a", "/Users/x/proj-a", "/Users/x/proj-a", "Proj A");
        session("b", "/Users/x/proj-b", "/Users/x/proj-b", "Proj B");
        let msg = |id: i64, sid: &str| {
            db.conn
                .execute(
                    "insert into messages (id, session_id, provider, seq, role, content) \
                     values (?1,?2,'claude',0,'user','shared keyword here')",
                    params![id, sid],
                )
                .unwrap();
        };
        msg(1, "a");
        msg(2, "b");

        // path_prefix scopes to sessions rooted under the prefix (cwd OR repo_root),
        // mirroring the session-level `--path` semantics.
        let scoped = db
            .search_messages(
                "keyword",
                &MessageFilters {
                    path_prefix: Some("/Users/x/proj-a".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            scoped
                .iter()
                .map(|h| h.session_id.clone())
                .collect::<Vec<_>>(),
            vec!["a"],
            "only the proj-a session matches the path prefix"
        );

        // No prefix → both sessions match.
        assert_eq!(
            db.search_messages("keyword", &MessageFilters::default())
                .unwrap()
                .len(),
            2
        );

        // session_metadata batch-enriches by id (used by the MCP search_messages serializer).
        let meta = db
            .session_metadata(&["a".to_string(), "b".to_string()])
            .unwrap();
        assert_eq!(meta["a"].cwd.as_deref(), Some("/Users/x/proj-a"));
        assert_eq!(meta["a"].repo_root.as_deref(), Some("/Users/x/proj-a"));
        assert_eq!(meta["a"].title.as_deref(), Some("Proj A"));
        assert_eq!(meta["b"].title.as_deref(), Some("Proj B"));
    }

    #[test]
    fn search_messages_excludes_paths_and_sessions_before_limit() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let session = |id: &str, cwd: &str, repo: &str, source_path: &str| {
            db.conn
                .execute(
                    "insert into sessions (id, provider, provider_session_id, cwd, repo_root, \
                     preview_text, source_path, parse_version, discovery_source) \
                     values (?1,'claude',?1,?2,?3,'',?4,'1','test')",
                    params![id, cwd, repo, source_path],
                )
                .unwrap();
        };
        session("a", "/Users/x/proj-a", "/Users/x", "/logs/a.jsonl");
        session("b", "/Users/x/proj-b", "/Users/x", "/logs/b.jsonl");
        session("c", "/Users/x/proj-c", "/Users/x", "/tmp/noisy/c.jsonl");
        for (id, seq) in [("a", 0), ("b", 0), ("c", 0)] {
            db.conn
                .execute(
                    "insert into messages (session_id, provider, seq, role, content) \
                     values (?1,'claude',?2,'user','shared needle')",
                    params![id, seq],
                )
                .unwrap();
        }

        let hits = db
            .search_messages(
                "needle",
                &MessageFilters {
                    path_prefix: Some("/Users/x".into()),
                    exclude_path_prefixes: vec!["/Users/x/proj-a".into(), "/tmp/noisy".into()],
                    limit: 1,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            hits.iter()
                .map(|hit| hit.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["b"],
            "exclusions apply before limit, including source_path exclusions"
        );

        let hits = db
            .search_messages(
                "needle",
                &MessageFilters {
                    exclude_session_ids: vec!["b".into()],
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            hits.iter()
                .map(|hit| hit.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "c"]
        );
    }

    #[test]
    fn path_prefix_matches_directory_boundary_and_escapes_like_metacharacters() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let session = |id: &str, cwd: &str| {
            db.conn
                .execute(
                    "insert into sessions (id, provider, provider_session_id, cwd, repo_root, \
                     preview_text, source_path, parse_version, discovery_source) \
                     values (?1,'claude',?1,?2,?2,'','/p','1','test')",
                    params![id, cwd],
                )
                .unwrap();
        };
        session("root", "/tmp/proj");
        session("child", "/tmp/proj/sub");
        session("sibling", "/tmp/project2");
        session("under", "/tmp/proj_under");
        session("percent", "/tmp/proj%literal");
        let msg = |id: i64, sid: &str| {
            db.conn
                .execute(
                    "insert into messages (id, session_id, provider, seq, role, content) \
                     values (?1,?2,'claude',0,'user','needle')",
                    params![id, sid],
                )
                .unwrap();
        };
        msg(1, "root");
        msg(2, "child");
        msg(3, "sibling");
        msg(4, "under");
        msg(5, "percent");

        let ids = |prefix: &str| -> Vec<String> {
            let mut ids: Vec<String> = db
                .search_messages(
                    "needle",
                    &MessageFilters {
                        path_prefix: Some(prefix.into()),
                        ..Default::default()
                    },
                )
                .unwrap()
                .into_iter()
                .map(|h| h.session_id)
                .collect();
            ids.sort();
            ids
        };

        assert_eq!(ids("/tmp/proj"), vec!["child", "root"]);
        assert_eq!(ids("/tmp/proj%literal"), vec!["percent"]);
    }

    #[test]
    fn exact_session_filter_does_not_merge_substring_matches() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        for id in ["abc", "xabcx"] {
            db.conn
                .execute(
                    "insert into sessions (id, provider, provider_session_id, preview_text, \
                     source_path, parse_version, discovery_source) \
                     values (?1,'claude',?1,'','/p','1','test')",
                    params![id],
                )
                .unwrap();
        }
        db.conn
            .execute(
                "insert into messages (id, session_id, provider, seq, role, content) \
                 values (1,'abc','claude',0,'user','same')",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "insert into messages (id, session_id, provider, seq, role, content) \
                 values (2,'xabcx','claude',0,'user','same')",
                [],
            )
            .unwrap();

        let exact = db
            .search_messages(
                "",
                &MessageFilters {
                    session_id: Some("abc".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].session_id, "abc");

        let fuzzy = db
            .search_messages(
                "",
                &MessageFilters {
                    session: Some("abc".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            fuzzy.len(),
            2,
            "exploratory --session keeps substring semantics"
        );
    }

    #[test]
    fn open_with_busy_timeout_sets_sqlite_busy_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let db =
            Db::open_with_busy_timeout(&dir.path().join("index.db"), TEST_BUSY_TIMEOUT_MS).unwrap();
        assert_eq!(db.busy_timeout_ms().unwrap(), TEST_BUSY_TIMEOUT_MS);
    }

    #[test]
    fn scoped_busy_timeout_restores_previous_value() {
        let dir = tempfile::tempdir().unwrap();
        let db =
            Db::open_with_busy_timeout(&dir.path().join("index.db"), TEST_BUSY_TIMEOUT_MS).unwrap();
        let observed = db
            .with_busy_timeout_ms(TEST_NO_WAIT_BUSY_TIMEOUT_MS, || db.busy_timeout_ms())
            .unwrap();
        assert_eq!(observed, TEST_NO_WAIT_BUSY_TIMEOUT_MS);
        assert_eq!(db.busy_timeout_ms().unwrap(), TEST_BUSY_TIMEOUT_MS);
    }

    #[test]
    fn sqlite_busy_error_detection_matches_locked_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let writer = Db::open(&path).unwrap();
        let contender = Db::open_with_busy_timeout(&path, TEST_NO_WAIT_BUSY_TIMEOUT_MS).unwrap();

        writer.conn.execute_batch("begin immediate").unwrap();
        let err = contender
            .conn
            .execute(
                "insert into sessions (id, provider, provider_session_id, preview_text, \
                 source_path, parse_version, discovery_source) \
                 values ('busy','claude','busy','','/p','1','test')",
                [],
            )
            .unwrap_err();
        let err = anyhow::Error::from(err);
        assert!(Db::is_sqlite_busy_error(&err));
        writer.conn.execute_batch("rollback").unwrap();
    }

    #[test]
    fn auto_reindex_completion_timestamp_controls_shared_freshness_window() {
        const COMPLETED_MS: i64 = 20_000;
        const INTERVAL_MS: u64 = 1_000;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let first = Db::open(&path).unwrap();
        let second = Db::open(&path).unwrap();

        assert!(!second
            .auto_reindex_is_fresh_at(COMPLETED_MS, INTERVAL_MS)
            .unwrap());
        first.mark_auto_reindex_complete_at(COMPLETED_MS).unwrap();
        assert_eq!(
            second
                .auto_reindex_completed_at()
                .unwrap()
                .unwrap()
                .timestamp_millis(),
            COMPLETED_MS
        );
        assert!(second
            .auto_reindex_is_fresh_at(COMPLETED_MS + 999, INTERVAL_MS)
            .unwrap());
        assert!(!second
            .auto_reindex_is_fresh_at(COMPLETED_MS + 1_000, INTERVAL_MS)
            .unwrap());
    }

    #[test]
    fn message_search_filters_by_session_local_seq_range() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        for id in ["claude:s1", "claude:s2"] {
            db.conn
                .execute(
                    "insert into sessions (id, provider, provider_session_id, preview_text, \
                     source_path, parse_version, discovery_source) \
                     values (?1,'claude',?1,'','/p','1','test')",
                    params![id],
                )
                .unwrap();
            for seq in 0..5 {
                db.conn
                    .execute(
                        "insert into messages (session_id, provider, seq, role, content) \
                         values (?1,'claude',?2,'user',?3)",
                        params![id, seq, format!("needle {id} {seq}")],
                    )
                    .unwrap();
            }
        }

        let bounded = db
            .search_messages(
                "needle",
                &MessageFilters {
                    session_id: Some("claude:s1".into()),
                    seq_from: Some(1),
                    seq_to: Some(3),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            bounded.iter().map(|h| h.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(bounded.iter().all(|h| h.session_id == "claude:s1"));

        let open_ended = db
            .search_messages(
                "",
                &MessageFilters {
                    session_id: Some("claude:s2".into()),
                    seq_from: Some(3),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            open_ended.iter().map(|h| h.seq).collect::<Vec<_>>(),
            vec![3, 4]
        );
    }

    #[test]
    fn find_corrections_honors_path_prefix() {
        // Regression: the analytics queries build bespoke SQL, so path_prefix must be applied
        // there too (it was silently ignored until push_path_prefix unified the predicate).
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let session = |id: &str, cwd: &str| {
            db.conn
                .execute(
                    "insert into sessions (id, provider, provider_session_id, cwd, repo_root, \
                     preview_text, source_path, parse_version, discovery_source) \
                     values (?1,'claude',?1,?2,?2,'','/p','1','test')",
                    params![id, cwd],
                )
                .unwrap();
        };
        session("a", "/Users/x/proj-a");
        session("b", "/Users/x/proj-b");
        let user_msg = |id: i64, sid: &str| {
            db.conn
                .execute(
                    "insert into messages (id, session_id, provider, seq, role, content) \
                     values (?1,?2,'claude',0,'user','that is wrong, please revert')",
                    params![id, sid],
                )
                .unwrap();
        };
        user_msg(1, "a");
        user_msg(2, "b");

        let patterns = vec![("misc".to_string(), regex::Regex::new("(?i)wrong").unwrap())];
        let scoped = db
            .find_corrections(
                &patterns,
                &MessageFilters {
                    path_prefix: Some("/Users/x/proj-a".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            scoped
                .iter()
                .map(|c| c.session_id.clone())
                .collect::<Vec<_>>(),
            vec!["a"],
            "path_prefix must scope corrections to the matching session"
        );
        // Without the prefix both sessions' corrections surface.
        assert_eq!(
            db.find_corrections(&patterns, &MessageFilters::default())
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn search_messages_filters_by_tool_name_and_surfaces_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute(
                "insert into sessions (id, provider, provider_session_id, preview_text, \
                 source_path, parse_version, discovery_source) \
                 values ('s1','claude','s1','','/p','1','test')",
                [],
            )
            .unwrap();
        let insert = |id: i64, seq: i64, role: &str, tool: Option<&str>, content: &str| {
            db.conn
                .execute(
                    "insert into messages (id, session_id, provider, seq, role, tool_name, content) \
                     values (?1,'s1','claude',?2,?3,?4,?5)",
                    params![id, seq, role, tool, content],
                )
                .unwrap();
        };
        insert(1, 0, "user", None, "run the build");
        insert(2, 1, "tool", Some("Bash"), "build ok");
        insert(3, 2, "tool", Some("Edit"), "edited the file");

        // tool_name is surfaced on the hit.
        let tools = db
            .search_messages(
                "",
                &MessageFilters {
                    role: Some(Role::Tool),
                    ..Default::default()
                },
            )
            .unwrap();
        let bash = tools
            .iter()
            .find(|h| h.seq == 1)
            .expect("Bash tool message");
        assert_eq!(bash.tool_name.as_deref(), Some("Bash"));

        // --tool is a case-insensitive substring filter and never matches NULL-tool rows.
        let only = db
            .search_messages(
                "",
                &MessageFilters {
                    tool: Some("bash".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].seq, 1);
        let none = db
            .search_messages(
                "",
                &MessageFilters {
                    tool: Some("zzz".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            none.is_empty(),
            "unknown tool matches nothing (incl. NULL-tool rows)"
        );
    }

    #[test]
    fn date_filter_until_covers_sub_second_tail_of_final_second() {
        use chrono::TimeZone;
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute(
                "insert into sessions (id, provider, provider_session_id, preview_text, \
                 source_path, parse_version, discovery_source) \
                 values ('s1','claude','s1','','/p','1','test')",
                [],
            )
            .unwrap();
        // A message stored with sub-second precision in the final second of 2026-01-15.
        db.conn
            .execute(
                "insert into messages (id, session_id, provider, seq, role, ts, content) \
                 values (1,'s1','claude',0,'user','2026-01-15T23:59:59.123456789+00:00','late')",
                [],
            )
            .unwrap();
        // dates.rs resolves `--until 2026-01-15` to the second-granular 23:59:59 instant.
        let until = Utc
            .with_ymd_and_hms(2026, 1, 15, 23, 59, 59)
            .single()
            .unwrap();
        let hits = db
            .search_messages(
                "",
                &MessageFilters {
                    until: Some(until),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            hits.len(),
            1,
            "an inclusive --until must cover sub-second timestamps in its final second"
        );
    }

    #[test]
    fn date_filter_keeps_messages_with_unknown_timestamp() {
        use chrono::TimeZone;
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute(
                "insert into sessions (id, provider, provider_session_id, preview_text, \
                 source_path, parse_version, discovery_source) \
                 values ('s1','claude','s1','','/p','1','test')",
                [],
            )
            .unwrap();
        // A message whose timestamp is unknown (NULL) — e.g. a provider/record with no ts.
        db.conn
            .execute(
                "insert into messages (id, session_id, provider, seq, role, ts, content) \
                 values (1,'s1','claude',0,'user',NULL,'undated correction')",
                [],
            )
            .unwrap();
        let since = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).single().unwrap();
        let until = Utc
            .with_ymd_and_hms(2026, 12, 31, 23, 59, 59)
            .single()
            .unwrap();
        let hits = db
            .search_messages(
                "",
                &MessageFilters {
                    since: Some(since),
                    until: Some(until),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            hits.is_empty(),
            "a NULL-timestamp message must not match every date filter; index a fallback timestamp instead"
        );
    }

    #[test]
    fn fts_candidate_query_tolerates_punctuation_only_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        // Punctuation-/operator-only tokens tokenize to an empty FTS phrase, which is a
        // MATCH syntax error; they must be dropped so search cleanly falls back to fuzzy.
        assert!(db
            .fts_candidate_ids("***", 50, FTS_CANDIDATE_FLOOR)
            .unwrap()
            .is_empty());
        assert!(db
            .fts_candidate_ids("\"", 50, FTS_CANDIDATE_FLOOR)
            .unwrap()
            .is_empty());
        assert!(db
            .fts_candidate_ids("   ", 50, FTS_CANDIDATE_FLOOR)
            .unwrap()
            .is_empty());
        // A real token mixed with punctuation must still run without error.
        assert!(db
            .fts_candidate_ids("--- hello", 50, FTS_CANDIDATE_FLOOR)
            .is_ok());
    }

    #[test]
    fn fts_candidate_floor_handles_zero_limit() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute(
                "insert into sessions (id, provider, provider_session_id, preview_text, \
                 source_path, parse_version, discovery_source) \
                 values ('s1','claude','s1','alpha preview','/p','1','test')",
                [],
            )
            .unwrap();
        let rowid: i64 = db
            .conn
            .query_row("select rowid from sessions where id='s1'", [], |r| r.get(0))
            .unwrap();
        db.conn
            .execute(
                "insert into sessions_fts(rowid, title, summary, preview_text, transcript_text) \
                 values (?1,'','','alpha preview','')",
                params![rowid],
            )
            .unwrap();
        // limit==0 (caller's unlimited) must not become SQL LIMIT 0 = zero candidates.
        let ids = db
            .fts_candidate_ids("alpha", 0, FTS_CANDIDATE_FLOOR)
            .unwrap();
        assert_eq!(ids, vec!["s1".to_string()]);
    }

    #[test]
    fn message_update_keeps_fts_in_sync() {
        // External-content messages_fts must track in-place UPDATEs, not just
        // insert/delete — otherwise a future `update messages set content=...` would
        // leave stale terms in the index and miss new ones.
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute(
                "insert into sessions (id, provider, provider_session_id, preview_text, \
                 source_path, parse_version, discovery_source) \
                 values ('s1','claude','s1','','/p','1','test')",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "insert into messages (id, session_id, provider, seq, role, content) \
                 values (1,'s1','claude',0,'user','alpha original')",
                [],
            )
            .unwrap();
        let count = |term: &str| -> i64 {
            db.conn
                .query_row(
                    "select count(*) from messages_fts where messages_fts match ?1",
                    params![term],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(count("original"), 1, "inserted content is searchable");
        db.conn
            .execute("update messages set content='beta updated' where id=1", [])
            .unwrap();
        assert_eq!(count("updated"), 1, "new content searchable after update");
        assert_eq!(count("original"), 0, "stale term dropped after update");
    }

    #[test]
    fn sessions_fts_upsert_replaces_stale_terms() {
        // Re-indexing a session whose title changed (the normal incremental-reindex
        // path) must not leave the old title's terms searchable in sessions_fts — a
        // regular (non-external-content) FTS5 table reached via the `insert or replace`
        // in upsert_session. If stale terms persisted, FTS search would return false
        // positives for content the session no longer contains.
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute(
                "insert into sessions (id, provider, provider_session_id, preview_text, \
                 source_path, parse_version, discovery_source) \
                 values ('s1','claude','s1','','/p','1','test')",
                [],
            )
            .unwrap();
        let rowid: i64 = db
            .conn
            .query_row("select rowid from sessions where id='s1'", [], |r| r.get(0))
            .unwrap();
        // Mirror upsert_session's exact FTS write (db.rs:316-329).
        let upsert_fts = |title: &str| {
            db.conn
                .execute(
                    "insert or replace into sessions_fts \
                     (rowid, title, summary, preview_text, transcript_text) \
                     values (?1, ?2, '', '', '')",
                    params![rowid, title],
                )
                .unwrap();
        };
        let count = |term: &str| -> i64 {
            db.conn
                .query_row(
                    "select count(*) from sessions_fts where sessions_fts match ?1",
                    params![term],
                    |r| r.get(0),
                )
                .unwrap()
        };
        upsert_fts("alphaunique");
        assert_eq!(
            count("alphaunique"),
            1,
            "first index makes the title searchable"
        );
        upsert_fts("betaunique");
        assert_eq!(
            count("betaunique"),
            1,
            "re-index makes the new title searchable"
        );
        assert_eq!(
            count("alphaunique"),
            0,
            "re-index must drop the old title's terms (no FTS ghost postings)"
        );
    }

    #[test]
    fn resolve_session_errors_are_actionable() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let insert = |id: &str| {
            db.conn
                .execute(
                    "insert into sessions (id, provider, provider_session_id, preview_text, \
                     source_path, parse_version, discovery_source) \
                     values (?1,'claude',?1,'','/p','1','test')",
                    params![id],
                )
                .unwrap();
        };
        insert("claude:abc123");
        insert("claude:abc456");

        // Unknown id → points at the commands that list/find sessions.
        let err = db.resolve_session("zzz").unwrap_err().to_string();
        assert!(err.contains("no session matches"));
        assert!(err.contains("sessiongrep list") || err.contains("sessiongrep search"));
        let err = db.resolve_session_record("zzz").unwrap_err().to_string();
        assert!(err.contains("no session matches"));
        assert!(err.contains("sessiongrep list") || err.contains("sessiongrep search"));

        // Ambiguous prefix → names the matching candidates so the user can disambiguate.
        let err = db.resolve_session("claude:abc").unwrap_err().to_string();
        assert!(err.contains("ambiguous"));
        assert!(
            err.contains("claude:abc123") && err.contains("claude:abc456"),
            "ambiguous error must list candidates: {err}"
        );
        let err = db
            .resolve_session_record("claude:abc")
            .unwrap_err()
            .to_string();
        assert!(err.contains("ambiguous"));
        assert!(
            err.contains("claude:abc123") && err.contains("claude:abc456"),
            "ambiguous error must list candidates: {err}"
        );
    }

    #[test]
    fn schema_backfill_flag_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        // Fresh DB: user_version defaults to 0 (< SCHEMA_VERSION) → a backfill is due. Any older
        // index (upstream baseline 0, which never set user_version, or an earlier generation) is
        // below SCHEMA_VERSION, so it migrates in a single full reindex (the gap size doesn't
        // matter — `mark_schema_current` stamps straight to SCHEMA_VERSION).
        assert!(db.needs_backfill().unwrap());
        db.mark_schema_current().unwrap();
        assert!(!db.needs_backfill().unwrap(), "stamping clears the flag");
    }

    #[test]
    fn open_drops_legacy_fts5_trigram_and_self_heals() {
        // An in-development index may carry the old FTS5 messages_trigram; opening with the current
        // binary must drop it and stand up the custom trigram_index — no out-of-repo transition code
        // needed. (Proves resetting SCHEMA_VERSION to 1 is safe for such indexes: init() fixes the
        // schema objects on open regardless of user_version.)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let db = Db::open(&path).unwrap();
            db.conn
                .execute_batch(
                    "create virtual table messages_trigram using fts5(content, \
                     content='messages', content_rowid='id', tokenize='trigram', detail='none');",
                )
                .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let legacy: i64 = db
            .conn
            .query_row(
                "select count(*) from sqlite_master where name='messages_trigram'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(legacy, 0, "legacy FTS5 messages_trigram dropped on open");
        let custom: i64 = db
            .conn
            .query_row(
                "select count(*) from sqlite_master where name='trigram_postings'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(custom, 1, "custom trigram_postings present after open");
        seed_messages(&db, &[("user", "an econnreset row")]);
        let hits = db
            .search_messages(
                "",
                &MessageFilters {
                    regex: Some("econnreset".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(hits.len(), 1, "regex search works after the self-heal");
    }

    #[test]
    fn messages_indexes_drop_redundant_singles_and_keep_composites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let count_index = |db: &Db, name: &str| -> i64 {
            db.conn
                .query_row(
                    "select count(*) from sqlite_master where type='index' and name=?1",
                    [name],
                    |row| row.get(0),
                )
                .unwrap()
        };

        // Simulate an older branch build that created the now-redundant standalone indexes.
        {
            let db = Db::open(&path).unwrap();
            db.conn
                .execute_batch(
                    "create index if not exists idx_messages_session on messages(session_id);
                     create index if not exists idx_messages_role on messages(role);",
                )
                .unwrap();
            assert_eq!(count_index(&db, "idx_messages_session"), 1, "precondition");
            assert_eq!(count_index(&db, "idx_messages_role"), 1, "precondition");
        }

        // Reopening runs init(), whose `drop index if exists` removes the redundant singles
        // (the composites subsume them by leftmost-prefix) and leaves the final index shape.
        let db = Db::open(&path).unwrap();
        assert_eq!(
            count_index(&db, "idx_messages_session"),
            0,
            "redundant (session_id) dropped"
        );
        assert_eq!(
            count_index(&db, "idx_messages_role"),
            0,
            "redundant (role) dropped"
        );
        for idx in [
            "idx_messages_session_seq",
            "idx_messages_role_ts",
            "idx_messages_ts",
        ] {
            assert_eq!(count_index(&db, idx), 1, "{idx} must exist");
        }
    }

    #[test]
    fn hot_message_queries_use_indexes_not_full_scans() {
        // Performance regression guard: the hot message queries must be served by an index
        // (or the FTS virtual table), never a full `SCAN` of the multi-GB messages table.
        // We populate enough rows and run ANALYZE so the planner's choice is statistics-
        // driven and deterministic, matching production rather than a tiny-table heuristic.
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute_batch(
                "insert into sessions(id, provider, provider_session_id, preview_text, \
                   source_path, parse_version, discovery_source) \
                 values('claude:s1','claude','s1','','/x','claude-v1','jsonl');",
            )
            .unwrap();
        {
            let tx = db.conn.unchecked_transaction().unwrap();
            {
                let mut stmt = tx
                    .prepare(
                        "insert into messages(session_id, provider, seq, role, ts, content) \
                         values('claude:s1','claude',?1,?2,?3,?4)",
                    )
                    .unwrap();
                for i in 0..2000i64 {
                    let role = if i % 7 == 0 { "user" } else { "assistant" };
                    let ts = format!("2026-06-{:02}T00:00:00+00:00", (i % 28) + 1);
                    stmt.execute(params![i, role, ts, format!("message number {i} alpha")])
                        .unwrap();
                }
            }
            tx.commit().unwrap();
        }
        db.conn.execute_batch("analyze").unwrap();

        // Join the EXPLAIN QUERY PLAN `detail` column (index 3) for each query.
        let plan = |sql: &str| -> String {
            let mut stmt = db
                .conn
                .prepare(&format!("explain query plan {sql}"))
                .unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(3)).unwrap();
            rows.filter_map(Result::ok).collect::<Vec<_>>().join(" | ")
        };

        // 1. Content search is driven by the messages_fts index (MATCH): the FTS virtual
        //    table supplies matching rowids and the messages rows are fetched by INTEGER
        //    PRIMARY KEY — never a full scan of the messages table.
        let p = plan(
            "select m.id from messages_fts f join messages m on m.id = f.rowid \
             where messages_fts match 'alpha'",
        );
        assert!(
            p.contains("VIRTUAL TABLE INDEX"),
            "content search must be driven by the messages_fts index: {p}"
        );
        assert!(
            p.contains("USING INTEGER PRIMARY KEY") && !p.contains("SCAN m "),
            "messages rows must be reached by rowid from the FTS matches, not scanned: {p}"
        );

        // 2. role [+ order by ts] (corrections / planning / stats) → idx_messages_role_ts.
        let p = plan("select content from messages where role = 'user' order by ts desc");
        assert!(
            p.contains("idx_messages_role_ts"),
            "role/ts query must use the composite: {p}"
        );

        // 3. session_id + seq range (message get / context) → idx_messages_session_seq.
        let p = plan(
            "select content from messages where session_id = 'claude:s1' \
             and seq between 10 and 20 order by seq",
        );
        assert!(
            p.contains("idx_messages_session_seq"),
            "session/seq query must use the composite: {p}"
        );
    }

    #[test]
    fn messages_fts_is_rebuilt_when_empty_but_messages_exist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        // Populate one message (triggers fill messages_fts), then simulate an index that
        // predates messages_fts by dropping the FTS shadow + its sync triggers.
        {
            let db = Db::open(&path).unwrap();
            db.conn
                .execute_batch(
                    "insert into sessions(id, provider, provider_session_id, preview_text, \
                       source_path, parse_version, discovery_source) \
                     values('claude:s1','claude','s1','','/x','claude-v1','jsonl'); \
                     insert into messages(session_id, provider, seq, role, content) \
                     values('claude:s1','claude',0,'user','findthisneedle');",
                )
                .unwrap();
            let hit: i64 = db
                .conn
                .query_row(
                    "select count(*) from messages_fts where messages_fts match 'findthisneedle'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(hit, 1, "precondition: triggers index the inserted message");
            db.conn
                .execute_batch(
                    "drop trigger messages_ai; drop trigger messages_ad; drop trigger messages_au; \
                     drop table messages_fts;",
                )
                .unwrap();
        }
        // Reopen: init() recreates messages_fts (empty) + triggers, and the integrity net
        // rebuilds it from the messages content table so search works again.
        let db = Db::open(&path).unwrap();
        let hit: i64 = db
            .conn
            .query_row(
                "select count(*) from messages_fts where messages_fts match 'findthisneedle'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            hit, 1,
            "messages_fts rebuilt on open when empty but messages exist"
        );
    }

    #[test]
    fn messages_fts_count_reports_index_documents_not_content_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute_batch(
                "insert into sessions(id, provider, provider_session_id, preview_text, \
                   source_path, parse_version, discovery_source) \
                 values('claude:s1','claude','s1','','/x','claude-v1','jsonl'); \
                 insert into messages(session_id, provider, seq, role, content) \
                 values('claude:s1','claude',0,'user','indexedtoken');",
            )
            .unwrap();
        assert_eq!(db.message_count().unwrap(), 1);
        assert_eq!(db.messages_fts_count().unwrap(), 1);

        // Simulate a broken/empty FTS index while leaving the external content table
        // (`messages`) populated. FTS5's external-content table view still reports the
        // content row; only the `_docsize` shadow exposes that no document is indexed.
        db.conn
            .execute("delete from messages_fts_docsize", [])
            .unwrap();
        let external_content_rows: i64 = db
            .conn
            .query_row("select count(*) from messages_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(external_content_rows, 1);
        assert_eq!(
            db.messages_fts_count().unwrap(),
            0,
            "helper must report indexed docs, not external content rows"
        );
    }

    #[test]
    fn substring_search_matches_inside_tokens_via_custom_index() {
        // The trigram prefilter matches ARBITRARY substrings (inside a token, and multi-word
        // phrases), built lazily by the custom trigram index on first regex use. Exercised
        // end-to-end via the public search_messages regex path.
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        seed_messages(
            &db,
            &[
                ("user", "the socket failed with ECONNRESET) today"),
                ("user", "you forgot the tests again"),
                ("assistant", "an unrelated message"),
            ],
        );
        let find = |needle: &str| -> usize {
            db.search_messages(
                "",
                &MessageFilters {
                    regex: Some(needle.into()),
                    ..Default::default()
                },
            )
            .unwrap()
            .len()
        };
        // 'ECONNRESET' is INSIDE the token 'ECONNRESET)' — only a substring index finds it.
        assert_eq!(find("ECONNRESET"), 1, "substring inside a token");
        assert_eq!(find("you forgot"), 1, "multi-word phrase substring");
        assert_eq!(find("nonexistent_zz"), 0, "no false positives");
    }

    #[test]
    fn trigram_base_rebuild_restores_searchability_including_short_docs() {
        // Regression: building the custom trigram base from existing content makes EVERY row
        // searchable — including a <3-char message that produces zero trigrams (it must not break
        // the build or the base_max accounting, and must not silently drop the other rows).
        // Exercised via the public search path, which builds the base lazily on first regex use.
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        seed_messages(
            &db,
            &[
                ("user", "alpha contains zebracode here"),
                ("user", "bravo contains zebracode too"),
                ("user", "charlie has zebracode as well"),
                ("user", "ok"), // zero-trigram short doc must not break the build
            ],
        );
        let hits = db
            .search_messages(
                "",
                &MessageFilters {
                    regex: Some("zebracode".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            hits.len(),
            3,
            "every zebracode row searchable after base build"
        );
        assert_eq!(
            crate::trigram_index::base_max_id(&db.conn).unwrap(),
            4,
            "base covers all messages including the zero-trigram short doc"
        );
    }

    #[test]
    fn messages_fts_updates_are_transactional_with_messages() {
        // #235 RAII / crash-safety: the messages_fts trigger updates are atomic with the message
        // rows. A rolled-back message insert must leave NEITHER a message row NOR an FTS entry — the
        // trigger writes participate in the surrounding transaction and unwind on rollback.
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute_batch(
                "insert into sessions(id, provider, provider_session_id, preview_text, \
                   source_path, parse_version, discovery_source) \
                 values('claude:s1','claude','s1','','/x','claude-v1','jsonl');",
            )
            .unwrap();
        let before = db.messages_fts_count().unwrap();
        {
            // Open a transaction, insert a message (the ai trigger indexes it into messages_fts),
            // then DROP the tx without committing → rollback.
            let tx = db.conn.unchecked_transaction().unwrap();
            tx.execute(
                "insert into messages(session_id, provider, seq, role, content) \
                 values('claude:s1','claude',0,'user','rollbackme token here')",
                [],
            )
            .unwrap();
        }
        assert_eq!(
            db.messages_fts_count().unwrap(),
            before,
            "rolled-back insert leaves no messages_fts entry"
        );
        let hit: i64 = db
            .conn
            .query_row(
                "select count(*) from messages_fts where messages_fts match 'rollbackme'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            hit, 0,
            "rolled-back content is not searchable via messages_fts"
        );
    }

    #[test]
    fn generated_trigram_query_is_a_superset_in_the_custom_index() {
        // P0b (closes the R1 gap): build the ACTUAL custom trigram index and assert its candidate
        // set (via the structured trigram_prefilter_groups) is a SUPERSET of regex matches.
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute_batch(
                "insert into sessions(id, provider, provider_session_id, preview_text, \
                   source_path, parse_version, discovery_source) \
                 values('claude:s1','claude','s1','','/x','claude-v1','jsonl');",
            )
            .unwrap();
        let rows = [
            "you forgot the tests",
            "well You Forgot it",
            "no, that's wrong",
            "we also need more coverage",
            "socket hang up ECONNRESET here",
            "please stop doing that",
            "totally unrelated message",
            "scatter the cats", // contains 'cat' as a substring but no word boundary
        ];
        {
            let tx = db.conn.unchecked_transaction().unwrap();
            {
                let mut stmt = tx
                    .prepare(
                        "insert into messages(session_id, provider, seq, role, content) \
                         values('claude:s1','claude',?1,'user',?2)",
                    )
                    .unwrap();
                for (i, row) in rows.iter().enumerate() {
                    stmt.execute(params![i as i64, row]).unwrap();
                }
            }
            tx.commit().unwrap();
        }
        crate::trigram_index::build(&db.conn).unwrap();
        let patterns = [
            r"\byou forgot\b",
            r"\bno,?\s+that'?s\b",
            r"\balso need\b",
            "ECONNRESET",
            r"\bstop doing\b",
            r"\bcat\b", // matches none (no boundary), but candidate "scatter the cats" is a superset
        ];
        for pat in patterns {
            let regex = regex::Regex::new(&format!("(?i){pat}")).unwrap();
            // Ground truth: ids whose content the regex matches.
            let expected: Vec<i64> = {
                let mut stmt = db.conn.prepare("select id, content from messages").unwrap();
                let iter = stmt
                    .query_map([], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                    })
                    .unwrap();
                iter.filter_map(Result::ok)
                    .filter(|(_, c)| regex.is_match(c))
                    .map(|(id, _)| id)
                    .collect()
            };
            if let Some(groups) = crate::trigram::trigram_prefilter_groups(pat) {
                let candidates = crate::trigram_index::candidates(&db.conn, &groups).unwrap();
                for id in &expected {
                    assert!(
                        candidates.contains(id),
                        "SUPERSET VIOLATION: {pat:?} -> {groups:?} missed id {id}",
                    );
                }
            }
            // When None, the caller falls back to a scan, which is trivially a superset.
        }
    }

    #[test]
    fn detail_mode_comparison_like_on_external_content() {
        // #230: empirically compare the trigram methods for an EXTERNAL-CONTENT table over
        // `messages`, to ground the #231 decision with real numbers (NOT the multi-GB real DB):
        //   (A) detail='full' + MATCH phrase  — the original baseline (positions stored; biggest).
        //   (B) detail='none' + content LIKE  — works on external content (FTS5 fetches the value
        //       from `messages` to reject false-positives; the SQLite forum notes LIKE/GLOB *fails*
        //       on fully *contentless* tables, which ours is not).
        //   (C) detail='none' + AND-of-trigrams MATCH — THE METHOD WE ADOPTED. The prefilter never
        //       needs adjacency (the regex re-verifies), so trigrams are ANDed as independent terms
        //       instead of a phrase; that reads only doclists, so detail='none' (no positions) is
        //       sufficient and ~3-5x smaller. This test asserts (C) returns the right rows on a
        //       real detail='none' external-content table and reports the size delta vs (A).
        let dir = tempfile::tempdir().unwrap();
        let size_of = |variant: &str, detail_clause: &str| -> (i64, rusqlite::Connection) {
            let conn =
                rusqlite::Connection::open(dir.path().join(format!("{variant}.db"))).unwrap();
            conn.execute_batch(
                "create table messages(id integer primary key, content text not null);",
            )
            .unwrap();
            {
                let tx = conn.unchecked_transaction().unwrap();
                {
                    let mut stmt = tx
                        .prepare("insert into messages(content) values(?1)")
                        .unwrap();
                    stmt.execute(["the socket failed with ECONNRESET) today"])
                        .unwrap();
                    stmt.execute(["you forgot the tests again"]).unwrap();
                    stmt.execute(["an unrelated assistant message"]).unwrap();
                    // Filler so the index-size delta between the two detail modes is measurable.
                    for i in 0..3000 {
                        stmt.execute([format!(
                            "filler row {i} lorem ipsum dolor sit amet consectetur adipiscing"
                        )])
                        .unwrap();
                    }
                }
                tx.commit().unwrap();
            }
            conn.execute_batch(&format!(
                "create virtual table tri using fts5(content, content='messages', \
                   content_rowid='id', tokenize='trigram'{detail_clause}); \
                 insert into tri(tri) values('rebuild');",
            ))
            .unwrap();
            let pages: i64 = conn
                .query_row("pragma page_count", [], |r| r.get(0))
                .unwrap();
            let page_size: i64 = conn
                .query_row("pragma page_size", [], |r| r.get(0))
                .unwrap();
            (pages * page_size, conn)
        };

        let (full_bytes, full) = size_of("full", "");
        let (none_bytes, none) = size_of("none", ", detail='none'");

        // (A) detail='full' + MATCH: substring inside a token + multi-word phrase.
        let full_match = |q: &str| -> i64 {
            full.query_row("select count(*) from tri where tri match ?1", [q], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert_eq!(
            full_match("\"econnreset\""),
            1,
            "detail=full MATCH substring-in-token"
        );
        assert_eq!(
            full_match("\"you forgot\""),
            1,
            "detail=full MATCH multi-word phrase"
        );

        // (B) THE key question: detail='none' + LIKE on EXTERNAL content. If FTS5 fetches the
        // value from `messages` to verify, these return the correct row; if it behaves like a
        // contentless table, they return 0 and detail='none'+LIKE is NOT viable here.
        let none_like = |needle: &str| -> i64 {
            none.query_row(
                "select count(*) from tri where content like ?1 escape '\\'",
                [format!("%{needle}%")],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            none_like("econnreset"),
            1,
            "detail=none LIKE substring-in-token MUST work on external content",
        );
        assert_eq!(
            none_like("you forgot"),
            1,
            "detail=none LIKE multi-word phrase MUST work on external content",
        );
        // A non-existent substring must return nothing (guards against a silent full match).
        assert_eq!(
            none_like("zzqqxx_absent"),
            0,
            "detail=none LIKE rejects absent substring"
        );

        // (C) detail='none' + AND-of-trigrams MATCH — THE PRODUCTION METHOD. Build the query the
        // way `trigram_prefilter` does (boolean AND of the needle's 3-grams, no phrase) and run it
        // against the real detail='none' table. It must return the matching row(s) (a SUPERSET the
        // caller's regex then verifies) and reject an absent needle — proving detail='none' is
        // sufficient for our prefilter without the positions that detail='full' would store.
        let none_match = |needle: &str| -> i64 {
            let query = crate::trigram::trigram_prefilter(needle)
                .unwrap_or_else(|| panic!("needle {needle:?} must be prefilterable"));
            none.query_row(
                "select count(*) from tri where tri match ?1",
                [query],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(
            none_match("econnreset") >= 1,
            "detail=none AND-of-trigrams MATCH finds substring-in-token on external content",
        );
        assert!(
            none_match("you forgot") >= 1,
            "detail=none AND-of-trigrams MATCH finds multi-word substring on external content",
        );
        assert_eq!(
            none_match("zzqqwwxx"),
            0,
            "detail=none AND-of-trigrams MATCH rejects an absent needle",
        );

        // (D) Confirm LIKE actually engages the trigram index rather than scanning `messages`.
        let plan: String = {
            let mut stmt = none
                .prepare(
                    "explain query plan select rowid from tri \
                     where content like '%econnreset%' escape '\\'",
                )
                .unwrap();
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(3))
                .unwrap()
                .filter_map(Result::ok)
                .collect::<Vec<_>>()
                .join(" | ");
            rows
        };
        // (E) Report the measured deltas for the #231 decision.
        eprintln!(
            "[#230] index size: detail=full={}KB  detail=none={}KB  (full is {:.1}x none)",
            full_bytes / 1024,
            none_bytes / 1024,
            full_bytes as f64 / none_bytes.max(1) as f64,
        );
        eprintln!("[#230] detail=none LIKE query plan: {plan}");
        assert!(
            !plan.to_lowercase().contains("scan messages"),
            "detail=none LIKE should not linear-scan the messages table; plan was: {plan}",
        );
    }

    #[test]
    fn regex_search_composes_with_role_and_session_scope() {
        // Each search scans only its needed SUBSET: a --regex query restricts via role / session
        // filters (the trigram prefilter is a SUPERSET the Rust regex re-verifies). Exercised via
        // the public search_messages path, which uses the custom trigram index (built lazily).
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute_batch(
                "insert into sessions(id, provider, provider_session_id, preview_text, \
                   source_path, parse_version, discovery_source) values \
                   ('claude:a','claude','a','','/x','v1','jsonl'), \
                   ('claude:b','claude','b','','/x','v1','jsonl');",
            )
            .unwrap();
        {
            let tx = db.conn.unchecked_transaction().unwrap();
            {
                let mut stmt = tx
                    .prepare(
                        "insert into messages(session_id, provider, seq, role, content) \
                         values(?1,'claude',?2,?3,?4)",
                    )
                    .unwrap();
                stmt.execute(params!["claude:a", 0, "user", "needle_xyz in a user"])
                    .unwrap();
                stmt.execute(params![
                    "claude:a",
                    1,
                    "assistant",
                    "needle_xyz in a assistant"
                ])
                .unwrap();
                stmt.execute(params!["claude:b", 0, "user", "needle_xyz in b user"])
                    .unwrap();
            }
            tx.commit().unwrap();
        }
        let count = |role: Option<Role>, session: Option<&str>| -> usize {
            db.search_messages(
                "",
                &MessageFilters {
                    regex: Some("needle_xyz".into()),
                    role,
                    session: session.map(str::to_string),
                    ..Default::default()
                },
            )
            .unwrap()
            .len()
        };
        assert_eq!(count(None, None), 3, "unscoped: all three rows");
        assert_eq!(
            count(Some(Role::User), None),
            2,
            "role scope narrows to user rows"
        );
        assert_eq!(
            count(None, Some("claude:a")),
            2,
            "session scope narrows to session a"
        );
        assert_eq!(
            count(Some(Role::User), Some("claude:a")),
            1,
            "role+session scope composes",
        );
    }

    #[test]
    fn trigram_search_correct_across_all_providers() {
        // #234: the trigram index + the real trigram_prefilter() generator return correct results
        // for every harness's content SHAPE — dense JSON (claude tool), code+markdown (codex),
        // short text (pi), unicode (antigravity), and through provider scoping.
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        for p in [
            "claude",
            "claude-desktop",
            "codex",
            "cursor",
            "antigravity",
            "pi",
        ] {
            db.conn
                .execute(
                    "insert into sessions(id, provider, provider_session_id, preview_text, \
                       source_path, parse_version, discovery_source) \
                     values(?1, ?2, 's', '', '/x', 'v1', 'jsonl')",
                    params![format!("{p}:s"), p],
                )
                .unwrap();
        }
        // Each provider-shaped message contains 'ECONNRESET' inside a token; only the cursor one
        // also contains the correction phrase 'you forgot'.
        let rows: &[(&str, &str, &str)] = &[
            (
                "claude",
                "tool",
                r#"{"type":"tool_result","content":"net error ECONNRESET) deploy"}"#,
            ),
            (
                "claude-desktop",
                "assistant",
                "Desktop local agent saw ECONNRESET too",
            ),
            (
                "codex",
                "assistant",
                "```rust\nconnect()?; // ECONNRESET retry\n```",
            ),
            ("cursor", "user", "hey you forgot the ECONNRESET retry path"),
            (
                "antigravity",
                "assistant",
                "MODEL: ECONNRESET observed — naïve café résumé",
            ),
            ("pi", "user", "ECONNRESET again"),
        ];
        {
            let tx = db.conn.unchecked_transaction().unwrap();
            {
                let mut stmt = tx
                    .prepare(
                        "insert into messages(session_id, provider, seq, role, content) \
                         values(?1,?2,?3,?4,?5)",
                    )
                    .unwrap();
                for (i, (p, role, content)) in rows.iter().enumerate() {
                    stmt.execute(params![format!("{p}:s"), p, i as i64, role, content])
                        .unwrap();
                }
            }
            tx.commit().unwrap();
        }
        // Substring 'ECONNRESET' (inside JSON / code / plain / unicode) hits ALL providers via the
        // public regex search (custom trigram index, lazily built on first use).
        let providers_for = |filters: MessageFilters| -> Vec<String> {
            let mut got: Vec<String> = db
                .search_messages("", &filters)
                .unwrap()
                .into_iter()
                .map(|h| h.provider.as_str().to_string())
                .collect();
            got.sort();
            got
        };
        let all = providers_for(MessageFilters {
            regex: Some("ECONNRESET".into()),
            ..Default::default()
        });
        assert_eq!(
            all.len(),
            6,
            "every provider's ECONNRESET found regardless of content shape"
        );
        let claude = providers_for(MessageFilters {
            regex: Some("ECONNRESET".into()),
            provider: Some(Provider::Claude),
            ..Default::default()
        });
        assert_eq!(
            claude,
            vec!["claude"],
            "provider scope restricts to the claude message"
        );
        let claude_desktop = providers_for(MessageFilters {
            regex: Some("ECONNRESET".into()),
            provider: Some(Provider::ClaudeDesktop),
            ..Default::default()
        });
        assert_eq!(
            claude_desktop,
            vec!["claude-desktop"],
            "provider scope restricts to the claude-desktop message"
        );
        // The correction phrase 'you forgot' appears only in the cursor message.
        let forgot = providers_for(MessageFilters {
            regex: Some(r"\byou forgot\b".into()),
            ..Default::default()
        });
        assert_eq!(
            forgot,
            vec!["cursor"],
            "you-forgot regex selects exactly cursor"
        );
    }

    /// Insert one claude session + the given (seq, role, content) rows for the wiring tests.
    #[cfg(test)]
    fn seed_messages(db: &Db, rows: &[(&str, &str)]) {
        db.conn
            .execute_batch(
                "insert into sessions(id, provider, provider_session_id, preview_text, \
                   source_path, parse_version, discovery_source) \
                 values('claude:s1','claude','s1','','/x','v1','jsonl');",
            )
            .unwrap();
        let tx = db.conn.unchecked_transaction().unwrap();
        {
            let mut stmt = tx
                .prepare(
                    "insert into messages(session_id, provider, seq, role, content) \
                     values('claude:s1','claude',?1,?2,?3)",
                )
                .unwrap();
            for (i, (role, content)) in rows.iter().enumerate() {
                stmt.execute(params![i as i64, role, content]).unwrap();
            }
        }
        tx.commit().unwrap();
    }

    #[test]
    fn purge_injected_messages_removes_only_leading_marker_user_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        seed_messages(
            &db,
            &[
                (
                    "user",
                    "<local-command-stdout>Set model to Opus</local-command-stdout>",
                ), // purge
                ("user", "<local-command-stderr>boom</local-command-stderr>"), // purge
                (
                    "user",
                    "<environment_context>\n<current_date>2026</current_date>",
                ), // purge
                ("user", "fix the failing login test before release"),         // KEEP (prompt)
                ("user", "what does <local-command-stdout> mean in the logs"), // KEEP (not leading)
                (
                    "assistant",
                    "<local-command-stdout>tool output</local-command-stdout>",
                ), // KEEP (not user)
            ],
        );
        let before = db.message_count().unwrap();
        let purged = db.purge_injected_messages().unwrap();
        assert_eq!(purged, 3, "the three leading-marker USER rows are deleted");
        assert_eq!(db.message_count().unwrap(), before - 3);
        // FTS + trigram stay in sync via the delete triggers.
        assert_eq!(
            db.messages_fts_count().unwrap(),
            db.message_count().unwrap()
        );
        let users: Vec<String> = db
            .search_messages(
                "",
                &MessageFilters {
                    role: Some(Role::User),
                    ..Default::default()
                },
            )
            .unwrap()
            .into_iter()
            .map(|h| h.content)
            .collect();
        assert!(
            users
                .iter()
                .any(|c| c.contains("fix the failing login test")),
            "real prompt kept"
        );
        assert!(
            users
                .iter()
                .any(|c| c.starts_with("what does <local-command-stdout>")),
            "a non-leading mention is kept"
        );
        assert_eq!(
            users.len(),
            2,
            "exactly the two legitimate user messages remain"
        );
    }

    #[test]
    fn regex_search_lazily_builds_custom_trigram_base_and_is_correct() {
        // The --regex path must call ensure_trigram_base() so an UNBUILT custom index (e.g. right
        // after a fresh reindex that does no trigram work) does not silently drop matches: the
        // search builds the base on first use and returns the correct row.
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        seed_messages(&db, &[("user", "socket failed with ECONNRESET) today")]);
        // Precondition: the custom base index has not been built yet (lazy by construction).
        assert_eq!(
            crate::trigram_index::base_max_id(&db.conn).unwrap(),
            0,
            "precondition: custom trigram base not built before first regex use"
        );
        let filters = MessageFilters {
            regex: Some("ECONNRESET".into()),
            ..Default::default()
        };
        let hits = db.search_messages("", &filters).unwrap();
        assert_eq!(
            hits.len(),
            1,
            "regex search returns the match despite an unbuilt index"
        );
        assert!(
            crate::trigram_index::base_max_id(&db.conn).unwrap() > 0,
            "regex search lazily built the custom trigram base index"
        );
    }

    #[test]
    fn lazy_trigram_build_busy_writer_falls_back_to_delta_scan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let writer = Db::open(&path).unwrap();
        let reader = Db::open_with_busy_timeout(&path, TEST_NO_WAIT_BUSY_TIMEOUT_MS).unwrap();
        seed_messages(
            &writer,
            &[
                (
                    "user",
                    "the deploy hit ECONNRESET while the trigram base was empty",
                ),
                ("assistant", "ack"),
            ],
        );
        assert_eq!(
            crate::trigram_index::base_max_id(&reader.conn).unwrap(),
            0,
            "precondition: custom trigram base starts empty"
        );

        writer.conn.execute_batch("begin immediate").unwrap();
        let filters = MessageFilters {
            regex: Some("ECONNRESET".into()),
            ..Default::default()
        };
        let hits = reader.search_messages("", &filters).unwrap();
        writer.conn.execute_batch("rollback").unwrap();

        assert_eq!(hits.len(), 1, "busy lazy build must not drop regex hits");
        assert_eq!(
            crate::trigram_index::base_max_id(&reader.conn).unwrap(),
            0,
            "busy fallback serves the existing base and leaves rebuild for a later query"
        );
    }

    #[test]
    fn progress_reporter_fires_on_lazy_build_only_and_is_silent_when_unset() {
        use std::cell::Cell;
        use std::rc::Rc;
        // Unset reporter: the library builds silently (no panic, no I/O), returns base_max.
        let dir = tempfile::tempdir().unwrap();
        let silent = Db::open(&dir.path().join("a.db")).unwrap();
        seed_messages(&silent, &[("user", "econnreset here")]);
        assert_eq!(silent.ensure_trigram_base().unwrap(), 1);

        // Injected reporter: fires exactly once, when (and only when) a build happens.
        let dir2 = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir2.path().join("b.db")).unwrap();
        let calls = Rc::new(Cell::new(0u32));
        let counter = calls.clone();
        db.set_progress_reporter(move |_msg| counter.set(counter.get() + 1));
        seed_messages(&db, &[("user", "econnreset here")]);
        assert_eq!(db.ensure_trigram_base().unwrap(), 1);
        assert_eq!(calls.get(), 1, "reporter fires once for the one-time build");
        db.ensure_trigram_base().unwrap();
        assert_eq!(calls.get(), 1, "no report when the base is already current");
    }

    #[test]
    fn search_messages_regex_prunes_lookaround_and_falls_back() {
        // #223 correctness: the prefilter only NARROWS; the Rust regex still verifies, so a
        // trigram candidate the full regex rejects (look-around) is pruned. Non-prefilterable
        // patterns (no >=3-char literal) fall back to a scan and stay correct.
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        seed_messages(
            &db,
            &[
                ("user", "socket failed with ECONNRESET) today"),
                ("user", "you forgot the tests"),
                ("assistant", "scatter the cats"), // contains 'cat' but NOT \bcat\b
                ("user", "a cat sat here"),        // matches \bcat\b
                ("assistant", "totally unrelated text 1234"),
            ],
        );
        let run = |pattern: &str| -> Vec<String> {
            let filters = MessageFilters {
                regex: Some(pattern.to_string()),
                ..Default::default()
            };
            let mut got: Vec<String> = db
                .search_messages("", &filters)
                .unwrap()
                .into_iter()
                .map(|h| h.content)
                .collect();
            got.sort();
            got
        };
        assert_eq!(
            run(r"\bcat\b"),
            vec!["a cat sat here".to_string()],
            "look-around pruned"
        );
        assert_eq!(
            run("ECONNRESET"),
            vec!["socket failed with ECONNRESET) today".to_string()],
            "substring inside a token",
        );
        assert_eq!(
            run(r"\byou forgot\b"),
            vec!["you forgot the tests".to_string()],
            "phrase"
        );
        assert_eq!(
            run(r"\d{4}"),
            vec!["totally unrelated text 1234".to_string()],
            "non-prefilterable pattern falls back to scan, still correct",
        );
        assert!(run(r"[0-9]{9}").is_empty(), "non-prefilterable, no match");
    }

    #[test]
    fn search_messages_provider_filter_scopes() {
        // #223 --provider scope: the new provider filter restricts results to one harness.
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        for p in ["claude", "codex"] {
            db.conn
                .execute(
                    "insert into sessions(id, provider, provider_session_id, preview_text, \
                       source_path, parse_version, discovery_source) \
                     values(?1, ?2, 's', '', '/x', 'v1', 'jsonl')",
                    params![format!("{p}:s"), p],
                )
                .unwrap();
            db.conn
                .execute(
                    "insert into messages(session_id, provider, seq, role, content) \
                     values(?1, ?2, 0, 'user', 'shared ECONNRESET token')",
                    params![format!("{p}:s"), p],
                )
                .unwrap();
        }
        let scoped = |provider: Option<Provider>| -> usize {
            let filters = MessageFilters {
                regex: Some("ECONNRESET".into()),
                provider,
                ..Default::default()
            };
            db.search_messages("", &filters).unwrap().len()
        };
        assert_eq!(scoped(None), 2, "unscoped: both providers");
        assert_eq!(scoped(Some(Provider::Claude)), 1, "scoped to claude");
        assert_eq!(scoped(Some(Provider::Codex)), 1, "scoped to codex");
    }

    #[test]
    fn find_corrections_scans_user_rows_and_classifies() {
        // #224 (revised): corrections scans only `role='user'` rows directly — no trigram prefilter
        // (see `find_corrections` doc: the role filter is the selective one, the prefilter would
        // only add cost). Verify it classifies each user row against the ordered patterns and
        // ignores non-user roles even when their content matches a pattern.
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute_batch(
                "insert into sessions(id, provider, provider_session_id, preview_text, \
                   source_path, parse_version, discovery_source) \
                 values('claude:s1','claude','s1','','/x','v1','jsonl');",
            )
            .unwrap();
        let rows = [
            ("user", "you forgot the unit tests again"), // skip_step
            ("user", "we also need integration coverage"), // incomplete
            ("user", "looks great, ship it"),            // no correction
            ("assistant", "you forgot nothing, here is the fix"), // role=assistant → ignored
            ("user", "the deploy hit econnreset once more"), // no correction
        ];
        {
            let tx = db.conn.unchecked_transaction().unwrap();
            {
                let mut stmt = tx
                    .prepare(
                        "insert into messages(session_id, provider, seq, role, content) \
                         values('claude:s1','claude',?1,?2,?3)",
                    )
                    .unwrap();
                for (i, (role, content)) in rows.iter().enumerate() {
                    stmt.execute(params![i as i64, role, content]).unwrap();
                }
            }
            tx.commit().unwrap();
        }
        let patterns = vec![
            (
                "skip_step".to_string(),
                regex::Regex::new(r"(?i)\byou forgot\b").unwrap(),
            ),
            (
                "incomplete".to_string(),
                regex::Regex::new(r"(?i)\balso need\b").unwrap(),
            ),
        ];
        let scan = db
            .find_corrections(&patterns, &MessageFilters::default())
            .unwrap();
        assert_eq!(
            scan.len(),
            2,
            "exactly the two user corrections, assistant ignored"
        );
        assert!(scan.iter().any(|c| c.category == "skip_step"));
        assert!(scan.iter().any(|c| c.category == "incomplete"));
        // The assistant row matches the skip_step regex but is role=assistant → must be excluded.
        assert!(
            !scan
                .iter()
                .any(|c| c.content.contains("you forgot nothing")),
            "assistant-role match excluded by the role='user' filter",
        );
    }

    #[test]
    fn find_corrections_parallel_matches_sequential() {
        // The parallel (rayon) classification must produce EXACTLY the sequential result: same
        // matches, in the same `order by ts desc`, with the same limit semantics. We seed 600 user
        // rows across many threads' worth of work; row i matches iff i % 5 == 0, and its content
        // embeds i so we can assert the precise descending order. (Order-preservation under
        // rayon's `collect` is the property most at risk from parallelism — this pins it.)
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute_batch(
                "insert into sessions(id, provider, provider_session_id, preview_text, \
                   source_path, parse_version, discovery_source) \
                 values('claude:s1','claude','s1','','/x','v1','jsonl');",
            )
            .unwrap();
        let base = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap();
        let n = 600i64;
        {
            let tx = db.conn.unchecked_transaction().unwrap();
            {
                let mut stmt = tx
                    .prepare(
                        "insert into messages(session_id, provider, seq, role, ts, content) \
                         values('claude:s1','claude',?1,'user',?2,?3)",
                    )
                    .unwrap();
                for i in 0..n {
                    let ts = (base + chrono::Duration::seconds(i)).to_rfc3339();
                    let content = if i % 5 == 0 {
                        format!("row-{i} you forgot the tests")
                    } else {
                        format!("row-{i} all good here")
                    };
                    stmt.execute(params![i, ts, content]).unwrap();
                }
            }
            tx.commit().unwrap();
        }
        let patterns = vec![(
            "skip_step".to_string(),
            regex::Regex::new(r"(?i)\byou forgot\b").unwrap(),
        )];

        // Expected: every 5th row, in DESCENDING i order (ts desc), starting at the largest
        // multiple of 5 below n.
        let expected: Vec<i64> = (0..n).rev().filter(|i| i % 5 == 0).collect();

        let all = db
            .find_corrections(&patterns, &MessageFilters::default())
            .unwrap();
        assert_eq!(all.len(), expected.len(), "match count");
        for (hit, want_i) in all.iter().zip(&expected) {
            assert_eq!(hit.category, "skip_step");
            assert_eq!(hit.matched_pattern, "you forgot");
            assert!(
                hit.content.starts_with(&format!("row-{want_i} ")),
                "order mismatch: got {:?}, expected row-{want_i}",
                hit.content
            );
        }

        // Limit keeps the first N in the same ts-desc order (identical to a sequential early-break).
        let limited = db
            .find_corrections(
                &patterns,
                &MessageFilters {
                    limit: 10,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(limited.len(), 10);
        for (hit, want_i) in limited.iter().zip(expected.iter().take(10)) {
            assert!(hit.content.starts_with(&format!("row-{want_i} ")));
        }
    }

    #[test]
    fn regex_search_corpus_gate_is_result_equivalent() {
        // #272: the trigram-prefilter corpus-size gate must change SPEED, never RESULTS. With a
        // role filter present the corpus is below the threshold, so the gate SKIPS the prefilter
        // (direct regex scan); with no structural filter it USES the prefilter (trigram path).
        // Both must agree: the role-filtered result equals the no-filter result restricted to
        // that role. (The fixture is tiny, so `narrows_corpus()` is what selects the branch.)
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        seed_messages(
            &db,
            &[
                ("user", "the deploy hit ECONNRESET in prod"),
                ("assistant", "ECONNRESET means the socket closed"),
                ("tool", "stderr: ECONNRESET at line 42"),
                ("user", "an unrelated note about apples"),
            ],
        );
        let search = |role: Option<Role>| -> Vec<(Role, String)> {
            let filters = MessageFilters {
                regex: Some("ECONNRESET".to_string()),
                role,
                ..Default::default()
            };
            db.search_messages("", &filters)
                .unwrap()
                .into_iter()
                .map(|hit| (hit.role, hit.content))
                .collect()
        };
        // No structural filter → narrows_corpus()==false → prefilter (trigram) path.
        let all = search(None);
        assert_eq!(
            all.len(),
            3,
            "ECONNRESET in the user + assistant + tool rows"
        );
        // role filter → narrows_corpus()==true, corpus < threshold → prefilter SKIPPED (scan path).
        let user_only = search(Some(Role::User));
        let expected_user: Vec<(Role, String)> = all
            .iter()
            .filter(|(role, _)| *role == Role::User)
            .cloned()
            .collect();
        assert_eq!(
            user_only, expected_user,
            "gate's scan path agrees with the prefilter path restricted to role=user"
        );
        assert_eq!(user_only.len(), 1, "exactly the one user ECONNRESET row");
    }

    #[test]
    fn explain_message_search_counts_candidates_within_corpus() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("index.db")).unwrap();
        db.apply_performance_config(&crate::config::PerformanceConfig {
            regex_prefilter_min_corpus: 1,
            ..Default::default()
        });
        // Four user messages; only the first carries the rare literal "zebracode".
        db.upsert_session(
            &parsed_with_messages(
                "claude:s1",
                &[
                    "zebracode appears here once",
                    "common text alpha",
                    "common text bravo",
                    "common text charlie",
                ],
            ),
            1,
            100,
        )
        .unwrap();

        // Selective regex anchored on the rare literal: a trigram prefilter exists and
        // narrows the 4-row corpus to the single zebracode row before regex verification.
        let selective = MessageFilters {
            role: Some(Role::User),
            regex: Some("(?i)zebra.ode".to_string()),
            ..Default::default()
        };
        let ex = db.explain_message_search(&selective).unwrap();
        assert_eq!(
            ex.corpus, 4,
            "all four user messages form the selectivity denominator"
        );
        assert!(
            ex.prefilter.is_some(),
            "a >=3-char literal yields a trigram prefilter"
        );
        let candidates = ex
            .candidates
            .expect("the regex path reports a candidate count");
        assert!(
            candidates <= ex.corpus,
            "candidates are always a subset of the corpus"
        );
        assert_eq!(
            candidates, 1,
            "only the zebracode row survives the trigram prefilter"
        );

        // A regex with no >=3-char literal run ("a.b") has no usable anchor: no prefilter,
        // hence no candidate count — the regex would scan the whole corpus.
        let no_anchor = MessageFilters {
            role: Some(Role::User),
            regex: Some("a.b".to_string()),
            ..Default::default()
        };
        let ex2 = db.explain_message_search(&no_anchor).unwrap();
        assert!(ex2.prefilter.is_none(), "no >=3-char anchor → no prefilter");
        assert!(
            ex2.candidates.is_none(),
            "no prefilter → no candidate count"
        );
        assert_eq!(ex2.corpus, 4);
    }

    #[test]
    fn search_with_explain_reports_when_trigram_prefilter_is_skipped_by_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("index.db")).unwrap();
        db.apply_performance_config(&crate::config::PerformanceConfig {
            regex_prefilter_min_corpus: 10,
            ..Default::default()
        });
        db.upsert_session(
            &parsed_with_messages("claude:s1", &["zebracode appears here once"]),
            1,
            100,
        )
        .unwrap();

        let filters = MessageFilters {
            role: Some(Role::User),
            regex: Some("zebracode".to_string()),
            ..Default::default()
        };
        let (hits, explain) = db.search_messages_with_explain("", &filters, true).unwrap();
        let explain = explain.expect("explain requested");

        assert_eq!(hits.len(), 1);
        assert!(explain.prefilter.is_some(), "anchor is available");
        assert!(
            explain.candidates.is_none(),
            "skipped prefilter does not report staged candidates"
        );
        assert!(explain
            .prefilter_skipped
            .as_deref()
            .unwrap()
            .contains("regex_prefilter_min_corpus (10)"));
    }

    #[test]
    fn search_with_explain_uses_configured_prefilter_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("index.db")).unwrap();
        db.apply_performance_config(&crate::config::PerformanceConfig {
            regex_prefilter_min_corpus: 1,
            ..Default::default()
        });
        db.upsert_session(
            &parsed_with_messages("claude:s1", &["zebracode appears here once"]),
            1,
            100,
        )
        .unwrap();

        let filters = MessageFilters {
            role: Some(Role::User),
            regex: Some("zebracode".to_string()),
            ..Default::default()
        };
        let (_hits, explain) = db.search_messages_with_explain("", &filters, true).unwrap();
        let explain = explain.expect("explain requested");

        assert_eq!(explain.candidates, Some(1));
        assert!(explain.prefilter_skipped.is_none());
    }

    /// Build a claude `ParsedSession` whose messages are the given contents (seq = index).
    #[cfg(test)]
    fn parsed_with_messages(id: &str, contents: &[&str]) -> crate::models::ParsedSession {
        use crate::models::{Message, ParsedSession, SessionRecord};
        let messages = contents
            .iter()
            .enumerate()
            .map(|(i, c)| Message {
                seq: i as i64,
                role: Role::User,
                ts: None,
                tool_name: None,
                is_compaction: false,
                content: c.to_string(),
            })
            .collect();
        ParsedSession {
            session: SessionRecord {
                id: id.to_string(),
                provider: Provider::Claude,
                provider_session_id: "s".into(),
                title: None,
                summary: None,
                cwd: None,
                repo_root: None,
                created_at: None,
                updated_at: None,
                last_message_at: None,
                preview_text: String::new(),
                source_path: "/x".into(),
                message_count: Some(contents.len() as i64),
                parse_version: "v1".into(),
                raw_metadata_json: None,
                parse_warning: None,
                discovery_source: "jsonl".into(),
            },
            transcript_text: contents.join("\n\n"),
            messages,
            file_edits: Vec::new(),
        }
    }

    #[test]
    fn upsert_appends_only_new_messages_when_session_grows() {
        // Root-cause reindex perf: an append-only session that GREW must NOT delete + re-insert
        // (and re-trigram-index) its unchanged prefix — that re-indexed entire multi-hundred-MB
        // sessions on every incremental reindex. Detected with a SENTINEL tag on the existing
        // rows: it survives an append but not a delete+re-insert. (Row ids can't tell them apart
        // — SQLite reuses freed rowids after a delete-all.)
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let contents = |db: &Db| -> Vec<String> {
            let mut s = db
                .conn
                .prepare("select content from messages where session_id='claude:s1' order by seq")
                .unwrap();
            s.query_map([], |r| r.get(0))
                .unwrap()
                .filter_map(Result::ok)
                .collect()
        };
        let tagged = |db: &Db| -> i64 {
            db.conn
                .query_row(
                    "select count(*) from messages where session_id='claude:s1' \
                     and tool_name='SENTINEL'",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        };
        db.upsert_session(
            &parsed_with_messages("claude:s1", &["alpha", "bravo", "charlie"]),
            1,
            100,
        )
        .unwrap();
        // Tag the existing rows; a fresh re-insert (from the parse) would not carry the sentinel.
        db.conn
            .execute(
                "update messages set tool_name='SENTINEL' where session_id='claude:s1'",
                [],
            )
            .unwrap();

        // Append-only growth: same prefix + 2 new messages.
        db.upsert_session(
            &parsed_with_messages("claude:s1", &["alpha", "bravo", "charlie", "delta", "echo"]),
            2,
            200,
        )
        .unwrap();
        assert_eq!(
            contents(&db),
            ["alpha", "bravo", "charlie", "delta", "echo"]
        );
        assert_eq!(
            tagged(&db),
            3,
            "prefix rows RETAINED the sentinel (appended, not re-indexed)"
        );
        // The appended message is findable by regex search (custom index built lazily over the
        // grown corpus, or covered by the un-indexed delta direct-scan).
        let new_found = db
            .search_messages(
                "",
                &MessageFilters {
                    regex: Some("delta".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(new_found.len(), 1, "the appended message is searchable");

        // Shrink → full replace (safe fallback): sentinel gone, content correct.
        db.upsert_session(
            &parsed_with_messages("claude:s1", &["alpha", "bravo"]),
            3,
            60,
        )
        .unwrap();
        assert_eq!(
            contents(&db),
            ["alpha", "bravo"],
            "shrink re-replaces fully"
        );
        assert_eq!(tagged(&db), 0, "shrink did a full replace");

        // Grow with a CHANGED boundary message (in-place rewrite) → full replace, correct content.
        db.upsert_session(
            &parsed_with_messages("claude:s1", &["alpha", "boundary"]),
            4,
            70,
        )
        .unwrap();
        db.conn
            .execute(
                "update messages set tool_name='SENTINEL' where session_id='claude:s1'",
                [],
            )
            .unwrap();
        db.upsert_session(
            &parsed_with_messages("claude:s1", &["alpha", "CHANGED", "extra"]),
            5,
            90,
        )
        .unwrap();
        assert_eq!(
            contents(&db),
            ["alpha", "CHANGED", "extra"],
            "boundary content changed → full replace keeps content correct",
        );
        assert_eq!(tagged(&db), 0, "boundary mismatch forced a full replace");
    }

    #[test]
    fn replace_session_rewrites_matching_prefix_metadata_on_full_reindex() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();

        db.replace_session(
            &parsed_with_messages("claude:s1", &["alpha", "bravo"]),
            1,
            100,
        )
        .unwrap();
        db.conn
            .execute(
                "update messages set role='tool', tool_name='STALE' where session_id='claude:s1' and seq=0",
                [],
            )
            .unwrap();

        db.replace_session(
            &parsed_with_messages("claude:s1", &["alpha", "bravo", "charlie"]),
            2,
            150,
        )
        .unwrap();
        let row: (String, Option<String>) = db
            .conn
            .query_row(
                "select role, tool_name from messages where session_id='claude:s1' and seq=0",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            ("user".to_string(), None),
            "full replace must repair stale prefix metadata even when content is unchanged"
        );
    }

    #[test]
    fn tail_flow_appends_without_reparsing_prefix() {
        // Drive the incremental tail path directly (parse_reader → tail_parse → append_tail) and
        // PROVE it appends only the new rows: a deliberately corrupted prefix row — which a full
        // reparse would overwrite via the boundary-mismatch replace — must survive untouched.
        use crate::providers::claude::ClaudeAdapter;
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("probe.jsonl");
        let line = |ts: &str, role: &str, text: &str| {
            format!(
                "{{\"type\":\"{role}\",\"sessionId\":\"probe\",\"timestamp\":\"{ts}\",\
                 \"message\":{{\"role\":\"{role}\",\"content\":[{{\"type\":\"text\",\"text\":\"{text}\"}}]}}}}\n"
            )
        };
        let initial = format!(
            "{}{}",
            line("2026-06-01T10:00:00Z", "user", "first prompt"),
            line("2026-06-01T10:00:05Z", "assistant", "first reply"),
        );
        std::fs::write(&file, &initial).unwrap();

        let claude = ClaudeAdapter::new(vec![dir.path().to_path_buf()]);
        let source = crate::models::SourceFile {
            provider: Provider::Claude,
            path: file.clone(),
            mtime_ns: 1,
            size_bytes: std::fs::metadata(&file).unwrap().len() as i64,
        };
        let source_path = crate::util::normalize_path(&file);
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let mut parsed = claude.parse(&source);
        crate::util::backfill_session_dates(&mut parsed.session, source.mtime_ns);
        db.upsert_session(&parsed, source.mtime_ns, source.size_bytes)
            .unwrap();
        db.set_file_checkpoint(
            Provider::Claude,
            &source_path,
            crate::tail::complete_prefix_offset(&file).unwrap(),
            &crate::tail::prefix_fingerprint(&file).unwrap(),
        )
        .unwrap();
        assert_eq!(db.message_count().unwrap(), 2);

        db.conn
            .execute(
                "update messages set content='CORRUPTED_PROBE' \
                 where session_id='claude:probe' and seq=0",
                [],
            )
            .unwrap();

        // Append a third turn, then run the tail path against the stored checkpoint.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap()
            .write_all(line("2026-06-01T10:01:00Z", "user", "second prompt").as_bytes())
            .unwrap();
        let new_size = std::fs::metadata(&file).unwrap().len() as i64;
        let (offset, stored_fp) = db
            .file_checkpoint(Provider::Claude, &source_path)
            .unwrap()
            .unwrap();
        assert!(
            crate::tail::fingerprint_matches(&file, &stored_fp).unwrap(),
            "an append must keep the head fingerprint matching"
        );
        let tail = crate::tail::tail_parse(&file, offset, |cursor, path| {
            claude.parse_reader(cursor, path)
        })
        .unwrap()
        .expect("a new complete line was appended");
        db.append_tail(&tail, 2, new_size).unwrap();

        assert_eq!(
            db.message_count().unwrap(),
            3,
            "the appended turn is indexed"
        );
        let seq2: String = db
            .conn
            .query_row(
                "select content from messages where session_id='claude:probe' and seq=2",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            seq2, "second prompt",
            "new message appended at the next seq"
        );
        let seq0: String = db
            .conn
            .query_row(
                "select content from messages where session_id='claude:probe' and seq=0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            seq0, "CORRUPTED_PROBE",
            "tail append must NOT reparse/replace the prefix rows"
        );
        assert_eq!(
            db.messages_fts_count().unwrap(),
            db.message_count().unwrap(),
            "FTS in sync"
        );
    }

    #[test]
    fn vocabulary_reports_term_frequencies() {
        // #226: fts5vocab term frequency — a term repeated across messages has the right doc and
        // total counts and sorts ahead of rarer terms; the trigram source yields 3-gram terms.
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        seed_messages(
            &db,
            &[
                ("user", "alpha alpha bravo"), // alpha x2, bravo x1
                ("user", "alpha charlie"),     // alpha x1, charlie x1
            ],
        );
        let vocab = db.vocabulary(false, 0).unwrap();
        let alpha = vocab
            .iter()
            .find(|(t, _, _)| t == "alpha")
            .expect("alpha present");
        assert_eq!(alpha.1, 2, "alpha appears in 2 documents");
        assert_eq!(alpha.2, 3, "alpha occurs 3 times total");
        // Ordered by total count desc → alpha (3) is first.
        assert_eq!(vocab[0].0, "alpha", "most frequent term first");
        // Trigram vocab yields 3-grams (substring stats), e.g. "alp" from "alpha".
        let tri = db.vocabulary(true, 0).unwrap();
        assert!(
            tri.iter().any(|(t, _, _)| t == "alp"),
            "trigram vocab has 3-gram terms"
        );
    }

    #[test]
    fn rank_flag_does_not_change_exact_literal_result_order() {
        // Exact literal search is no longer defined by FTS, so BM25 must not silently trade
        // correctness/predictability for relevance ordering. The compatibility flag preserves the
        // same deterministic session/seq order and the same match set.
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        // seq 0 = long/diluted (inserted first), seq 1 = short/dense.
        seed_messages(
            &db,
            &[
                (
                    "user",
                    "needle buried in a very long haystack with lots of other unrelated words",
                ),
                ("user", "needle"),
            ],
        );
        let search = |rank: bool| -> Vec<String> {
            let filters = MessageFilters {
                rank,
                ..Default::default()
            };
            db.search_messages("needle", &filters)
                .unwrap()
                .into_iter()
                .map(|h| h.content)
                .collect()
        };
        let unranked = search(false);
        assert_eq!(unranked.len(), 2);
        assert!(
            unranked[0].starts_with("needle buried"),
            "unranked = insertion order (seq)"
        );
        let ranked = search(true);
        assert_eq!(ranked, unranked);
    }

    #[test]
    fn regex_prefilter_reaches_rows_by_primary_key_not_full_scan() {
        // #207: a prefilterable regex search resolves candidates through the custom trigram index
        // (staged into the `_trigram_cand` temp table) and reaches message rows by primary key —
        // never a full table scan of `messages`.
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        seed_messages(&db, &[("user", "socket failed with ECONNRESET) today")]);
        // Build the base + stage candidates exactly as search_messages does, then check the plan of
        // the candidate-restricted scan.
        let base_max = db.ensure_trigram_base().unwrap();
        let groups = crate::trigram::trigram_prefilter_groups("ECONNRESET").unwrap();
        let cands = crate::trigram_index::candidates(&db.conn, &groups).unwrap();
        db.stage_candidates(base_max, &cands).unwrap();
        let plan = {
            let mut stmt = db
                .conn
                .prepare(
                    "explain query plan select m.id from messages m \
                     where m.id in (select id from _trigram_cand) order by m.session_id, m.seq",
                )
                .unwrap();
            stmt.query_map([], |r| r.get::<_, String>(3))
                .unwrap()
                .filter_map(Result::ok)
                .collect::<Vec<_>>()
                .join(" | ")
        };
        assert!(
            plan.contains("PRIMARY KEY") || plan.contains("USING INTEGER PRIMARY KEY"),
            "messages must be reached by primary key, not scanned: {plan}",
        );
        assert!(
            !plan.contains("SCAN m "),
            "no full scan of the messages table: {plan}",
        );
    }

    #[test]
    fn checkpoint_truncate_is_safe_and_preserves_data() {
        // #240: the WAL truncate-checkpoint runs without error and the index stays queryable
        // (it folds the WAL into the main DB; the substring index must still match afterward).
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        seed_messages(&db, &[("user", "the deploy hit ECONNRESET) again")]);
        db.ensure_trigram_base().unwrap();
        db.checkpoint_truncate().unwrap();
        // Idempotent: a second checkpoint on a quiescent DB is fine.
        db.checkpoint_truncate().unwrap();
        let hits = db
            .search_messages(
                "",
                &MessageFilters {
                    regex: Some("ECONNRESET".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(hits.len(), 1, "data intact and searchable after checkpoint");
    }
}
