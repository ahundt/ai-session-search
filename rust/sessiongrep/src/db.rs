use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use rusqlite::{Connection, OptionalExtension, params};

use crate::models::{
    CorrectionMatch, EditOp, FileCrossRef, FileEdit, FileEditSummary, FileQuery, MessageFilters,
    MessageHit, ParsedSession, PlanningCount, Provider, Role, SearchFilters, SearchHit,
    SessionRecord, SessionWithTranscript,
};
use crate::util::snippet_from_match;

/// On-disk index generation (NOT the package version). Bump whenever a reindex must
/// backfill newly added columns/tables that incremental indexing would otherwise skip,
/// or when a parse-logic change makes existing rows stale and they must be re-parsed.
/// [`Db::needs_backfill`] compares this against SQLite's `pragma user_version` to
/// trigger a one-time full reindex after an upgrade, without re-parsing on every run.
///   1: messages table (Phase 1) + file_edits table (Phase 5)
///   2: slash commands re-classified from the `<command-name>` tag (Phase 5 follow-up)
///   3: claude tool results (role:user) re-classified as `tool` (clean user analytics)
///   4: codex injected context (agent-history / AGENTS.md) re-classified as `tool`
///   5: claude compaction summaries (isCompactSummary) re-classified as `compaction`
///   6: all claude isMeta messages (hook feedback / notices) dropped from the index
///   7: file edits carry the `replace_all` flag; `edits_json` reshaped from `[old,new]`
///      pairs to `{old,new,replace_all}` objects (old rows must be re-parsed)
///   8: session dates backfilled from file mtime — previously-undated rows must be
///      re-indexed so created_at/updated_at/last_message_at are always populated
///   9: tool output indexed cross-provider — pi `toolResult` and codex
///      `function_call_output` now become `tool` messages, and `tool_name` is populated
///      on claude/pi/codex tool messages (old rows must be re-parsed to gain them)
pub const SCHEMA_VERSION: i64 = 9;

