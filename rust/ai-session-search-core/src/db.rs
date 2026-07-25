use std::collections::HashMap;
use std::fs;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as NucleoConfig, Matcher as NucleoMatcher, Utf32Str};
use rayon::prelude::*;
use rusqlite::{params, Connection, ErrorCode, OptionalExtension};

use crate::models::{
    CorrectionMatch, EditOp, FileCrossRef, FileEdit, FileEditSummary, FileQuery, MessageFilters,
    MessageHit, MessageSearchMode, ParsedSession, ParserHealth, PlanningCount, Provider,
    ProviderParserHealth, Role, SearchExplain, SearchField, SearchFilters, SearchHit,
    SessionRecord, SessionTimeProfile, SessionWithTranscript,
};
use crate::runtime::ExecutionRuntime;
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
///   2: semantic message kinds and tool-call IDs are populated by the provider parsers.
///   3: codex `<turn_aborted>` harness-control records are excluded from user messages; the
///      post-reindex archive purge also removes them when their source transcript is unavailable.
///   4: exact/regex message substring acceleration uses SQLite FTS5 word+trigram indexes.
pub const SCHEMA_VERSION: i64 = 4;
const PARSER_SCHEMA_VERSION: i64 = 3;
/// Oldest on-disk generation that has every table and column required for correct reads. A
/// readable older generation can be served while `auto` upgrades parser-derived rows in a
/// background process; older and future-unknown generations require synchronous preparation.
pub const MIN_READABLE_SCHEMA_VERSION: i64 = 2;

/// Closed result of inspecting the on-disk schema before choosing query or maintenance authority.
/// `Missing` and `RecoveryRequired` are produced by the coordinator before `Db` is opened; an
/// ordinary opened connection can report only the version-derived states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SchemaState {
    Missing,
    Current,
    Older {
        current: i64,
        required: i64,
    },
    Newer {
        current: i64,
        supported: i64,
    },
    /// Current version stamp, but the derived message-search layout is hybrid/incomplete while every
    /// base table (including `messages`) is intact. The derived FTS5 objects can be rebuilt online
    /// from the intact base rows — no transcript re-read, no data loss — so a writable command can
    /// self-heal in place instead of demanding an offline `reindex --full`.
    RepairableLayout {
        reason: String,
    },
    RecoveryRequired {
        reason: String,
    },
}

impl SchemaState {
    pub(crate) fn from_version(current: i64) -> Self {
        match current.cmp(&SCHEMA_VERSION) {
            std::cmp::Ordering::Less => Self::Older {
                current,
                required: SCHEMA_VERSION,
            },
            std::cmp::Ordering::Equal => Self::Current,
            std::cmp::Ordering::Greater => Self::Newer {
                current,
                supported: SCHEMA_VERSION,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum MessageOrder {
    OldestFirst,
    NewestFirst,
}

/// Corpus-size threshold below which a regex `messages search` skips the trigram prefilter and
/// scans the structurally-filtered rows directly. The prefilter's win is amortized over a large
/// corpus; once a role/session/time/tool filter narrows the scan to a small slice, a direct regex
/// pass over that slice can beat intersecting it against the whole-corpus trigram index. See
/// [`Db::search_messages`].
const TRIGRAM_PREFILTER_MIN_CORPUS: i64 = 50_000;

/// How many newer-than-base messages may accumulate before the custom trigram base index is
/// rebuilt (in parallel). Below this, messages with `id > base_max` form the "delta" and are
/// regex-verified by a direct scan rather than via the index — bounded by the SAME magnitude as
/// [`TRIGRAM_PREFILTER_MIN_CORPUS`] so the un-indexed delta a query may direct-scan stays in the
/// range a direct scan already handles cheaply. See [`Db::ensure_trigram_base`].
const TRIGRAM_BASE_REBUILD_DELTA: i64 = TRIGRAM_PREFILTER_MIN_CORPUS;

/// Maximum message rows retained for one parallel fuzzy-scoring batch. Global ranking keeps only
/// `offset + limit` scored rows between batches, so query memory is independent of corpus size.
const FUZZY_SCORE_BATCH_SIZE: usize = 512;

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

/// Per-connection SQLite page-cache target. SQLite interprets a negative `cache_size` as KiB;
/// pages are allocated on demand, so an idle connection does not reserve this entire amount.
const SQLITE_PAGE_CACHE_KIB: i64 = 64 * 1_024;

/// Maximum database prefix eligible for SQLite memory-mapped reads. This is virtual address space,
/// not eagerly allocated resident memory, and SQLite/OS may choose a lower platform-safe value.
const SQLITE_MMAP_BYTES: i64 = 256 * 1_024 * 1_024;

/// Caller-injected sink for human-facing progress notices (see [`Db::set_progress_reporter`]).
type ProgressReporter = Box<dyn Fn(&str) + Send + Sync>;

const AUTO_REINDEX_COMPLETED_MS_KEY: &str = "auto_reindex_completed_ms";

struct TrigramRebuild {
    base_max: i64,
    rebuilt: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StorageAllocation {
    pub total_bytes: u64,
    pub reclaimable_bytes: u64,
}

fn elapsed_ms(now_ms: i64, earlier_ms: i64) -> u64 {
    now_ms.saturating_sub(earlier_ms).max(0) as u64
}

pub struct Db {
    conn: Connection,
    runtime: ExecutionRuntime,
    access_scope: crate::search_scope::EffectiveAccessScope,
    /// Fixed corpus-size threshold used only by the pre-v4 compatibility prefilter.
    prefilter_min_corpus: i64,
    /// Fixed un-indexed delta size before the pre-v4 compatibility base is rebuilt.
    trigram_rebuild_delta: i64,
    /// Whether read operations may perform persistent lazy index maintenance. Disabled by the
    /// `existing-only` refresh policy; searches then reuse any existing base and scan its delta.
    implicit_index_maintenance: bool,
    /// Optional sink for human-facing progress notices (e.g. the one-time lazy index build). The
    /// library NEVER writes to stderr/stdout itself — the caller injects how (or whether) to report:
    /// the CLI sets an `eprintln` sink, the MCP server leaves it unset (silent, so nothing can
    /// pollute its stdio JSON-RPC channel). Mirrors the indexer's `progress` callback.
    progress: Option<ProgressReporter>,
}

const QUERY_PROGRESS_HANDLER_OPCODES: i32 = 10_000;

struct ProgressHandlerReset<'connection>(&'connection Connection);

impl Drop for ProgressHandlerReset<'_> {
    fn drop(&mut self) {
        self.0.progress_handler(0, None::<fn() -> bool>);
    }
}

pub(crate) fn with_sqlite_query_timeout<T>(
    connection: &Connection,
    timeout_ms: Option<NonZeroU64>,
    operation: &str,
    recovery: &str,
    run: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let Some(timeout_ms) = timeout_ms else {
        return run();
    };
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(timeout_ms.get()))
        .ok_or_else(|| anyhow!("{operation} timeout_ms is too large for this platform"))?;
    connection.progress_handler(
        QUERY_PROGRESS_HANDLER_OPCODES,
        Some(move || Instant::now() >= deadline),
    );
    let reset = ProgressHandlerReset(connection);
    let result = run();
    drop(reset);
    result.map_err(|error| {
        if error.chain().any(|cause| {
            matches!(
                cause.downcast_ref::<rusqlite::Error>(),
                Some(rusqlite::Error::SqliteFailure(inner, _))
                    if inner.code == rusqlite::ErrorCode::OperationInterrupted
            )
        }) {
            anyhow!(
                "{operation} timed out after {} ms; {recovery}",
                timeout_ms.get()
            )
        } else {
            error
        }
    })
}

macro_rules! session_record_columns {
    () => {
        "s.id, s.provider, s.provider_session_id, s.title, s.summary, s.cwd, s.repo_root, \
         s.created_at, s.updated_at, s.last_message_at, s.preview_text, s.source_path, \
         s.message_count, s.parse_version, s.raw_metadata_json, s.parse_warning, s.discovery_source, \
         s.parent_session_id, s.agent_label"
    };
}

impl Db {
    pub(crate) fn with_query_timeout<T>(
        &self,
        timeout_ms: Option<NonZeroU64>,
        run: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        with_sqlite_query_timeout(
            &self.conn,
            timeout_ms,
            "message search",
            "narrow the query or increase search.budgets.sqlite_timeout_ms",
            run,
        )
    }

    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_busy_timeout(path, DEFAULT_BUSY_TIMEOUT_MS)
    }

    pub fn open_with_busy_timeout(path: &Path, busy_timeout_ms: u64) -> Result<Self> {
        let worker_threads = std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN);
        Self::open_with_threads(path, busy_timeout_ms, worker_threads)
    }

