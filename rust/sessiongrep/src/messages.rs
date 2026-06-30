//! `messages` command group: search, read, and timeline per-message rows.
//!
//! Thin command glue over [`crate::db::Db`] + [`crate::render`], so `cli.rs` stays a
//! dispatcher. `--limit 0` means unlimited (avoids the session `--limit 25` trap).
//! Date filtering (`--since/--until/--when`) is the shared [`crate::dates::DateRange`],
//! which accepts EDTF / ISO / duration / natural language.

use std::collections::{BTreeMap, HashSet};
use std::io::{self, Write};

use anyhow::{bail, Result};
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::dates::DateRange;
use crate::db::Db;
use crate::models::{MessageFilters, MessageHit, Provider, Role};
use crate::refs::{extract_refs_from_text, ref_summary, MessageRef};
use crate::render::{render, OutputFormat, Row};
use crate::util::truncate_for_display;

/// Max characters of content shown in tabular formats (json/jsonl keep full content).
const TABLE_CONTENT_CHARS: usize = 120;

impl Row for MessageHit {
    fn headers() -> &'static [&'static str] {
        &[
            "session", "provider", "seq", "role", "tool", "ts", "content",
        ]
    }

    fn cells(&self) -> Vec<String> {
        vec![
            self.session_id.clone(),
            self.provider.as_str().to_string(),
            self.seq.to_string(),
            self.role.as_str().to_string(),
            self.tool_name.clone().unwrap_or_default(),
            self.ts.map(|ts| ts.to_rfc3339()).unwrap_or_default(),
            truncate_for_display(&self.content, TABLE_CONTENT_CHARS),
        ]
    }
}

/// A message rendered as part of a `--context` window: like [`MessageHit`] plus a
/// `match` marker (`*` for the matched row, blank for surrounding context).
#[derive(Debug, Clone, Serialize)]
struct ContextRow {
    session_id: String,
    seq: i64,
    role: String,
    ts: Option<String>,
    is_match: bool,
    content: String,
}

impl Row for ContextRow {
    fn headers() -> &'static [&'static str] {
        &["session", "seq", "role", "match", "content"]
    }
    fn cells(&self) -> Vec<String> {
        vec![
            self.session_id.clone(),
            self.seq.to_string(),
            self.role.clone(),
            if self.is_match { "*" } else { "" }.to_string(),
            truncate_for_display(&self.content, TABLE_CONTENT_CHARS),
        ]
    }
}

#[derive(Serialize)]
struct MessageHitWithRefs {
    #[serde(flatten)]
    hit: MessageHit,
    ref_summary: String,
    refs: Vec<MessageRef>,
}

impl Row for MessageHitWithRefs {
    fn headers() -> &'static [&'static str] {
        &[
            "session", "provider", "seq", "role", "tool", "ts", "refs", "content",
        ]
    }

    fn cells(&self) -> Vec<String> {
        vec![
            self.hit.session_id.clone(),
            self.hit.provider.as_str().to_string(),
            self.hit.seq.to_string(),
            self.hit.role.as_str().to_string(),
            self.hit.tool_name.clone().unwrap_or_default(),
            self.hit.ts.map(|ts| ts.to_rfc3339()).unwrap_or_default(),
            self.ref_summary.clone(),
            truncate_for_display(&self.hit.content, TABLE_CONTENT_CHARS),
        ]
    }
}

#[derive(Clone, Serialize)]
struct ContextRowWithRefs {
    session_id: String,
    seq: i64,
    role: String,
    ts: Option<String>,
    is_match: bool,
    ref_summary: String,
    refs: Vec<MessageRef>,
    content: String,
}

impl Row for ContextRowWithRefs {
    fn headers() -> &'static [&'static str] {
        &["session", "seq", "role", "match", "refs", "content"]
    }

    fn cells(&self) -> Vec<String> {
        vec![
            self.session_id.clone(),
            self.seq.to_string(),
            self.role.clone(),
            if self.is_match { "*" } else { "" }.to_string(),
            self.ref_summary.clone(),
            truncate_for_display(&self.content, TABLE_CONTENT_CHARS),
        ]
    }
}