/// Minimum number of FTS candidate sessions to retrieve before fuzzy re-ranking. The
/// candidate set is re-scored in [`Db::search`], so it must be wider than the final
/// `--limit` (a strong fuzzy match can rank low under raw FTS `rank`), and it must never
/// collapse to 0 when a caller requests "unlimited" (limit == 0).
const FTS_CANDIDATE_FLOOR: usize = 200;

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let conn = Connection::open(path)?;
        let db = Self { conn };
        db.init()?;
        Ok(db)
    }

    fn init(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            pragma journal_mode = wal;
            pragma foreign_keys = on;
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
            create index if not exists idx_messages_session on messages(session_id);
            create index if not exists idx_messages_role on messages(role);
            create index if not exists idx_messages_ts on messages(ts);
            -- Composite indexes that let the planner satisfy the hot ORDER BYs from the
            -- index instead of a temp B-tree sort: (role, ts) for corrections
            -- (where role=? order by ts), (session_id, seq) for message search/get
            -- (order by session_id, seq).
            create index if not exists idx_messages_role_ts on messages(role, ts);
            create index if not exists idx_messages_session_seq on messages(session_id, seq);
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
            self.conn
                .execute_batch("drop table sessions_fts")?;
        }
        self.conn.execute_batch(
            "create virtual table if not exists sessions_fts using fts5(
                title, summary, preview_text, transcript_text
            )",
        )?;
        // External-content FTS over message bodies. The insert/delete/update triggers are
        // the full canonical FTS5 external-content set, so every mutation path stays in
        // sync — the delete+reinsert reindex path (upsert_session) and any future in-place
        // `update messages set content=...`. NOTE: `search_messages` currently matches with
        // a case-insensitive substring scan (`instr`), so this index is maintained for
        // correctness/future token search rather than read by the search path today.
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
        // Auto-populate FTS if sessions exist but FTS is empty (e.g. after schema upgrade)
        let sessions_count: i64 =
            self.conn
                .query_row("select count(*) from sessions", [], |row| row.get(0))?;
        let fts_count: i64 = self
            .conn
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
        // Re-sync per-message rows (idempotent: a re-parsed session replaces its rows).
        // FTS stays in sync via the messages_ai/messages_ad triggers (see init()).
        tx.execute(
            "delete from messages where session_id = ?1",
            params![session.id],
        )?;
        {
            let mut stmt = tx.prepare(
                "insert into messages
                    (session_id, provider, seq, role, ts, tool_name, is_compaction, content)
                 values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for message in &parsed.messages {
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

    /// Message-level search. A literal `query` is matched against the `messages_fts` index
    /// as a phrase with a prefix on the final token (indexed; "handle" also matches
    /// "handler"); a punctuation-only query with no FTS tokens (e.g. "->") falls back to a
    /// substring scan. When `filters.regex` is set it is applied as a Rust regex
    /// (linear-time) over the rows matching the structured filters. `limit == 0` = unlimited.
    pub fn search_messages(&self, query: &str, filters: &MessageFilters) -> Result<Vec<MessageHit>> {
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
            "select m.session_id, m.provider, m.seq, m.role, m.ts, m.content from {from} where 1 = 1"
        );
        let mut args: Vec<Value> = Vec::new();
        if let Some(fts) = &fts_query {
            sql.push_str(" and messages_fts match ?");
            args.push(Value::Text(fts.clone()));
        }
        if let Some(role) = filters.role {
            sql.push_str(" and m.role = ?");
            args.push(Value::Text(role.as_str().to_string()));
        }
        if let Some(session) = &filters.session {
            sql.push_str(" and m.session_id like ?");
            args.push(Value::Text(format!("%{session}%")));
        }
        push_ts_window(&mut sql, &mut args, "m.ts", filters.since, filters.until);
        if filters.no_compaction {
            sql.push_str(" and m.is_compaction = 0");
        }
        // Substring scan fallback: only when not using FTS and not --regex (e.g. a
        // punctuation-only query the tokenizer can't index).
        if fts_query.is_none() && filters.regex.is_none() && !query.is_empty() {
            sql.push_str(" and instr(lower(m.content), lower(?)) > 0");
            args.push(Value::Text(query.to_string()));
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
        let raw = stmt.query_map(rusqlite::params_from_iter(args.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;

        let mut hits = Vec::new();
        for row in raw {
            let (session_id, provider, seq, role, ts, content) = row?;
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
                content,
            });
            if filters.limit > 0 && hits.len() >= filters.limit {
                break;
            }
        }
        Ok(hits)
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
            "select session_id, provider, seq, role, ts, content from messages
             where session_id = ?1 and seq between ?2 and ?3 order by seq",
        )?;
        let rows = stmt.query_map(
            params![session_id, seq - before, seq + after],
            |row| {
                Ok(MessageHit {
                    session_id: row.get(0)?,
                    provider: row
                        .get::<_, String>(1)?
                        .parse()
                        .unwrap_or(Provider::Claude),
                    seq: row.get(2)?,
                    role: row.get::<_, String>(3)?.parse().unwrap_or(Role::User),
                    ts: row
                        .get::<_, Option<String>>(4)?
                        .and_then(|value| {
                            chrono::DateTime::parse_from_rfc3339(&value)
                                .ok()
                                .map(|dt| dt.with_timezone(&Utc))
                        }),
                    content: row.get(5)?,
                })
            },
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    /// Scan user messages and tag each against the ordered `patterns` (first match wins,
    /// so `other` must be last). Streams rows; only matches are materialized.
    /// `filters.limit == 0` means unlimited.
    pub fn find_corrections(
        &self,
        patterns: &[(String, regex::Regex)],
        filters: &MessageFilters,
    ) -> Result<Vec<CorrectionMatch>> {
        use rusqlite::types::Value;

        let mut sql =
            String::from("select session_id, provider, ts, content from messages where role = 'user'");
        let mut args: Vec<Value> = Vec::new();
        if let Some(session) = &filters.session {
            sql.push_str(" and session_id like ?");
            args.push(Value::Text(format!("%{session}%")));
        }
        push_ts_window(&mut sql, &mut args, "ts", filters.since, filters.until);
        sql.push_str(" order by ts desc");

        let mut stmt = self.conn.prepare(&sql)?;
        let raw = stmt.query_map(rusqlite::params_from_iter(args.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in raw {
            let (session_id, provider, ts, content) = row?;
            let matched = patterns
                .iter()
                .find_map(|(cat, re)| re.find(&content).map(|m| (cat.clone(), m.as_str().to_string())));
            if let Some((category, matched_pattern)) = matched {
                out.push(CorrectionMatch {
                    session_id,
                    provider: provider.parse().unwrap_or(Provider::Claude),
                    ts: ts.and_then(|value| {
                        chrono::DateTime::parse_from_rfc3339(&value)
                            .ok()
                            .map(|dt| dt.with_timezone(&Utc))
                    }),
                    category,
                    matched_pattern,
                    content,
                });
                if filters.limit > 0 && out.len() >= filters.limit {
                    break;
                }
            }
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
        counts.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.command.cmp(&b.command)));
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
            let (session_id, provider, seq, ts, tool, file_path, file_name, new_content, edits_json) =
                row?;
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
    ) -> Result<Vec<SearchHit>> {
        // Try FTS first for efficient candidate retrieval
        let fts_ids = self.fts_candidate_ids(query, filters.limit * 5)?;
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
                        "title" => 600,
                        "summary" => 450,
                        "cwd" | "repo" => 350,
                        "preview" => 250,
                        _ => 100,
                    };
                }
                let mut tokens_hit = 0usize;
                for token in &tokens {
                    if !token.is_empty() && lowered.contains(token) {
                        source_score += 40;
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
                score += 150;
            }

            if let Some(updated_at) = record.session.updated_at {
                let age_days = (Utc::now() - updated_at).num_days().clamp(0, 90);
                score += (90 - age_days) * 2;
            }
            if let (Some(current_repo), Some(repo_root)) =
                (current_repo, record.session.repo_root.as_deref())
            {
                if current_repo == repo_root {
                    score += 200;
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
    fn fts_candidate_ids(&self, query: &str, limit: usize) -> Result<Vec<String>> {
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
        let cap = limit.max(FTS_CANDIDATE_FLOOR);
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
            0 => Err(anyhow!("no session matches '{value}'")),
            1 => Ok(matches.remove(0)),
            _ => Err(anyhow!("session prefix '{value}' is ambiguous")),
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
        assert_eq!(glob_clause("src/*.rs"), ("file_path", "%src/%.rs".to_string()));
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
            .planning_usage(&MessageFilters::default(), &[regex::Regex::new("plannew").unwrap()])
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
                &MessageFilters { regex: Some("h.ndler".into()), ..Default::default() },
            )
            .unwrap();
        assert_eq!(re.into_iter().map(|h| h.seq).collect::<Vec<_>>(), vec![1]);
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
        let until = Utc.with_ymd_and_hms(2026, 1, 15, 23, 59, 59).single().unwrap();
        let hits = db
            .search_messages("", &MessageFilters { until: Some(until), ..Default::default() })
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
        let until = Utc.with_ymd_and_hms(2026, 12, 31, 23, 59, 59).single().unwrap();
        let hits = db
            .search_messages(
                "",
                &MessageFilters { since: Some(since), until: Some(until), ..Default::default() },
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
        assert!(db.fts_candidate_ids("***", 50).unwrap().is_empty());
        assert!(db.fts_candidate_ids("\"", 50).unwrap().is_empty());
        assert!(db.fts_candidate_ids("   ", 50).unwrap().is_empty());
        // A real token mixed with punctuation must still run without error.
        assert!(db.fts_candidate_ids("--- hello", 50).is_ok());
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
        let ids = db.fts_candidate_ids("alpha", 0).unwrap();
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
        assert_eq!(count("alphaunique"), 1, "first index makes the title searchable");
        upsert_fts("betaunique");
        assert_eq!(count("betaunique"), 1, "re-index makes the new title searchable");
        assert_eq!(
            count("alphaunique"),
            0,
            "re-index must drop the old title's terms (no FTS ghost postings)"
        );
    }

    #[test]
    fn schema_backfill_flag_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        // Fresh DB: user_version defaults to 0 (< SCHEMA_VERSION) → a backfill is due.
        assert!(db.needs_backfill().unwrap());
        db.mark_schema_current().unwrap();
        assert!(!db.needs_backfill().unwrap(), "stamping clears the flag");
    }
}