    /// Open a database with an application-owned fixed-size worker runtime.
    pub fn open_with_threads(
        path: &Path,
        busy_timeout_ms: u64,
        worker_threads: NonZeroUsize,
    ) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_millis(busy_timeout_ms))?;
        crate::sql_functions::register(&conn)?;
        let db = Self {
            conn,
            runtime: ExecutionRuntime::new(worker_threads),
            access_scope: crate::search_scope::EffectiveAccessScope::All,
            prefilter_min_corpus: TRIGRAM_PREFILTER_MIN_CORPUS,
            trigram_rebuild_delta: TRIGRAM_BASE_REBUILD_DELTA,
            implicit_index_maintenance: true,
            progress: None,
        };
        db.init()?;
        Ok(db)
    }

    /// Open an existing index with SQLite-enforced read-only/query-only authority. This path
    /// deliberately skips [`Db::init`]: it never creates directories, tables, triggers, indexes,
    /// or compatibility objects and never changes durable database contents. SQLite itself may
    /// create or update empty `-wal`/`-shm` coordination sidecars when they are absent. UDF
    /// registration and cache/temp/mmap pragmas affect
    /// only this connection and are required by the shared query implementation.
    pub(crate) fn open_existing_read_only_with_threads(
        path: &Path,
        busy_timeout_ms: u64,
        worker_threads: NonZeroUsize,
    ) -> Result<Self> {
        let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_URI
            | rusqlite::OpenFlags::SQLITE_OPEN_PRIVATE_CACHE;
        // Callers only reach this open after IndexCoordinator::inspect_schema confirmed the
        // index exists and is readable (service.rs bails out earlier, before ever calling this,
        // when the schema state is Missing). So a failure here is NOT "it doesn't exist yet" —
        // either file permissions changed, or another process removed/relocated it in the
        // brief window between that check and this open.
        let conn = Connection::open_with_flags(path, flags).with_context(|| {
            format!(
                "failed to open existing index {} read-only even though it was just verified to \
                 exist; check file permissions, or another process may have removed or \
                 relocated it in the meantime",
                path.display()
            )
        })?;
        conn.busy_timeout(Duration::from_millis(busy_timeout_ms))?;
        crate::sql_functions::register(&conn)?;
        conn.pragma_update(None, "query_only", true)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.pragma_update(None, "temp_store", 2_i64)?;
        conn.pragma_update(None, "cache_size", -SQLITE_PAGE_CACHE_KIB)?;
        conn.pragma_update(None, "mmap_size", SQLITE_MMAP_BYTES)?;
        Ok(Self {
            conn,
            runtime: ExecutionRuntime::new(worker_threads),
            access_scope: crate::search_scope::EffectiveAccessScope::All,
            prefilter_min_corpus: TRIGRAM_PREFILTER_MIN_CORPUS,
            trigram_rebuild_delta: TRIGRAM_BASE_REBUILD_DELTA,
            implicit_index_maintenance: false,
            progress: None,
        })
    }

    /// Number of data-parallel workers owned by this database lifecycle.
    pub fn worker_threads(&self) -> usize {
        self.runtime.worker_threads()
    }

    pub(crate) fn set_access_scope(
        &mut self,
        access_scope: crate::search_scope::EffectiveAccessScope,
    ) {
        self.access_scope = access_scope;
    }

    fn validate_access_scope(&self) -> Result<()> {
        self.access_scope.validate_stable()
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

    /// Return main-database allocation from SQLite metadata without scanning rows or pages.
    pub(crate) fn storage_allocation(&self) -> Result<StorageAllocation> {
        let (page_size, page_count, freelist_count): (i64, i64, i64) = self.conn.query_row(
            "select (select page_size from pragma_page_size),
                    (select page_count from pragma_page_count),
                    (select freelist_count from pragma_freelist_count)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if page_size < 0 || page_count < 0 || freelist_count < 0 {
            return Err(anyhow::anyhow!(
                "SQLite returned negative allocation metadata: page_size={page_size}, page_count={page_count}, freelist_count={freelist_count}"
            ));
        }
        let page_size = page_size as u64;
        let total_bytes = page_size.checked_mul(page_count as u64).ok_or_else(|| {
            anyhow::anyhow!(
                "SQLite allocation size overflow: page_size={page_size}, page_count={page_count}"
            )
        })?;
        let reclaimable_bytes = page_size
            .checked_mul(freelist_count as u64)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "SQLite reclaimable size overflow: page_size={page_size}, freelist_count={freelist_count}"
                )
            })?;
        Ok(StorageAllocation {
            total_bytes,
            reclaimable_bytes,
        })
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
            // Both failed: chain the restore failure onto the original error instead of
            // discarding it, since the busy_timeout pragma may now be left at
            // `busy_timeout_ms` rather than restored to `original`.
            (Err(err), Err(restore_err)) => Err(err.context(format!(
                "also failed to restore busy_timeout to its prior value ({original}ms): {restore_err:#}"
            ))),
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

    /// Control whether reads may build persistent derived indexes lazily.
    pub(crate) fn set_implicit_index_maintenance(&mut self, enabled: bool) {
        self.implicit_index_maintenance = enabled;
    }

    /// Inject a sink for human-facing progress notices (e.g. the one-time lazy trigram-index build).
    /// Lets a terminal frontend report progress without the library hardcoding stderr; leave it unset
    /// for silent operation (the MCP server, tests). Call once after [`Db::open`].
    pub fn set_progress_reporter(&mut self, reporter: impl Fn(&str) + Send + Sync + 'static) {
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
                discovery_source text not null,
                parent_session_id text,
                agent_label text
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
                parse_version text not null default '',
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
                kind text not null default 'unknown',
                tool_call_id text,
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
            create index if not exists idx_messages_tool_name
                on messages(tool_name) where tool_name is not null;
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
            create index if not exists idx_file_edits_provider on file_edits(provider);
            create index if not exists idx_file_edits_path on file_edits(file_path);
            create index if not exists idx_file_edits_name on file_edits(file_name);
            ",
        )?;
        self.conn
            .pragma_update(None, "cache_size", -SQLITE_PAGE_CACHE_KIB)?;
        self.conn
            .pragma_update(None, "mmap_size", SQLITE_MMAP_BYTES)?;
        // `create table if not exists` does not add columns to a version-1 index. Additive
        // migration keeps legacy rows readable; `user_version < SCHEMA_VERSION` then requests
        // the full parser backfill that replaces their `unknown` kinds with provider evidence.
        for (name, definition) in [
            ("kind", "kind text not null default 'unknown'"),
            ("tool_call_id", "tool_call_id text"),
        ] {
            let exists: bool = self.conn.query_row(
                "select exists(select 1 from pragma_table_info('messages') where name = ?1)",
                params![name],
                |row| row.get(0),
            )?;
            if !exists {
                self.conn
                    .execute_batch(&format!("alter table messages add column {definition}"))?;
            }
        }
        self.conn.execute_batch(
            "create index if not exists idx_messages_tool_calls
             on messages(session_id, seq) where kind = 'tool_call'",
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
        crate::fts::install_released_message_word_index(&self.conn)?;
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
        // Initialization needs only empty/non-empty state. EXISTS stops after the first row;
        // COUNT(*) scanned 1.49M rows plus both FTS populations on every CLI/MCP/Python open.
        let messages_exist: bool =
            self.conn
                .query_row("select exists(select 1 from messages)", [], |row| {
                    row.get(0)
                })?;
        // A brand-new empty database has no incompatible data to migrate. Install the current
        // layout immediately so its first indexing pass maintains both FTS indexes atomically.
        // Existing schema-0 databases with messages still take the parser-backfill/v3 path first.
        if self.schema_version()? == 0 && !messages_exist {
            // Install the target layout and stamp the version ATOMICALLY. `user_version` is a
            // transactional header field, so a single transaction makes "create the FTS5 message
            // objects" and "declare this database current" all-or-nothing: a crash can never commit
            // the current stamp over a partially-built layout — the exact v4-stamped-but-missing-
            // trigram hybrid that the self-heal path (see `SchemaState::RepairableLayout`) exists to
            // repair. Ordering alone previously kept this safe; the transaction also protects any
            // future reordering.
            let tx = self.conn.unchecked_transaction()?;
            crate::fts::install_target_message_search_indexes(&tx)?;
            tx.execute_batch(&format!("pragma user_version = {SCHEMA_VERSION}"))?;
            tx.commit()?;
        }
        let indexed_messages_exist: bool = self.conn.query_row(
            "select exists(select 1 from messages_fts_docsize)",
            [],
            |row| row.get(0),
        )?;
        if messages_exist && !indexed_messages_exist {
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
        if self.schema_version()? < 4 {
            crate::trigram_index::ensure_schema(&self.conn)?;
            // No released schema before v4 owns an FTS5 messages_trigram table. Remove abandoned
            // development copies so the v3 custom index remains the sole compatibility path.
            self.conn.execute_batch(
                "drop trigger if exists messages_tri_ai;
                 drop trigger if exists messages_tri_ad;
                 drop trigger if exists messages_tri_au;
                 drop table if exists messages_trigram_terms;
                 drop table if exists messages_trigram_vocab;
                 drop table if exists messages_trigram;",
            )?;
        }
        // Zero-storage word-term-frequency view (fts5vocab 'row' → term,doc,cnt) for `vocab`.
        // (Trigram vocab is served from the custom index's `trigram_postings.df` column instead.)
        self.conn.execute_batch(
            "create virtual table if not exists messages_vocab
                 using fts5vocab('messages_fts', 'row');",
        )?;
        // Self-enforce the v4 message-search layout on this writable open. `install_released_message_
        // word_index` above uses `create trigger if not exists`, so on a database stamped current
        // whose dual `messages_fts`+`messages_trigram` triggers were dropped (or whose FTS5 trigram
        // tables are missing) it would otherwise silently leave word-only triggers and stop
        // maintaining `messages_trigram`, making substring/fuzzy search return incomplete results
        // with no error. Rebuilding from the intact `messages` rows here means a direct `Db::open`
        // (embedder path) — not just `SessionSearch::open`'s coordinator — cannot leave a v4 index
        // half-maintained. Only fires when the base rows a rebuild would read are present; the
        // rebuild is idempotent, so a consistent layout costs one schema scan and no rebuild.
        if self.schema_version()? == SCHEMA_VERSION
            && crate::indexer::current_schema_layout_problem(&self.conn)?.is_some()
            && crate::indexer::base_data_intact(&self.conn)?
        {
            self.migrate_message_search_schema_exclusive()?;
        }
        // Auto-populate FTS if sessions exist but FTS is empty (e.g. after schema upgrade)
        let sessions_exist: bool =
            self.conn
                .query_row("select exists(select 1 from sessions)", [], |row| {
                    row.get(0)
                })?;
        let indexed_sessions_exist: bool =
            self.conn
                .query_row("select exists(select 1 from sessions_fts)", [], |row| {
                    row.get(0)
                })?;
        if sessions_exist && !indexed_sessions_exist {
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
        let parse_version_added = self.ensure_column(
            "files_seen",
            "parse_version",
            "parse_version text not null default ''",
        )?;
        if parse_version_added {
            self.backfill_source_parse_versions()?;
        }
        // Same reasoning for the subagent-origin columns on an existing `sessions` table. NULL
        // on existing rows means "not known to be a subagent run", which is what every row
        // indexed before these columns existed truthfully was. Rows gain real values on the
        // next reindex, so no backfill is possible or needed: the fact lives in the transcript.
        self.ensure_column("sessions", "parent_session_id", "parent_session_id text")?;
        self.ensure_column("sessions", "agent_label", "agent_label text")?;
        Ok(())
    }

    /// Add `column_decl` to `table` if the column is not already present (idempotent
    /// schema evolution). Used for the `files_seen` cache columns; a no-op once the
    /// column exists, so it is safe to call on every `open`.
    fn ensure_column(&self, table: &str, column: &str, column_decl: &str) -> Result<bool> {
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
        Ok(!present)
    }

    fn backfill_source_parse_versions(&self) -> Result<()> {
        self.conn.execute_batch(
            "with source_versions as (
                 select provider, source_path, min(parse_version) as parse_version
                 from sessions
                 group by provider, source_path
                 having count(distinct parse_version) = 1
             )
             update files_seen
             set parse_version = coalesce((
                 select source_versions.parse_version
                 from source_versions
                 where source_versions.provider = files_seen.provider
                   and source_versions.source_path = files_seen.source_path
             ), '')
             where parse_version = ''",
        )?;
        Ok(())
    }

    /// Ensure the custom [`crate::trigram_index`] base is built and current enough to serve as the
    /// regex/substring prefilter, returning its `base_max_id`. Builds (in parallel) when the base
    /// is empty or the un-indexed delta (`max(id) - base_max`) exceeds
    /// `TRIGRAM_BASE_REBUILD_DELTA`; otherwise the recent delta is left for the caller to
    /// direct-scan (`id > base_max`). This makes the one-time parallel build lazy — paid on first
    /// regex use, never by `list`/`show`/`paths`/`resume` — and keeps incremental reindex free of
    /// trigram work (no triggers): new messages just accumulate in the delta until a rebuild.
    pub(crate) fn ensure_trigram_base(&self) -> Result<i64> {
        anyhow::ensure!(
            self.schema_version()? < 4,
            "custom trigram maintenance is unavailable on schema v4; SQLite FTS5 maintains messages_trigram incrementally"
        );
        if !crate::trigram_index::schema_is_compatible(&self.conn)? {
            // trigram_postings/trigram_meta are entirely derived from `messages` (like the
            // FTS5 prefilter they replace on schema v4+), so an incompatible derived-table shape
            // is safe to rebuild. Do not infer corruption from an arbitrary base_max_id error:
            // lock, I/O, and database-level failures must propagate without dropping anything.
            anyhow::ensure!(
                self.implicit_index_maintenance,
                "substring/regex search index has an incompatible schema and automatic \
                 maintenance is disabled; rerun without --index-refresh existing-only to rebuild it"
            );
            return self
                .rebuild_trigram_base_with_writer_lock()
                .map(|rebuild| rebuild.base_max);
        }
        let base_max = crate::trigram_index::base_max_id(&self.conn)?;
        if !self.implicit_index_maintenance {
            return Ok(base_max);
        }
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
            let schema_compatible = crate::trigram_index::schema_is_compatible(&self.conn)?;
            if !schema_compatible {
                self.report_progress(
                    "substring/regex search index had an incompatible schema; rebuilding it from the message table",
                );
                self.conn.execute_batch(
                    "drop table if exists trigram_postings; drop table if exists trigram_meta;",
                )?;
                crate::trigram_index::ensure_schema(&self.conn)?;
            }
            let base_max = crate::trigram_index::base_max_id(&self.conn)?;
            let max_id: i64 =
                self.conn
                    .query_row("select coalesce(max(id), 0) from messages", [], |row| {
                        row.get(0)
                    })?;
            if schema_compatible
                && !((base_max == 0 && max_id > 0)
                    || (max_id - base_max) > self.trigram_rebuild_delta)
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
            let base_max = crate::trigram_index::build_in_current_transaction(
                &self.conn,
                &self.runtime,
            )?;
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
        if self.schema_version()? >= 4 {
            self.conn.execute_batch(
                "insert into messages_trigram(messages_trigram) values('optimize');",
            )?;
        }
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
        Ok(self.schema_version()? < PARSER_SCHEMA_VERSION)
    }

    /// True when the current binary can query this schema correctly before parser-derived rows
    /// are refreshed. Future schema generations fail closed because compatibility is unknown.
    pub fn schema_is_readable(&self) -> Result<bool> {
        Ok(match self.schema_state()? {
            SchemaState::Current => true,
            SchemaState::Older { current, .. } => current >= MIN_READABLE_SCHEMA_VERSION,
            SchemaState::Missing
            | SchemaState::Newer { .. }
            | SchemaState::RepairableLayout { .. }
            | SchemaState::RecoveryRequired { .. } => false,
        })
    }

    pub(crate) fn schema_state(&self) -> Result<SchemaState> {
        Ok(SchemaState::from_version(self.schema_version()?))
    }

    pub fn schema_version(&self) -> Result<i64> {
        self.conn
            .query_row("pragma user_version", [], |row| row.get(0))
            .map_err(Into::into)
    }

    /// Promote the message-search layout on this already-elected writer connection. The caller
    /// must hold the cross-process maintenance permit for the complete full-reindex + migration
    /// operation; SQLite independently requires an exclusive journal transition and restores WAL.
    pub(crate) fn migrate_message_search_schema_exclusive(&self) -> Result<()> {
        // Idempotent: if the database already owns the current layout there is nothing to rebuild.
        // This lets `init()` self-enforce a broken v4 open while a caller that ALSO requests the heal
        // (SessionSearch::open's coordinator) becomes a cheap no-op instead of a second full rebuild.
        if self.schema_version()? == SCHEMA_VERSION
            && crate::indexer::current_schema_layout_problem(&self.conn)?.is_none()
        {
            return Ok(());
        }
        // Notify via the injected progress sink (the CLI prints it; the MCP server and tests stay
        // silent) so a one-time in-place rebuild of the message-search indexes isn't an unexplained
        // pause. Mirrors the lazy trigram-base rebuild notice in
        // [`Db::rebuild_trigram_base_with_writer_lock`].
        let count: i64 = self
            .conn
            .query_row("select count(*) from messages", [], |row| row.get(0))?;
        self.report_progress(&format!(
            "rebuilding message-search indexes in place (one-time over {count} messages)…"
        ));
        crate::fts::migrate_message_search_schema_offline(&self.conn, SCHEMA_VERSION)
    }

    /// Stamp the on-disk `user_version` after a full reindex so subsequent runs take the fast
    /// incremental path. This caps at `PARSER_SCHEMA_VERSION` and only records
    /// `SCHEMA_VERSION` when the database has ALREADY reached it — it never promotes a pre-v4
    /// index to current, because the v4 message-search layout is built and stamped atomically only
    /// by the fresh install and by `Db::migrate_message_search_schema_exclusive`. Stamping v4
    /// here (without that layout) would declare a database current while missing its trigram
    /// objects — the exact hybrid the self-heal path exists to repair.
    pub fn mark_schema_current(&self) -> Result<()> {
        let target = if self.schema_version()? >= SCHEMA_VERSION {
            SCHEMA_VERSION
        } else {
            PARSER_SCHEMA_VERSION
        };
        self.conn
            .execute_batch(&format!("pragma user_version = {target}"))?;
        Ok(())
    }

    /// Explicit, total wipe of all indexed data. NOT used by [`crate::indexer::reindex`],
    /// which is a durable archive (it never deletes sessions whose source files were
    /// removed). This is the deliberate "start over" reset for embedders / corruption
    /// recovery; the user-facing equivalent is deleting the index file.
    pub fn clear_all(&self) -> Result<()> {
        // One transaction so an interruption (or a failing statement) leaves the index either fully
        // cleared or fully intact — never a logically inconsistent mix such as sessions carrying a
        // stale message_count while their messages have already been deleted.
        self.with_immediate_transaction(|| {
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
        })
    }

    pub(crate) fn clear_trigram_base(&self) -> Result<()> {
        if self.schema_version()? >= 4 {
            return Ok(());
        }
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
        parse_version: &str,
    ) -> Result<bool> {
        let result = self
            .conn
            .query_row(
                "select mtime_ns, size_bytes, parse_version from files_seen
                 where provider = ?1 and source_path = ?2",
                params![provider.as_str(), path],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        Ok(matches!(
            result,
            Some((stored_mtime, stored_size, stored_version))
                if stored_mtime == mtime_ns
                    && stored_size == size
                    && stored_version == parse_version
        ))
    }

    pub(crate) fn indexed_source_identities(
        &self,
    ) -> Result<Vec<(Provider, String, usize, String)>> {
        let mut stmt = self.conn.prepare(
            "select provider, source_path, count(*), min(id) from sessions
             group by provider, source_path",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    Provider::from_db_str(&row.get::<_, String>(0)?),
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)? as usize,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn upsert_session(
        &self,
        parsed: &ParsedSession,
        mtime_ns: i64,
        size_bytes: i64,
    ) -> Result<()> {
        self.upsert_session_with_mode(parsed, mtime_ns, size_bytes, true, &[])
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
        self.upsert_session_with_mode(parsed, mtime_ns, size_bytes, false, &[])
    }

    pub(crate) fn upsert_session_reconciling_sources(
        &self,
        parsed: &ParsedSession,
        mtime_ns: i64,
        size_bytes: i64,
        source_aliases: &[String],
        allow_append_optimization: bool,
    ) -> Result<()> {
        self.upsert_session_with_mode(
            parsed,
            mtime_ns,
            size_bytes,
            allow_append_optimization,
            source_aliases,
        )
    }

    fn upsert_session_with_mode(
        &self,
        parsed: &ParsedSession,
        mtime_ns: i64,
        size_bytes: i64,
        allow_append_optimization: bool,
        source_aliases: &[String],
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let session = &parsed.session;
        // A complete source parse represents one canonical session. Parser improvements or
        // mutable sidecar metadata can change the provider session ID; remove prior identities
        // for this provider/source in the same transaction before publishing the replacement.
        // Preserve unrelated providers even if they happen to name the same filesystem path.
        tx.execute(
            "delete from sessions_fts where rowid in (
                 select rowid from sessions
                 where provider = ?1 and source_path = ?2 and id != ?3
             )",
            params![session.provider.as_str(), session.source_path, session.id],
        )?;
        for alias in source_aliases
            .iter()
            .filter(|alias| alias.as_str() != session.source_path)
        {
            tx.execute(
                "delete from sessions_fts where rowid in (
                     select rowid from sessions where provider = ?1 and source_path = ?2
                 )",
                params![session.provider.as_str(), alias],
            )?;
            tx.execute(
                "delete from sessions where provider = ?1 and source_path = ?2",
                params![session.provider.as_str(), alias],
            )?;
            tx.execute(
                "delete from files_seen where provider = ?1 and source_path = ?2",
                params![session.provider.as_str(), alias],
            )?;
        }
        tx.execute(
            "delete from sessions
             where provider = ?1 and source_path = ?2 and id != ?3",
            params![session.provider.as_str(), session.source_path, session.id],
        )?;
        tx.execute(
            "
            insert into sessions (
                id, provider, provider_session_id, title, summary, cwd, repo_root, created_at,
                updated_at, last_message_at, preview_text, source_path, message_count, parse_version,
                raw_metadata_json, parse_warning, discovery_source, parent_session_id, agent_label
            ) values (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                ?9, ?10, ?11, ?12, ?13, ?14,
                ?15, ?16, ?17, ?18, ?19
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
                discovery_source = excluded.discovery_source,
                parent_session_id = excluded.parent_session_id,
                agent_label = excluded.agent_label
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
                session.parent_session_id,
                session.agent_label,
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
            insert into files_seen (
                provider, source_path, mtime_ns, size_bytes, last_indexed_at,
                content_hash, parse_version
            )
            values (?1, ?2, ?3, ?4, ?5, null, ?6)
            on conflict(provider, source_path) do update set
                mtime_ns = excluded.mtime_ns,
                size_bytes = excluded.size_bytes,
                last_indexed_at = excluded.last_indexed_at,
                parse_version = excluded.parse_version
            ",
            params![
                session.provider.as_str(),
                session.source_path,
                mtime_ns,
                size_bytes,
                Utc::now().to_rfc3339(),
                session.parse_version,
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
    /// `<environment_context>` / `<turn_aborted>` (codex). The current parser already excludes
    /// these from re-parsed files, but sessions whose source file was deleted are never re-visited
    /// (durable archive), so their already-indexed injected rows persist; this one-time data purge
    /// reaches them. Returns the number of rows deleted. The `messages_fts` delete trigger keeps the
    /// word index in sync; the custom trigram base is rebuilt lazily on next use. Run during the
    /// schema migration (see indexer.rs).
    pub fn purge_injected_messages(&self) -> Result<usize> {
        let deleted = self.conn.execute(
            "delete from messages where role = 'user' and (\
                 content like '<local-command-stdout>%' \
              or content like '<local-command-stderr>%' \
              or content like '<local-command-caveat>%' \
              or content like '<environment_context>%' \
              or content like '<turn_aborted>%')",
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
                tail_byte_offset = ?6, prefix_fingerprint = ?7, parse_version = ?8
             where provider = ?1 and source_path = ?2",
            params![
                session.provider.as_str(),
                session.source_path,
                mtime_ns,
                size_bytes,
                Utc::now().to_rfc3339(),
                tail.new_tail_offset,
                tail.new_fingerprint,
                session.parse_version,
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

    /// Whether at least one session has been indexed, without counting the full table.
    pub fn has_sessions(&self) -> Result<bool> {
        Ok(self
            .conn
            .query_row("select exists(select 1 from sessions limit 1)", [], |row| {
                row.get(0)
            })?)
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

        self.validate_access_scope()?;

        let mut sql = String::from("select m.role, count(*) from messages m where 1 = 1");
        let mut args: Vec<Value> = Vec::new();
        append_message_filters(&mut sql, &mut args, filters, &self.access_scope);
        sql.push_str(" group by m.role order by m.role");

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Term-frequency vocabulary over the message indexes. Returns
    /// `(term, doc_count, total_count)` ordered by total occurrences descending, then term.
    /// `trigram=false` reads the word-token `messages_vocab` view and reports true occurrence
    /// counts. `trigram=true` reads schema-v4's row-level `messages_trigram_terms` view. Because the
    /// compact trigram index uses `detail=none`, it records term/document membership rather than
    /// per-position occurrence counts; doc and total count therefore both mean document frequency.
    /// Schema versions older than v4 retain the custom-index compatibility source until migration.
    /// `limit == 0` returns all terms.
    pub fn vocabulary(&self, trigram: bool, limit: usize) -> Result<Vec<(String, i64, i64)>> {
        let lim: i64 = if limit == 0 { -1 } else { limit as i64 };
        let schema_version = self.schema_version()?;
        let sql = if trigram && schema_version >= 4 {
            // The row vocabulary makes SQLite aggregate postings once inside FTS5. Reading the
            // instance vocabulary here would emit one row per term/document and then re-aggregate
            // millions of postings for even a five-row result.
            "select term, doc, doc as total_count
               from messages_trigram_terms
              order by doc desc, term
              limit ?1"
        } else if trigram {
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
    /// `mishandled` all match literally). Schema v4 uses SQLite word/trigram indexes to admit
    /// candidates; exact literal or Rust regex verification remains authoritative. Fuzzy mode
    /// uses bounded-memory Nucleo sequence scoring across every structurally eligible row, then
    /// applies `offset` and `limit`. It is sequence matching rather than edit distance.
    /// Exact/regex `limit == 0` is unlimited. Fuzzy requires a query of at least three characters
    /// and a positive limit.
    pub fn search_messages(
        &self,
        query: &str,
        filters: &MessageFilters,
    ) -> Result<Vec<MessageHit>> {
        Ok(self.search_messages_with_explain(query, filters, false)?.0)
    }

    pub(crate) fn search_message_plan(
        &self,
        plan: &crate::message_search::MessageRetrievalPlan,
        include_explain: bool,
    ) -> Result<(Vec<MessageHit>, Option<SearchExplain>)> {
        use crate::message_search::{MatchWindow, MessageQuery, ResolvedExtent};

        let query = plan.query.text().unwrap_or("");
        let match_mode = match &plan.query {
            MessageQuery::All | MessageQuery::Literal(_) => MessageSearchMode::Exact,
            MessageQuery::Regex(_) => MessageSearchMode::Regex,
            MessageQuery::Fuzzy(_) => MessageSearchMode::Fuzzy,
        };
        let (limit, offset) = match plan.extent {
            ResolvedExtent::Page { limit, offset } => (
                limit
                    .get()
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("message page limit plus look-ahead overflows"))?,
                offset,
            ),
            ResolvedExtent::AllResults { offset } => (0, offset),
        };
        let filters = MessageFilters {
            role: plan.predicates.role,
            kinds: plan.predicates.kinds.clone(),
            field: Some(plan.target.field()),
            argument_path: plan
                .target
                .argument_path()
                .map(|pointer| pointer.as_str().to_string()),
            provider: plan.predicates.provider,
            session_id: plan.predicates.session_id.clone(),
            workspace_path_prefix: plan.predicates.workspace_path_prefix.clone(),
            transcript_path_prefix: plan.predicates.transcript_path_prefix.clone(),
            exclude_workspace_path_prefixes: plan
                .predicates
                .exclude_workspace_path_prefixes
                .clone(),
            exclude_transcript_path_prefixes: plan
                .predicates
                .exclude_transcript_path_prefixes
                .clone(),
            exclude_session_ids: plan.predicates.exclude_session_ids.clone(),
            since: plan.predicates.time.since(),
            until: plan.predicates.time.until(),
            seq_from: plan.predicates.sequence.and_then(|range| range.from()),
            seq_to: plan.predicates.sequence.and_then(|range| range.to()),
            match_mode,
            tool: plan.predicates.tool_name_contains.clone(),
            no_compaction: !plan.predicates.include_compaction,
            limit,
            offset,
            ..Default::default()
        };
        let order = if plan.match_window == Some(MatchWindow::Latest) {
            MessageOrder::NewestFirst
        } else {
            MessageOrder::OldestFirst
        };
        self.search_messages_with_explain_order(query, &filters, include_explain, order)
    }

    pub(crate) fn search_messages_ordered(
        &self,
        query: &str,
        filters: &MessageFilters,
        order: MessageOrder,
    ) -> Result<Vec<MessageHit>> {
        anyhow::ensure!(
            order == MessageOrder::OldestFirst || filters.session_id.is_some(),
            "newest-first message order requires session_id because sequence numbers are session-local"
        );
        Ok(self
            .search_messages_with_explain_order(query, filters, false, order)?
            .0)
    }

    /// Read one session's messages, selecting the first (oldest) or last (newest)
    /// `filters.limit` by `order`, and ALWAYS returning them in chronological (seq-ascending)
    /// order so the caller reads oldest→newest regardless of the selection direction.
    ///
    /// `order` decides WHICH messages are kept, not just their display order (so
    /// `NewestFirst` + `limit = 75` is the last 75, not the first 75 shown backwards — this
    /// avoids the `git log --reverse` trap where reverse applies after the limit). It is the
    /// clean equivalent of a hand-written `ORDER BY seq DESC LIMIT N`.
    ///
    /// Scope, role, dates, `limit` (unsigned; 0 = all), `offset`, and `seq_from`/`seq_to` come
    /// from `filters`. `filters.session_id` is required because sequence numbers are
    /// session-local, so a newest-first window is otherwise undefined. `seq_from`/`seq_to`
    /// bound the set before the window applies, giving non-overlapping chunked reads; `offset`
    /// skips from the leading edge of the chosen direction.
    pub fn read_session_messages(
        &self,
        filters: &MessageFilters,
        order: MessageOrder,
    ) -> Result<Vec<MessageHit>> {
        anyhow::ensure!(
            filters.session_id.is_some(),
            "read_session_messages requires filters.session_id because sequence numbers are session-local"
        );
        let mut hits = self.search_messages_ordered("", filters, order)?;
        if order == MessageOrder::NewestFirst {
            // A newest-first fetch returns seq-descending rows; restore chronological order.
            hits.reverse();
        }
        Ok(hits)
    }

    /// Like [`Db::search_messages`], optionally returning the exact planner diagnostics used by
    /// this search. This keeps MCP `explain`, CLI `--explain`, and the search path on one shared
    /// FTS/trigram decision instead of running the planner twice. Candidate counts are interpreted
    /// with the named strategy: message IDs for content/tool-argument FTS, distinct names for the
    /// bounded tool-name vocabulary, or retained top-K rows for pre-v4 compatibility streaming.
    pub fn search_messages_with_explain(
        &self,
        query: &str,
        filters: &MessageFilters,
        include_explain: bool,
    ) -> Result<(Vec<MessageHit>, Option<SearchExplain>)> {
        self.search_messages_with_explain_order(
            query,
            filters,
            include_explain,
            MessageOrder::OldestFirst,
        )
    }

    fn search_messages_with_explain_order(
        &self,
        query: &str,
        filters: &MessageFilters,
        include_explain: bool,
        order: MessageOrder,
    ) -> Result<(Vec<MessageHit>, Option<SearchExplain>)> {
        use rusqlite::types::Value;

        self.validate_access_scope()?;
        filters.validate(query)?;
        let field = filters.field.unwrap_or(SearchField::Content);
        if field != SearchField::Content {
            return self.search_derived_message_field(
                query,
                filters,
                field,
                include_explain,
                order,
            );
        }

        let mut sql = String::from(
            "select m.session_id, m.provider, m.seq, m.role, m.ts, m.tool_name, m.kind, m.tool_call_id, m.content \
             from messages m where 1 = 1",
        );
        let mut args: Vec<Value> = Vec::new();
        append_message_filters(&mut sql, &mut args, filters, &self.access_scope);
        if filters.match_mode == MessageSearchMode::Fuzzy {
            sql.push_str(if order == MessageOrder::NewestFirst {
                " order by m.session_id, m.seq desc"
            } else {
                " order by m.session_id, m.seq"
            });
            let ranked_limit = fuzzy_ranked_limit(filters)?;
            let pattern = Pattern::new(
                query,
                CaseMatching::Ignore,
                Normalization::Smart,
                AtomKind::Fuzzy,
            );
            let query_lower = query.to_lowercase();
            let mut stmt = self.conn.prepare(&sql)?;
            let rows =
                stmt.query_map(rusqlite::params_from_iter(args.iter()), row_to_message_hit)?;
            let mut batch = Vec::with_capacity(FUZZY_SCORE_BATCH_SIZE);
            let mut top = Vec::new();
            let mut corpus = 0_i64;
            let mut matched = 0_i64;
            for row in rows {
                batch.push(row?);
                corpus += 1;
                if batch.len() == FUZZY_SCORE_BATCH_SIZE {
                    let scored = self.runtime.install(|| {
                        score_fuzzy_message_hits(&pattern, &query_lower, std::mem::take(&mut batch))
                    })?;
                    matched += scored.len() as i64;
                    top.extend(scored);
                    if top.len() >= top_k_compaction_threshold(ranked_limit) {
                        retain_top_fuzzy_hits(&mut top, ranked_limit);
                    }
                }
            }
            if !batch.is_empty() {
                let scored = self
                    .runtime
                    .install(|| score_fuzzy_message_hits(&pattern, &query_lower, batch))?;
                matched += scored.len() as i64;
                top.extend(scored);
            }
            let hits = finish_fuzzy_hits(top, ranked_limit, filters.offset);
            let explain = include_explain.then(|| SearchExplain {
                prefilter: None,
                candidates: Some(matched),
                prefilter_skipped: Some(
                    "complete filtered corpus scored with bounded top-K retention".to_string(),
                ),
                corpus,
            });
            return Ok((hits, explain));
        }

        let literal_query = filters.match_mode == MessageSearchMode::Exact && !query.is_empty();
        if literal_query {
            sql.push_str(" and unicode_lower_contains(m.content, ?)");
            args.push(Value::Text(query.to_lowercase()));
        }
        if filters.match_mode == MessageSearchMode::Regex {
            regex::Regex::new(query).map_err(|error| anyhow!("invalid regex: {error}"))?;
            sql.push_str(" and rust_regexp(?, m.content)");
            args.push(Value::Text(query.to_string()));
        }
        let prefilter_pattern = if filters.match_mode == MessageSearchMode::Regex {
            Some(query.to_string())
        } else if literal_query {
            Some(regex::escape(query))
        } else {
            None
        };
        let explain = if self.schema_version()? >= 4 {
            let prefilter = prefilter_pattern
                .as_deref()
                .and_then(crate::trigram::trigram_prefilter);
            if let Some(fts_query) = &prefilter {
                sql.push_str(
                    " and m.id in (
                         select rowid from messages_trigram
                          where messages_trigram match ?
                     )",
                );
                args.push(Value::Text(fts_query.clone()));
            }
            if include_explain {
                Some(SearchExplain {
                    prefilter: prefilter.clone(),
                    candidates: prefilter
                        .as_deref()
                        .map(|fts_query| self.fts5_candidate_count(filters, fts_query))
                        .transpose()?,
                    prefilter_skipped: prefilter
                        .is_none()
                        .then(|| "no required literal of at least three characters".into()),
                    corpus: self.filtered_corpus_count(filters)?,
                })
            } else {
                None
            }
        } else {
            let (use_trigram_candidates, explain) = self.prepare_content_prefilter(
                prefilter_pattern.as_deref(),
                filters,
                include_explain,
            )?;
            if use_trigram_candidates {
                sql.push_str(" and m.id in (select id from _trigram_cand)");
            }
            explain
        };
        sql.push_str(if order == MessageOrder::NewestFirst {
            " order by m.session_id, m.seq desc"
        } else {
            " order by m.session_id, m.seq"
        });
        if filters.limit > 0 {
            sql.push_str(" limit ?");
            args.push(Value::Integer(filters.limit as i64));
            if filters.offset > 0 {
                sql.push_str(" offset ?");
                args.push(Value::Integer(filters.offset as i64));
            }
        } else if filters.offset > 0 {
            sql.push_str(" limit -1 offset ?");
            args.push(Value::Integer(filters.offset as i64));
        }
        let mut stmt = self.conn.prepare(&sql)?;
        let hits = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), row_to_message_hit)?
            .collect::<rusqlite::Result<_>>()?;
        Ok((hits, explain))
    }

    fn fts5_candidate_count(&self, filters: &MessageFilters, fts_query: &str) -> Result<i64> {
        use rusqlite::types::Value;
        let mut sql = String::from("select count(*) from messages m where 1 = 1");
        let mut args = Vec::new();
        append_message_filters(&mut sql, &mut args, filters, &self.access_scope);
        sql.push_str(
            " and m.id in (
                 select rowid from messages_trigram where messages_trigram match ?
             )",
        );
        args.push(Value::Text(fts_query.to_string()));
        Ok(self
            .conn
            .query_row(&sql, rusqlite::params_from_iter(args.iter()), |row| {
                row.get(0)
            })?)
    }

    fn search_derived_message_field(
        &self,
        query: &str,
        filters: &MessageFilters,
        field: SearchField,
        include_explain: bool,
        order: MessageOrder,
    ) -> Result<(Vec<MessageHit>, Option<SearchExplain>)> {
        if field == SearchField::ToolName
            && filters.match_mode == MessageSearchMode::Fuzzy
            && self.schema_version()? >= 4
        {
            return self.search_tool_name_fuzzy_indexed(query, filters, include_explain);
        }
        let mut sql = String::from(
            "select m.session_id, m.provider, m.seq, m.role, m.ts, m.tool_name, m.kind, m.tool_call_id, m.content \
             from messages m where 1 = 1",
        );
        let mut args = Vec::new();
        append_message_filters(&mut sql, &mut args, filters, &self.access_scope);
        let sql_filters_projection = match (field, filters.match_mode) {
            (SearchField::ToolName, MessageSearchMode::Exact) => {
                sql.push_str(" and unicode_lower_contains(m.tool_name, ?)");
                args.push(rusqlite::types::Value::Text(query.to_lowercase()));
                true
            }
            (SearchField::ToolName, MessageSearchMode::Regex) => {
                regex::Regex::new(query).map_err(|error| anyhow!("invalid regex: {error}"))?;
                sql.push_str(" and rust_regexp(?, m.tool_name)");
                args.push(rusqlite::types::Value::Text(query.to_string()));
                true
            }
            // TODO(perf): exact/regex tool-argument search parses every filtered tool_call
            // row's JSON (O(rows x JSON bytes)); an exact literal of >= 3 chars could be
            // routed through the messages_trigram prefilter first (the literal must appear
            // in raw content for the pointer projection to contain it), like fuzzy already
            // does. Deferred past rc.1: correctness-sensitive to prefilter supersets.
            (SearchField::ToolArgument, MessageSearchMode::Exact) => {
                sql.push_str(" and unicode_lower_contains(rust_json_pointer(?, m.content), ?)");
                args.push(rusqlite::types::Value::Text(
                    filters.argument_path.clone().unwrap_or_default(),
                ));
                args.push(rusqlite::types::Value::Text(query.to_lowercase()));
                true
            }
            (SearchField::ToolArgument, MessageSearchMode::Regex) => {
                regex::Regex::new(query).map_err(|error| anyhow!("invalid regex: {error}"))?;
                sql.push_str(" and rust_regexp(?, rust_json_pointer(?, m.content))");
                args.push(rusqlite::types::Value::Text(query.to_string()));
                args.push(rusqlite::types::Value::Text(
                    filters.argument_path.clone().unwrap_or_default(),
                ));
                true
            }
            _ => false,
        };
        if field == SearchField::ToolArgument && filters.kinds.is_none() {
            sql.push_str(" and m.kind = 'tool_call'");
        }
        sql.push_str(if order == MessageOrder::NewestFirst {
            " order by m.session_id, m.seq desc"
        } else {
            " order by m.session_id, m.seq"
        });
        if sql_filters_projection && filters.limit > 0 {
            sql.push_str(" limit ? offset ?");
            args.push(rusqlite::types::Value::Integer(filters.limit as i64));
            args.push(rusqlite::types::Value::Integer(filters.offset as i64));
        } else if sql_filters_projection && filters.offset > 0 {
            sql.push_str(" limit -1 offset ?");
            args.push(rusqlite::types::Value::Integer(filters.offset as i64));
        }
        if sql_filters_projection {
            let candidates = self.query_message_hits(&sql, &args)?;
            let corpus = candidates.len() as i64;
            let explain = include_explain.then(|| SearchExplain {
                prefilter: None,
                candidates: Some(corpus),
                prefilter_skipped: Some(
                    "derived field verified and paginated in SQLite".to_string(),
                ),
                corpus,
            });
            return Ok((candidates, explain));
        }
        debug_assert_eq!(filters.match_mode, MessageSearchMode::Fuzzy);
        let pattern = Pattern::new(
            query,
            CaseMatching::Ignore,
            Normalization::Smart,
            AtomKind::Fuzzy,
        );
        let query_lower = query.to_lowercase();
        let mut matcher = NucleoMatcher::new(NucleoConfig::DEFAULT);
        let mut utf32_buf = Vec::new();
        let ranked_limit = fuzzy_ranked_limit(filters)?;
        let mut top = Vec::new();
        let mut corpus = 0_i64;
        let mut matched = 0_i64;
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), row_to_message_hit)?;
        for row in rows {
            let mut hit = row?;
            corpus += 1;
            let Some(value) = message_field_value(&hit, field, filters.argument_path.as_deref())
            else {
                continue;
            };
            utf32_buf.clear();
            if let Some(score) = pattern.score(Utf32Str::new(&value, &mut utf32_buf), &mut matcher)
            {
                matched += 1;
                hit.fuzzy_score = Some(score);
                let exact_phrase = value.to_lowercase().contains(&query_lower);
                top.push((hit, exact_phrase));
                if top.len() >= top_k_compaction_threshold(ranked_limit) {
                    retain_top_fuzzy_hits(&mut top, ranked_limit);
                }
            }
        }
        let hits = finish_fuzzy_hits(top, ranked_limit, filters.offset);
        let explain = include_explain.then(|| SearchExplain {
            prefilter: None,
            candidates: Some(matched),
            prefilter_skipped: Some(
                "complete filtered corpus scored with bounded top-K retention".to_string(),
            ),
            corpus,
        });
        Ok((hits, explain))
    }

    fn search_tool_name_fuzzy_indexed(
        &self,
        query: &str,
        filters: &MessageFilters,
        include_explain: bool,
    ) -> Result<(Vec<MessageHit>, Option<SearchExplain>)> {
        let mut sql = String::from(
            "select m.session_id, m.provider, m.seq, m.role, m.ts, m.tool_name,
                    m.kind, m.tool_call_id, m.content
               from messages m
              where m.tool_name is not null",
        );
        let mut args = Vec::new();
        append_message_filters(&mut sql, &mut args, filters, &self.access_scope);
        sql.push_str(" order by m.tool_name, m.session_id, m.seq");

        let ranked_limit = fuzzy_ranked_limit(filters)?;
        let pattern = Pattern::new(
            query,
            CaseMatching::Ignore,
            Normalization::Smart,
            AtomKind::Fuzzy,
        );
        let query_lower = query.to_lowercase();
        let mut matcher = NucleoMatcher::new(NucleoConfig::DEFAULT);
        let mut utf32_buf = Vec::new();
        let mut cached_name = None;
        let mut cached_match = None;
        let mut top = Vec::new();
        let mut corpus = 0_i64;
        let mut matched = 0_i64;
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), row_to_message_hit)?;
        for row in rows {
            let mut hit = row?;
            corpus += 1;
            let name = hit
                .tool_name
                .as_deref()
                .expect("SQL excludes messages without tool names");
            if cached_name.as_deref() != Some(name) {
                utf32_buf.clear();
                cached_match = pattern
                    .score(Utf32Str::new(name, &mut utf32_buf), &mut matcher)
                    .map(|score| (score, name.to_lowercase().contains(&query_lower)));
                cached_name = Some(name.to_string());
            }
            if let Some((score, exact_phrase)) = cached_match {
                matched += 1;
                hit.fuzzy_score = Some(score);
                top.push((hit, exact_phrase));
                if top.len() >= top_k_compaction_threshold(ranked_limit) {
                    retain_top_fuzzy_hits(&mut top, ranked_limit);
                }
            }
        }
        let hits = finish_fuzzy_hits(top, ranked_limit, filters.offset);
        let explain = include_explain.then(|| SearchExplain {
            prefilter: None,
            candidates: Some(matched),
            prefilter_skipped: Some(
                "complete filtered corpus scored with bounded top-K retention".to_string(),
            ),
            corpus,
        });
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
        append_message_filters(&mut sql, &mut args, filters, &self.access_scope);
        Ok(self
            .conn
            .query_row(&sql, rusqlite::params_from_iter(args.iter()), |row| {
                row.get(0)
            })?)
    }

    /// Determine whether the structurally filtered corpus reaches `threshold` without counting
    /// beyond the planner decision boundary. Unlike [`Db::filtered_corpus_count`], this is safe on
    /// the normal hot path because SQLite stops the inner relation after `threshold` rows.
    fn filtered_corpus_reaches(&self, filters: &MessageFilters, threshold: i64) -> Result<bool> {
        use rusqlite::types::Value;
        anyhow::ensure!(threshold > 0, "corpus probe threshold must be positive");
        let mut inner = String::from("select 1 from messages m where 1 = 1");
        let mut args: Vec<Value> = Vec::new();
        append_message_filters(&mut inner, &mut args, filters, &self.access_scope);
        inner.push_str(" limit 1 offset ?");
        args.push(Value::Integer(threshold - 1));
        let sql = format!("select exists({inner})");
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
        append_message_filters(&mut sql, &mut args, filters, &self.access_scope);
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

        let corpus = if include_explain {
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
            || if let Some(corpus) = corpus {
                corpus >= self.prefilter_min_corpus
            } else {
                self.filtered_corpus_reaches(filters, self.prefilter_min_corpus)?
            };
        let prefilter = include_explain.then(|| crate::trigram::render_prefilter_groups(&groups));
        if !use_prefilter {
            let explain = if include_explain {
                Some(SearchExplain {
                    prefilter,
                    candidates: None,
                    prefilter_skipped: Some(format!(
                        "structured filters reduced the corpus below the pre-v4 compatibility prefilter threshold ({})",
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

    /// Execute the actual message-search plan and return its diagnostics while discarding hits.
    /// Prefer [`Db::search_messages_with_explain`] when the caller also needs results so the query
    /// runs only once. This method deliberately shares every field/mode/schema branch with search;
    /// it never invokes a second legacy-only planner.
    pub fn explain_message_search(
        &self,
        query: &str,
        filters: &MessageFilters,
    ) -> Result<SearchExplain> {
        let (_, explain) = self.search_messages_with_explain(query, filters, true)?;
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
        use rusqlite::types::Value;

        self.validate_access_scope()?;
        anyhow::ensure!(before >= 0, "context before must be non-negative");
        anyhow::ensure!(after >= 0, "context after must be non-negative");
        let mut sql = String::from(
            "select session_id, provider, seq, role, ts, tool_name, kind, tool_call_id, content from messages
             where session_id = ? and seq between ? and ?",
        );
        // Saturate instead of wrapping: a huge `context` request (e.g. i64::MAX) must widen
        // to the whole session, not overflow into a negative BETWEEN bound that silently
        // matches nothing (release) or panics (debug).
        let mut args = vec![
            Value::Text(session_id.to_string()),
            Value::Integer(seq.saturating_sub(before)),
            Value::Integer(seq.saturating_add(after)),
        ];
        push_access_scope(&mut sql, &mut args, "session_id", &self.access_scope);
        sql.push_str(" order by seq");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), row_to_message_hit)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Fetch context for multiple anchors in one SQLite statement. The returned windows preserve
    /// anchor order and include empty entries for anchors that have no visible rows.
    pub(crate) fn message_context_windows(
        &self,
        anchors: &[(String, i64)],
        before: i64,
        after: i64,
    ) -> Result<Vec<Vec<MessageHit>>> {
        use rusqlite::types::Value;

        self.validate_access_scope()?;
        anyhow::ensure!(before >= 0, "context before must be non-negative");
        anyhow::ensure!(after >= 0, "context after must be non-negative");
        let mut windows = vec![Vec::new(); anchors.len()];
        if anchors.is_empty() {
            return Ok(windows);
        }
        let bounds = anchors
            .iter()
            .map(|(session_id, seq)| {
                (
                    session_id,
                    seq.saturating_sub(before),
                    seq.saturating_add(after),
                )
            })
            .collect::<Vec<_>>();
        let mut sql = String::from(
            "with anchors(ord, session_id, lower_seq, upper_seq) as materialized (
                 select cast(key as integer),
                        json_extract(value, '$[0]'),
                        json_extract(value, '$[1]'),
                        json_extract(value, '$[2]')
                   from json_each(?)
             )
             select a.ord, m.session_id, m.provider, m.seq, m.role, m.ts, m.tool_name,
                    m.kind, m.tool_call_id, m.content
               from anchors a
               join messages m on m.session_id = a.session_id
                              and m.seq between a.lower_seq and a.upper_seq
              where 1 = 1",
        );
        let mut args = vec![Value::Text(serde_json::to_string(&bounds)?)];
        push_access_scope(&mut sql, &mut args, "m.session_id", &self.access_scope);
        sql.push_str(" order by a.ord, m.seq");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), |row| {
            Ok((row.get::<_, usize>(0)?, row_to_message_hit_at(row, 1)?))
        })?;
        for row in rows {
            let (ordinal, hit) = row?;
            windows[ordinal].push(hit);
        }
        Ok(windows)
    }

    /// Fetch compact session metadata for a set of session ids in ONE query, keyed by
    /// id. Used by the MCP `search_messages` serializer to enrich each hit with its
    /// session context without an N+1 per-hit lookup. Unknown ids are simply absent from the map.
    pub fn session_metadata(
        &self,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, crate::models::SessionMeta>> {
        use crate::models::SessionMeta;
        use rusqlite::types::Value;
        use std::collections::HashMap;
        self.validate_access_scope()?;
        let mut map = HashMap::new();
        if ids.is_empty() {
            return Ok(map);
        }
        let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let mut sql = format!(
            "select id, provider_session_id, cwd, repo_root, title, updated_at, last_message_at, \
             message_count, parse_warning from sessions where id in ({placeholders})"
        );
        let mut args: Vec<Value> = ids.iter().cloned().map(Value::Text).collect();
        push_access_scope(&mut sql, &mut args, "id", &self.access_scope);
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), |row| {
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

        self.validate_access_scope()?;
        let mut sql = String::from(
            "select m.session_id, m.provider, m.ts, m.content from messages m where 1 = 1",
        );
        let mut args: Vec<Value> = Vec::new();
        let mut filters = filters.clone();
        filters.role = Some(Role::User);
        append_message_filters(&mut sql, &mut args, &filters, &self.access_scope);
        sql.push_str(" order by m.ts desc");

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
        let mut out: Vec<CorrectionMatch> = if self.runtime.worker_threads() <= 1 {
            rows.into_iter().filter_map(classify).collect()
        } else {
            self.runtime
                .install(|| rows.into_par_iter().filter_map(classify).collect())?
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

        self.validate_access_scope()?;
        let mut sql = String::from(
            "select m.session_id, s.repo_root, s.cwd, m.content from messages m \
             join sessions s on s.id = m.session_id where 1 = 1",
        );
        let mut args: Vec<Value> = Vec::new();
        let mut filters = filters.clone();
        filters.role = Some(Role::Slash);
        append_message_filters(&mut sql, &mut args, &filters, &self.access_scope);

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

        self.validate_access_scope()?;
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
        push_file_filters(&mut sql, &mut args, query, &self.access_scope);
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
        if query.limit > 0 || query.offset > 0 {
            sql.push_str(" limit ? offset ?");
            args.push(Value::Integer(if query.limit == 0 {
                -1
            } else {
                query.limit as i64
            }));
            args.push(Value::Integer(query.offset as i64));
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

        self.validate_access_scope()?;
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
        push_file_filters(&mut sql, &mut args, query, &self.access_scope);
        push_ts_window(&mut sql, &mut args, "ts", query.since, query.until);
        sql.push_str(" group by file_path, session_id order by file_path, edits desc");
        if query.limit > 0 || query.offset > 0 {
            sql.push_str(" limit ? offset ?");
            args.push(Value::Integer(if query.limit == 0 {
                -1
            } else {
                query.limit as i64
            }));
            args.push(Value::Integer(query.offset as i64));
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
    /// basename, exact path, or path suffix (`%/file`), optionally scoped to an exact session ID.
    /// Results are ordered by `(session_id, seq)` so callers can number versions per
    /// session and replay deltas deterministically.
    pub fn file_edits_for(
        &self,
        file: &str,
        session_id: Option<&str>,
    ) -> Result<Vec<(String, Provider, FileEdit)>> {
        let session_id = session_id
            .map(|id| self.resolve_session_record(id).map(|session| session.id))
            .transpose()?;
        self.file_edits_for_query(
            file,
            &FileQuery {
                session_id,
                ..Default::default()
            },
        )
    }

    pub fn file_edits_for_query(
        &self,
        file: &str,
        query: &FileQuery,
    ) -> Result<Vec<(String, Provider, FileEdit)>> {
        self.file_edits_for_scoped(file, query)
    }

    pub fn file_edits_for_session_id(
        &self,
        file: &str,
        session_id: &str,
    ) -> Result<Vec<(String, Provider, FileEdit)>> {
        self.file_edits_for_scoped(
            file,
            &FileQuery {
                session_id: Some(session_id.to_string()),
                ..Default::default()
            },
        )
    }

    fn file_edits_for_scoped(
        &self,
        file: &str,
        query: &FileQuery,
    ) -> Result<Vec<(String, Provider, FileEdit)>> {
        use rusqlite::types::Value;

        self.validate_access_scope()?;
        let mut sql = String::from(
            "select session_id, provider, seq, ts, tool, file_path, file_name, new_content, edits_json \
             from file_edits where (file_name = ? or file_path = ? or file_path like ?)",
        );
        let mut args: Vec<Value> = vec![
            Value::Text(file.to_string()),
            Value::Text(file.to_string()),
            Value::Text(format!("%/{file}")),
        ];
        push_file_filters(&mut sql, &mut args, query, &self.access_scope);
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
        self.list_recent_page(filters, 0)
    }

    pub fn list_recent_page(
        &self,
        filters: &SearchFilters,
        offset: usize,
    ) -> Result<Vec<SessionRecord>> {
        self.validate_access_scope()?;
        let mut sql = format!(
            "select {} from sessions s where 1 = 1",
            session_record_columns!()
        );
        let mut params_vec = Vec::new();
        push_session_filters(&mut sql, &mut params_vec, filters, &self.access_scope);
        use std::fmt::Write as _;
        sql.push_str(" order by s.updated_at desc, s.id asc");
        if filters.limit != 0 {
            write!(sql, " limit {}", filters.limit)?;
            if offset != 0 {
                write!(sql, " offset {offset}")?;
            }
        } else if offset != 0 {
            write!(sql, " limit -1 offset {offset}")?;
        }

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(params_vec.iter()),
            row_to_session_record,
        )?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    pub(crate) fn analysis_documents(
        &self,
        filters: &SearchFilters,
        cursor: Option<&crate::models::AnalysisCursor>,
    ) -> Result<crate::models::AnalysisDocumentPage> {
        self.validate_access_scope()?;
        if filters.limit == 0 {
            return Err(anyhow!(
                "analysis document page limit must be greater than zero"
            ));
        }
        // Keep session metadata and all per-session message reads on one SQLite snapshot.
        // The read-only transaction rolls back via RAII on every return/error path.
        let transaction = self.conn.unchecked_transaction()?;
        analysis_document_page(
            &transaction,
            filters,
            cursor,
            filters.limit,
            &self.access_scope,
        )
    }

    /// Visit each matching session's metadata and its user-message contents as an ordered
    /// row stream, in bounded keyset pages under one read snapshot. Message text is never
    /// concatenated or retained here, so a single session's aggregate user text no longer
    /// bounds memory. `filters.limit == 0` visits every matching session; otherwise it is
    /// the total visit bound, independent of the in-memory page size.
    pub(crate) fn visit_analysis_sessions(
        &self,
        filters: &SearchFilters,
        session_batch_size: std::num::NonZeroUsize,
        mut visitor: impl FnMut(
            crate::models::SessionRecord,
            i64,
            i64,
            &mut dyn Iterator<Item = Result<String>>,
        ) -> Result<()>,
    ) -> Result<usize> {
        self.validate_access_scope()?;
        let transaction = self.conn.unchecked_transaction()?;
        let mut count_stmt = transaction.prepare(ANALYSIS_MESSAGE_COUNTS_SQL)?;
        let mut user_message_stmt = transaction.prepare(ANALYSIS_USER_MESSAGES_SQL)?;
        let mut cursor = None;
        let mut visited = 0_usize;
        loop {
            let remaining = if filters.limit == 0 {
                usize::MAX
            } else {
                filters.limit.saturating_sub(visited)
            };
            if remaining == 0 {
                break;
            }
            let limit = session_batch_size.get().min(remaining);
            let (sessions, next_cursor) = analysis_session_page(
                &transaction,
                filters,
                cursor.as_ref(),
                limit,
                &self.access_scope,
            )?;
            if sessions.is_empty() {
                if next_cursor.is_some() {
                    // A pagination cursor with an empty page is a contradiction in
                    // analysis_session_page's own bookkeeping, not a data/environment problem —
                    // reindexing would not fix it. Point at filing a bug instead of a data-repair
                    // command that would not address the actual cause.
                    bail!(
                        "internal error: an analysis page returned a pagination cursor but no \
                         documents; this indicates a bug in aise's pagination logic — please \
                         file an issue at https://github.com/ahundt/ai-session-search/issues \
                         with the filters/query that triggered it, or send a pull request fixing \
                         analysis_session_page's cursor logic in db.rs"
                    );
                }
                break;
            }
            let page_len = sessions.len();
            for session in sessions {
                let (message_count, user_message_count) = count_stmt
                    .query_row([&session.id], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                    })?;
                let mut rows = user_message_stmt
                    .query_map([&session.id], |row| row.get::<_, String>(0))?
                    .map(|row| row.map_err(anyhow::Error::from));
                visitor(session, message_count, user_message_count, &mut rows)?;
            }
            visited = visited
                .checked_add(page_len)
                .ok_or_else(|| anyhow!("analysis document count overflow"))?;
            let Some(next_cursor) = next_cursor else {
                break;
            };
            if cursor
                .as_ref()
                .is_some_and(|previous: &crate::models::AnalysisCursor| {
                    previous.as_str() >= next_cursor.as_str()
                })
            {
                // Same reasoning as the empty-page case above: a non-advancing cursor is an
                // internal pagination bug, not a data problem.
                bail!(
                    "internal error: analysis pagination did not advance to a new cursor; this \
                     indicates a bug in aise's pagination logic — please file an issue at \
                     https://github.com/ahundt/ai-session-search/issues with the filters/query \
                     that triggered it, or send a pull request fixing analysis_session_page's \
                     cursor logic in db.rs"
                );
            }
            cursor = Some(next_cursor);
        }
        Ok(visited)
    }

    pub fn search(
        &self,
        query: &str,
        filters: &SearchFilters,
        current_repo: Option<&str>,
        scoring: &crate::config::ScoringConfig,
    ) -> Result<Vec<SearchHit>> {
        self.validate_access_scope()?;
        let matcher = SkimMatcherV2::default().smart_case();
        let query_lower = query.to_lowercase();
        let tokens: Vec<&str> = query_lower.split_whitespace().collect();
        let mut hits = Vec::new();
        let mut sql = format!(
            "select {}, coalesce(t.transcript_text, '') as transcript_text
               from sessions s
               left join transcripts t on t.session_id = s.id
              where 1 = 1",
            session_record_columns!()
        );
        let mut params_vec = Vec::new();
        push_session_filters(&mut sql, &mut params_vec, filters, &self.access_scope);
        sql.push_str(" order by s.id asc");
        let mut stmt = self.conn.prepare(&sql)?;
        let candidates = stmt.query_map(
            rusqlite::params_from_iter(params_vec.iter()),
            row_to_session_with_transcript,
        )?;

        for record in candidates {
            let record = record?;
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

            let mut term_coverage = vec![false; tokens.len()];
            let mut matched = false;
            // TODO(perf): this lowercases every haystack per candidate, including the full
            // transcript (~2x candidate transcript bytes of churn per query). A caseless
            // substring search or a reusable buffer removes the copies without changing
            // ranking; deferred past rc.1 because it touches scoring behavior.
            for (source, value) in haystacks {
                let lowered = value.to_lowercase();
                let mut source_score = 0i64;
                if lowered.contains(&query_lower) {
                    matched = true;
                    source_score += match source {
                        "title" => scoring.title_score,
                        "summary" => scoring.summary_score,
                        "cwd" | "repo" => scoring.path_score,
                        "preview" => scoring.preview_score,
                        _ => scoring.other_score,
                    };
                }
                for (index, token) in tokens.iter().enumerate() {
                    if !token.is_empty() && lowered.contains(token) {
                        matched = true;
                        source_score += scoring.token_bonus;
                        term_coverage[index] = true;
                    }
                }
                if matches!(source, "title" | "cwd" | "repo" | "preview") {
                    if let Some(fuzzy_score) = matcher.fuzzy_match(value, query) {
                        matched = true;
                        source_score += fuzzy_score;
                    }
                }

                score += source_score;
                if source_score > best_source_score {
                    best_source_score = source_score;
                    best_source = source.to_string();
                    best_snippet = snippet_from_match(value, query, 160);
                }
            }
            // Bonus when every whitespace-delimited query term matched at least one field in this
            // session. Coverage never crosses session boundaries.
            if !matched {
                continue;
            }
            if tokens.len() > 1 && term_coverage.iter().all(|matched| *matched) {
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
            hits.push(SearchHit {
                session: record.session,
                score,
                match_source: best_source,
                match_snippet: best_snippet,
            });
            if filters.limit > 0 && hits.len() >= top_k_compaction_threshold(filters.limit) {
                retain_top_session_hits(&mut hits, filters.limit);
            }
        }

        if filters.limit > 0 {
            retain_top_session_hits(&mut hits, filters.limit);
        }
        hits.sort_by(compare_session_hits);
        Ok(hits)
    }

    pub fn resolve_session(&self, value: &str) -> Result<SessionWithTranscript> {
        self.access_scope.validate_stable()?;
        let pattern = format!("{value}%");
        let mut sql = RESOLVE_SESSION_SQL.to_string();
        let mut args = vec![
            rusqlite::types::Value::Text(value.to_string()),
            rusqlite::types::Value::Text(pattern),
        ];
        push_access_scope(&mut sql, &mut args, "s.id", &self.access_scope);
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(args.iter()),
            row_to_session_with_transcript,
        )?;
        let mut matches = Vec::new();
        for row in rows {
            matches.push(row?);
        }
        unique_session_match(value, matches, |session| &session.session.id)
    }

    pub fn resolve_session_record(&self, value: &str) -> Result<SessionRecord> {
        self.access_scope.validate_stable()?;
        let pattern = format!("{value}%");
        let mut sql = RESOLVE_SESSION_RECORD_SQL.to_string();
        let mut args = vec![
            rusqlite::types::Value::Text(value.to_string()),
            rusqlite::types::Value::Text(pattern),
        ];
        push_access_scope(&mut sql, &mut args, "s.id", &self.access_scope);
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(args.iter()),
            row_to_session_record,
        )?;
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

    pub fn parser_health(&self) -> Result<ParserHealth> {
        let schema_version: i64 = self
            .conn
            .query_row("pragma user_version", [], |row| row.get(0))?;
        let mut stmt = self.conn.prepare(
            "select provider, parse_version, count(*) from sessions
             group by provider, parse_version",
        )?;
        let grouped = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let providers = [
            Provider::Claude,
            Provider::ClaudeDesktop,
            Provider::Codex,
            Provider::Cursor,
            Provider::Antigravity,
            Provider::Pi,
            Provider::AiStudio,
            Provider::GeminiCli,
        ]
        .into_iter()
        .map(|provider| {
            let expected = crate::util::provider_parse_version(provider);
            let mut indexed_sessions = 0;
            let mut current_sessions = 0;
            for (stored_provider, parse_version, count) in &grouped {
                if stored_provider == provider.as_str() {
                    indexed_sessions += count;
                    if parse_version == expected {
                        current_sessions += count;
                    }
                }
            }
            ProviderParserHealth {
                provider,
                expected_parse_version: expected.to_string(),
                indexed_sessions,
                current_sessions,
                stale_sessions: indexed_sessions - current_sessions,
            }
        })
        .collect::<Vec<_>>();
        let indexed_sessions = providers.iter().map(|item| item.indexed_sessions).sum();
        let current_sessions = providers.iter().map(|item| item.current_sessions).sum();

        Ok(ParserHealth {
            schema_version,
            expected_schema_version: SCHEMA_VERSION,
            schema_current: schema_version == SCHEMA_VERSION,
            indexed_sessions,
            current_sessions,
            stale_sessions: indexed_sessions - current_sessions,
            parse_warnings: self.count_parse_warnings()?,
            providers,
        })
    }

    pub(crate) fn stale_session_sources(&self) -> Result<Vec<(Provider, String)>> {
        let mut stmt = self
            .conn
            .prepare("select provider, parse_version, source_path from sessions")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(provider, parse_version, source_path)| {
                let provider = Provider::from_db_str(&provider);
                (parse_version != crate::util::provider_parse_version(provider))
                    .then_some((provider, source_path))
            })
            .collect())
    }

    pub fn session_time_profile(&self, session_id: &str) -> Result<SessionTimeProfile> {
        use rusqlite::types::Value;

        self.validate_access_scope()?;
        let mut sql = String::from(
            "with ordered as (
                 select ts, kind, lag(ts) over (order by seq) as previous_ts
                 from messages where session_id = ?",
        );
        let mut args = vec![Value::Text(session_id.to_string())];
        push_access_scope(&mut sql, &mut args, "session_id", &self.access_scope);
        sql.push_str(
            ") select count(*), count(ts), min(ts), max(ts),
                    max(case when previous_ts is null or ts is null then null
                             else unixepoch(ts) - unixepoch(previous_ts) end),
                    coalesce(sum(kind = 'tool_call'), 0),
                    coalesce(sum(kind = 'tool_result'), 0)
               from ordered",
        );
        let row = self
            .conn
            .query_row(&sql, rusqlite::params_from_iter(args.iter()), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })?;
        let parse_ts = |value: Option<String>| {
            value.and_then(|value| {
                chrono::DateTime::parse_from_rfc3339(&value)
                    .ok()
                    .map(|timestamp| timestamp.with_timezone(&Utc))
            })
        };
        let first_timestamp = parse_ts(row.2);
        let last_timestamp = parse_ts(row.3);
        Ok(SessionTimeProfile {
            messages: row.0,
            timestamped_messages: row.1,
            undated_messages: row.0 - row.1,
            observed_span_seconds: first_timestamp
                .zip(last_timestamp)
                .map(|(first, last)| (last - first).num_seconds()),
            first_timestamp,
            last_timestamp,
            max_message_gap_seconds: row.4,
            tool_calls: row.5,
            tool_results: row.6,
        })
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
}

const ANALYSIS_MESSAGE_COUNTS_SQL: &str =
    "select count(*), coalesce(sum(case when role = 'user' then 1 else 0 end), 0)
     from messages where session_id = ?1";

const ANALYSIS_USER_MESSAGES_SQL: &str = "select content from messages
     where session_id = ?1 and role = 'user' order by seq asc";

/// One keyset page of matching session records, ordered by canonical session ID.
fn analysis_session_page(
    transaction: &rusqlite::Transaction<'_>,
    filters: &SearchFilters,
    cursor: Option<&crate::models::AnalysisCursor>,
    page_limit: usize,
    access: &crate::search_scope::EffectiveAccessScope,
) -> Result<(Vec<SessionRecord>, Option<crate::models::AnalysisCursor>)> {
    use crate::models::AnalysisCursor;
    use std::fmt::Write as _;

    if page_limit == 0 {
        bail!("analysis document page limit must be greater than zero");
    }
    let fetch_limit = page_limit
        .checked_add(1)
        .ok_or_else(|| anyhow!("analysis document page limit is too large"))?;
    let mut sql = format!(
        "select {} from sessions s where 1 = 1",
        session_record_columns!()
    );
    let mut params_vec = Vec::new();
    push_session_filters(&mut sql, &mut params_vec, filters, access);
    if let Some(cursor) = cursor {
        sql.push_str(" and s.id > ? ");
        params_vec.push(cursor.as_str().to_string());
    }
    write!(sql, " order by s.id asc limit {fetch_limit}")?;

    let mut session_stmt = transaction.prepare(&sql)?;
    let session_rows = session_stmt.query_map(
        rusqlite::params_from_iter(params_vec.iter()),
        row_to_session_record,
    )?;
    let mut sessions = Vec::new();
    for row in session_rows {
        sessions.push(row?);
    }
    let has_more = sessions.len() > page_limit;
    sessions.truncate(page_limit);
    let next_cursor = has_more
        .then(|| sessions.last().map(|session| session.id.clone()))
        .flatten()
        .map(AnalysisCursor::after);
    Ok((sessions, next_cursor))
}

fn analysis_document_page(
    transaction: &rusqlite::Transaction<'_>,
    filters: &SearchFilters,
    cursor: Option<&crate::models::AnalysisCursor>,
    page_limit: usize,
    access: &crate::search_scope::EffectiveAccessScope,
) -> Result<crate::models::AnalysisDocumentPage> {
    use crate::models::{AnalysisDocument, AnalysisDocumentPage};

    let (sessions, next_cursor) =
        analysis_session_page(transaction, filters, cursor, page_limit, access)?;
    let mut count_stmt = transaction.prepare(ANALYSIS_MESSAGE_COUNTS_SQL)?;
    let mut user_message_stmt = transaction.prepare(ANALYSIS_USER_MESSAGES_SQL)?;
    let mut documents = Vec::with_capacity(sessions.len());
    for session in sessions {
        let (message_count, user_message_count) = count_stmt.query_row([&session.id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;
        let rows = user_message_stmt.query_map([&session.id], |row| row.get::<_, String>(0))?;
        let mut user_text = String::new();
        let mut first_user_text = None;
        for row in rows {
            let content = row?;
            if first_user_text.is_none() {
                first_user_text = Some(content.clone());
            }
            if !user_text.is_empty() {
                user_text.push(' ');
            }
            user_text.push_str(&content);
        }
        documents.push(AnalysisDocument {
            session,
            user_text,
            first_user_text,
            message_count,
            user_message_count,
        });
    }
    Ok(AnalysisDocumentPage {
        documents,
        next_cursor,
    })
}

fn push_session_filters(
    sql: &mut String,
    params_vec: &mut Vec<String>,
    filters: &SearchFilters,
    access: &crate::search_scope::EffectiveAccessScope,
) {
    if let Some(provider) = filters.provider {
        sql.push_str(" and s.provider = ? ");
        params_vec.push(provider.as_str().to_string());
    }
    if let Some(path_prefix) = &filters.path_prefix {
        push_session_path_prefix(sql, params_vec, path_prefix);
    }
    push_session_access_scope(sql, params_vec, access);
    push_session_exclusions(sql, params_vec, filters);
    push_session_kinds(sql, filters);
    if let Some(parent_session_id) = &filters.parent_session_id {
        sql.push_str(" and s.parent_session_id = ? ");
        params_vec.push(parent_session_id.clone());
    }
    push_session_time_window(sql, params_vec, filters.since, filters.until);
    if filters.warnings_only {
        sql.push_str(" and s.parse_warning is not null and s.parse_warning != '' ");
    }
}

/// RFC3339 text for an inclusive UPPER date bound. `dates.rs` already expands imprecise calendar,
/// hour, and minute periods to the final nanosecond while preserving explicit RFC3339 instants.
/// Always rendering nine fractional digits makes lexicographic SQLite comparison agree with
/// chronological order and preserves an exact fractional bound for race reconstruction.
fn until_bound_text(until: chrono::DateTime<Utc>) -> String {
    until.to_rfc3339_opts(chrono::SecondsFormat::Nanos, false)
}

/// Append the `path_prefix` predicate — restrict rows to sessions rooted at the prefix — onto a
/// query whose rows expose `session_id` as `id_col` (e.g. `m.session_id` or a bare `session_id`).
/// The `sessions` table is tiny relative to messages/file edits, so a subquery is cheap and needs
/// no dedicated index. Mirrors the session-level `path_prefix` semantics in `list_recent`/`search`
/// (exact directory or a child path, with LIKE metacharacters escaped) so `--path` behaves
/// identically across session, message, analytics, and file surfaces. No-op when `path_prefix` is
/// None.
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

#[derive(Clone, Copy)]
enum SessionPathDomain {
    Workspace,
    Transcript,
}

fn push_domain_path_prefix(
    sql: &mut String,
    args: &mut Vec<rusqlite::types::Value>,
    id_col: &str,
    prefix: Option<&str>,
    domain: SessionPathDomain,
) {
    use std::fmt::Write as _;
    if let Some(prefix) = prefix {
        let _ = write!(sql, " and {id_col} in (select id from sessions where ");
        push_domain_path_condition(sql, args, prefix, domain);
        sql.push(')');
    }
}

fn push_domain_path_condition(
    sql: &mut String,
    args: &mut Vec<rusqlite::types::Value>,
    prefix: &str,
    domain: SessionPathDomain,
) {
    use rusqlite::types::Value;
    let (exact, child_pattern) = path_prefix_patterns(prefix);
    match domain {
        SessionPathDomain::Workspace => {
            sql.push_str(
                "(coalesce(cwd, '') = ? or coalesce(cwd, '') like ? escape '\\' \
                  or coalesce(repo_root, '') = ? or coalesce(repo_root, '') like ? escape '\\')",
            );
            args.push(Value::Text(exact.clone()));
            args.push(Value::Text(child_pattern.clone()));
            args.push(Value::Text(exact));
            args.push(Value::Text(child_pattern));
        }
        SessionPathDomain::Transcript => {
            sql.push_str(
                "(coalesce(source_path, '') = ? or coalesce(source_path, '') like ? escape '\\')",
            );
            args.push(Value::Text(exact));
            args.push(Value::Text(child_pattern));
        }
    }
}

fn push_access_scope(
    sql: &mut String,
    args: &mut Vec<rusqlite::types::Value>,
    id_col: &str,
    access: &crate::search_scope::EffectiveAccessScope,
) {
    use std::fmt::Write as _;
    let prefixes: Vec<&str> = access.workspace_prefixes().collect();
    if prefixes.is_empty() {
        return;
    }
    let _ = write!(sql, " and {id_col} in (select id from sessions where ");
    for (index, prefix) in prefixes.into_iter().enumerate() {
        if index > 0 {
            sql.push_str(" or ");
        }
        push_domain_path_condition(sql, args, prefix, SessionPathDomain::Workspace);
    }
    sql.push(')');
}

fn push_session_access_scope(
    sql: &mut String,
    args: &mut Vec<String>,
    access: &crate::search_scope::EffectiveAccessScope,
) {
    let prefixes: Vec<&str> = access.workspace_prefixes().collect();
    if prefixes.is_empty() {
        return;
    }
    sql.push_str(" and (");
    for (index, prefix) in prefixes.into_iter().enumerate() {
        if index > 0 {
            sql.push_str(" or ");
        }
        let (exact, child_pattern) = path_prefix_patterns(prefix);
        sql.push_str(
            "(coalesce(s.cwd, '') = ? or coalesce(s.cwd, '') like ? escape '\\' \
              or coalesce(s.repo_root, '') = ? or coalesce(s.repo_root, '') like ? escape '\\')",
        );
        args.push(exact.clone());
        args.push(child_pattern.clone());
        args.push(exact);
        args.push(child_pattern);
    }
    sql.push(')');
}

fn push_exclude_domain_path_prefixes(
    sql: &mut String,
    args: &mut Vec<rusqlite::types::Value>,
    id_col: &str,
    prefixes: &[String],
    domain: SessionPathDomain,
) {
    use std::fmt::Write as _;
    if prefixes.is_empty() {
        return;
    }
    let _ = write!(sql, " and {id_col} not in (select id from sessions where ");
    for (index, prefix) in prefixes.iter().enumerate() {
        if index > 0 {
            sql.push_str(" or ");
        }
        push_domain_path_condition(sql, args, prefix, domain);
    }
    sql.push(')');
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
    access: &crate::search_scope::EffectiveAccessScope,
) {
    use rusqlite::types::Value;
    if let Some(role) = filters.role {
        sql.push_str(" and m.role = ?");
        args.push(Value::Text(role.as_str().to_string()));
    }
    // One clause decides which classes come back. `kinds` and `no_compaction` are resolved
    // into a single set first, so no combination can both select and exclude a class.
    //
    // PATTERN: do not add a second class predicate here. A `kind != 'harness_notice'` clause
    // was added beside this one and removed: combined with `kind = 'harness_notice'` it
    // produced an always-empty result that reads as "no such messages exist". The removed
    // `is_compaction = 0` clause was the same redundancy, since role Compaction always infers
    // kind Compaction. Class selection changes belong in `MessageFilters::effective_kinds`.
    match filters.kind_predicate() {
        crate::models::KindPredicate::AllExcept(excluded) => {
            for kind in excluded {
                sql.push_str(" and m.kind != ?");
                args.push(Value::Text(kind.as_str().to_string()));
            }
        }
        crate::models::KindPredicate::Only(kinds) if kinds.is_empty() => {
            // Every named class was removed. Match nothing rather than silently matching all.
            sql.push_str(" and 0");
        }
        crate::models::KindPredicate::Only(kinds) => {
            sql.push_str(" and m.kind in (");
            for (i, kind) in kinds.iter().enumerate() {
                if i > 0 {
                    sql.push_str(", ");
                }
                sql.push('?');
                args.push(Value::Text(kind.as_str().to_string()));
            }
            sql.push(')');
        }
    }
    if let Some(provider) = filters.provider {
        sql.push_str(" and m.provider = ?");
        args.push(Value::Text(provider.as_str().to_string()));
    }
    if let Some(session_id) = &filters.session_id {
        sql.push_str(" and m.session_id = ?");
        args.push(Value::Text(session_id.clone()));
    }
    push_access_scope(sql, args, "m.session_id", access);
    push_path_prefix(sql, args, "m.session_id", filters.path_prefix.as_deref());
    push_exclude_path_prefixes(sql, args, "m.session_id", &filters.exclude_path_prefixes);
    push_domain_path_prefix(
        sql,
        args,
        "m.session_id",
        filters.workspace_path_prefix.as_deref(),
        SessionPathDomain::Workspace,
    );
    push_domain_path_prefix(
        sql,
        args,
        "m.session_id",
        filters.transcript_path_prefix.as_deref(),
        SessionPathDomain::Transcript,
    );
    push_exclude_domain_path_prefixes(
        sql,
        args,
        "m.session_id",
        &filters.exclude_workspace_path_prefixes,
        SessionPathDomain::Workspace,
    );
    push_exclude_domain_path_prefixes(
        sql,
        args,
        "m.session_id",
        &filters.exclude_transcript_path_prefixes,
        SessionPathDomain::Transcript,
    );
    for session_id in &filters.exclude_session_ids {
        sql.push_str(" and m.session_id <> ?");
        args.push(Value::Text(session_id.clone()));
    }
    if let Some(tool) = &filters.tool {
        // NULL tool_name rows are excluded because the scalar predicate returns false.
        sql.push_str(" and unicode_lower_contains(m.tool_name, ?)");
        args.push(Value::Text(tool.to_lowercase()));
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
    // Class selection, including compaction and harness notices, is applied by the single
    // `m.kind in (...)` clause above. `is_compaction` is left unused here because it is
    // redundant with `kind`: role Compaction always infers kind Compaction, so filtering on
    // both would be two sources of truth for one fact.
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
            (session_id, provider, seq, role, ts, tool_name, kind, tool_call_id, is_compaction, content)
         values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )?;
    for (seq, message) in rows {
        stmt.execute(params![
            session.id,
            session.provider.as_str(),
            seq,
            message.role.as_str(),
            message.ts.map(|ts| ts.to_rfc3339()),
            message.tool_name,
            message.kind.as_str(),
            message.tool_call_id,
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
/// preserves the exact resolved endpoint (see [`until_bound_text`]).
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

fn push_file_filters(
    sql: &mut String,
    args: &mut Vec<rusqlite::types::Value>,
    query: &FileQuery,
    access: &crate::search_scope::EffectiveAccessScope,
) {
    use rusqlite::types::Value;
    if let Some(provider) = query.provider {
        sql.push_str(" and provider = ?");
        args.push(Value::Text(provider.as_str().to_string()));
    }
    if let Some(session_id) = query.session_id.as_deref() {
        sql.push_str(" and session_id = ?");
        args.push(Value::Text(session_id.to_string()));
    }
    push_access_scope(sql, args, "session_id", access);
    push_path_prefix(sql, args, "session_id", query.path_prefix.as_deref());
    push_exclude_path_prefixes(sql, args, "session_id", &query.exclude_path_prefixes);
    for session_id in &query.exclude_session_ids {
        sql.push_str(" and session_id <> ?");
        args.push(Value::Text(session_id.clone()));
    }
}

fn path_prefix_parts(prefix: &str) -> (String, String) {
    let bytes = prefix.as_bytes();
    let windows_style = prefix.starts_with(r"\\")
        || matches!(bytes, [drive, b':', b'\\' | b'/', ..] if drive.is_ascii_alphabetic());
    let separator = if windows_style { '\\' } else { '/' };
    let exact = prefix.trim_end_matches(separator).to_string();
    let child = format!("{exact}{separator}");
    (exact, child)
}

fn path_prefix_patterns(prefix: &str) -> (String, String) {
    let (exact, child) = path_prefix_parts(prefix);
    let mut escaped = String::with_capacity(child.len() + 2);
    for ch in child.chars() {
        match ch {
            '%' | '_' | '\\' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            other => escaped.push(other),
        }
    }
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

/// Restrict which classes of session come back, ORing each selected class's own predicate.
///
/// PATTERN: one clause decides session class, the way `append_message_filters` has one clause
/// for message class. Do not add a second predicate beside it — a `parent_session_id is null`
/// clause added elsewhere would combine with `session_kinds = [subagent]` into an always-empty
/// result that reads as "no such sessions exist". Class selection belongs in
/// [`SearchFilters::effective_session_kinds`].
///
/// Takes no `args` because [`SessionKind::sql_predicate`] is a fixed null test, not a bound
/// value: the class is derived from `parent_session_id`, never stored as text.
fn push_session_kinds(sql: &mut String, filters: &SearchFilters) {
    let selected = filters.effective_session_kinds();
    if selected.is_empty() {
        // Every class was removed. Match nothing rather than silently matching all.
        sql.push_str(" and 0 ");
        return;
    }
    if selected.len() == crate::models::SessionKind::all().len() {
        // Every class is selected, so the predicate would be a tautology.
        return;
    }
    sql.push_str(" and (");
    for (index, kind) in selected.iter().enumerate() {
        if index > 0 {
            sql.push_str(" or ");
        }
        sql.push_str(&kind.sql_predicate("s"));
    }
    sql.push_str(") ");
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

fn message_field_value(
    hit: &MessageHit,
    field: SearchField,
    argument_path: Option<&str>,
) -> Option<String> {
    match field {
        SearchField::Content => Some(hit.content.clone()),
        SearchField::ToolName => hit.tool_name.clone(),
        SearchField::ToolArgument => {
            let envelope: serde_json::Value = serde_json::from_str(&hit.content).ok()?;
            let args = envelope.get("args")?;
            let value = match argument_path.unwrap_or("") {
                "" => args,
                pointer => args.pointer(pointer)?,
            };
            Some(match value {
                serde_json::Value::String(value) => value.clone(),
                other => serde_json::to_string(other).ok()?,
            })
        }
    }
}

fn row_to_message_hit(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageHit> {
    row_to_message_hit_at(row, 0)
}

fn row_to_message_hit_at(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<MessageHit> {
    let ts: Option<String> = row.get(offset + 4)?;
    Ok(MessageHit {
        session_id: row.get(offset)?,
        provider: Provider::from_db_str(&row.get::<_, String>(offset + 1)?),
        seq: row.get(offset + 2)?,
        role: Role::from_db_str(&row.get::<_, String>(offset + 3)?),
        ts: ts.and_then(|value| {
            chrono::DateTime::parse_from_rfc3339(&value)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        }),
        tool_name: row.get(offset + 5)?,
        kind: crate::models::MessageKind::from_db_str(&row.get::<_, String>(offset + 6)?),
        tool_call_id: row.get(offset + 7)?,
        fuzzy_score: None,
        content: row.get(offset + 8)?,
    })
}

fn score_fuzzy_message_hits(
    pattern: &Pattern,
    query_lower: &str,
    hits: Vec<MessageHit>,
) -> Vec<(MessageHit, bool)> {
    hits.into_par_iter()
        .map_init(
            || (NucleoMatcher::new(NucleoConfig::DEFAULT), Vec::new()),
            |(matcher, utf32_buf), mut hit| {
                let score = {
                    let haystack = Utf32Str::new(&hit.content, utf32_buf);
                    pattern.score(haystack, matcher)
                };
                score.map(|score| {
                    hit.fuzzy_score = Some(score);
                    let exact_phrase = hit.content.to_lowercase().contains(query_lower);
                    (hit, exact_phrase)
                })
            },
        )
        .filter_map(std::convert::identity)
        .collect()
}

fn compare_fuzzy_hits(left: &(MessageHit, bool), right: &(MessageHit, bool)) -> std::cmp::Ordering {
    right
        .0
        .fuzzy_score
        .unwrap_or_default()
        .cmp(&left.0.fuzzy_score.unwrap_or_default())
        .then_with(|| right.1.cmp(&left.1))
        .then_with(|| left.0.session_id.cmp(&right.0.session_id))
        .then_with(|| left.0.seq.cmp(&right.0.seq))
}

fn fuzzy_ranked_limit(filters: &MessageFilters) -> Result<usize> {
    filters
        .offset
        .checked_add(filters.limit)
        .ok_or_else(|| anyhow!("fuzzy offset + limit exceeds the platform addressable range"))
}

/// Compact after retained candidates roughly double. This keeps memory `O(K)` while making the
/// repeated linear selections amortize to `O(N)` rather than rescanning `K` rows every fixed batch.
fn top_k_compaction_threshold(limit: usize) -> usize {
    limit.saturating_mul(2).max(FUZZY_SCORE_BATCH_SIZE)
}

fn retain_top_fuzzy_hits(scored: &mut Vec<(MessageHit, bool)>, limit: usize) {
    if scored.len() > limit {
        scored.select_nth_unstable_by(limit, compare_fuzzy_hits);
        scored.truncate(limit);
    }
}

fn finish_fuzzy_hits(
    mut scored: Vec<(MessageHit, bool)>,
    ranked_limit: usize,
    offset: usize,
) -> Vec<MessageHit> {
    retain_top_fuzzy_hits(&mut scored, ranked_limit);
    scored.sort_by(compare_fuzzy_hits);
    scored
        .into_iter()
        .skip(offset)
        .map(|(hit, _)| hit)
        .collect()
}

fn compare_session_hits(left: &SearchHit, right: &SearchHit) -> std::cmp::Ordering {
    right
        .score
        .cmp(&left.score)
        .then_with(|| right.session.updated_at.cmp(&left.session.updated_at))
        .then_with(|| left.session.id.cmp(&right.session.id))
}

fn retain_top_session_hits(hits: &mut Vec<SearchHit>, limit: usize) {
    if hits.len() > limit {
        hits.select_nth_unstable_by(limit, compare_session_hits);
        hits.truncate(limit);
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

// TODO(perf): the OR'd case-insensitive LIKE terms defeat the id indexes, so every
// session resolution is an O(S) table scan (EXPLAIN QUERY PLAN: `SCAN s`). Milliseconds
// at realistic session counts, but replace with indexable range probes
// (`id >= ?1 AND id < ?1 || x'F7BFBFBF'` per column, case folded) if S grows.
macro_rules! session_id_match_sql {
    () => {
        "s.id = ?1 or s.provider_session_id = ?1 or s.id like ?2 or s.provider_session_id like ?2"
    };
}

const RESOLVE_SESSION_SQL: &str = concat!(
    "select ",
    session_record_columns!(),
    // Aliased so the reader can take it by name and stay immune to columns being added to
    // session_record_columns!.
    ", coalesce(t.transcript_text, '') as transcript_text \
     from sessions s \
     left join transcripts t on t.session_id = s.id \
     where (",
    session_id_match_sql!(),
    ")"
);

const RESOLVE_SESSION_RECORD_SQL: &str = concat!(
    "select ",
    session_record_columns!(),
    " from sessions s where (",
    session_id_match_sql!(),
    ")"
);

fn unique_session_match<T>(
    value: &str,
    mut matches: Vec<T>,
    id_of: impl Fn(&T) -> &str,
) -> Result<T> {
    match matches.len() {
        0 => Err(anyhow!(
            "no session matches '{value}' — run `aise list` to see recent session \
             ids, or `aise search <keywords>` to find one"
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
        // By NAME, not position: this column follows `session_record_columns!`, so every column
        // added to that macro shifts its index. Adding parent_session_id and agent_label broke
        // it exactly that way, and the failure was a confusing "Invalid column type Null at
        // index 17" rather than anything naming this line.
        transcript_text: row.get("transcript_text")?,
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
        parent_session_id: row.get(17)?,
        agent_label: row.get(18)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::SessionKind;

    const TEST_BUSY_TIMEOUT_MS: u64 = 250;
    const TEST_NO_WAIT_BUSY_TIMEOUT_MS: u64 = 0;

    fn schema_fingerprint(conn: &Connection) -> Vec<(String, String, String, Option<String>)> {
        conn.prepare(
            "select type, name, tbl_name, sql
               from sqlite_schema
              where name not like 'sqlite_autoindex_%'
              order by type, name, tbl_name",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }

    fn enable_v3_custom_trigram_compatibility(db: &Db) {
        db.conn
            .execute_batch(
                "drop trigger if exists messages_ai;
                 drop trigger if exists messages_ad;
                 drop trigger if exists messages_au;
                 drop table if exists messages_trigram_terms;
                 drop table if exists messages_trigram_vocab;
                 drop table if exists messages_trigram;",
            )
            .unwrap();
        crate::fts::install_released_message_word_index(&db.conn).unwrap();
        crate::trigram_index::ensure_schema(&db.conn).unwrap();
        db.conn.pragma_update(None, "user_version", 3).unwrap();
    }

    #[derive(Debug, Clone, Copy)]
    enum MessageContentMode {
        Exact,
        Regex,
        Fuzzy,
    }

    impl MessageContentMode {
        fn search(
            self,
            db: &Db,
            pattern: &str,
            mut filters: MessageFilters,
        ) -> Result<Vec<MessageHit>> {
            filters.match_mode = match self {
                Self::Exact => MessageSearchMode::Exact,
                Self::Regex => MessageSearchMode::Regex,
                Self::Fuzzy => MessageSearchMode::Fuzzy,
            };
            db.search_messages(pattern, &filters)
        }
    }

    const MESSAGE_CONTENT_MODE_CASES: [(MessageContentMode, &str); 3] = [
        (MessageContentMode::Exact, "shared needle"),
        (MessageContentMode::Regex, r"shared\s+needle"),
        (MessageContentMode::Fuzzy, "shared needle"),
    ];

    fn hit_keys(hits: Vec<MessageHit>) -> Vec<(String, i64)> {
        hits.into_iter()
            .map(|hit| (hit.session_id, hit.seq))
            .collect()
    }

    fn utc(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

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

    /// Subagent runs outnumber the sessions that spawned them — 4,051 against 858 user-started
    /// claude sessions on the machine this was built against — so selecting a class has to
    /// work, and the default has to return both rather than quietly halving the index.
    #[test]
    fn session_kinds_select_user_started_runs_or_both() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute_batch(
                "insert into sessions (
                     id, provider, provider_session_id, updated_at, preview_text,
                     source_path, parse_version, discovery_source, parent_session_id, agent_label
                 ) values
                   ('claude:parent', 'claude', 'parent', '2026-03-01T00:00:00+00:00', '',
                    '/parent', 'v1', 'test', null, null),
                   ('claude:parent/agent-a', 'claude', 'parent/agent-a', '2026-03-02T00:00:00+00:00',
                    '', '/a', 'v1', 'test', 'claude:parent', 'Explore'),
                   ('claude:other/agent-b', 'claude', 'other/agent-b', '2026-03-03T00:00:00+00:00',
                    '', '/b', 'v1', 'test', 'claude:other', 'general-purpose');",
            )
            .unwrap();

        let ids = |kinds: Option<Vec<SessionKind>>, parent: Option<&str>| {
            let mut ids: Vec<String> = db
                .list_recent(&SearchFilters {
                    session_kinds: kinds,
                    parent_session_id: parent.map(str::to_string),
                    ..SearchFilters::default()
                })
                .unwrap()
                .into_iter()
                .map(|session| session.id)
                .collect();
            ids.sort();
            ids
        };

        assert_eq!(
            ids(None, None),
            vec![
                "claude:other/agent-b",
                "claude:parent",
                "claude:parent/agent-a"
            ],
            "naming no class returns every class, so indexed work is never invisible"
        );
        assert_eq!(
            ids(Some(vec![SessionKind::User]), None),
            vec!["claude:parent"]
        );
        assert_eq!(
            ids(Some(vec![SessionKind::Subagent]), None),
            vec!["claude:other/agent-b", "claude:parent/agent-a"]
        );
        assert_eq!(
            ids(Some(vec![SessionKind::User, SessionKind::Subagent]), None),
            ids(None, None),
            "naming both classes is the default set spelled out"
        );
        assert!(
            ids(Some(Vec::new()), None).is_empty(),
            "deselecting every class matches nothing rather than silently matching all"
        );

        // `parent_session_id` holds the parent row's whole id, so the value a caller already
        // holds from a listing is the value that selects that session's runs.
        assert_eq!(
            ids(None, Some("claude:parent")),
            vec!["claude:parent/agent-a"]
        );
        assert!(ids(None, Some("claude:parent/agent-a")).is_empty());
    }

    #[test]
    fn list_recent_is_sql_bounded_and_does_not_read_transcripts() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute_batch(
                "insert into sessions (
                     id, provider, provider_session_id, updated_at, preview_text,
                     source_path, parse_version, discovery_source
                 ) values
                   ('claude:old', 'claude', 'old', '2026-01-01T00:00:00+00:00', '', '/old', 'v1', 'test'),
                   ('claude:middle', 'claude', 'middle', '2026-02-01T00:00:00+00:00', '', '/middle', 'v1', 'test'),
                   ('claude:new', 'claude', 'new', '2026-03-01T00:00:00+00:00', '', '/new', 'v1', 'test');
                 drop table transcripts;",
            )
            .unwrap();

        let mut filters = SearchFilters {
            limit: 2,
            ..SearchFilters::default()
        };
        let sessions = db.list_recent(&filters).unwrap();

        assert_eq!(
            sessions
                .into_iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
            ["claude:new", "claude:middle"]
        );

        filters.limit = 0;
        assert_eq!(
            db.list_recent(&filters)
                .unwrap()
                .into_iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
            ["claude:new", "claude:middle", "claude:old"]
        );
    }

    #[test]
    fn analysis_documents_keyset_pages_normalized_user_text_and_empty_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute_batch(
                "insert into sessions (
                     id, provider, provider_session_id, preview_text,
                     source_path, parse_version, discovery_source
                 ) values
                   ('claude:a', 'claude', 'a', '', '/a', 'v1', 'test'),
                   ('claude:b', 'claude', 'b', '', '/b', 'v1', 'test'),
                   ('codex:c', 'codex', 'c', '', '/c', 'v1', 'test');
                 insert into messages (session_id, provider, seq, role, content) values
                   ('claude:a', 'claude', 0, 'user', 'first request'),
                   ('claude:a', 'claude', 1, 'assistant', 'answer'),
                   ('claude:a', 'claude', 2, 'user', 'second request'),
                   ('codex:c', 'codex', 0, 'user', 'other provider');
                 drop table transcripts;",
            )
            .unwrap();
        let filters = SearchFilters {
            provider: Some(Provider::Claude),
            limit: 1,
            ..SearchFilters::default()
        };

        let first = db.analysis_documents(&filters, None).unwrap();
        assert_eq!(first.documents.len(), 1);
        assert_eq!(first.documents[0].session.id, "claude:a");
        assert_eq!(first.documents[0].message_count, 3);
        assert_eq!(first.documents[0].user_message_count, 2);
        assert_eq!(first.documents[0].user_text, "first request second request");
        assert_eq!(
            first.documents[0].first_user_text.as_deref(),
            Some("first request")
        );

        let second = db
            .analysis_documents(&filters, first.next_cursor.as_ref())
            .unwrap();
        assert_eq!(second.documents.len(), 1);
        assert_eq!(second.documents[0].session.id, "claude:b");
        assert_eq!(second.documents[0].message_count, 0);
        assert_eq!(second.documents[0].user_message_count, 0);
        assert!(second.documents[0].user_text.is_empty());
        assert!(second.documents[0].first_user_text.is_none());
        assert!(second.next_cursor.is_none());
    }

    #[test]
    fn analysis_visit_keeps_one_snapshot_across_bounded_pages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let db = Db::open(&path).unwrap();
        let writer = Db::open(&path).unwrap();
        for (id, source) in [
            ("claude:a", "/fixture/a.jsonl"),
            ("claude:z", "/fixture/z.jsonl"),
        ] {
            let mut parsed =
                crate::util::minimal_record(Provider::Claude, Path::new(source), "test".into());
            parsed.session.id = id.into();
            parsed.session.provider_session_id = id.into();
            db.upsert_session(&parsed, 0, 0).unwrap();
        }
        let mut late = crate::util::minimal_record(
            Provider::Claude,
            Path::new("/fixture/m.jsonl"),
            "test".into(),
        );
        late.session.id = "claude:m".into();
        late.session.provider_session_id = "claude:m".into();
        let filters = SearchFilters::default();
        let mut seen = Vec::new();
        let visited = db
            .visit_analysis_sessions(
                &filters,
                std::num::NonZeroUsize::new(1).unwrap(),
                |session, _message_count, _user_message_count, _chunks| {
                    seen.push(session.id);
                    if seen.len() == 1 {
                        writer.upsert_session(&late, 0, 0)?;
                    }
                    Ok(())
                },
            )
            .unwrap();

        assert_eq!(visited, 2);
        assert_eq!(seen, ["claude:a", "claude:z"]);
        assert_eq!(
            db.resolve_session_record("claude:m").unwrap().id,
            "claude:m"
        );
    }

    #[test]
    fn analysis_documents_rejects_unbounded_pages() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let error = db
            // limit stated rather than defaulted: an unbounded page is what this rejects, so
            // the value under test must not move if the default ever does.
            .analysis_documents(
                &SearchFilters {
                    limit: 0,
                    ..SearchFilters::default()
                },
                None,
            )
            .unwrap_err();
        assert!(error.to_string().contains("greater than zero"));
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
        slash(1, "s1", "claude", 0, "/cmd-a make a plan");
        slash(2, "s1", "claude", 1, "/cmd-b");
        slash(3, "s1", "claude", 2, "/cmd-a refine it");
        slash(4, "s2", "codex", 0, "/cmd-c ship the fix");

        // No filter (config default) → every slash command is counted.
        let all = db.planning_usage(&MessageFilters::default(), &[]).unwrap();
        assert_eq!(all.len(), 3, "all distinct commands counted");

        // A configured filter restricts to matching commands.
        let only = db
            .planning_usage(
                &MessageFilters::default(),
                &[regex::Regex::new(r"^/cmd-a$").unwrap()],
            )
            .unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].command, "/cmd-a");
        assert_eq!(only[0].count, 2);

        let command_token_only = db
            .planning_usage(
                &MessageFilters::default(),
                &[regex::Regex::new(r"^/cmd-b$").unwrap()],
            )
            .unwrap();
        assert_eq!(
            command_token_only
                .iter()
                .map(|row| row.command.as_str())
                .collect::<Vec<_>>(),
            vec!["/cmd-b"],
            "planning filters command tokens, not arbitrary slash-message body text"
        );

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
        assert_eq!(codex[0].command, "/cmd-c");
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
        insert(10, 9, "CAFÉ diagnostic");

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
        assert_eq!(seqs("café"), vec![9]);
        // Empty query lists everything (structured filters only).
        assert_eq!(seqs("").len(), 10);
        // --regex still matches arbitrary patterns over the rows (scan path).
        let re = db
            .search_messages(
                "h.ndler",
                &MessageFilters {
                    match_mode: MessageSearchMode::Regex,
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
                "magic config",
                &MessageFilters {
                    match_mode: MessageSearchMode::Fuzzy,
                    limit: 10,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(fuzzy.iter().map(|hit| hit.seq).collect::<Vec<_>>(), vec![0]);
        assert!(fuzzy[0].fuzzy_score.is_some());

        let fuzzy_phrase = db
            .search_messages(
                "magic values",
                &MessageFilters {
                    match_mode: MessageSearchMode::Fuzzy,
                    limit: 10,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(fuzzy_phrase[0].seq, 0, "exact phrase wins fuzzy ties");
    }

    #[test]
    fn schema_v4_fuzzy_search_filters_structurally_before_exhaustive_scoring() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let db = Db::open(&path).unwrap();
            for (id, provider, seq, role, content) in [
                (
                    "s1",
                    "claude",
                    0,
                    "user",
                    "incremental trigram acceleration",
                ),
                ("s1", "claude", 1, "assistant", "unrelated response"),
                ("s2", "codex", 0, "user", "incremental index design"),
            ] {
                db.conn
                    .execute(
                        "insert or ignore into sessions
                         (id, provider, provider_session_id, preview_text, source_path,
                          parse_version, discovery_source)
                         values (?1, ?2, ?1, '', '/p', '1', 'test')",
                        params![id, provider],
                    )
                    .unwrap();
                db.conn
                    .execute(
                        "insert into messages
                         (session_id, provider, seq, role, content)
                         values (?1, ?2, ?3, ?4, ?5)",
                        params![id, provider, seq, role, content],
                    )
                    .unwrap();
            }
        }

        let db = Db::open(&path).unwrap();
        let (hits, explain) = db
            .search_messages_with_explain(
                "incrmental trigram",
                &MessageFilters {
                    provider: Some(Provider::Claude),
                    role: Some(Role::User),
                    match_mode: MessageSearchMode::Fuzzy,
                    limit: 10,
                    ..Default::default()
                },
                true,
            )
            .unwrap();
        assert_eq!(hits.iter().map(|hit| hit.seq).collect::<Vec<_>>(), vec![0]);
        assert!(hits[0].fuzzy_score.is_some());
        let explain = explain.unwrap();
        assert_eq!(explain.prefilter.as_deref(), None);
        assert_eq!(
            explain.prefilter_skipped.as_deref(),
            Some("complete filtered corpus scored with bounded top-K retention")
        );
        assert_eq!(explain.corpus, 1);
    }

    #[test]
    fn exhaustive_fuzzy_ranking_keeps_a_best_hit_beyond_the_old_candidate_cap() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute(
                "insert into sessions (id, provider, provider_session_id, preview_text,
                 source_path, parse_version, discovery_source)
                 values ('s1','claude','s1','','/p','1','test')",
                [],
            )
            .unwrap();
        let tx = db.conn.unchecked_transaction().unwrap();
        {
            let mut insert = tx
                .prepare(
                    "insert into messages(session_id, provider, seq, role, content)
                     values ('s1','claude',?1,'user',?2)",
                )
                .unwrap();
            for seq in 0..1_300_i64 {
                insert
                    .execute(params![
                        seq,
                        format!("magic values configurable filler {seq}")
                    ])
                    .unwrap();
            }
            insert.execute(params![1_300_i64, "magic config"]).unwrap();
        }
        tx.commit().unwrap();

        let hits = db
            .search_messages(
                "magic config",
                &MessageFilters {
                    match_mode: MessageSearchMode::Fuzzy,
                    limit: 1,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].seq, 1_300);

        let first_two = db
            .search_messages(
                "magic config",
                &MessageFilters {
                    match_mode: MessageSearchMode::Fuzzy,
                    limit: 2,
                    ..Default::default()
                },
            )
            .unwrap();
        let second = db
            .search_messages(
                "magic config",
                &MessageFilters {
                    match_mode: MessageSearchMode::Fuzzy,
                    limit: 1,
                    offset: 1,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].session_id, first_two[1].session_id);
        assert_eq!(second[0].seq, first_two[1].seq);
        assert_eq!(second[0].fuzzy_score, first_two[1].fuzzy_score);
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

        for (mode, pattern) in MESSAGE_CONTENT_MODE_CASES {
            let hits = mode
                .search(
                    &db,
                    pattern,
                    MessageFilters {
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
                "{mode:?}: exclusions apply before limit, including source_path exclusions"
            );
        }

        for (mode, pattern) in MESSAGE_CONTENT_MODE_CASES {
            let hits = mode
                .search(
                    &db,
                    pattern,
                    MessageFilters {
                        exclude_session_ids: vec!["b".into()],
                        limit: 10,
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(
                hits.iter()
                    .map(|hit| hit.session_id.as_str())
                    .collect::<Vec<_>>(),
                vec!["a", "c"],
                "{mode:?}: session exclusions apply before content matching"
            );
        }
    }

    #[test]
    fn search_messages_content_modes_share_structural_filters() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let session = |id: &str, provider: Provider, cwd: &str, repo: &str, source_path: &str| {
            db.conn
                .execute(
                    "insert into sessions (id, provider, provider_session_id, cwd, repo_root, \
                     preview_text, source_path, parse_version, discovery_source) \
                     values (?1,?2,?1,?3,?4,'',?5,'1','test')",
                    params![id, provider.as_str(), cwd, repo, source_path],
                )
                .unwrap();
        };
        session(
            "target",
            Provider::Claude,
            "/Users/x/proj-a",
            "/Users/x/proj-a",
            "/logs/target.jsonl",
        );
        session(
            "wrong-provider",
            Provider::Codex,
            "/Users/x/proj-a",
            "/Users/x/proj-a",
            "/logs/wrong-provider.jsonl",
        );
        session(
            "wrong-path",
            Provider::Claude,
            "/Users/x/proj-b",
            "/Users/x/proj-b",
            "/logs/wrong-path.jsonl",
        );
        let msg =
            |id: i64, session_id: &str, provider: Provider, seq: i64, role: Role, ts: &str| {
                db.conn
                    .execute(
                        "insert into messages (id, session_id, provider, seq, role, ts, content) \
                     values (?1,?2,?3,?4,?5,?6,'shared needle phrase')",
                        params![id, session_id, provider.as_str(), seq, role.as_str(), ts],
                    )
                    .unwrap();
            };
        msg(
            1,
            "target",
            Provider::Claude,
            10,
            Role::User,
            "2026-01-02T00:00:00Z",
        );
        msg(
            2,
            "target",
            Provider::Claude,
            11,
            Role::Assistant,
            "2026-01-02T00:00:00Z",
        );
        msg(
            3,
            "wrong-provider",
            Provider::Codex,
            10,
            Role::User,
            "2026-01-02T00:00:00Z",
        );
        msg(
            4,
            "wrong-path",
            Provider::Claude,
            10,
            Role::User,
            "2026-01-02T00:00:00Z",
        );
        msg(
            5,
            "target",
            Provider::Claude,
            9,
            Role::User,
            "2025-12-31T00:00:00Z",
        );

        let filters = MessageFilters {
            role: Some(Role::User),
            provider: Some(Provider::Claude),
            path_prefix: Some("/Users/x/proj-a".into()),
            since: Some(utc("2026-01-01T00:00:00Z")),
            until: Some(utc("2026-01-31T00:00:00Z")),
            seq_from: Some(10),
            seq_to: Some(10),
            limit: 10,
            ..Default::default()
        };

        for (mode, pattern) in MESSAGE_CONTENT_MODE_CASES {
            assert_eq!(
                hit_keys(mode.search(&db, pattern, filters.clone()).unwrap()),
                vec![("target".to_string(), 10)],
                "{mode:?}: content mode must compose with role/provider/path/time/seq filters"
            );
        }
    }

    #[test]
    fn read_session_messages_selects_by_order_within_range_and_offset() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute(
                "insert into sessions (id, provider, provider_session_id, cwd, repo_root, \
                 preview_text, source_path, parse_version, discovery_source) \
                 values ('claude:s','claude','s','/p','/p','','/p','1','test')",
                [],
            )
            .unwrap();
        let msg = |seq: i64, role: &str, content: &str| {
            db.conn
                .execute(
                    "insert into messages (session_id, provider, seq, role, content) \
                     values ('claude:s','claude',?1,?2,?3)",
                    params![seq, role, content],
                )
                .unwrap();
        };
        // seq 0..3, roles: user, assistant, user, slash (mirrors the CLAUDE_FIXTURE shape).
        msg(0, "user", "first user turn");
        msg(1, "assistant", "assistant reply");
        msg(2, "user", "second user turn");
        msg(3, "slash", "/cmd make a plan");

        let seqs = |hits: Vec<MessageHit>| hits.iter().map(|h| h.seq).collect::<Vec<i64>>();
        let filters = |mutate: &dyn Fn(&mut MessageFilters)| {
            let mut f = MessageFilters {
                session_id: Some("claude:s".to_string()),
                ..Default::default()
            };
            mutate(&mut f);
            f
        };

        // OldestFirst + limit 2 = the first (oldest) 2, in chronological order.
        assert_eq!(
            seqs(
                db.read_session_messages(&filters(&|f| f.limit = 2), MessageOrder::OldestFirst)
                    .unwrap()
            ),
            vec![0, 1]
        );
        // NewestFirst + limit 2 = the last (newest) 2, STILL returned oldest-first. Order drives
        // SELECTION, not just display (no git --reverse-after-limit trap).
        assert_eq!(
            seqs(
                db.read_session_messages(&filters(&|f| f.limit = 2), MessageOrder::NewestFirst)
                    .unwrap()
            ),
            vec![2, 3]
        );
        // limit 0 = all, chronological regardless of the fetch direction.
        assert_eq!(
            seqs(
                db.read_session_messages(&filters(&|f| f.limit = 0), MessageOrder::NewestFirst)
                    .unwrap()
            ),
            vec![0, 1, 2, 3]
        );
        // Role filter composes with the newest-N window.
        assert_eq!(
            seqs(
                db.read_session_messages(
                    &filters(&|f| {
                        f.role = Some(Role::User);
                        f.limit = 1;
                    }),
                    MessageOrder::NewestFirst
                )
                .unwrap()
            ),
            vec![2]
        );
        // Inclusive seq range is the non-overlapping chunked-read primitive.
        assert_eq!(
            seqs(
                db.read_session_messages(
                    &filters(&|f| {
                        f.seq_from = Some(1);
                        f.seq_to = Some(2);
                    }),
                    MessageOrder::OldestFirst
                )
                .unwrap()
            ),
            vec![1, 2]
        );
        // Offset paginates the oldest-first window.
        assert_eq!(
            seqs(
                db.read_session_messages(
                    &filters(&|f| {
                        f.offset = 1;
                        f.limit = 2;
                    }),
                    MessageOrder::OldestFirst
                )
                .unwrap()
            ),
            vec![1, 2]
        );
    }

    #[test]
    fn read_session_messages_requires_a_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        // Without a session_id the newest-first direction is undefined (seq is session-local).
        assert!(db
            .read_session_messages(&MessageFilters::default(), MessageOrder::NewestFirst)
            .is_err());
    }

    #[test]
    fn read_session_messages_edge_cases_from_the_premortem_catalog() {
        // Covers failure modes F6-F14 from
        // notes/2026_07_20_2230_read_capability_premortem_and_failure_modes.md.
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute(
                "insert into sessions (id, provider, provider_session_id, cwd, repo_root, \
                 preview_text, source_path, parse_version, discovery_source) \
                 values ('claude:s','claude','s','/p','/p','','/p','1','test')",
                [],
            )
            .unwrap();
        // A single-message session to exercise F14 in isolation.
        db.conn
            .execute(
                "insert into sessions (id, provider, provider_session_id, cwd, repo_root, \
                 preview_text, source_path, parse_version, discovery_source) \
                 values ('claude:one','claude','one','/p','/p','','/p','1','test')",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "insert into messages (session_id, provider, seq, role, content) \
                 values ('claude:one','claude',0,'user','only turn')",
                [],
            )
            .unwrap();
        for seq in 0..6 {
            db.conn
                .execute(
                    "insert into messages (session_id, provider, seq, role, content) \
                     values ('claude:s','claude',?1,'user',?2)",
                    params![seq, format!("turn {seq}")],
                )
                .unwrap();
        }
        let seqs = |hits: Vec<MessageHit>| hits.iter().map(|h| h.seq).collect::<Vec<i64>>();
        let f = |mutate: &dyn Fn(&mut MessageFilters)| {
            let mut m = MessageFilters {
                session_id: Some("claude:s".to_string()),
                ..Default::default()
            };
            mutate(&mut m);
            m
        };

        // F7: an inclusive range includes BOTH endpoints exactly.
        assert_eq!(
            seqs(
                db.read_session_messages(
                    &f(&|m| {
                        m.seq_from = Some(1);
                        m.seq_to = Some(3);
                    }),
                    MessageOrder::OldestFirst
                )
                .unwrap()
            ),
            vec![1, 2, 3]
        );
        // F11: range then limit — newest 2 WITHIN [1,4] is [3,4], still chronological.
        assert_eq!(
            seqs(
                db.read_session_messages(
                    &f(&|m| {
                        m.seq_from = Some(1);
                        m.seq_to = Some(4);
                        m.limit = 2;
                    }),
                    MessageOrder::NewestFirst
                )
                .unwrap()
            ),
            vec![3, 4]
        );
        // F11 (oldest side): first 2 within [1,4] is [1,2].
        assert_eq!(
            seqs(
                db.read_session_messages(
                    &f(&|m| {
                        m.seq_from = Some(1);
                        m.seq_to = Some(4);
                        m.limit = 2;
                    }),
                    MessageOrder::OldestFirst
                )
                .unwrap()
            ),
            vec![1, 2]
        );
        // F10: offset skips from the NEWEST edge under NewestFirst (skip seq 5), then limit 2 →
        // [4,3] reversed to chronological [3,4].
        assert_eq!(
            seqs(
                db.read_session_messages(
                    &f(&|m| {
                        m.offset = 1;
                        m.limit = 2;
                    }),
                    MessageOrder::NewestFirst
                )
                .unwrap()
            ),
            vec![3, 4]
        );
        // F9: an offset past the end returns empty, not a panic or wrap.
        assert_eq!(
            seqs(
                db.read_session_messages(&f(&|m| m.offset = 99), MessageOrder::OldestFirst)
                    .unwrap()
            ),
            Vec::<i64>::new()
        );
        // F8: a seq range beyond the max seq returns empty.
        assert_eq!(
            seqs(
                db.read_session_messages(
                    &f(&|m| {
                        m.seq_from = Some(100);
                        m.seq_to = Some(200);
                    }),
                    MessageOrder::OldestFirst
                )
                .unwrap()
            ),
            Vec::<i64>::new()
        );
        // F6: from>to is rejected at the DB layer too (MessageFilters::validate), not silently
        // returned as empty — so the same invalid range fails the same way through every surface.
        assert!(db
            .read_session_messages(
                &f(&|m| {
                    m.seq_from = Some(4);
                    m.seq_to = Some(1);
                }),
                MessageOrder::OldestFirst
            )
            .is_err());
        // F13: a role with no matches is empty, not misleading.
        assert_eq!(
            seqs(
                db.read_session_messages(
                    &f(&|m| m.role = Some(Role::Assistant)),
                    MessageOrder::NewestFirst
                )
                .unwrap()
            ),
            Vec::<i64>::new()
        );
        // F14: a single-message session returns that one row for both directions.
        let one = |order| {
            db.read_session_messages(
                &MessageFilters {
                    session_id: Some("claude:one".to_string()),
                    ..Default::default()
                },
                order,
            )
            .unwrap()
        };
        assert_eq!(seqs(one(MessageOrder::OldestFirst)), vec![0]);
        assert_eq!(seqs(one(MessageOrder::NewestFirst)), vec![0]);
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

        session("windows-root", r"C:\Users\x\proj");
        session("windows-child", r"C:\Users\x\proj\sub");
        session("windows-sibling", r"C:\Users\x\project2");
        msg(6, "windows-root");
        msg(7, "windows-child");
        msg(8, "windows-sibling");
        assert_eq!(
            ids(r"C:\Users\x\proj"),
            vec!["windows-child", "windows-root"]
        );
    }

    #[test]
    fn restricted_fuzzy_search_scores_only_authorized_messages() {
        use crate::config::{SearchScopeConfig, SearchScopeMode};
        use crate::search_scope::{EffectiveAccessScope, TrustedAccessInputs};

        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("allowed");
        let hidden = dir.path().join("hidden");
        fs::create_dir_all(&allowed).unwrap();
        fs::create_dir_all(&hidden).unwrap();
        let mut db = Db::open(&dir.path().join("index.db")).unwrap();
        db.mark_schema_current().unwrap();
        let transaction = db.conn.unchecked_transaction().unwrap();
        {
            let mut insert_session = transaction
                .prepare(
                    "insert into sessions (
                         id, provider, provider_session_id, cwd, repo_root, preview_text,
                         source_path, parse_version, discovery_source
                     ) values (?, 'claude', ?, ?, ?, '', '/fixture', '1', 'test')",
                )
                .unwrap();
            let mut insert_message = transaction
                .prepare(
                    "insert into messages (
                         id, session_id, provider, seq, role, kind, content
                     ) values (?, ?, 'claude', 0, 'user', 'conversation', 'needle')",
                )
                .unwrap();
            for index in 0..1_201_i64 {
                let id = format!("hidden-{index:04}");
                let workspace = hidden.to_string_lossy();
                insert_session
                    .execute(params![id, id, workspace.as_ref(), workspace.as_ref()])
                    .unwrap();
                insert_message.execute(params![index + 1, id]).unwrap();
            }
            let allowed_id = "allowed";
            let workspace = allowed.to_string_lossy();
            insert_session
                .execute(params![
                    allowed_id,
                    allowed_id,
                    workspace.as_ref(),
                    workspace.as_ref()
                ])
                .unwrap();
            insert_message
                .execute(params![1_202_i64, allowed_id])
                .unwrap();
        }
        transaction.commit().unwrap();

        db.set_access_scope(
            EffectiveAccessScope::resolve(
                &SearchScopeConfig {
                    mode: SearchScopeMode::AllowedRoots,
                    roots: vec![allowed.to_string_lossy().into_owned()],
                    include_invocation_directory: false,
                },
                TrustedAccessInputs::default(),
            )
            .unwrap(),
        );
        let hits = db
            .search_messages(
                "needle",
                &MessageFilters {
                    match_mode: MessageSearchMode::Fuzzy,
                    limit: 1,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "allowed");
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
    fn analytics_queries_honor_exact_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let session = |id: &str| {
            db.conn
                .execute(
                    "insert into sessions (id, provider, provider_session_id, preview_text, \
                     source_path, parse_version, discovery_source) \
                     values (?1,'claude',?1,'','/p','1','test')",
                    params![id],
                )
                .unwrap();
        };
        session("s1");
        session("s10");
        let message = |id: i64, sid: &str, seq: i64, role: &str, content: &str| {
            db.conn
                .execute(
                    "insert into messages (id, session_id, provider, seq, role, content) \
                     values (?1,?2,'claude',?3,?4,?5)",
                    params![id, sid, seq, role, content],
                )
                .unwrap();
        };
        message(1, "s1", 0, "user", "that is wrong");
        message(2, "s1", 1, "slash", "/cmd-a");
        message(3, "s10", 0, "user", "that is wrong");
        message(4, "s10", 1, "slash", "/cmd-b");

        let filters = MessageFilters {
            session_id: Some("s1".into()),
            ..Default::default()
        };
        let patterns = vec![("misc".to_string(), regex::Regex::new("(?i)wrong").unwrap())];
        assert_eq!(
            db.find_corrections(&patterns, &filters)
                .unwrap()
                .iter()
                .map(|row| row.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["s1"],
            "exact session_id must not match s10"
        );
        assert_eq!(
            db.message_role_counts(&filters).unwrap(),
            vec![("slash".to_string(), 1), ("user".to_string(), 1)]
        );
        let planning = db.planning_usage(&filters, &[]).unwrap();
        assert_eq!(planning.len(), 1);
        assert_eq!(planning[0].command, "/cmd-a");
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
        insert(4, 3, "tool", Some("ÄTool"), "unicode tool");

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
        let unicode = db
            .search_messages(
                "ät",
                &MessageFilters {
                    field: Some(SearchField::ToolName),
                    limit: 1,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(unicode.len(), 1);
        assert_eq!(unicode[0].tool_name.as_deref(), Some("ÄTool"));
        db.conn
            .execute_batch(
                "with recursive n(value) as (
                     values(1) union all select value + 1 from n where value < 100
                 )
                 insert into messages (
                     session_id, provider, seq, role, tool_name, content
                 )
                 select 's1', 'claude', 100 + value, 'tool',
                        printf('other_tool_%03d', value), 'unrelated'
                   from n;",
            )
            .unwrap();
        let (fuzzy, explain) = db
            .search_messages_with_explain(
                "edt",
                &MessageFilters {
                    field: Some(SearchField::ToolName),
                    match_mode: MessageSearchMode::Fuzzy,
                    limit: 5,
                    ..Default::default()
                },
                true,
            )
            .unwrap();
        assert_eq!(fuzzy.len(), 1);
        assert_eq!(fuzzy[0].tool_name.as_deref(), Some("Edit"));
        let explain = explain.unwrap();
        assert_eq!(explain.prefilter, None);
        assert_eq!(explain.candidates, Some(1));
        assert_eq!(
            explain.prefilter_skipped.as_deref(),
            Some("complete filtered corpus scored with bounded top-K retention")
        );
        assert!(explain.candidates.unwrap() < explain.corpus);
        let vocabulary_plan = db
            .conn
            .prepare(
                "explain query plan select tool_name from messages
                  where tool_name is not null order by tool_name",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("\n");
        assert!(
            vocabulary_plan.contains("idx_messages_tool_name"),
            "{vocabulary_plan}"
        );
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
    fn fuzzy_tool_name_searches_the_complete_filtered_vocabulary() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute_batch(
                "insert into sessions (
                     id, provider, provider_session_id, preview_text, source_path,
                     parse_version, discovery_source
                 ) values ('s1','claude','s1','','/p','1','test')",
            )
            .unwrap();
        let tx = db.conn.unchecked_transaction().unwrap();
        {
            let mut insert = tx
                .prepare(
                    "insert into messages (
                         session_id, provider, seq, role, tool_name, content
                     ) values ('s1', 'claude', ?1, 'tool', ?2, '')",
                )
                .unwrap();
            for seq in 0..=10_000_usize {
                insert
                    .execute(params![seq as i64, format!("tool_{seq:05}")])
                    .unwrap();
            }
        }
        tx.commit().unwrap();

        db.conn
            .execute(
                "insert into messages (
                     session_id, provider, seq, role, tool_name, content
                 ) values ('s1', 'claude', ?1, 'tool', 'tol', '')",
                params![10_001_i64],
            )
            .unwrap();

        let (hits, explain) = db
            .search_messages_with_explain(
                "tol",
                &MessageFilters {
                    field: Some(SearchField::ToolName),
                    match_mode: MessageSearchMode::Fuzzy,
                    limit: 1,
                    ..Default::default()
                },
                true,
            )
            .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].tool_name.as_deref(), Some("tol"));
        let explain = explain.unwrap();
        assert_eq!(explain.corpus, 10_002);
        assert_eq!(explain.candidates, Some(10_002));
    }

    #[test]
    fn date_filter_until_covers_sub_second_tail_of_final_second() {
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
        let until =
            crate::dates::parse_bound("2026-01-15", crate::dates::Bound::End, Utc::now()).unwrap();
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
    fn date_filter_preserves_exact_fractional_upper_bound() {
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
        for (id, seq, ts) in [
            (1, 0, "2026-07-07T02:53:22.357999999+00:00"),
            (2, 1, "2026-07-07T02:53:22.358000000+00:00"),
            (3, 2, "2026-07-07T02:53:22.358000001+00:00"),
        ] {
            db.conn
                .execute(
                    "insert into messages (id, session_id, provider, seq, role, ts, content) \
                     values (?1,'s1','claude',?2,'user',?3,'event')",
                    rusqlite::params![id, seq, ts],
                )
                .unwrap();
        }
        let until = crate::dates::parse_bound(
            "2026-07-07T02:53:22.358Z",
            crate::dates::Bound::End,
            Utc::now(),
        )
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
            hits.iter().map(|hit| hit.seq).collect::<Vec<_>>(),
            vec![0, 1],
            "an exact fractional --until must exclude later events in the same second"
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
                "zebracode",
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
    fn session_search_zero_is_unlimited_and_large_limit_does_not_overflow() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        for id in ["s1", "s2"] {
            db.conn
                .execute(
                    "insert into sessions (id, provider, provider_session_id, preview_text, \
                     source_path, parse_version, discovery_source) \
                     values (?1,'claude',?1,'alpha preview',?2,'1','test')",
                    params![id, format!("/{id}.jsonl")],
                )
                .unwrap();
            let rowid: i64 = db
                .conn
                .query_row("select rowid from sessions where id = ?1", [id], |row| {
                    row.get(0)
                })
                .unwrap();
            db.conn
                .execute(
                    "insert into sessions_fts(rowid, title, summary, preview_text, transcript_text) \
                     values (?1,'','','alpha preview','')",
                    params![rowid],
                )
                .unwrap();
        }

        let scoring = crate::config::ScoringConfig::default();
        let mut filters = SearchFilters::default();

        assert_eq!(
            db.search("alpha", &filters, None, &scoring).unwrap().len(),
            2
        );

        filters.limit = 1;
        assert_eq!(
            db.search("alpha", &filters, None, &scoring).unwrap().len(),
            1
        );

        filters.limit = usize::MAX;
        assert_eq!(
            db.search("alpha", &filters, None, &scoring).unwrap().len(),
            2
        );
    }

    #[test]
    fn session_search_requires_a_text_match_before_ranking_bonuses() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute(
                "insert into sessions (
                     id, provider, provider_session_id, cwd, repo_root, preview_text,
                     source_path, updated_at, parse_version, discovery_source
                 ) values ('s1','claude','s1','/repo','/repo','ordinary text','/p',?1,'1','test')",
                params![Utc::now().to_rfc3339()],
            )
            .unwrap();

        let hits = db
            .search(
                "⸘⸘⸘",
                &SearchFilters::default(),
                Some("/repo"),
                &crate::config::ScoringConfig::default(),
            )
            .unwrap();

        assert!(
            hits.is_empty(),
            "recency and repository bonuses must not admit an unrelated session"
        );
    }

    #[test]
    fn session_search_combines_query_term_coverage_across_one_session() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute(
                "insert into sessions (
                     id, provider, provider_session_id, title, summary, preview_text,
                     source_path, parse_version, discovery_source
                 ) values ('s1','claude','s1','alpha','beta','','/p','1','test')",
                [],
            )
            .unwrap();
        let scoring = crate::config::ScoringConfig {
            title_score: 0,
            summary_score: 0,
            path_score: 0,
            preview_score: 0,
            other_score: 0,
            token_bonus: 0,
            all_tokens_bonus: 150,
            recency_weight: 0,
            current_repo_bonus: 0,
            ..Default::default()
        };

        let hits = db
            .search("alpha beta", &SearchFilters::default(), None, &scoring)
            .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session.id, "s1");
        assert_eq!(hits[0].score, 150);
    }

    #[test]
    fn session_search_positive_limit_is_a_prefix_of_unlimited_ranking() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        for (id, title, preview) in [("s1", "", "alpha"), ("s2", "alpha", "")] {
            db.conn
                .execute(
                    "insert into sessions (
                         id, provider, provider_session_id, title, preview_text,
                         source_path, parse_version, discovery_source
                     ) values (?1,'claude',?1,?2,?3,?4,'1','test')",
                    params![id, title, preview, format!("/{id}.jsonl")],
                )
                .unwrap();
            let rowid: i64 = db
                .conn
                .query_row("select rowid from sessions where id = ?1", [id], |row| {
                    row.get(0)
                })
                .unwrap();
            db.conn
                .execute(
                    "insert into sessions_fts(rowid, title, summary, preview_text, transcript_text)
                     values (?1,?2,'',?3,'')",
                    params![rowid, title, preview],
                )
                .unwrap();
        }
        let scoring = crate::config::ScoringConfig {
            recency_weight: 0,
            ..Default::default()
        };
        let all_filters = SearchFilters {
            limit: 0,
            ..Default::default()
        };
        let all = db.search("alpha", &all_filters, None, &scoring).unwrap();
        let one_filter = SearchFilters {
            limit: 1,
            ..all_filters
        };
        let one = db.search("alpha", &one_filter, None, &scoring).unwrap();

        assert_eq!(one.len(), 1);
        assert_eq!(one[0].session.id, all[0].session.id);
        assert_eq!(one[0].session.id, "s2");
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
        assert!(err.contains("aise list") || err.contains("aise search"));
        let err = db.resolve_session_record("zzz").unwrap_err().to_string();
        assert!(err.contains("no session matches"));
        assert!(err.contains("aise list") || err.contains("aise search"));

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
    fn open_adds_typed_message_columns_to_version_one_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let legacy = Connection::open(&path).unwrap();
            legacy
                .execute_batch(
                    "create table messages (
                        id integer primary key,
                        session_id text not null,
                        provider text not null,
                        seq integer not null,
                        role text not null,
                        ts text,
                        tool_name text,
                        is_compaction integer not null default 0,
                        content text not null
                    );
                    pragma user_version = 1;",
                )
                .unwrap();
        }

        let db = Db::open(&path).unwrap();
        let columns = db
            .conn
            .prepare("pragma table_info(messages)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(columns.iter().any(|column| column == "kind"));
        assert!(columns.iter().any(|column| column == "tool_call_id"));
        assert!(db.needs_backfill().unwrap());
    }

    #[test]
    fn tool_argument_search_uses_explicit_json_pointer_and_excludes_results() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        seed_messages(&db, &[("tool", "call"), ("tool", "cargo test output")]);
        db.conn
            .execute_batch(
                r#"update messages set
                       kind = 'tool_call', tool_name = 'exec_command',
                       content = '{"args":{"cmd":"cargo test","request":{"path":"src/lib.rs"}},"kind":"tool_call","tool_name":"exec_command"}'
                   where seq = 0;
                   update messages set kind = 'tool_result', tool_name = 'exec_command'
                   where seq = 1;"#,
            )
            .unwrap();

        let hits = db
            .search_messages(
                "cargo test",
                &MessageFilters {
                    field: Some(SearchField::ToolArgument),
                    argument_path: Some("/cmd".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].seq, 0);

        let nested = db
            .search_messages(
                "src/lib.rs",
                &MessageFilters {
                    field: Some(SearchField::ToolArgument),
                    argument_path: Some("/request/path".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(nested.len(), 1);

        db.conn
            .execute_batch(
                r#"with recursive n(value) as (
                       values(1) union all select value + 1 from n where value < 100
                   )
                   insert into messages (
                       session_id, provider, seq, role, tool_name, kind, content
                   )
                   select 'claude:s1', 'claude', 10 + value, 'tool', 'exec_command',
                          'tool_call',
                          printf('{"args":{"cmd":"unrelated payload %d"}}', value)
                     from n;"#,
            )
            .unwrap();
        let fuzzy_filters = MessageFilters {
            field: Some(SearchField::ToolArgument),
            argument_path: Some("/cmd".to_string()),
            match_mode: MessageSearchMode::Fuzzy,
            limit: 5,
            ..Default::default()
        };
        let (fuzzy, explain) = db
            .search_messages_with_explain("crgo tst", &fuzzy_filters, true)
            .unwrap();
        assert_eq!(fuzzy.len(), 1);
        assert_eq!(fuzzy[0].seq, 0);
        let explain = explain.unwrap();
        assert_eq!(
            explain.prefilter_skipped.as_deref(),
            Some("complete filtered corpus scored with bounded top-K retention")
        );
        assert!(
            explain.candidates.unwrap() < explain.corpus,
            "only matching tool-argument projections should count as fuzzy candidates: {explain:?}"
        );

        let plan = db
            .conn
            .prepare(
                "explain query plan select session_id, seq from messages
                 where kind = 'tool_call' order by session_id, seq",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("\n");
        assert!(plan.contains("idx_messages_tool_calls"), "{plan}");

        let error = db
            .search_messages(
                "cargo",
                &MessageFilters {
                    field: Some(SearchField::ToolArgument),
                    argument_path: Some("cmd".to_string()),
                    ..Default::default()
                },
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("RFC 6901"));
    }

    #[test]
    fn message_search_three_modes_by_three_fields_share_one_result_contract() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        seed_messages(
            &db,
            &[
                ("tool", "placeholder"),
                ("tool", "distractor"),
                ("tool", "second target"),
            ],
        );
        db.conn
            .execute_batch(
                r#"update messages set
                       kind = 'tool_call', tool_name = 'exec_command',
                       content = '{"args":{"cmd":"cargo test --workspace"},"kind":"tool_call","tool_name":"exec_command"}'
                   where seq = 0;
                   update messages set
                       kind = 'tool_call', tool_name = 'read_file',
                       content = '{"args":{"cmd":"open notes.md"},"kind":"tool_call","tool_name":"read_file"}'
                   where seq = 1;
                   update messages set
                       kind = 'tool_call', tool_name = 'exec_command',
                       content = '{"args":{"cmd":"cargo test --workspace"},"kind":"tool_call","tool_name":"exec_command"}'
                   where seq = 2;"#,
            )
            .unwrap();

        let cases = [
            (SearchField::Content, MessageSearchMode::Exact, "cargo test"),
            (
                SearchField::Content,
                MessageSearchMode::Regex,
                r"cargo\s+test",
            ),
            (SearchField::Content, MessageSearchMode::Fuzzy, "crgo tst"),
            (SearchField::ToolName, MessageSearchMode::Exact, "exec"),
            (SearchField::ToolName, MessageSearchMode::Regex, r"^exec_"),
            (SearchField::ToolName, MessageSearchMode::Fuzzy, "excmd"),
            (
                SearchField::ToolArgument,
                MessageSearchMode::Exact,
                "cargo test",
            ),
            (
                SearchField::ToolArgument,
                MessageSearchMode::Regex,
                r"cargo\s+test",
            ),
            (
                SearchField::ToolArgument,
                MessageSearchMode::Fuzzy,
                "crgo tst",
            ),
        ];

        for (field, match_mode, query) in cases {
            let (hits, explain) = db
                .search_messages_with_explain(
                    query,
                    &MessageFilters {
                        kinds: Some(vec![crate::models::MessageKind::ToolCall]),
                        field: Some(field),
                        argument_path: (field == SearchField::ToolArgument)
                            .then(|| "/cmd".to_string()),
                        match_mode,
                        limit: 10,
                        ..Default::default()
                    },
                    true,
                )
                .unwrap_or_else(|error| panic!("{field:?}/{match_mode:?}: {error:#}"));
            if match_mode == MessageSearchMode::Fuzzy {
                assert_eq!(
                    hits[0].fuzzy_score, hits[1].fuzzy_score,
                    "identical projected values must exercise the stable identity tie-break"
                );
            }
            assert_eq!(
                hit_keys(hits),
                vec![("claude:s1".into(), 0), ("claude:s1".into(), 2)],
                "{field:?}/{match_mode:?}"
            );
            assert!(explain.is_some(), "{field:?}/{match_mode:?}");
            let page = db
                .search_messages(
                    query,
                    &MessageFilters {
                        kinds: Some(vec![crate::models::MessageKind::ToolCall]),
                        field: Some(field),
                        argument_path: (field == SearchField::ToolArgument)
                            .then(|| "/cmd".to_string()),
                        match_mode,
                        limit: 1,
                        offset: 1,
                        ..Default::default()
                    },
                )
                .unwrap_or_else(|error| panic!("paged {field:?}/{match_mode:?}: {error:#}"));
            assert_eq!(
                hit_keys(page),
                vec![("claude:s1".into(), 2)],
                "paged {field:?}/{match_mode:?}"
            );
        }
    }

    #[test]
    fn message_search_validation_is_identical_for_all_three_fields() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        for field in [
            SearchField::Content,
            SearchField::ToolName,
            SearchField::ToolArgument,
        ] {
            let filters = |match_mode, limit, offset| MessageFilters {
                field: Some(field),
                argument_path: (field == SearchField::ToolArgument).then(|| "/cmd".into()),
                match_mode,
                limit,
                offset,
                ..Default::default()
            };
            let malformed = db
                .search_messages("[", &filters(MessageSearchMode::Regex, 10, 0))
                .unwrap_err()
                .to_string();
            assert!(
                malformed.contains("invalid regex"),
                "{field:?}: {malformed}"
            );

            let short = db
                .search_messages("ab", &filters(MessageSearchMode::Fuzzy, 10, 0))
                .unwrap_err()
                .to_string();
            assert!(
                short.contains("at least 3 characters"),
                "{field:?}: {short}"
            );

            let unlimited = db
                .search_messages("abc", &filters(MessageSearchMode::Fuzzy, 0, 0))
                .unwrap_err()
                .to_string();
            assert!(
                unlimited.contains("finite non-zero limit"),
                "{field:?}: {unlimited}"
            );

            let large_offset =
                db.search_messages("abc", &filters(MessageSearchMode::Fuzzy, 2, 9_999));
            assert!(
                large_offset.is_ok(),
                "{field:?}: finite fuzzy offsets have no arbitrary result window: {large_offset:?}"
            );
        }
    }

    #[test]
    fn tool_argument_search_handles_json_shapes_unicode_punctuation_and_large_values() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        seed_messages(
            &db,
            &[
                ("tool", "malformed"),
                ("tool", "missing args"),
                ("tool", "scalar args"),
                ("tool", "null"),
                ("tool", "array"),
                ("tool", "nested"),
                ("tool", "large"),
            ],
        );
        let large = format!("{} cargo test --workspace", "x".repeat(128 * 1024));
        let rows = [
            "{malformed".to_string(),
            r#"{"kind":"tool_call"}"#.to_string(),
            r#"{"args":"scalar","kind":"tool_call"}"#.to_string(),
            r#"{"args":{"cmd":null},"kind":"tool_call"}"#.to_string(),
            r#"{"args":{"cmd":["café","C++","--path"]},"kind":"tool_call"}"#.to_string(),
            r#"{"args":{"request":{"path":"src/quoted file.rs"}},"kind":"tool_call"}"#.to_string(),
            serde_json::json!({"args": {"cmd": large}, "kind": "tool_call"}).to_string(),
        ];
        for (seq, content) in rows.iter().enumerate() {
            db.conn
                .execute(
                    "update messages set kind='tool_call', tool_name='ÄTool::C++', content=?1
                     where seq=?2",
                    params![content, seq as i64],
                )
                .unwrap();
        }
        let argument = |path: &str, mode| MessageFilters {
            field: Some(SearchField::ToolArgument),
            argument_path: Some(path.into()),
            match_mode: mode,
            limit: 10,
            ..Default::default()
        };

        assert_eq!(
            hit_keys(
                db.search_messages("café", &argument("/cmd", MessageSearchMode::Exact))
                    .unwrap()
            ),
            vec![("claude:s1".into(), 4)]
        );
        assert_eq!(
            hit_keys(
                db.search_messages(
                    r#"C\+\+.*--path"#,
                    &argument("/cmd", MessageSearchMode::Regex),
                )
                .unwrap()
            ),
            vec![("claude:s1".into(), 4)]
        );
        assert_eq!(
            hit_keys(
                db.search_messages(
                    "quoted file.rs",
                    &argument("/request/path", MessageSearchMode::Exact),
                )
                .unwrap()
            ),
            vec![("claude:s1".into(), 5)]
        );
        assert_eq!(
            hit_keys(
                db.search_messages("null", &argument("/cmd", MessageSearchMode::Exact))
                    .unwrap()
            ),
            vec![("claude:s1".into(), 3)]
        );
        assert_eq!(
            hit_keys(
                db.search_messages(
                    "cargo test --workspace",
                    &argument("/cmd", MessageSearchMode::Exact),
                )
                .unwrap()
            ),
            vec![("claude:s1".into(), 6)]
        );
        assert!(
            db.search_messages("scalar", &argument("/cmd", MessageSearchMode::Exact))
                .unwrap()
                .is_empty(),
            "a pointer below scalar args and malformed/missing envelopes must project NULL"
        );

        for mode in [MessageSearchMode::Exact, MessageSearchMode::Regex] {
            let query = if mode == MessageSearchMode::Exact {
                "ätool::c++"
            } else {
                r#"(?i)^ätool::c\+\+$"#
            };
            assert_eq!(
                db.search_messages(
                    query,
                    &MessageFilters {
                        field: Some(SearchField::ToolName),
                        match_mode: mode,
                        limit: 10,
                        ..Default::default()
                    },
                )
                .unwrap()
                .len(),
                7,
                "{mode:?} must preserve Unicode and punctuation in tool names"
            );
        }
    }

    #[test]
    fn fuzzy_content_reports_complete_filtered_corpus_scoring() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute(
                "insert into sessions (
                     id, provider, provider_session_id, preview_text, source_path,
                     parse_version, discovery_source
                 ) values ('s1','claude','s1','','/p','1','test')",
                [],
            )
            .unwrap();
        let tx = db.conn.unchecked_transaction().unwrap();
        {
            let mut insert = tx
                .prepare(
                    "insert into messages(session_id, provider, seq, role, content)
                     values ('s1','claude',?1,'user',?2)",
                )
                .unwrap();
            for seq in 0..1_205_i64 {
                insert
                    .execute(params![seq, format!("abc candidate {seq:04}")])
                    .unwrap();
            }
        }
        tx.commit().unwrap();

        let (hits, explain) = db
            .search_messages_with_explain(
                "abc",
                &MessageFilters {
                    match_mode: MessageSearchMode::Fuzzy,
                    limit: 1,
                    ..Default::default()
                },
                true,
            )
            .unwrap();

        assert_eq!(hits.len(), 1);
        let explain = explain.unwrap();
        assert_eq!(
            explain.prefilter_skipped.as_deref(),
            Some("complete filtered corpus scored with bounded top-K retention")
        );
        assert_eq!(explain.candidates, Some(1_205));
        assert_eq!(explain.corpus, 1_205);
    }

    #[test]
    fn v4_prefilter_work_stays_selective_across_one_two_four_x_corpora() {
        for corpus in [200_i64, 400, 800] {
            let dir = tempfile::tempdir().unwrap();
            let db = Db::open(&dir.path().join("index.db")).unwrap();
            db.conn
                .execute(
                    "insert into sessions (
                         id, provider, provider_session_id, preview_text, source_path,
                         parse_version, discovery_source
                     ) values ('s1','claude','s1','','/p','1','test')",
                    [],
                )
                .unwrap();
            let tx = db.conn.unchecked_transaction().unwrap();
            {
                let mut insert = tx
                    .prepare(
                        "insert into messages(session_id, provider, seq, role, content)
                         values ('s1','claude',?1,'user',?2)",
                    )
                    .unwrap();
                insert
                    .execute(params![0_i64, "rare fuzzy anchor C++ --path"])
                    .unwrap();
                for seq in 1..corpus {
                    insert
                        .execute(params![seq, format!("unrelated filler row {seq:04}")])
                        .unwrap();
                }
            }
            tx.commit().unwrap();

            for (mode, query) in [
                (MessageSearchMode::Exact, "fuzzy anchor C++"),
                (MessageSearchMode::Regex, r"fuzzy\s+anchor\s+C\+\+"),
                (MessageSearchMode::Fuzzy, "fzzy anchr C++"),
            ] {
                let (hits, explain) = db
                    .search_messages_with_explain(
                        query,
                        &MessageFilters {
                            match_mode: mode,
                            limit: 10,
                            ..Default::default()
                        },
                        true,
                    )
                    .unwrap();
                assert_eq!(hit_keys(hits), vec![("s1".into(), 0)], "{corpus}/{mode:?}");
                let explain = explain.unwrap();
                assert_eq!(explain.corpus, corpus, "{corpus}/{mode:?}");
                assert!(
                    explain.candidates.is_some_and(|count| count < corpus / 4),
                    "candidate work must stay selective as the unrelated corpus scales: \
                     corpus={corpus} mode={mode:?} explain={explain:?}"
                );
            }
        }
    }

    #[test]
    fn message_kind_call_id_and_offset_share_one_query_contract() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        seed_messages(&db, &[("tool", "first call"), ("tool", "first result")]);
        db.conn
            .execute_batch(
                "update messages set kind = 'tool_call', tool_call_id = 'call-1' where seq = 0;
                 update messages set kind = 'tool_result', tool_call_id = 'call-1' where seq = 1;",
            )
            .unwrap();

        let calls = db
            .search_messages(
                "",
                &MessageFilters {
                    kinds: Some(vec![crate::models::MessageKind::ToolCall]),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].kind, crate::models::MessageKind::ToolCall);
        assert_eq!(calls[0].tool_call_id.as_deref(), Some("call-1"));

        let page = db
            .search_messages(
                "",
                &MessageFilters {
                    limit: 1,
                    offset: 1,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].seq, 1);
        assert_eq!(page[0].kind, crate::models::MessageKind::ToolResult);
    }

    #[test]
    fn message_context_rejects_negative_window_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let error = db
            .message_context("missing", 0, -1, 0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("before must be non-negative"), "{error}");
        let error = db
            .message_context("missing", 0, 0, -1)
            .unwrap_err()
            .to_string();
        assert!(error.contains("after must be non-negative"), "{error}");
    }

    #[test]
    fn query_timeout_interrupts_expensive_work_and_resets_the_handler() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();

        let error = db
            .with_query_timeout(std::num::NonZeroU64::new(1), || {
                db.conn
                    .query_row(
                        "with recursive n(x) as (values(0) union all select x + 1 from n where x < 100000000) select sum(x) from n",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .map_err(Into::into)
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("timed out after 1 ms"), "{error}");

        assert_eq!(
            db.conn
                .query_row("select 1", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1,
            "the RAII guard must clear SQLite's progress handler after interruption"
        );
    }

    #[test]
    fn query_timeout_never_panics_when_deadline_exceeds_platform_instant_range() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();

        let result = db.with_query_timeout(std::num::NonZeroU64::new(u64::MAX), || {
            db.conn
                .query_row("select 1", [], |row| row.get::<_, i64>(0))
                .map_err(Into::into)
        });

        match result {
            Ok(value) => assert_eq!(value, 1),
            Err(error) => assert!(
                error.to_string().contains("timeout_ms is too large"),
                "{error}"
            ),
        }
        assert_eq!(
            db.conn
                .query_row("select 2", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            2,
            "an unrepresentable timeout must not leave a progress handler installed"
        );
    }

    #[test]
    fn message_context_windows_batch_preserves_anchor_order_and_empty_windows() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        seed_messages(
            &db,
            &[
                ("user", "zero"),
                ("assistant", "one"),
                ("user", "two"),
                ("assistant", "three"),
            ],
        );
        let anchors = vec![
            ("claude:s1".to_string(), 3),
            ("missing".to_string(), 10),
            ("claude:s1".to_string(), 0),
        ];

        let windows = db.message_context_windows(&anchors, 1, 1).unwrap();

        assert_eq!(windows.len(), 3);
        assert_eq!(
            windows[0].iter().map(|hit| hit.seq).collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert!(windows[1].is_empty());
        assert_eq!(
            windows[2].iter().map(|hit| hit.seq).collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    #[test]
    fn session_time_profile_is_bounded_and_uses_typed_message_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        seed_messages(
            &db,
            &[
                ("user", "start"),
                ("tool", "call"),
                ("tool", "result"),
                ("assistant", "end"),
            ],
        );
        db.conn
            .execute_batch(
                "update messages set ts = '2026-07-10T10:00:00Z' where seq = 0;
                 update messages set ts = '2026-07-10T10:00:05Z', kind = 'tool_call' where seq = 1;
                 update messages set ts = '2026-07-10T10:00:20Z', kind = 'tool_result' where seq = 2;",
            )
            .unwrap();

        let profile = db.session_time_profile("claude:s1").unwrap();
        assert_eq!(profile.messages, 4);
        assert_eq!(profile.timestamped_messages, 3);
        assert_eq!(profile.undated_messages, 1);
        assert_eq!(profile.observed_span_seconds, Some(20));
        assert_eq!(profile.max_message_gap_seconds, Some(15));
        assert_eq!((profile.tool_calls, profile.tool_results), (1, 1));
    }

    #[test]
    fn session_time_profile_uses_intermediate_events_across_months() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        seed_messages(
            &db,
            &[
                ("user", "january"),
                ("assistant", "february"),
                ("user", "april"),
                ("assistant", "undated tail"),
            ],
        );
        db.conn
            .execute_batch(
                "update sessions
                    set created_at = '2026-04-30T00:00:00Z',
                        updated_at = '2026-04-30T00:00:01Z',
                        last_message_at = '2026-04-30T00:00:01Z'
                  where id = 'claude:s1';
                 update messages set ts = '2026-01-01T00:00:00.100Z' where seq = 0;
                 update messages set ts = '2026-02-01T00:00:00.200Z' where seq = 1;
                 update messages set ts = '2026-04-30T00:00:00.300Z' where seq = 2;
                 update messages set ts = null where seq = 3;",
            )
            .unwrap();

        let profile = db.session_time_profile("claude:s1").unwrap();

        assert_eq!(profile.timestamped_messages, 3);
        assert_eq!(profile.undated_messages, 1);
        assert_eq!(
            profile.first_timestamp.unwrap().to_rfc3339(),
            "2026-01-01T00:00:00.100+00:00"
        );
        assert_eq!(
            profile.last_timestamp.unwrap().to_rfc3339(),
            "2026-04-30T00:00:00.300+00:00"
        );
        assert_eq!(profile.observed_span_seconds, Some(10_281_600));
        assert_eq!(profile.max_message_gap_seconds, Some(7_603_200));
    }

    #[test]
    fn parser_health_uses_shared_provider_versions_and_consistent_totals() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.mark_schema_current().unwrap();
        db.conn
            .execute_batch(
                "insert into sessions(id, provider, provider_session_id, preview_text, source_path, parse_version, parse_warning, discovery_source) values
                 ('claude:current','claude','current','','/a','claude-v2',null,'jsonl'),
                 ('claude:stale','claude','stale','','/b','claude-v1','old parser','jsonl');",
            )
            .unwrap();
        db.conn
            .execute(
                "insert into sessions(id, provider, provider_session_id, preview_text, source_path, parse_version, discovery_source)
                 values ('codex:current','codex','current','','/c',?1,'jsonl')",
                [crate::util::provider_parse_version(Provider::Codex)],
            )
            .unwrap();

        let health = db.parser_health().unwrap();
        assert!(health.schema_current);
        assert_eq!(health.indexed_sessions, 3);
        assert_eq!(health.current_sessions, 2);
        assert_eq!(health.stale_sessions, 1);
        assert_eq!(health.parse_warnings, 1);
        let claude = health
            .providers
            .iter()
            .find(|item| item.provider == Provider::Claude)
            .unwrap();
        assert_eq!(claude.expected_parse_version, "claude-v2");
        assert_eq!((claude.current_sessions, claude.stale_sessions), (1, 1));
        assert_eq!(
            db.stale_session_sources().unwrap(),
            vec![(Provider::Claude, "/b".to_string())]
        );
    }

    #[test]
    fn complete_parse_replaces_superseded_identity_for_the_same_source() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let source = std::path::Path::new("/fixture/changing-identity.jsonl");
        let mut old = crate::util::minimal_record(Provider::ClaudeDesktop, source, String::new());
        old.session.id = "claude-desktop:old".into();
        old.session.provider_session_id = "old".into();
        old.session.parse_version = "claude-desktop-local-agent-v1".into();
        old.messages = vec![crate::models::Message {
            seq: 0,
            role: Role::User,
            kind: crate::models::MessageKind::Conversation,
            ts: None,
            tool_name: None,
            tool_call_id: None,
            is_compaction: false,
            content: "obsolete".into(),
        }];
        db.upsert_session(&old, 1, 1).unwrap();

        let mut current =
            crate::util::minimal_record(Provider::ClaudeDesktop, source, String::new());
        current.session.id = "claude-desktop:current".into();
        current.session.provider_session_id = "current".into();
        current.messages = vec![crate::models::Message {
            seq: 0,
            role: Role::User,
            kind: crate::models::MessageKind::Conversation,
            ts: None,
            tool_name: None,
            tool_call_id: None,
            is_compaction: false,
            content: "replacement".into(),
        }];
        db.upsert_session(&current, 2, 2).unwrap();

        assert!(db.resolve_session_record("claude-desktop:old").is_err());
        assert_eq!(
            db.resolve_session_record("claude-desktop:current")
                .unwrap()
                .source_path,
            source.to_string_lossy()
        );
        assert!(db
            .search_messages("obsolete", &MessageFilters::default())
            .unwrap()
            .is_empty());
        assert!(db
            .is_file_current(
                Provider::ClaudeDesktop,
                &source.to_string_lossy(),
                2,
                2,
                crate::util::provider_parse_version(Provider::ClaudeDesktop),
            )
            .unwrap());
    }

    #[test]
    fn source_checkpoint_version_backfill_requires_a_matching_source_path() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let source = dir.path().join("session.jsonl");
        let parsed = crate::util::minimal_record(Provider::Claude, &source, String::new());
        db.upsert_session(&parsed, 10, 20).unwrap();
        db.conn
            .execute("update files_seen set parse_version = ''", [])
            .unwrap();

        db.backfill_source_parse_versions().unwrap();
        assert!(db
            .is_file_current(
                Provider::Claude,
                &source.to_string_lossy(),
                10,
                20,
                crate::util::provider_parse_version(Provider::Claude),
            )
            .unwrap());

        db.conn
            .execute_batch(
                "update sessions set source_path = '/different/source.jsonl';
                 update files_seen set parse_version = '';",
            )
            .unwrap();
        db.backfill_source_parse_versions().unwrap();
        assert!(!db
            .is_file_current(
                Provider::Claude,
                &source.to_string_lossy(),
                10,
                20,
                crate::util::provider_parse_version(Provider::Claude),
            )
            .unwrap());
    }

    #[test]
    fn complete_parse_reconciles_verified_source_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let alias = std::path::Path::new("/fixture/alias/session.jsonl");
        let canonical = std::path::Path::new("/fixture/canonical/session.jsonl");
        let mut old = crate::util::minimal_record(Provider::Claude, alias, String::new());
        old.session.id = "claude:old-alias".into();
        old.session.provider_session_id = "old-alias".into();
        db.upsert_session(&old, 1, 1).unwrap();
        let mut current = crate::util::minimal_record(Provider::Claude, canonical, String::new());
        current.session.id = "claude:current".into();
        current.session.provider_session_id = "current".into();

        db.upsert_session_reconciling_sources(
            &current,
            2,
            2,
            &[alias.to_string_lossy().into_owned()],
            true,
        )
        .unwrap();

        assert!(db.resolve_session_record("claude:old-alias").is_err());
        assert_eq!(db.indexed_source_identities().unwrap().len(), 1);
        assert_eq!(
            db.indexed_source_identities().unwrap()[0].1,
            canonical.to_string_lossy()
        );
    }

    #[test]
    fn schema_backfill_flag_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn.pragma_update(None, "user_version", 2).unwrap();
        assert!(db.needs_backfill().unwrap());
        db.mark_schema_current().unwrap();
        assert!(!db.needs_backfill().unwrap(), "stamping clears the flag");
        assert_eq!(db.schema_version().unwrap(), PARSER_SCHEMA_VERSION);
    }

    #[test]
    fn mark_schema_current_never_promotes_below_v4_to_current() {
        // Recurrence guard for the v4-stamped-but-broken hybrid schema. `mark_schema_current` runs
        // on the reindex fast path; it must NEVER stamp SCHEMA_VERSION over a database that has not
        // already reached it. Only the atomic fresh-install (init) and the atomic offline migration
        // — each of which builds the v4 message-search layout in the SAME transaction as the stamp —
        // may promote to v4. If a future edit makes this method stamp SCHEMA_VERSION unconditionally,
        // a pre-v4 or partially-built index would be declared "current" without its trigram objects
        // and dead-lock every command at open. This test fails loudly if that invariant is broken.
        let dir = tempfile::tempdir().unwrap();
        for start in [0_i64, 1, 2, PARSER_SCHEMA_VERSION] {
            let path = dir.path().join(format!("v{start}.db"));
            let db = Db::open(&path).unwrap();
            db.conn.pragma_update(None, "user_version", start).unwrap();
            db.mark_schema_current().unwrap();
            assert!(
                db.schema_version().unwrap() < SCHEMA_VERSION,
                "start={start}: mark_schema_current must not promote a pre-v{SCHEMA_VERSION} index to current",
            );
        }
    }

    #[test]
    fn clear_all_is_atomic_and_rolls_back_on_failure() {
        // clear_all must be all-or-nothing: a failure partway through must not leave sessions whose
        // messages were already deleted (a stale message_count with zero messages). Force the final
        // delete to fail by dropping files_seen; the transaction must roll the earlier deletes back.
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute_batch(
                "insert into sessions (id, provider, provider_session_id, preview_text, source_path,
                     parse_version, discovery_source)
                     values ('s', 'claude', 's', '', '/s.jsonl', 'test', 'fixture');
                 insert into messages (session_id, provider, seq, role, kind, tool_name, content)
                     values ('s', 'claude', 0, 'user', 'message', null, 'keep me');",
            )
            .unwrap();
        db.conn.execute_batch("drop table files_seen;").unwrap();

        assert!(
            db.clear_all().is_err(),
            "clear_all should surface the failing delete"
        );
        let (sessions, messages): (i64, i64) = db
            .conn
            .query_row(
                "select (select count(*) from sessions), (select count(*) from messages)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (sessions, messages),
            (1, 1),
            "clear_all must roll back all deletes when one fails"
        );
    }

    #[test]
    fn direct_db_open_self_enforces_v4_layout_after_triggers_dropped() {
        // A direct Db::open (embedder path, bypassing SessionSearch's coordinator) on a v4 database
        // whose dual triggers were dropped must self-enforce the layout: after open, inserting a
        // message must maintain messages_trigram rather than silently falling back to word-only
        // indexing (which would make substring/fuzzy search return incomplete results).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let db = Db::open(&path).unwrap();
            db.conn
                .execute_batch(
                    "insert into sessions (id, provider, provider_session_id, preview_text,
                         source_path, parse_version, discovery_source)
                         values ('s', 'claude', 's', '', '/s.jsonl', 'test', 'fixture');
                     insert into messages (session_id, provider, seq, role, kind, tool_name, content)
                         values ('s', 'claude', 0, 'user', 'message', null, 'existing-row');
                     drop trigger messages_ai;
                     drop trigger messages_ad;
                     drop trigger messages_au;",
                )
                .unwrap();
        }

        // Reopen directly via Db::open — init() must heal the drifted v4 layout.
        let db = Db::open(&path).unwrap();
        assert!(
            crate::indexer::current_schema_layout_problem(&db.conn)
                .unwrap()
                .is_none(),
            "Db::open must self-enforce a consistent v4 layout"
        );
        // The restored dual trigger must index a NEW insert into messages_trigram. messages_trigram
        // is a detail=none FTS5 table (no phrase/MATCH queries), so observe maintenance through its
        // fts5vocab term table: inserting content with trigrams absent from the existing rows must
        // grow the distinct-term count.
        let terms_before: i64 = db
            .conn
            .query_row("select count(*) from messages_trigram_terms", [], |row| {
                row.get(0)
            })
            .unwrap();
        db.conn
            .execute_batch(
                "insert into messages (session_id, provider, seq, role, kind, tool_name, content)
                     values ('s', 'claude', 1, 'user', 'message', null, 'freshtrigramrow');",
            )
            .unwrap();
        let terms_after: i64 = db
            .conn
            .query_row("select count(*) from messages_trigram_terms", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(
            terms_after > terms_before,
            "new insert must be maintained in messages_trigram (terms {terms_before} -> {terms_after})"
        );
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
                    "drop trigger messages_ai;
                     drop trigger messages_ad;
                     drop trigger messages_au;
                     drop table messages_trigram_terms;
                     drop table messages_trigram_vocab;
                     drop table messages_trigram;
                     create virtual table messages_trigram using fts5(content, \
                     content='messages', content_rowid='id', tokenize='trigram', detail='none');",
                )
                .unwrap();
            crate::fts::install_released_message_word_index(&db.conn).unwrap();
            db.conn.pragma_update(None, "user_version", 3).unwrap();
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
                "econnreset",
                &MessageFilters {
                    match_mode: MessageSearchMode::Regex,
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
                needle,
                &MessageFilters {
                    match_mode: MessageSearchMode::Regex,
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
        enable_v3_custom_trigram_compatibility(&db);
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
                "zebracode",
                &MessageFilters {
                    match_mode: MessageSearchMode::Regex,
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
    fn storage_allocation_reports_freed_pages_without_scanning_content() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .execute_batch(
                "create table allocation_probe (payload blob not null);
                 insert into allocation_probe(payload) values(zeroblob(2097152));
                 delete from allocation_probe;",
            )
            .unwrap();

        let allocation = db.storage_allocation().unwrap();
        assert!(allocation.total_bytes >= allocation.reclaimable_bytes);
        assert!(
            allocation.reclaimable_bytes > 0,
            "deleted overflow pages must be reported as reclaimable"
        );
    }

    #[test]
    fn replacement_freelist_compaction_preserves_rows_fts_and_search_results() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let large_suffix = " allocation-padding".repeat(32_768);

        for index in 0..6 {
            let id = format!("claude:allocation-{index}");
            let content = format!("original-token-{index}{large_suffix}");
            let mut parsed = parsed_with_messages(&id, &[&content]);
            parsed.session.provider_session_id = format!("allocation-{index}");
            parsed.session.source_path = format!("/allocation/{index}.jsonl");
            db.replace_session(&parsed, index, index).unwrap();
        }
        assert_eq!(db.message_count().unwrap(), 6);
        assert_eq!(db.messages_fts_count().unwrap(), 6);

        for index in 0..3 {
            let id = format!("claude:allocation-{index}");
            let content = format!("replacement-token-{index}");
            let mut parsed = parsed_with_messages(&id, &[&content]);
            parsed.session.provider_session_id = format!("allocation-{index}");
            parsed.session.source_path = format!("/allocation/{index}.jsonl");
            db.replace_session(&parsed, 10 + index, 10 + index).unwrap();
        }
        db.optimize_fts().unwrap();

        let before = db.storage_allocation().unwrap();
        assert!(before.reclaimable_bytes > 0);
        assert_eq!(db.message_count().unwrap(), 6);
        assert_eq!(db.messages_fts_count().unwrap(), 6);
        for index in 0..3 {
            assert_eq!(
                db.search_messages(
                    &format!("replacement-token-{index}"),
                    &MessageFilters::default(),
                )
                .unwrap()
                .len(),
                1
            );
        }

        db.vacuum().unwrap();
        db.checkpoint_truncate().unwrap();

        let after = db.storage_allocation().unwrap();
        assert_eq!(after.reclaimable_bytes, 0);
        assert!(after.total_bytes < before.total_bytes);
        assert_eq!(db.message_count().unwrap(), 6);
        assert_eq!(db.messages_fts_count().unwrap(), 6);
        for index in 0..3 {
            assert_eq!(
                db.search_messages(
                    &format!("replacement-token-{index}"),
                    &MessageFilters::default(),
                )
                .unwrap()
                .len(),
                1
            );
        }
    }

    #[test]
    fn session_replacement_rolls_back_every_index_when_sqlite_is_full() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let mut original = parsed_with_messages("claude:s1", &["original durable token"]);
        original.session.title = Some("original title".into());
        db.replace_session(&original, 1, 1).unwrap();
        db.checkpoint_truncate().unwrap();

        let original_max_pages: i64 = db
            .conn
            .query_row("pragma max_page_count", [], |row| row.get(0))
            .unwrap();
        let current_pages: i64 = db
            .conn
            .query_row("pragma page_count", [], |row| row.get(0))
            .unwrap();
        let free_pages: i64 = db
            .conn
            .query_row("pragma freelist_count", [], |row| row.get(0))
            .unwrap();
        let page_size: i64 = db
            .conn
            .query_row("pragma page_size", [], |row| row.get(0))
            .unwrap();
        db.conn
            .pragma_update(None, "max_page_count", current_pages)
            .unwrap();

        let mut replacement = original.clone();
        replacement.session.title = Some("replacement title".into());
        const REQUIRED_GROWTH_PAGES: i64 = 2;
        const REPLACEMENT_TOKEN: &str = "replacement-full-token ";
        let required_bytes = (free_pages + REQUIRED_GROWTH_PAGES) * page_size;
        let repeats = required_bytes as usize / REPLACEMENT_TOKEN.len() + 1;
        replacement.transcript_text = REPLACEMENT_TOKEN.repeat(repeats);
        replacement.messages[0].content = replacement.transcript_text.clone();
        let error = db.replace_session(&replacement, 2, 2).unwrap_err();

        db.conn
            .pragma_update(None, "max_page_count", original_max_pages)
            .unwrap();
        assert!(error.chain().any(|cause| {
            cause
                .downcast_ref::<rusqlite::Error>()
                .and_then(rusqlite::Error::sqlite_error_code)
                == Some(ErrorCode::DiskFull)
        }));

        let persisted = db.resolve_session("claude:s1").unwrap();
        assert_eq!(persisted.session.title.as_deref(), Some("original title"));
        assert_eq!(persisted.transcript_text, "original durable token");
        assert_eq!(
            db.search_messages("original durable token", &MessageFilters::default())
                .unwrap()
                .len(),
            1
        );
        assert!(db
            .search_messages("replacement-full-token", &MessageFilters::default())
            .unwrap()
            .is_empty());
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
        crate::trigram_index::build(&db.conn, &db.runtime).unwrap();
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
        let count = |role: Option<Role>, session_id: Option<&str>| -> usize {
            db.search_messages(
                "needle_xyz",
                &MessageFilters {
                    match_mode: MessageSearchMode::Regex,
                    role,
                    session_id: session_id.map(str::to_string),
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
        let providers_for = |query: &str, filters: MessageFilters| -> Vec<String> {
            let mut got: Vec<String> = db
                .search_messages(query, &filters)
                .unwrap()
                .into_iter()
                .map(|h| h.provider.as_str().to_string())
                .collect();
            got.sort();
            got
        };
        let all = providers_for(
            "ECONNRESET",
            MessageFilters {
                match_mode: MessageSearchMode::Regex,
                ..Default::default()
            },
        );
        assert_eq!(
            all.len(),
            6,
            "every provider's ECONNRESET found regardless of content shape"
        );
        let claude = providers_for(
            "ECONNRESET",
            MessageFilters {
                match_mode: MessageSearchMode::Regex,
                provider: Some(Provider::Claude),
                ..Default::default()
            },
        );
        assert_eq!(
            claude,
            vec!["claude"],
            "provider scope restricts to the claude message"
        );
        let claude_desktop = providers_for(
            "ECONNRESET",
            MessageFilters {
                match_mode: MessageSearchMode::Regex,
                provider: Some(Provider::ClaudeDesktop),
                ..Default::default()
            },
        );
        assert_eq!(
            claude_desktop,
            vec!["claude-desktop"],
            "provider scope restricts to the claude-desktop message"
        );
        // The correction phrase 'you forgot' appears only in the cursor message.
        let forgot = providers_for(
            r"\byou forgot\b",
            MessageFilters {
                match_mode: MessageSearchMode::Regex,
                ..Default::default()
            },
        );
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
                (
                    "user",
                    "<turn_aborted>\nThe user interrupted the previous turn on purpose.",
                ), // purge
                ("user", "fix the failing login test before release"),         // KEEP (prompt)
                ("user", "what does <local-command-stdout> mean in the logs"), // KEEP (not leading)
                (
                    "user",
                    "what does <turn_aborted> mean in a Codex transcript",
                ), // KEEP (not leading)
                (
                    "assistant",
                    "<local-command-stdout>tool output</local-command-stdout>",
                ), // KEEP (not user)
            ],
        );
        let before = db.message_count().unwrap();
        let purged = db.purge_injected_messages().unwrap();
        assert_eq!(purged, 4, "the four leading-marker USER rows are deleted");
        assert_eq!(db.message_count().unwrap(), before - 4);
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
            3,
            "exactly the three legitimate user messages remain"
        );
    }

    #[test]
    fn regex_search_lazily_builds_custom_trigram_base_and_is_correct() {
        // The --regex path must call ensure_trigram_base() so an UNBUILT custom index (e.g. right
        // after a fresh reindex that does no trigram work) does not silently drop matches: the
        // search builds the base on first use and returns the correct row.
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        enable_v3_custom_trigram_compatibility(&db);
        seed_messages(&db, &[("user", "socket failed with ECONNRESET) today")]);
        // Precondition: the custom base index has not been built yet (lazy by construction).
        assert_eq!(
            crate::trigram_index::base_max_id(&db.conn).unwrap(),
            0,
            "precondition: custom trigram base not built before first regex use"
        );
        let filters = MessageFilters {
            match_mode: MessageSearchMode::Regex,
            ..Default::default()
        };
        let hits = db.search_messages("ECONNRESET", &filters).unwrap();
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
            match_mode: MessageSearchMode::Regex,
            ..Default::default()
        };
        let hits = reader.search_messages("ECONNRESET", &filters).unwrap();
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
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;
        // Unset reporter: the library builds silently (no panic, no I/O), returns base_max.
        let dir = tempfile::tempdir().unwrap();
        let silent = Db::open(&dir.path().join("a.db")).unwrap();
        enable_v3_custom_trigram_compatibility(&silent);
        seed_messages(&silent, &[("user", "econnreset here")]);
        assert_eq!(silent.ensure_trigram_base().unwrap(), 1);

        // Injected reporter: fires exactly once, when (and only when) a build happens.
        let dir2 = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir2.path().join("b.db")).unwrap();
        enable_v3_custom_trigram_compatibility(&db);
        let calls = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&calls);
        db.set_progress_reporter(move |_msg| {
            counter.fetch_add(1, Ordering::Relaxed);
        });
        seed_messages(&db, &[("user", "econnreset here")]);
        assert_eq!(db.ensure_trigram_base().unwrap(), 1);
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "reporter fires once for the one-time build"
        );
        db.ensure_trigram_base().unwrap();
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "no report when the base is already current"
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
                match_mode: MessageSearchMode::Regex,
                ..Default::default()
            };
            let mut got: Vec<String> = db
                .search_messages(pattern, &filters)
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
                match_mode: MessageSearchMode::Regex,
                provider,
                ..Default::default()
            };
            db.search_messages("ECONNRESET", &filters).unwrap().len()
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
    fn find_corrections_honors_provider_scope() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        for provider in [Provider::Claude, Provider::Codex] {
            let session_id = format!("{}:s1", provider.as_str());
            db.conn
                .execute(
                    "insert into sessions(id, provider, provider_session_id, preview_text, \
                       source_path, parse_version, discovery_source) \
                     values(?1, ?2, 's1', '', '/x', 'v1', 'jsonl')",
                    params![session_id, provider.as_str()],
                )
                .unwrap();
            db.conn
                .execute(
                    "insert into messages(session_id, provider, seq, role, content) \
                     values(?1, ?2, 0, 'user', 'you forgot the provider filter')",
                    params![session_id, provider.as_str()],
                )
                .unwrap();
        }
        let patterns = vec![(
            "skip_step".to_string(),
            regex::Regex::new(r"(?i)\byou forgot\b").unwrap(),
        )];

        let hits = db
            .find_corrections(
                &patterns,
                &MessageFilters {
                    provider: Some(Provider::Claude),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].provider, Provider::Claude);
        assert_eq!(hits[0].session_id, "claude:s1");
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
                match_mode: MessageSearchMode::Regex,
                role,
                ..Default::default()
            };
            db.search_messages("ECONNRESET", &filters)
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
        db.prefilter_min_corpus = 1;
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
            match_mode: MessageSearchMode::Regex,
            ..Default::default()
        };
        let ex = db
            .explain_message_search("(?i)zebra.ode", &selective)
            .unwrap();
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
            match_mode: MessageSearchMode::Regex,
            ..Default::default()
        };
        let ex2 = db.explain_message_search("a.b", &no_anchor).unwrap();
        assert!(ex2.prefilter.is_none(), "no >=3-char anchor → no prefilter");
        assert!(
            ex2.candidates.is_none(),
            "no prefilter → no candidate count"
        );
        assert_eq!(ex2.corpus, 4);
    }

    #[test]
    fn schema_v4_search_explain_uses_sqlite_prefilter_without_legacy_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.upsert_session(
            &parsed_with_messages("claude:s1", &["zebracode appears here once"]),
            1,
            100,
        )
        .unwrap();

        let filters = MessageFilters {
            role: Some(Role::User),
            match_mode: MessageSearchMode::Regex,
            ..Default::default()
        };
        let (hits, explain) = db
            .search_messages_with_explain("zebracode", &filters, true)
            .unwrap();
        let explain = explain.expect("explain requested");

        assert_eq!(hits.len(), 1);
        assert!(explain.prefilter.is_some(), "anchor is available");
        assert_eq!(explain.candidates, Some(1));
        assert!(explain.prefilter_skipped.is_none());
    }

    #[test]
    fn internal_message_order_is_typed_scoped_and_reversible() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.upsert_session(
            &parsed_with_messages("claude:ordered", &["zero", "one", "two", "three", "four"]),
            1,
            1,
        )
        .unwrap();

        let newest = db
            .search_messages_ordered(
                "",
                &MessageFilters {
                    session_id: Some("claude:ordered".to_string()),
                    limit: 2,
                    ..Default::default()
                },
                MessageOrder::NewestFirst,
            )
            .unwrap();
        assert_eq!(
            newest.iter().map(|hit| hit.seq).collect::<Vec<_>>(),
            vec![4, 3]
        );

        let oldest = db
            .search_messages(
                "",
                &MessageFilters {
                    session_id: Some("claude:ordered".to_string()),
                    limit: 2,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            oldest.iter().map(|hit| hit.seq).collect::<Vec<_>>(),
            vec![0, 1]
        );

        let error = db
            .search_messages_ordered("", &MessageFilters::default(), MessageOrder::NewestFirst)
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("newest-first message order requires session_id"));
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
                kind: crate::models::MessageKind::Conversation,
                tool_call_id: None,
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
                // No spawn concept on this path: subagent runs are either excluded from
                // discovery or unmarked by this provider. See models.rs SessionRecord.
                parent_session_id: None,
                agent_label: None,
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
                "delta",
                &MessageFilters {
                    match_mode: MessageSearchMode::Regex,
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
        let schema_before = schema_fingerprint(&db.conn);
        let row_vocab_exists: bool = db
            .conn
            .query_row(
                "select exists(
                     select 1 from sqlite_schema
                      where type = 'table' and name = 'messages_trigram_terms'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            row_vocab_exists,
            "v4 must provide the row-level trigram vocabulary used for bounded output"
        );
        let tri = db.vocabulary(true, 0).unwrap();
        assert!(
            tri.iter().any(|(t, _, _)| t == "alp"),
            "trigram vocab has 3-gram terms"
        );
        assert_eq!(
            schema_fingerprint(&db.conn),
            schema_before,
            "reading v4 trigram vocabulary must not mutate any schema object"
        );
    }

    #[test]
    fn schema_v4_rejects_custom_trigram_maintenance_without_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let schema_before = schema_fingerprint(&db.conn);

        let error = db.ensure_trigram_base().unwrap_err().to_string();

        assert!(error.contains("unavailable on schema v4"), "{error}");
        assert_eq!(schema_fingerprint(&db.conn), schema_before);
    }

    #[test]
    fn ensure_trigram_base_self_heals_corrupt_metadata_instead_of_failing_every_search() {
        // trigram_postings/trigram_meta are entirely derived from `messages`, so corruption
        // there must self-heal (rebuild) rather than turn every subsequent regex/substring
        // search into a hard failure requiring a manual `aise reindex --full`.
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        enable_v3_custom_trigram_compatibility(&db);
        seed_messages(&db, &[("user", "socket failed with ECONNRESET today")]);

        // Corrupt trigram_meta the same way trigram_index::tests does: recreate it with an
        // incompatible shape (no `value` column) so the metadata read fails for a real reason.
        db.conn
            .execute_batch(
                "drop table trigram_meta; create table trigram_meta (key text primary key);",
            )
            .unwrap();

        let base_max = db
            .ensure_trigram_base()
            .expect("corruption must self-heal, not propagate an error");

        assert_eq!(
            base_max, 1,
            "self-heal rebuilds the base from the message table"
        );
        assert_eq!(
            crate::trigram_index::base_max_id(&db.conn).unwrap(),
            1,
            "trigram_meta is valid and queryable again after self-heal"
        );
        let groups = crate::trigram::trigram_prefilter_groups("ECONNRESET").unwrap();
        let cands = crate::trigram_index::candidates(&db.conn, &groups).unwrap();
        assert!(
            cands.contains(&1),
            "the rebuilt index actually finds the seeded message, not just an empty shell"
        );
    }

    #[test]
    fn ensure_trigram_base_does_not_repair_metadata_when_maintenance_is_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("index.db")).unwrap();
        enable_v3_custom_trigram_compatibility(&db);
        seed_messages(&db, &[("user", "socket failed with ECONNRESET today")]);
        db.conn
            .execute_batch(
                "drop table trigram_meta; create table trigram_meta (key text primary key);",
            )
            .unwrap();
        db.set_implicit_index_maintenance(false);

        let error = db.ensure_trigram_base().unwrap_err().to_string();
        assert!(
            error.contains("automatic maintenance is disabled"),
            "{error}"
        );
        assert!(
            !crate::trigram_index::schema_is_compatible(&db.conn).unwrap(),
            "a maintenance-disabled read must not replace the malformed table"
        );
    }

    #[test]
    fn read_only_open_does_not_repair_incompatible_trigram_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let db = Db::open(&path).unwrap();
        enable_v3_custom_trigram_compatibility(&db);
        seed_messages(&db, &[("user", "socket failed with ECONNRESET today")]);
        db.conn
            .execute_batch(
                "drop table trigram_meta; create table trigram_meta (key text primary key);",
            )
            .unwrap();
        let schema_before = schema_fingerprint(&db.conn);
        drop(db);

        let read_only = Db::open_existing_read_only_with_threads(
            &path,
            TEST_BUSY_TIMEOUT_MS,
            NonZeroUsize::MIN,
        )
        .unwrap();
        let error = read_only.ensure_trigram_base().unwrap_err().to_string();

        assert!(
            error.contains("automatic maintenance is disabled"),
            "{error}"
        );
        assert_eq!(
            schema_fingerprint(&read_only.conn),
            schema_before,
            "SQLite read-only mode must leave the incompatible derived schema unchanged"
        );
    }

    #[test]
    fn ensure_trigram_base_does_not_create_missing_tables_when_maintenance_is_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("index.db")).unwrap();
        enable_v3_custom_trigram_compatibility(&db);
        db.conn
            .execute_batch("drop table trigram_postings; drop table trigram_meta;")
            .unwrap();
        db.set_implicit_index_maintenance(false);

        let error = db.ensure_trigram_base().unwrap_err().to_string();
        assert!(
            error.contains("automatic maintenance is disabled"),
            "{error}"
        );
        let derived_tables: i64 = db
            .conn
            .query_row(
                "select count(*) from sqlite_master where name in ('trigram_postings', 'trigram_meta')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            derived_tables, 0,
            "a read-only probe must not create schema"
        );
    }

    #[test]
    fn ensure_trigram_base_rolls_back_schema_repair_when_rebuild_fails() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        enable_v3_custom_trigram_compatibility(&db);
        db.conn
            .execute_batch(
                "drop table trigram_meta;
                 create table trigram_meta (key text primary key);
                 drop table messages;",
            )
            .unwrap();

        let error = db.ensure_trigram_base().unwrap_err().to_string();
        assert!(error.contains("no such table: messages"), "{error}");
        assert!(
            !crate::trigram_index::schema_is_compatible(&db.conn).unwrap(),
            "the failed transaction must restore the original incompatible schema"
        );
    }

    #[test]
    fn ensure_trigram_base_busy_repair_leaves_incompatible_tables_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let writer = Db::open(&path).unwrap();
        enable_v3_custom_trigram_compatibility(&writer);
        seed_messages(&writer, &[("user", "socket failed with ECONNRESET today")]);
        writer
            .conn
            .execute_batch(
                "drop table trigram_meta; create table trigram_meta (key text primary key);",
            )
            .unwrap();
        let schema_before = schema_fingerprint(&writer.conn);

        let contender = Db::open_with_busy_timeout(&path, TEST_NO_WAIT_BUSY_TIMEOUT_MS).unwrap();
        writer.conn.execute_batch("begin immediate").unwrap();
        let error = contender
            .ensure_trigram_base()
            .expect_err("a competing writer must prevent derived-table repair");
        writer.conn.execute_batch("rollback").unwrap();

        assert!(Db::is_sqlite_busy_error(&error), "{error:#}");
        assert!(
            !crate::trigram_index::schema_is_compatible(&contender.conn).unwrap(),
            "a failed repair attempt must not replace either derived table"
        );
        assert_eq!(
            schema_fingerprint(&contender.conn),
            schema_before,
            "the failed repair must preserve the entire preexisting schema"
        );
    }

    #[test]
    fn regex_prefilter_reaches_rows_by_primary_key_not_full_scan() {
        // #207: a prefilterable regex search resolves candidates through the custom trigram index
        // (staged into the `_trigram_cand` temp table) and reaches message rows by primary key —
        // never a full table scan of `messages`.
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        enable_v3_custom_trigram_compatibility(&db);
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
        db.checkpoint_truncate().unwrap();
        // Idempotent: a second checkpoint on a quiescent DB is fine.
        db.checkpoint_truncate().unwrap();
        let hits = db
            .search_messages(
                "ECONNRESET",
                &MessageFilters {
                    match_mode: MessageSearchMode::Regex,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(hits.len(), 1, "data intact and searchable after checkpoint");
    }

    #[test]
    fn schema_state_classification_is_total_and_preserves_readable_older_policy() {
        assert_eq!(
            SchemaState::from_version(SCHEMA_VERSION),
            SchemaState::Current
        );
        assert_eq!(
            SchemaState::from_version(SCHEMA_VERSION - 1),
            SchemaState::Older {
                current: SCHEMA_VERSION - 1,
                required: SCHEMA_VERSION,
            }
        );
        assert_eq!(
            SchemaState::from_version(SCHEMA_VERSION + 1),
            SchemaState::Newer {
                current: SCHEMA_VERSION + 1,
                supported: SCHEMA_VERSION,
            }
        );

        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.conn
            .pragma_update(None, "user_version", MIN_READABLE_SCHEMA_VERSION)
            .unwrap();
        assert!(db.needs_backfill().unwrap());
        assert!(db.schema_is_readable().unwrap());
    }

    #[test]
    fn corpus_threshold_probe_stops_at_the_decision_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        seed_messages(
            &db,
            &[
                ("user", "one"),
                ("user", "two"),
                ("user", "three"),
                ("user", "four"),
                ("assistant", "five"),
            ],
        );
        let filters = MessageFilters {
            role: Some(Role::User),
            ..Default::default()
        };

        assert!(db.filtered_corpus_reaches(&filters, 3).unwrap());
        assert!(db.filtered_corpus_reaches(&filters, 4).unwrap());
        assert!(!db.filtered_corpus_reaches(&filters, 5).unwrap());
        assert_eq!(db.filtered_corpus_count(&filters).unwrap(), 4);
    }
}
