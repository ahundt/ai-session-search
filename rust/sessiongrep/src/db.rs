use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use rusqlite::{params, Connection, OptionalExtension};

use crate::models::{
    CorrectionMatch, EditOp, FileCrossRef, FileEdit, FileEditSummary, FileQuery, MessageFilters,
    MessageHit, ParsedSession, PlanningCount, Provider, Role, SearchExplain, SearchFilters,
    SearchHit, SessionRecord, SessionWithTranscript,
};
use crate::util::snippet_from_match;

/// On-disk index generation (NOT the package version). Bump whenever a reindex must
/// backfill newly added columns/tables that incremental indexing would otherwise skip,
/// or when a parse-logic change makes existing rows stale and they must be re-parsed.
/// [`Db::needs_backfill`] compares this against SQLite's `pragma user_version` — which
/// defaults to 0 on any index built by an older generation (the upstream release never
/// set it) — to trigger a one-time full reindex after an upgrade, without re-parsing on
/// every run. Each new generation increments by exactly 1; an upgrading user reindexes once.
///
///   1: message-level index — generation 1 over the upstream session-only schema
///      (`sessions` + `transcripts` + `sessions_fts`). It adds, as one coherent migration:
///      the per-message `messages` table (normalized role / `tool_name` / ts / compaction
///      across all five providers) with its `messages_fts` external-content index, and the
///      `file_edits` table backing file-version recovery (`files …`). An upstream-built
///      index is at `user_version = 0 < 1`, so the first run on this generation performs a
///      single full reindex to populate both tables, then stamps `user_version = 1`.
///   2: parse-logic fix — exclude harness-injected output that was leaking into the `user`
///      role and polluting user analytics: claude `<local-command-stdout>`/`-stderr`/`-caveat`
///      (e.g. `/model` "Set model to …" output) and codex `<environment_context>`. ~9% of the
///      real user corpus were such rows; existing indexes have them persisted, so this bump
///      forces a one-time full reindex to re-parse and drop them.
///   3: substring/regex prefilter moved from the FTS5 `messages_trigram` virtual table to the
///      custom, parallel-built [`crate::trigram_index`] (`trigram_postings`/`trigram_meta`). The
///      FTS5 trigram + its triggers/vocab are dropped in `init`; the new index builds lazily on
///      first regex use. The bump forces a one-time full reindex so `messages` is consistent;
///      the custom index then builds on demand (no per-row trigram work during reindex).
pub const SCHEMA_VERSION: i64 = 3;

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