#[derive(Debug, Subcommand)]
pub enum MessagesCmd {
    /// Search messages by content / role / date across sessions.
    Search(MessageSearchArgs),
    /// Read all messages from one session (by id or prefix).
    Get(MessageGetArgs),
    /// Print one session's messages in order (optionally filtered by role/grep/regex).
    Timeline(TimelineArgs),
}

#[derive(Debug, Args)]
pub struct MessageSearchArgs {
    /// Literal FTS phrase/prefix query over message content. Omit to list all.
    /// Mutually exclusive with `--regex` (which would otherwise silently win).
    #[arg(conflicts_with = "regex")]
    pub query: Option<String>,
    /// Filter by role (user|assistant|tool|slash|compaction).
    #[arg(long = "type", value_enum)]
    pub role: Option<Role>,
    /// Restrict to one harness (claude|claude-desktop|codex|cursor|antigravity|pi).
    #[arg(long, value_enum)]
    pub provider: Option<Provider>,
    /// Match content with a Rust regex instead of a literal substring.
    #[arg(long)]
    pub regex: Option<String>,
    /// Scope to one session id (substring/prefix match).
    #[arg(long)]
    pub session: Option<String>,
    /// Scope to one exact session id or unique prefix. Prefer this over --session when you
    /// already have a session id from search output; it avoids substring matches.
    #[arg(long, conflicts_with = "session")]
    pub session_id: Option<String>,
    /// Restrict to messages whose session's cwd or repo root starts with this path
    /// prefix (e.g. `--path ~/src/sessiongrep`). Spans sessions, unlike `--session`.
    /// Accepts absolute, `~`, or relative paths; relative resolves against the current
    /// directory and `.`/`..`/symlinks are resolved to match the stored absolute paths.
    #[arg(long)]
    pub path: Option<String>,
    /// Keep only tool messages whose tool name contains this (case-insensitive substring,
    /// e.g. `exec` for codex `exec_command`, `edit` for claude `Edit`/`MultiEdit`).
    #[arg(long)]
    pub tool: Option<String>,
    #[command(flatten)]
    pub dates: DateRange,
    /// Lower inclusive message sequence bound. Only valid with --session-id or --session because
    /// seq numbers are local to each session.
    #[arg(long)]
    pub seq_from: Option<i64>,
    /// Upper inclusive message sequence bound. Only valid with --session-id or --session because
    /// seq numbers are local to each session.
    #[arg(long)]
    pub seq_to: Option<i64>,
    /// Include extracted URL/resource references in output. Default output is unchanged.
    #[arg(long)]
    pub refs: bool,
    /// Exclude context-compaction messages.
    #[arg(long)]
    pub no_compaction: bool,
    /// Order literal-query results by BM25 relevance (most relevant first) instead of
    /// session/seq. No effect with --regex or an empty query (no full-text score there).
    #[arg(long)]
    pub rank: bool,
    /// Print trigram-prefilter selectivity (candidate rows vs. corpus) to stderr
    /// before results. Explains why a `--regex` query is slow: candidates close to
    /// the corpus size mean the prefilter barely narrowed the scan (anchor the
    /// regex on a rarer literal). See the bugs-limitations L1 note.
    #[arg(long)]
    pub explain: bool,
    /// Show N messages of context on both sides of each match.
    #[arg(long, default_value_t = 0)]
    pub context: i64,
    /// Show N messages of context before each match (overrides --context for before).
    #[arg(long)]
    pub context_before: Option<i64>,
    /// Show N messages of context after each match (overrides --context for after).
    #[arg(long)]
    pub context_after: Option<i64>,
    /// Max results. 0 = unlimited.
    #[arg(long, default_value_t = 0)]
    pub limit: usize,
    /// Output format. `plain` is headerless and tab-separated, one line per
    /// message, with the same columns (in order) as the `table` header, and
    /// `csv` emits that header row first. Content is always the LAST field
    /// (field 7 for search/get: session, provider, seq, role, tool, ts,
    /// content). `json`/`jsonl` keep full untruncated content.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct MessageGetArgs {
    /// Session id or prefix.
    pub id: String,
    /// Optional message sequence number. When set, returns a focused message window instead of
    /// the whole session.
    #[arg(long)]
    pub seq: Option<i64>,
    /// With --seq, include this many messages before and after the selected seq.
    #[arg(long, default_value_t = 0)]
    pub context: i64,
    /// Include extracted URL/resource references in output. Default output is unchanged.
    #[arg(long)]
    pub refs: bool,
    /// Filter by role.
    #[arg(long = "type", value_enum)]
    pub role: Option<Role>,
    #[command(flatten)]
    pub dates: DateRange,
    /// Max results. 0 = unlimited.
    #[arg(long, default_value_t = 0)]
    pub limit: usize,
    /// Output format. `plain` is headerless and tab-separated, one line per
    /// message, with the same columns (in order) as the `table` header, and
    /// `csv` emits that header row first. Content is always the LAST field
    /// (field 7 for search/get: session, provider, seq, role, tool, ts,
    /// content). `json`/`jsonl` keep full untruncated content.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct TimelineArgs {
    /// Session id or prefix.
    pub id: String,
    /// Filter by role.
    #[arg(long = "type", value_enum)]
    pub role: Option<Role>,
    /// Keep only messages containing this literal substring.
    /// Mutually exclusive with `--regex` (which would otherwise silently win).
    #[arg(long, conflicts_with = "regex")]
    pub grep: Option<String>,
    /// Keep only messages matching this Rust regex.
    #[arg(long)]
    pub regex: Option<String>,
    /// Include extracted URL/resource references in output. Default output is unchanged.
    #[arg(long)]
    pub refs: bool,
    /// Exclude context-compaction messages.
    #[arg(long)]
    pub no_compaction: bool,
    #[command(flatten)]
    pub dates: DateRange,
    /// Output format. `plain` is headerless and tab-separated, one line per
    /// message, with the same columns (in order) as the `table` header, and
    /// `csv` emits that header row first. Content is always the LAST field
    /// (field 7 for search/get: session, provider, seq, role, tool, ts,
    /// content). `json`/`jsonl` keep full untruncated content.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

pub fn run(db: &Db, cmd: &MessagesCmd) -> Result<()> {
    match cmd {
        MessagesCmd::Search(args) => run_search(db, args),
        MessagesCmd::Get(args) => {
            let session = db.resolve_session(&args.id)?;
            if let Some(seq) = args.seq {
                if args.role.is_some()
                    || args.limit > 0
                    || args.dates.since.is_some()
                    || args.dates.until.is_some()
                    || args.dates.when.is_some()
                {
                    bail!("--seq mode cannot be combined with --type, --limit, --since, --until, or --when");
                }
                let context = args.context.max(0);
                let matched_rows: HashSet<(String, i64)> =
                    HashSet::from([(session.session.id.clone(), seq)]);
                let rows = db.message_context(&session.session.id, seq, context, context)?;
                if args.refs {
                    let rows = rows
                        .into_iter()
                        .map(|ctx| {
                            let key = (ctx.session_id.clone(), ctx.seq);
                            let refs =
                                extract_refs_from_text(&ctx.content, ctx.tool_name.as_deref());
                            ContextRowWithRefs {
                                session_id: ctx.session_id,
                                seq: ctx.seq,
                                role: ctx.role.as_str().to_string(),
                                ts: ctx.ts.map(|ts| ts.to_rfc3339()),
                                is_match: matched_rows.contains(&key),
                                ref_summary: ref_summary(&refs),
                                refs,
                                content: ctx.content,
                            }
                        })
                        .collect::<Vec<_>>();
                    return emit(&rows, args.format);
                }
                let rows = rows
                    .into_iter()
                    .map(|ctx| {
                        let key = (ctx.session_id.clone(), ctx.seq);
                        ContextRow {
                            session_id: ctx.session_id,
                            seq: ctx.seq,
                            role: ctx.role.as_str().to_string(),
                            ts: ctx.ts.map(|ts| ts.to_rfc3339()),
                            is_match: matched_rows.contains(&key),
                            content: ctx.content,
                        }
                    })
                    .collect::<Vec<_>>();
                return emit(&rows, args.format);
            }
            if args.context != 0 {
                bail!("--context requires --seq");
            }
            let (since, until) = args.dates.resolve_now()?;
            let filters = MessageFilters {
                role: args.role,
                session_id: Some(session.session.id),
                since,
                until,
                limit: args.limit,
                ..Default::default()
            };
            let hits = db.search_messages("", &filters)?;
            emit_message_hits(&hits, args.refs, args.format)
        }
        MessagesCmd::Timeline(args) => {
            let session = db.resolve_session(&args.id)?;
            let (since, until) = args.dates.resolve_now()?;
            let filters = MessageFilters {
                role: args.role,
                session_id: Some(session.session.id),
                since,
                until,
                regex: args.regex.clone(),
                no_compaction: args.no_compaction,
                ..Default::default()
            };
            let hits = db.search_messages(args.grep.as_deref().unwrap_or(""), &filters)?;
            emit_message_hits(&hits, args.refs, args.format)
        }
    }
}

fn run_search(db: &Db, args: &MessageSearchArgs) -> Result<()> {
    let (since, until) = args.dates.resolve_now()?;
    if args.seq_from.is_some() || args.seq_to.is_some() {
        if args.session.is_none() && args.session_id.is_none() {
            bail!("--seq-from/--seq-to require --session-id or --session because seq is session-local");
        }
        if let (Some(from), Some(to)) = (args.seq_from, args.seq_to) {
            if from > to {
                bail!("--seq-from must be <= --seq-to");
            }
        }
    }
    let exact_session_id = args
        .session_id
        .as_deref()
        .map(|id| db.resolve_session(id).map(|s| s.session.id))
        .transpose()?;
    let filters = MessageFilters {
        role: args.role,
        provider: args.provider,
        session_id: exact_session_id,
        session: args.session.clone(),
        path_prefix: args.path.as_deref().map(crate::util::normalize_path_prefix),
        since,
        until,
        seq_from: args.seq_from,
        seq_to: args.seq_to,
        regex: args.regex.clone(),
        tool: args.tool.clone(),
        no_compaction: args.no_compaction,
        rank: args.rank,
        limit: args.limit,
    };
    if args.explain {
        let explain = db.explain_message_search(&filters)?;
        eprintln!("{}", explain.summary(args.regex.is_some()));
    }
    let hits = db.search_messages(args.query.as_deref().unwrap_or(""), &filters)?;

    let before = args.context_before.unwrap_or(args.context).max(0);
    let after = args.context_after.unwrap_or(args.context).max(0);
    if before == 0 && after == 0 {
        return emit_message_hits(&hits, args.refs, args.format);
    }

    // Expand each match into a seq-ordered, de-duplicated window with the matched
    // rows marked. BTreeMap key (session_id, seq) yields the final ordering for free.
    let matched: HashSet<(String, i64)> =
        hits.iter().map(|h| (h.session_id.clone(), h.seq)).collect();
    if args.refs {
        let mut rows: BTreeMap<(String, i64), ContextRowWithRefs> = BTreeMap::new();
        for hit in &hits {
            for ctx in db.message_context(&hit.session_id, hit.seq, before, after)? {
                let key = (ctx.session_id.clone(), ctx.seq);
                let is_match = matched.contains(&key);
                rows.entry(key).or_insert_with(|| {
                    let refs = extract_refs_from_text(&ctx.content, ctx.tool_name.as_deref());
                    ContextRowWithRefs {
                        session_id: ctx.session_id,
                        seq: ctx.seq,
                        role: ctx.role.as_str().to_string(),
                        ts: ctx.ts.map(|ts| ts.to_rfc3339()),
                        is_match,
                        ref_summary: ref_summary(&refs),
                        refs,
                        content: ctx.content,
                    }
                });
            }
        }
        let windowed: Vec<ContextRowWithRefs> = rows.into_values().collect();
        emit(&windowed, args.format)
    } else {
        let mut rows: BTreeMap<(String, i64), ContextRow> = BTreeMap::new();
        for hit in &hits {
            for ctx in db.message_context(&hit.session_id, hit.seq, before, after)? {
                let key = (ctx.session_id.clone(), ctx.seq);
                let is_match = matched.contains(&key);
                rows.entry(key).or_insert_with(|| ContextRow {
                    session_id: ctx.session_id,
                    seq: ctx.seq,
                    role: ctx.role.as_str().to_string(),
                    ts: ctx.ts.map(|ts| ts.to_rfc3339()),
                    is_match,
                    content: ctx.content,
                });
            }
        }
        let windowed: Vec<ContextRow> = rows.into_values().collect();
        emit(&windowed, args.format)
    }
}

fn emit<T: Serialize + Row>(rows: &[T], format: OutputFormat) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    render(rows, format, &mut out)?;
    out.flush()?;
    Ok(())
}

