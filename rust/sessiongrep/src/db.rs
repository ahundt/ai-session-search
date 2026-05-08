use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use rusqlite::{Connection, OptionalExtension, params};

use crate::models::{
    ParsedSession, Provider, SearchFilters, SearchHit, SessionRecord, SessionWithTranscript,
};
use crate::util::snippet_from_match;

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

    pub fn clear_all(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            delete from sessions_fts;
            delete from transcripts;
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
        tx.commit()?;
        Ok(())
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

    /// Query FTS5 index for candidate session IDs matching the query.
    fn fts_candidate_ids(&self, query: &str, limit: usize) -> Result<Vec<String>> {
        // Escape FTS5 special characters and build a simple token query
        let fts_query: String = query
            .split_whitespace()
            .map(|token| {
                let escaped = token.replace('"', "\"\"");
                format!("\"{escaped}\"")
            })
            .collect::<Vec<_>>()
            .join(" OR ");
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "select s.id
             from sessions_fts f
             join sessions s on s.rowid = f.rowid
             where sessions_fts match ?1
             order by rank
             limit ?2",
        )?;
        let rows = stmt.query_map(params![fts_query, limit as i64], |row| {
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
            sql.push_str(" and coalesce(s.updated_at, s.created_at, '') >= ? ");
            params_vec.push(since.to_rfc3339());
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
                t.transcript_text
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
            sql.push_str(" and coalesce(s.updated_at, s.created_at, '') >= ? ");
            params_vec.push(since.to_rfc3339());
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