pub struct Db {
    conn: Connection,
    /// Corpus-size threshold for the regex prefilter (default [`TRIGRAM_PREFILTER_MIN_CORPUS`],
    /// overridable via `[performance] regex_prefilter_min_corpus`).
    prefilter_min_corpus: i64,
    /// Un-indexed delta size before the trigram base is rebuilt (default
    /// [`TRIGRAM_BASE_REBUILD_DELTA`], overridable via `[performance] trigram_rebuild_delta`).
    trigram_rebuild_delta: i64,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let conn = Connection::open(path)?;
        let db = Self {
            conn,
            prefilter_min_corpus: TRIGRAM_PREFILTER_MIN_CORPUS,
            trigram_rebuild_delta: TRIGRAM_BASE_REBUILD_DELTA,
        };
        db.init()?;
        Ok(db)
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
        // External-content FTS over message bodies — this IS the index `search_messages`
        // queries (phrase + trailing-prefix `MATCH`; a punctuation-only query the tokenizer
        // can't index falls back to an `instr` substring scan). The insert/delete/update
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
        // Substring/regex PREFILTER over message content (the Google Code Search trigram
        // technique): turns regex-literal/substring lookups into indexed candidate queries that the
        // Rust regex re-verifies. This is the custom, parallel-built [`crate::trigram_index`] — NOT
        // an FTS5 virtual table — because FTS5's trigram tokenizer builds single-threaded inside the
        // one SQLite writer, which is ~80% of a cold build (measured ~145 s / 1.8 GB). The custom
        // index tokenizes with Rayon and bulk-loads compact delta-varint postings: ~5x faster build,
        // same on-disk size, sub-3 ms candidate queries (see ~/.claude/notes/sessiongrep_perf_
        // benchmarks/). It is built LAZILY on first regex use ([`Db::ensure_trigram_base`]), so
        // `reindex` does NO trigram work and `list`/`show`/`paths`/`resume` never pay for it.
        crate::trigram_index::ensure_schema(&self.conn)?;
        // Migration: drop a prior generation's FTS5 `messages_trigram` (+ its sync triggers and
        // fts5vocab view) if present. The custom index supersedes it; the SCHEMA_VERSION bump
        // triggers a one-time reindex, after which the first regex search builds the new index.
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
            // The one-time parallel build can take tens of seconds on a large corpus; tell the user
            // so a first regex/substring search isn't a silent multi-second pause (the result is the
            // same, the wait is just the index being built once).
            let count: i64 = self
                .conn
                .query_row("select count(*) from messages", [], |row| row.get(0))?;
            eprintln!(
                "sessiongrep: building substring/regex search index in parallel \
                 (one-time over {count} messages)…"
            );
            let built = crate::trigram_index::build(&self.conn)?;
            // Fold the large build out of the WAL so the -wal file doesn't retain the index size.
            self.checkpoint_truncate()?;
            return Ok(built);
        }
        Ok(base_max)
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
        {
            let mut stmt = self
                .conn
                .prepare("insert or ignore into _trigram_cand(id) values (?1)")?;
            for id in candidates {
                stmt.execute([id])?;
            }
        }
        self.conn.execute(
            "insert or ignore into _trigram_cand(id) select id from messages where id > ?1",
            [base_max],
        )?;
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

    pub fn upsert_session(
        &self,
        parsed: &ParsedSession,
        mtime_ns: i64,
        size_bytes: i64,
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
        let append_from: Option<usize> = if existing_count > 0 && parsed_count > existing_count {
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
        {
            let mut stmt = tx.prepare(
                "insert into messages
                    (session_id, provider, seq, role, ts, tool_name, is_compaction, content)
                 values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for message in new_messages {
                stmt.execute(params![
                    session.id,
                    session.provider.as_str(),
                    message.seq,
                    message.role.as_str(),
                    message.ts.map(|ts| ts.to_rfc3339()),
                    message.tool_name,
                    message.is_compaction as i64,
                    message.content,
                ])?;
            }
        }
        // Re-sync file-edit rows (idempotent, same as messages). `edits` are stored as a
        // JSON array of [old, new] pairs; `new_content` holds full content for Write only.
        tx.execute(
            "delete from file_edits where session_id = ?1",
            params![session.id],
        )?;
        {
            let mut stmt = tx.prepare(
                "insert into file_edits
                    (session_id, provider, seq, ts, tool, file_path, file_name, new_content, edits_json)
                 values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for edit in &parsed.file_edits {
                let edits_json = if edit.edits.is_empty() {
                    None
                } else {
                    Some(serde_json::to_string(&edit.edits)?)
                };
                stmt.execute(params![
                    session.id,
                    session.provider.as_str(),
                    edit.seq,
                    edit.ts.map(|ts| ts.to_rfc3339()),
                    edit.tool,
                    edit.file_path,
                    edit.file_name,
                    edit.new_content,
                    edits_json,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete `user`-role messages that are harness-injected output, not prompts — content
    /// leading with `<local-command-stdout>` / `-stderr` / `-caveat` (claude) or
    /// `<environment_context>` (codex). The parse fix (SCHEMA_VERSION 2) keeps these out of
    /// re-parsed files, but sessions whose source file was deleted are never re-visited (durable
    /// archive), so their already-indexed injected rows persist; this one-time data purge reaches
    /// them. Returns the number of rows deleted. The `messages_fts` delete trigger keeps the word
    /// index in sync; the custom trigram base is rebuilt lazily on next use. Run during the schema
    /// migration (see cli.rs).
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
        {
            let mut stmt = tx.prepare(
                "insert into messages
                    (session_id, provider, seq, role, ts, tool_name, is_compaction, content)
                 values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for (i, message) in tail.new_messages.iter().enumerate() {
                stmt.execute(params![
                    session.id,
                    session.provider.as_str(),
                    existing_count + i as i64,
                    message.role.as_str(),
                    message.ts.map(|ts| ts.to_rfc3339()),
                    message.tool_name,
                    message.is_compaction as i64,
                    message.content,
                ])?;
            }
        }

        // New file edits, re-sequenced after the existing ones.
        let existing_edit_seq: i64 = tx.query_row(
            "select coalesce(max(seq), -1) from file_edits where session_id = ?1",
            params![session.id],
            |row| row.get(0),
        )?;
        {
            let mut stmt = tx.prepare(
                "insert into file_edits
                    (session_id, provider, seq, ts, tool, file_path, file_name, new_content, edits_json)
                 values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for (i, edit) in tail.new_file_edits.iter().enumerate() {
                let edits_json = if edit.edits.is_empty() {
                    None
                } else {
                    Some(serde_json::to_string(&edit.edits)?)
                };
                stmt.execute(params![
                    session.id,
                    session.provider.as_str(),
                    existing_edit_seq + 1 + i as i64,
                    edit.ts.map(|ts| ts.to_rfc3339()),
                    edit.tool,
                    edit.file_path,
                    edit.file_name,
                    edit.new_content,
                    edits_json,
                ])?;
            }
        }

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

    /// Rows in the message FTS index. Used to assert trigger sync (== `message_count`).
    pub fn messages_fts_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("select count(*) from messages_fts", [], |row| row.get(0))?)
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
        if trigram {
            self.ensure_trigram_base()?;
            let mut stmt = self
                .conn
                .prepare("select tg, df, df from trigram_postings order by df desc, tg limit ?1")?;
            let rows = stmt
                .query_map([lim], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            return Ok(rows);
        }
        let mut stmt = self.conn.prepare(
            "select term, doc, cnt from messages_vocab order by cnt desc, term limit ?1",
        )?;
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

    /// Message-level search. A literal `query` is matched against the `messages_fts` index
    /// as a phrase with a prefix on the final token (indexed; "handle" also matches
    /// "handler"); a punctuation-only query with no FTS tokens (e.g. "->") falls back to a
    /// substring scan. When `filters.regex` is set it is applied as a Rust regex
    /// (linear-time) over the rows matching the structured filters. `limit == 0` = unlimited.
    pub fn search_messages(
        &self,
        query: &str,
        filters: &MessageFilters,
    ) -> Result<Vec<MessageHit>> {
        use rusqlite::types::Value;

        // Literal content matching strategy (non-regex):
        //   * tokenizable query   → FTS5 phrase+prefix MATCH on the messages_fts index;
        //   * punctuation-only    → instr substring scan fallback (rare, e.g. "->");
        //   * empty query         → no content filter (list all matching the filters).
        let fts_query = (filters.regex.is_none() && !query.is_empty())
            .then(|| fts_message_query(query))
            .flatten();
        let from = if fts_query.is_some() {
            "messages_fts f join messages m on m.id = f.rowid"
        } else {
            "messages m"
        };
        let mut sql = format!(
            "select m.session_id, m.provider, m.seq, m.role, m.ts, m.tool_name, m.content \
             from {from} where 1 = 1"
        );
        let mut args: Vec<Value> = Vec::new();
        if let Some(fts) = &fts_query {
            sql.push_str(" and messages_fts match ?");
            args.push(Value::Text(fts.clone()));
        }
        append_message_filters(&mut sql, &mut args, filters);
        // Substring scan fallback: only when not using FTS and not --regex (e.g. a
        // punctuation-only query the tokenizer can't index).
        if fts_query.is_none() && filters.regex.is_none() && !query.is_empty() {
            sql.push_str(" and instr(lower(m.content), lower(?)) > 0");
            args.push(Value::Text(query.to_string()));
        }
        // Regex path: narrow candidates with the trigram index (Google Code Search technique)
        // when the pattern yields a usable literal prefilter, then let the Rust regex below
        // verify (the candidate set is a superset — look-around can let a literal-containing row
        // fail the full regex). Lazily build the index on first use. Patterns with no >=3-char
        // literal yield `None` and fall through to the existing full scan, still correct.
        //
        // Corpus-size gate: only query the trigram index when the structurally-filtered corpus is
        // large enough to benefit. A role/session/ts/tool filter can restrict the scan to a small
        // slice (e.g. `--type user` ≈ 7.7k rows), where a direct regex scan beats intersecting
        // against the whole-corpus trigram index (95% of which is tool output the filter discards)
        // — and it also avoids triggering the lazy index build for a tiny query. The COUNT is paid
        // only when a structural filter is present (otherwise the corpus is the full table, always
        // above the threshold). Regression-free: the prefilter is a superset the Rust regex below
        // re-verifies, so skipping it returns identical rows.
        if let Some(pattern) = &filters.regex {
            if let Some(groups) = crate::trigram::trigram_prefilter_groups(pattern) {
                let use_prefilter = !filters.narrows_corpus()
                    || self.filtered_corpus_count(filters)? >= self.prefilter_min_corpus;
                if use_prefilter {
                    // Custom parallel-built trigram index (base) + un-indexed delta; the Rust regex
                    // below re-verifies every candidate, so this is a SUPERSET filter exactly like
                    // the old FTS5 prefilter (parity asserted by
                    // `trigram_index_candidates_match_fts5_prefilter`).
                    let base_max = self.ensure_trigram_base()?;
                    let candidates = crate::trigram_index::candidates(&self.conn, &groups)?;
                    self.stage_candidates(base_max, &candidates)?;
                    sql.push_str(" and m.id in (select id from _trigram_cand)");
                }
            }
        }
        if filters.rank && fts_query.is_some() {
            // BM25 relevance, most-relevant first. fts5 `bm25()` returns a NEGATIVE score where
            // more-negative = more relevant, so ascending order is best-first (NOT `desc`). Only
            // valid on the FTS path (the match drives the score); session/seq breaks ties.
            sql.push_str(" order by bm25(messages_fts), m.session_id, m.seq");
        } else {
            sql.push_str(" order by m.session_id, m.seq");
        }
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
        let raw = stmt.query_map(rusqlite::params_from_iter(args.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?;

        let mut hits = Vec::new();
        for row in raw {
            let (session_id, provider, seq, role, ts, tool_name, content) = row?;
            if let Some(re) = &compiled {
                if !re.is_match(&content) {
                    continue;
                }
            }
            hits.push(MessageHit {
                session_id,
                provider: provider.parse().unwrap_or(Provider::Claude),
                seq,
                role: role.parse().unwrap_or(Role::User),
                ts: ts.and_then(|value| {
                    chrono::DateTime::parse_from_rfc3339(&value)
                        .ok()
                        .map(|dt| dt.with_timezone(&Utc))
                }),
                tool_name,
                content,
            });
            if filters.limit > 0 && hits.len() >= filters.limit {
                break;
            }
        }
        Ok(hits)
    }

    /// Count the messages matching the structural filters (role / provider / session / time /
    /// tool / no-compaction) — the corpus that content matching (FTS / substring / regex) then
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

    /// Explain how selective a regex message search's trigram prefilter is — the
    /// dominant driver of query latency (bugs-limitations L1). Returns the corpus
    /// size under the structural filters (the denominator), the trigram `MATCH`
    /// query derived from the regex literals, and the candidate-row count that
    /// query yields (the rows the Rust regex must then verify). Candidates close
    /// to corpus = a non-selective prefilter = a slow query. Uses the SAME filter
    /// predicates as [`Db::search_messages`] (via [`append_message_filters`]) so
    /// the count reflects exactly what the search scans.
    pub fn explain_message_search(&self, filters: &MessageFilters) -> Result<SearchExplain> {
        use rusqlite::types::Value;

        let corpus: i64 = self.filtered_corpus_count(filters)?;

        // `prefilter` is the human-readable AND-of-trigrams string (display only); the candidate
        // COUNT is computed via the custom trigram index over the same trigram groups, so it
        // reflects exactly what [`Db::search_messages`] now scans.
        let prefilter = filters
            .regex
            .as_deref()
            .and_then(crate::trigram::trigram_prefilter);
        let groups = filters
            .regex
            .as_deref()
            .and_then(crate::trigram::trigram_prefilter_groups);
        let candidates = match groups {
            Some(groups) => {
                let base_max = self.ensure_trigram_base()?;
                let cands = crate::trigram_index::candidates(&self.conn, &groups)?;
                self.stage_candidates(base_max, &cands)?;
                let mut csql = String::from("select count(*) from messages m where 1 = 1");
                let mut cargs: Vec<Value> = Vec::new();
                append_message_filters(&mut csql, &mut cargs, filters);
                csql.push_str(" and m.id in (select id from _trigram_cand)");
                Some(self.conn.query_row(
                    &csql,
                    rusqlite::params_from_iter(cargs.iter()),
                    |row| row.get(0),
                )?)
            }
            None => None,
        };
        Ok(SearchExplain {
            prefilter,
            candidates,
            corpus,
        })
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
                provider: row.get::<_, String>(1)?.parse().unwrap_or(Provider::Claude),
                seq: row.get(2)?,
                role: row.get::<_, String>(3)?.parse().unwrap_or(Role::User),
                ts: row.get::<_, Option<String>>(4)?.and_then(|value| {
                    chrono::DateTime::parse_from_rfc3339(&value)
                        .ok()
                        .map(|dt| dt.with_timezone(&Utc))
                }),
                tool_name: row.get(5)?,
                content: row.get(6)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
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

        // Classify each row against the ordered patterns in PARALLEL. Regex matching is the
        // CPU-bound cost here (measured ~98% of one core: ~13 MB × the category regexes) and every
        // row is independent, so this is embarrassingly parallel. `patterns` is shared read-only
        // (`regex::Regex` is `Sync`); `par_iter` uses the global pool sized by
        // `Config::resolve_threads` (auto = all cores). Rayon's `collect` preserves the source
        // order (the SQL `order by ts desc`), so the result is identical to a sequential scan —
        // verified by `find_corrections_parallel_matches_sequential`.
        use rayon::prelude::*;
        let mut out: Vec<CorrectionMatch> = rows
            .par_iter()
            .filter_map(|(session_id, provider, ts, content)| {
                patterns.iter().find_map(|(cat, re)| {
                    re.find(content).map(|m| CorrectionMatch {
                        session_id: session_id.clone(),
                        provider: provider.parse().unwrap_or(Provider::Claude),
                        ts: ts.as_deref().and_then(|value| {
                            chrono::DateTime::parse_from_rfc3339(value)
                                .ok()
                                .map(|dt| dt.with_timezone(&Utc))
                        }),
                        category: cat.clone(),
                        matched_pattern: m.as_str().to_string(),
                        content: content.clone(),
                    })
                })
            })
            .collect();
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
             join sessions s on s.id = m.session_id where m.role = 'slash'",
        );
        let mut args: Vec<Value> = Vec::new();
        if let Some(session) = &filters.session {
            sql.push_str(" and m.session_id like ?");
            args.push(Value::Text(format!("%{session}%")));
        }
        push_ts_window(&mut sql, &mut args, "m.ts", filters.since, filters.until);

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
        if let Some(session) = &query.session {
            sql.push_str(" and session_id like ?");
            args.push(Value::Text(format!("%{session}%")));
        }
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
        if let Some(session) = &query.session {
            sql.push_str(" and session_id like ?");
            args.push(Value::Text(format!("%{session}%")));
        }
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
                    provider: provider.parse().unwrap_or(Provider::Claude),
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
        if let Some(session) = session {
            sql.push_str(" and session_id like ?");
            args.push(Value::Text(format!("%{session}%")));
        }
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
            let edits: Vec<EditOp> = edits_json
                .as_deref()
                .and_then(|json| serde_json::from_str(json).ok())
                .unwrap_or_default();
            out.push((
                session_id,
                provider.parse().unwrap_or(Provider::Claude),
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
            sql.push_str(" and (coalesce(s.cwd, '') like ? or coalesce(s.repo_root, '') like ?) ");
            let pattern = format!("{path_prefix}%");
            params_vec.push(pattern.clone());
            params_vec.push(pattern);
        }
        if let Some(since) = filters.since {
            sql.push_str(" and coalesce(s.updated_at, s.created_at) >= ? ");
            params_vec.push(since.to_rfc3339());
        }
        if let Some(until) = filters.until {
            sql.push_str(" and coalesce(s.updated_at, s.created_at) <= ? ");
            params_vec.push(until_bound_text(until));
        }
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
        let mut stmt = self.conn.prepare(
            "
            select
                s.id, s.provider, s.provider_session_id, s.title, s.summary, s.cwd, s.repo_root,
                s.created_at, s.updated_at, s.last_message_at, s.preview_text, s.source_path,
                s.message_count, s.parse_version, s.raw_metadata_json, s.parse_warning, s.discovery_source,
                coalesce(t.transcript_text, '')
            from sessions s
            left join transcripts t on t.session_id = s.id
            where s.id = ?1 or s.provider_session_id = ?1 or s.id like ?2 or s.provider_session_id like ?2
            ",
        )?;

        let pattern = format!("{value}%");
        let rows = stmt.query_map(params![value, pattern], row_to_session_with_transcript)?;
        let mut matches = Vec::new();
        for row in rows {
            matches.push(row?);
        }
        match matches.len() {
            0 => Err(anyhow!(
                "no session matches '{value}' — run `sessiongrep list` to see recent session \
                 ids, or `sessiongrep search <keywords>` to find one"
            )),
            1 => Ok(matches.remove(0)),
            _ => {
                // Show the candidates so the user can disambiguate instead of guessing.
                let shown: Vec<String> = matches
                    .iter()
                    .take(8)
                    .map(|m| m.session.id.clone())
                    .collect();
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
            sql.push_str(" and (coalesce(s.cwd, '') like ? or coalesce(s.repo_root, '') like ?) ");
            let pattern = format!("{path_prefix}%");
            params_vec.push(pattern.clone());
            params_vec.push(pattern);
        }
        if let Some(since) = filters.since {
            sql.push_str(" and coalesce(s.updated_at, s.created_at) >= ? ");
            params_vec.push(since.to_rfc3339());
        }
        if let Some(until) = filters.until {
            sql.push_str(" and coalesce(s.updated_at, s.created_at) <= ? ");
            params_vec.push(until_bound_text(until));
        }
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

/// Build an FTS5 query for a literal `messages search`: all tokenizable terms as one
/// contiguous phrase with a prefix on the final token, so "handle" also matches
/// "handler"/"handles" and "error handl" matches "error handling". Returns `None` when
/// the query has no tokenizable content (e.g. "->"), so the caller falls back to a
/// substring scan. Embedded quotes are doubled per FTS5 phrase-escaping rules.
fn fts_message_query(query: &str) -> Option<String> {
    let terms: Vec<String> = query
        .split_whitespace()
        .filter(|t| t.chars().any(char::is_alphanumeric))
        .map(|t| t.replace('"', "\"\""))
        .collect();
    if terms.is_empty() {
        return None;
    }
    Some(format!("\"{}\"*", terms.join(" ")))
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

/// Append the inclusive timestamp-window clauses and push their rfc3339 args,
/// centralizing the date filter shared by every time-scoped query (messages,
/// corrections, planning, files). `col` lets callers target `ts` or a table-qualified
/// `m.ts`. Args are pushed since-then-until to match the SQL order. Two robustness
/// rules: the upper bound covers the whole final second (see [`until_bound_text`]), and
/// a row whose timestamp is NULL (unknown) is never silently dropped by a date filter —
/// `or <col> is null` keeps it rather than letting SQL three-valued logic exclude it.
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
    if let Some(session) = &filters.session {
        sql.push_str(" and m.session_id like ?");
        args.push(Value::Text(format!("%{session}%")));
    }
    if let Some(tool) = &filters.tool {
        // NULL tool_name (non-tool rows) is correctly excluded by instr(NULL,..) = NULL.
        sql.push_str(" and instr(lower(m.tool_name), lower(?)) > 0");
        args.push(Value::Text(tool.clone()));
    }
    push_ts_window(sql, args, "m.ts", filters.since, filters.until);
    if filters.no_compaction {
        sql.push_str(" and m.is_compaction = 0");
    }
}

fn push_ts_window(
    sql: &mut String,
    args: &mut Vec<rusqlite::types::Value>,
    col: &str,
    since: Option<chrono::DateTime<Utc>>,
    until: Option<chrono::DateTime<Utc>>,
) {
    use rusqlite::types::Value;
    if let Some(since) = since {
        sql.push_str(&format!(" and ({col} >= ? or {col} is null)"));
        args.push(Value::Text(since.to_rfc3339()));
    }
    if let Some(until) = until {
        sql.push_str(&format!(" and ({col} <= ? or {col} is null)"));
        args.push(Value::Text(until_bound_text(until)));
    }
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

fn row_to_session_with_transcript(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<SessionWithTranscript> {
    let provider: String = row.get(1)?;
    Ok(SessionWithTranscript {
        session: SessionRecord {
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
        },
        transcript_text: row.get(17)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let slash = |id: i64, seq: i64, content: &str| {
            db.conn
                .execute(
                    "insert into messages (id, session_id, provider, seq, role, content) \
                     values (?1,'s1','claude',?2,'slash',?3)",
                    params![id, seq, content],
                )
                .unwrap();
        };
        slash(1, 0, "/ar:plannew make a plan");
        slash(2, 1, "/help");
        slash(3, 2, "/ar:plannew refine it");

        // No filter (config default) → every slash command is counted.
        let all = db.planning_usage(&MessageFilters::default(), &[]).unwrap();
        assert_eq!(all.len(), 2, "both distinct commands counted");

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
    }

    #[test]
    fn search_messages_uses_fts_phrase_prefix_with_substring_fallback() {
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
        // Token + last-token prefix: "handle" matches the word "handle" (seq 0) and the
        // prefix "handler" (seq 1), but NOT the infix "mishandled" (seq 2) — proving the
        // FTS index is queried, not a raw substring scan.
        assert_eq!(seqs("handle"), vec![0, 1]);
        // A multi-word query is a contiguous phrase.
        assert_eq!(seqs("error handling"), vec![3]);
        // Punctuation-only query (no FTS tokens) falls back to a substring scan.
        assert_eq!(seqs("=>"), vec![4]);
        // Empty query lists everything (structured filters only).
        assert_eq!(seqs("").len(), 5);
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
        assert_eq!(
            hits.len(),
            1,
            "a NULL-timestamp message must not be silently dropped by a date filter"
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

        // Ambiguous prefix → names the matching candidates so the user can disambiguate.
        let err = db.resolve_session("claude:abc").unwrap_err().to_string();
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
        for p in ["claude", "codex", "cursor", "antigravity", "pi"] {
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
            5,
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
        let db = Db::open(&dir.path().join("index.db")).unwrap();
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
    fn bm25_rank_orders_literal_results_by_relevance() {
        // #225: with rank=true on a literal (FTS) query, the more relevant message (the term in a
        // short, dense document) sorts before a long diluted one — regardless of insertion order.
        // Without rank, results follow session/seq (insertion order).
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
        assert_eq!(ranked.len(), 2, "same set, reordered");
        assert_eq!(ranked[0], "needle", "BM25 ranks the short, dense doc first");
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