fn emit_message_hits(hits: &[MessageHit], include_refs: bool, format: OutputFormat) -> Result<()> {
    if !include_refs {
        return emit(hits, format);
    }
    let rows = hits
        .iter()
        .cloned()
        .map(|hit| {
            let refs = extract_refs_from_text(&hit.content, hit.tool_name.as_deref());
            MessageHitWithRefs {
                hit,
                ref_summary: ref_summary(&refs),
                refs,
            }
        })
        .collect::<Vec<_>>();
    emit(&rows, format)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(subcommand)]
        cmd: MessagesCmd,
    }

    #[test]
    fn search_query_and_regex_are_mutually_exclusive() {
        // Passing both the positional query and --regex must be a clear clap error,
        // not a silent drop of the positional query (the substring path is regex-gated).
        assert!(TestCli::try_parse_from(["sg", "search", "foo", "--regex", "bar"]).is_err());
        // Either alone parses fine.
        assert!(TestCli::try_parse_from(["sg", "search", "foo"]).is_ok());
        assert!(TestCli::try_parse_from(["sg", "search", "--regex", "bar"]).is_ok());
    }

    #[test]
    fn search_accepts_session_scoped_seq_bounds() {
        assert!(TestCli::try_parse_from([
            "sg",
            "search",
            "needle",
            "--session-id",
            "claude:s1",
            "--seq-from",
            "2",
            "--seq-to",
            "5",
        ])
        .is_ok());
    }

    #[test]
    fn get_accepts_focused_seq_window() {
        assert!(TestCli::try_parse_from([
            "sg",
            "get",
            "claude:s1",
            "--seq",
            "2",
            "--context",
            "1",
        ])
        .is_ok());
    }

    #[test]
    fn message_commands_accept_refs_enrichment_flag() {
        assert!(TestCli::try_parse_from(["sg", "search", "https://example.com", "--refs"]).is_ok());
        assert!(TestCli::try_parse_from(["sg", "get", "claude:s1", "--refs"]).is_ok());
        assert!(TestCli::try_parse_from(["sg", "timeline", "claude:s1", "--refs"]).is_ok());
    }

    #[test]
    fn timeline_grep_and_regex_are_mutually_exclusive() {
        assert!(TestCli::try_parse_from([
            "sg", "timeline", "s1", "--grep", "foo", "--regex", "bar"
        ])
        .is_err());
        assert!(TestCli::try_parse_from(["sg", "timeline", "s1", "--grep", "foo"]).is_ok());
        assert!(TestCli::try_parse_from(["sg", "timeline", "s1", "--regex", "bar"]).is_ok());
    }
}
