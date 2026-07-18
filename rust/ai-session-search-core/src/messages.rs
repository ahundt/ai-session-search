//! `messages` command group: search, read, and timeline per-message rows.
//!
//! Thin command glue over [`crate::db::Db`] + [`crate::render`], so `cli.rs` stays a
//! dispatcher. Exact/regex `--limit 0` means unlimited; fuzzy requires a positive finite page.
//! Date filtering (`--since/--until/--when`) is the shared [`crate::dates::DateRange`],
//! which accepts EDTF / ISO / duration / natural language.

use std::collections::{BTreeMap, HashSet};
use std::io::{self, Write};

use anyhow::{bail, Result};
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::config::CliConfig;
use crate::dates::DateRange;
use crate::db::Db;
use crate::inspect::{inspection_rows, InspectionOptions};
use crate::models::{
    MessageFilters, MessageHit, MessageKind, MessageSearchMode, Provider, Role, SearchField,
};
use crate::refs::{extract_refs_from_text, ref_summary, MessageRef};
use crate::render::{render, OutputFormat, Row};
use crate::service::CatalogService;
use crate::util::{select_message_lines, truncate_for_display};

const LINES_PER_MESSAGE_HELP: &str = "Limit each returned message's displayed content: positive keeps its first N lines, negative keeps its last N lines, and 0 keeps its complete content. This presentation window does not change matches, ranking, result count, pagination, context membership, or reference extraction. Use it to keep many search hits or long tool outputs skimmable without discarding hits. Omit it to use [cli].lines_per_message; use aise show --transcript-lines to window one whole session transcript.";

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
    #[serde(flatten)]
    hit: MessageHit,
    is_match: bool,
}

impl Row for ContextRow {
    fn headers() -> &'static [&'static str] {
        &[
            "session", "provider", "seq", "role", "tool", "ts", "match", "content",
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
            if self.is_match { "*" } else { "" }.to_string(),
            truncate_for_display(&self.hit.content, TABLE_CONTENT_CHARS),
        ]
    }
}

impl ContextRow {
    fn from_hit(
        hit: MessageHit,
        matched_rows: &HashSet<(String, i64)>,
        lines_per_message: i64,
    ) -> Self {
        let key = (hit.session_id.clone(), hit.seq);
        let content = select_message_lines(&hit.content, lines_per_message);
        Self {
            hit: MessageHit { content, ..hit },
            is_match: matched_rows.contains(&key),
        }
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
    #[serde(flatten)]
    row: ContextRow,
    ref_summary: String,
    refs: Vec<MessageRef>,
}

impl Row for ContextRowWithRefs {
    fn headers() -> &'static [&'static str] {
        &[
            "session", "provider", "seq", "role", "tool", "ts", "match", "refs", "content",
        ]
    }

    fn cells(&self) -> Vec<String> {
        vec![
            self.row.hit.session_id.clone(),
            self.row.hit.provider.as_str().to_string(),
            self.row.hit.seq.to_string(),
            self.row.hit.role.as_str().to_string(),
            self.row.hit.tool_name.clone().unwrap_or_default(),
            self.row
                .hit
                .ts
                .map(|ts| ts.to_rfc3339())
                .unwrap_or_default(),
            if self.row.is_match { "*" } else { "" }.to_string(),
            self.ref_summary.clone(),
            truncate_for_display(&self.row.hit.content, TABLE_CONTENT_CHARS),
        ]
    }
}

impl ContextRowWithRefs {
    fn from_hit(
        hit: MessageHit,
        matched_rows: &HashSet<(String, i64)>,
        lines_per_message: i64,
    ) -> Self {
        // Refs come from the full content so a per-message line cap never hides references.
        let refs = extract_refs_from_text(&hit.content, hit.tool_name.as_deref());
        Self {
            row: ContextRow::from_hit(hit, matched_rows, lines_per_message),
            ref_summary: ref_summary(&refs),
            refs,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum MessagesCmd {
    /// Search messages by content / role / date across sessions.
    Search(Box<MessageSearchArgs>),
    /// Read messages from one session, or a focused seq/context window.
    Get(MessageGetArgs),
    /// Print one session's messages in order (optionally filtered by role/grep/regex).
    Timeline(TimelineArgs),
    /// Compact session evidence: purpose, tool activity, refs, changed files, follow-ups.
    Evidence(MessageEvidenceArgs),
}

#[derive(Debug, Args)]
pub struct MessageSearchArgs {
    /// Text to find in message content. Exact literal by default; add --fuzzy for approximate
    /// matching. Punctuation is significant without --fuzzy: "/goal" matches "/goal", not every
    /// "goal". Omit to list all.
    #[arg(value_name = "QUERY", conflicts_with = "query_arg")]
    pub positional_query: Option<String>,
    /// Text to find. Use this for leading-dash strings, e.g. `-e --path`.
    #[arg(
        short = 'e',
        long = "query",
        value_name = "QUERY",
        allow_hyphen_values = true
    )]
    pub query_arg: Option<String>,
    /// Filter by role: user (non-command prompts), assistant, tool (calls/results),
    /// slash (human-entered commands), or compaction.
    #[arg(long = "role", value_enum)]
    pub role: Option<Role>,
    /// Restrict by semantic message kind; tool calls and results are distinct.
    #[arg(long, value_enum)]
    pub kind: Option<MessageKind>,
    /// QUERY searches only this field: content, canonical tool name, or one tool-argument path.
    #[arg(long, value_enum, default_value_t = SearchField::Content)]
    pub field: SearchField,
    /// RFC 6901 JSON pointer relative to tool-call args, e.g. /cmd or /request/path.
    #[arg(long)]
    pub argument_path: Option<String>,
    /// Filter by session source. The generated help lists every accepted provider ID.
    #[arg(long, value_enum)]
    pub provider: Option<Provider>,
    /// Interpret QUERY/--query as a Rust regex instead of an exact literal substring.
    #[arg(long, conflicts_with = "fuzzy")]
    pub regex: bool,
    /// Interpret QUERY/--query with bounded fuzzy matching (minimum 3 characters and finite
    /// --limit). Exact literal search supports shorter/unlimited text; use --regex for patterns.
    #[arg(long)]
    pub fuzzy: bool,
    /// Scope to one exact session id or unique prefix.
    #[arg(long)]
    pub session_id: Option<String>,
    /// Restrict to messages whose session cwd, repo root, or transcript path starts with this path
    /// prefix (e.g. `--path ~/src/aise`). Spans sessions, unlike `--session-id`.
    /// Accepts absolute, `~`, or relative paths; relative resolves against the current
    /// directory and `.`/`..`/symlinks are resolved to match the stored absolute paths.
    #[arg(long)]
    pub path: Option<String>,
    /// Exclude messages whose session cwd, repo root, or transcript path starts with this path.
    /// Repeat to exclude multiple noisy worktrees or exported transcript directories.
    #[arg(long = "exclude-path")]
    pub exclude_paths: Vec<String>,
    /// Exclude one exact session id. Repeat to exclude multiple sessions.
    #[arg(long = "exclude-session")]
    pub exclude_sessions: Vec<String>,
    /// Also require canonical tool_name to contain this case-insensitive substring, independent
    /// of --field (e.g. `exec` matches Codex `exec_command`; `edit` matches Claude `Edit`).
    #[arg(long)]
    pub tool: Option<String>,
    #[command(flatten)]
    pub dates: DateRange,
    /// Lower inclusive message sequence bound. Only valid with --session-id because
    /// seq numbers are local to each session.
    #[arg(long)]
    pub seq_from: Option<i64>,
    /// Upper inclusive message sequence bound. Only valid with --session-id because
    /// seq numbers are local to each session.
    #[arg(long)]
    pub seq_to: Option<i64>,
    /// Include extracted URL references in output. Pair with --context for source audits or with
    /// --regex to find URL-like text, including scheme-less domains.
    #[arg(long)]
    pub refs: bool,
    /// Exclude context-compaction messages.
    #[arg(long)]
    pub no_compaction: bool,
    /// Print search planner diagnostics to stderr before results. For regex, explains
    /// trigram prefilter selectivity. For fuzzy, reports the bounded candidate strategy.
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
    /// Max results. 0 = unlimited for exact/regex; fuzzy requires 1 or more and
    /// offset + limit must not exceed 10,000.
    #[arg(long, default_value_t = 0)]
    pub limit: usize,
    /// Skip this many matching messages before returning results.
    #[arg(long, default_value_t = 0)]
    pub offset: usize,
    /// Limit each returned message's displayed content without changing which messages return.
    #[arg(long, allow_hyphen_values = true, long_help = LINES_PER_MESSAGE_HELP)]
    pub lines_per_message: Option<i64>,
    /// Output format. `plain` is headerless and tab-separated, one line per
    /// message, with the same columns (in order) as the `table` header, and
    /// `csv` emits that header row first. Content is always the LAST field
    /// (field 7 for search/get: session, provider, seq, role, tool, ts,
    /// content). `json`/`jsonl` keep full untruncated content unless
    /// --lines-per-message caps it.
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
    /// Include extracted URL references in output for the focused --seq window or whole session.
    #[arg(long)]
    pub refs: bool,
    /// Filter by role.
    #[arg(long = "role", value_enum)]
    pub role: Option<Role>,
    #[command(flatten)]
    pub dates: DateRange,
    /// Max results. 0 = unlimited.
    #[arg(long, default_value_t = 0)]
    pub limit: usize,
    /// Limit each returned message's displayed content without changing which messages return.
    #[arg(long, allow_hyphen_values = true, long_help = LINES_PER_MESSAGE_HELP)]
    pub lines_per_message: Option<i64>,
    /// Output format. `plain` is headerless and tab-separated, one line per
    /// message, with the same columns (in order) as the `table` header, and
    /// `csv` emits that header row first. Content is always the LAST field
    /// (field 7 for search/get: session, provider, seq, role, tool, ts,
    /// content). `json`/`jsonl` keep full untruncated content unless
    /// --lines-per-message caps it.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct TimelineArgs {
    /// Session id or prefix.
    pub id: String,
    /// Filter by role.
    #[arg(long = "role", value_enum)]
    pub role: Option<Role>,
    /// Keep only messages containing this literal substring.
    /// Mutually exclusive with `--regex` (which would otherwise silently win).
    #[arg(long, conflicts_with = "regex")]
    pub grep: Option<String>,
    /// Keep only messages matching this Rust regex.
    #[arg(long)]
    pub regex: Option<String>,
    /// Include extracted URL references in output for timeline rows.
    #[arg(long)]
    pub refs: bool,
    /// Lower inclusive message sequence bound.
    #[arg(long)]
    pub seq_from: Option<i64>,
    /// Upper inclusive message sequence bound.
    #[arg(long)]
    pub seq_to: Option<i64>,
    /// Exclude context-compaction messages.
    #[arg(long)]
    pub no_compaction: bool,
    #[command(flatten)]
    pub dates: DateRange,
    /// Limit each returned message's displayed content without changing which messages return.
    #[arg(long, allow_hyphen_values = true, long_help = LINES_PER_MESSAGE_HELP)]
    pub lines_per_message: Option<i64>,
    /// Output format. `plain` is headerless and tab-separated, one line per
    /// message, with the same columns (in order) as the `table` header, and
    /// `csv` emits that header row first. Content is always the LAST field
    /// (field 7 for search/get: session, provider, seq, role, tool, ts,
    /// content). `json`/`jsonl` keep full untruncated content unless
    /// --lines-per-message caps it.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct MessageEvidenceArgs {
    /// Session id or unique prefix.
    pub id: String,
    /// Maximum characters per preview in the compact summary. Omit to use
    /// `[cli].evidence_preview_chars` from config.
    #[arg(long)]
    pub preview_chars: Option<usize>,
    /// Aggregate evidence window: positive=first, negative=last, 0=all. Omit to use
    /// `[cli].summary_items` from config.
    #[arg(long, allow_hyphen_values = true)]
    pub summary_items: Option<i64>,
    /// Add bounded optional evidence sections.
    #[arg(long, value_enum)]
    pub include: Vec<EvidenceInclude>,
    /// Output format. `json`/`jsonl` return one structured inspection object; table/csv/plain
    /// flatten it into section/key/value rows.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum EvidenceInclude {
    TimeProfile,
}

pub fn run(db: &Db, cmd: &MessagesCmd, config: &CliConfig) -> Result<()> {
    match cmd {
        MessagesCmd::Search(args) => run_search(db, args, config),
        MessagesCmd::Get(args) => {
            let lines_per_message = args.lines_per_message.unwrap_or(config.lines_per_message);
            let session = db.resolve_session_record(&args.id)?;
            if let Some(seq) = args.seq {
                if args.role.is_some()
                    || args.limit > 0
                    || args.dates.since.is_some()
                    || args.dates.until.is_some()
                    || args.dates.when.is_some()
                {
                    bail!("--seq mode cannot be combined with --role, --limit, --since, --until, or --when");
                }
                let context = args.context.max(0);
                let matched_rows: HashSet<(String, i64)> =
                    HashSet::from([(session.id.clone(), seq)]);
                let rows = db.message_context(&session.id, seq, context, context)?;
                if args.refs {
                    let rows = rows
                        .into_iter()
                        .map(|ctx| {
                            ContextRowWithRefs::from_hit(ctx, &matched_rows, lines_per_message)
                        })
                        .collect::<Vec<_>>();
                    return emit(&rows, args.format);
                }
                let rows = rows
                    .into_iter()
                    .map(|ctx| ContextRow::from_hit(ctx, &matched_rows, lines_per_message))
                    .collect::<Vec<_>>();
                return emit(&rows, args.format);
            }
            if args.context != 0 {
                bail!("--context requires --seq");
            }
            let (since, until) = args.dates.resolve_now()?;
            let filters = MessageFilters {
                role: args.role,
                session_id: Some(session.id),
                since,
                until,
                limit: args.limit,
                ..Default::default()
            };
            let hits = db.search_messages("", &filters)?;
            emit_message_hits(&hits, args.refs, args.format, lines_per_message)
        }
        MessagesCmd::Timeline(args) => {
            let session = db.resolve_session_record(&args.id)?;
            validate_seq_bounds(args.seq_from, args.seq_to)?;
            let (since, until) = args.dates.resolve_now()?;
            let query = args.regex.as_deref().or(args.grep.as_deref()).unwrap_or("");
            let filters = MessageFilters {
                role: args.role,
                session_id: Some(session.id),
                since,
                until,
                seq_from: args.seq_from,
                seq_to: args.seq_to,
                match_mode: if args.regex.is_some() {
                    MessageSearchMode::Regex
                } else {
                    MessageSearchMode::Exact
                },
                no_compaction: args.no_compaction,
                ..Default::default()
            };
            let hits = db.search_messages(query, &filters)?;
            emit_message_hits(
                &hits,
                args.refs,
                args.format,
                args.lines_per_message.unwrap_or(config.lines_per_message),
            )
        }
        MessagesCmd::Evidence(args) => {
            let options = InspectionOptions {
                preview_chars: args
                    .preview_chars
                    .unwrap_or(config.evidence_preview_chars)
                    .max(1),
                evidence_window: crate::inspect::EvidenceWindow::from_signed_items(
                    args.summary_items.unwrap_or(config.summary_items),
                )?,
                include_time_profile: args.include.contains(&EvidenceInclude::TimeProfile),
            };
            let inspection = CatalogService::new(db).inspect(&args.id, options)?;
            emit_inspection(&inspection, options, args.format)
        }
    }
}

fn run_search(db: &Db, args: &MessageSearchArgs, config: &CliConfig) -> Result<()> {
    let lines_per_message = args.lines_per_message.unwrap_or(config.lines_per_message);
    let (since, until) = args.dates.resolve_now()?;
    if args.seq_from.is_some() || args.seq_to.is_some() {
        if args.session_id.is_none() {
            bail!("--seq-from/--seq-to require --session-id because seq is session-local");
        }
        validate_seq_bounds(args.seq_from, args.seq_to)?;
    }
    let exact_session_id = args
        .session_id
        .as_deref()
        .map(|id| db.resolve_session_record(id).map(|s| s.id))
        .transpose()?;
    let query = args
        .query_arg
        .as_deref()
        .or(args.positional_query.as_deref())
        .unwrap_or("");
    if (args.regex || args.fuzzy) && query.is_empty() {
        let flag = if args.regex { "--regex" } else { "--fuzzy" };
        bail!("{flag} requires QUERY or --query <QUERY>");
    }
    let filters = MessageFilters {
        role: args.role,
        kind: args.kind,
        field: Some(args.field),
        argument_path: args.argument_path.clone(),
        provider: args.provider,
        session_id: exact_session_id,
        path_prefix: args.path.as_deref().map(crate::util::normalize_path_prefix),
        exclude_path_prefixes: args
            .exclude_paths
            .iter()
            .map(|path| crate::util::normalize_path_prefix(path))
            .collect(),
        exclude_session_ids: args.exclude_sessions.clone(),
        since,
        until,
        seq_from: args.seq_from,
        seq_to: args.seq_to,
        match_mode: if args.regex {
            MessageSearchMode::Regex
        } else if args.fuzzy {
            MessageSearchMode::Fuzzy
        } else {
            MessageSearchMode::Exact
        },
        tool: args.tool.clone(),
        no_compaction: args.no_compaction,
        limit: args.limit,
        offset: args.offset,
    };
    let (hits, explain) = db.search_messages_with_explain(query, &filters, args.explain)?;
    if let Some(explain) = explain {
        let has_content_query = args.regex || args.fuzzy || !query.is_empty();
        eprintln!("{}", explain.summary(has_content_query));
    }

    let before = args.context_before.unwrap_or(args.context).max(0);
    let after = args.context_after.unwrap_or(args.context).max(0);
    if before == 0 && after == 0 {
        return emit_message_hits(&hits, args.refs, args.format, lines_per_message);
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
                rows.entry(key).or_insert_with(|| {
                    ContextRowWithRefs::from_hit(ctx, &matched, lines_per_message)
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
                rows.entry(key)
                    .or_insert_with(|| ContextRow::from_hit(ctx, &matched, lines_per_message));
            }
        }
        let windowed: Vec<ContextRow> = rows.into_values().collect();
        emit(&windowed, args.format)
    }
}

fn validate_seq_bounds(seq_from: Option<i64>, seq_to: Option<i64>) -> Result<()> {
    if let (Some(from), Some(to)) = (seq_from, seq_to) {
        if from > to {
            bail!("--seq-from must be <= --seq-to");
        }
    }
    Ok(())
}

fn emit<T: Serialize + Row>(rows: &[T], format: OutputFormat) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    render(rows, format, &mut out)?;
    out.flush()?;
    Ok(())
}

fn emit_message_hits(
    hits: &[MessageHit],
    include_refs: bool,
    format: OutputFormat,
    lines_per_message: i64,
) -> Result<()> {
    if !include_refs {
        if lines_per_message == 0 {
            return emit(hits, format);
        }
        let capped = hits
            .iter()
            .cloned()
            .map(|mut hit| {
                hit.content = select_message_lines(&hit.content, lines_per_message);
                hit
            })
            .collect::<Vec<_>>();
        return emit(&capped, format);
    }
    let rows = hits
        .iter()
        .cloned()
        .map(|mut hit| {
            // Refs come from the full content so a per-message line cap never hides references.
            let refs = extract_refs_from_text(&hit.content, hit.tool_name.as_deref());
            hit.content = select_message_lines(&hit.content, lines_per_message);
            MessageHitWithRefs {
                hit,
                ref_summary: ref_summary(&refs),
                refs,
            }
        })
        .collect::<Vec<_>>();
    emit(&rows, format)
}

fn emit_inspection(
    inspection: &crate::inspect::SessionInspection,
    options: InspectionOptions,
    format: OutputFormat,
) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    match format {
        OutputFormat::Json => writeln!(out, "{}", serde_json::to_string_pretty(inspection)?)?,
        OutputFormat::Jsonl => writeln!(out, "{}", serde_json::to_string(inspection)?)?,
        OutputFormat::Table | OutputFormat::Csv | OutputFormat::Plain => {
            render(&inspection_rows(inspection, options), format, &mut out)?;
        }
    }
    out.flush()?;
    Ok(())
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

    fn assert_parses<const N: usize>(args: [&str; N]) {
        TestCli::try_parse_from(args)
            .unwrap_or_else(|err| panic!("expected messages args to parse: {args:?}: {err}"));
    }

    fn assert_rejects<const N: usize>(args: [&str; N]) {
        assert!(
            TestCli::try_parse_from(args).is_err(),
            "expected messages args to be rejected: {args:?}"
        );
    }

    #[test]
    fn search_query_and_regex_are_mutually_exclusive() {
        // QUERY is the single pattern operand; --regex changes how that operand is interpreted.
        assert_parses(["sg", "search", "foo"]);
        assert_parses(["sg", "search", "foo", "--regex"]);
        assert_parses(["sg", "search", "--query", "foo", "--regex"]);
        assert_parses(["sg", "search", "--regex", "bar"]);
        assert_parses(["sg", "search", "TODO|FIXME", "--regex", "--role", "user"]);
    }

    #[test]
    fn search_accepts_session_scoped_seq_bounds() {
        assert_parses([
            "sg",
            "search",
            "needle",
            "--session-id",
            "claude:s1",
            "--seq-from",
            "2",
            "--seq-to",
            "5",
        ]);
    }

    #[test]
    fn get_accepts_focused_seq_window() {
        assert_parses(["sg", "get", "claude:s1", "--seq", "2", "--context", "1"]);
    }

    #[test]
    fn message_commands_accept_refs_enrichment_flag() {
        assert_parses(["sg", "search", "https://example.com", "--refs"]);
        assert_parses(["sg", "get", "claude:s1", "--refs"]);
        assert_parses(["sg", "timeline", "claude:s1", "--refs"]);
    }

    #[test]
    fn structured_context_rows_reuse_complete_message_hit_metadata() {
        let hit = MessageHit {
            session_id: "codex:context-contract".to_string(),
            provider: Provider::Codex,
            seq: 7,
            role: Role::Tool,
            kind: MessageKind::ToolResult,
            ts: None,
            tool_name: Some("exec_command".to_string()),
            tool_call_id: Some("call-7".to_string()),
            fuzzy_score: None,
            content: "first\nsecond".to_string(),
        };
        let matched = HashSet::from([("codex:context-contract".to_string(), 7)]);

        let value = serde_json::to_value(ContextRow::from_hit(hit, &matched, 1)).unwrap();

        assert_eq!(value["session_id"], "codex:context-contract");
        assert_eq!(value["provider"], "codex");
        assert_eq!(value["seq"], 7);
        assert_eq!(value["role"], "tool");
        assert_eq!(value["kind"], "tool_result");
        assert_eq!(value["tool_name"], "exec_command");
        assert_eq!(value["tool_call_id"], "call-7");
        assert_eq!(value["content"], "first");
        assert_eq!(value["is_match"], true);
    }

    #[test]
    fn timeline_grep_and_regex_are_mutually_exclusive() {
        assert_rejects(["sg", "timeline", "s1", "--grep", "foo", "--regex", "bar"]);
        assert_parses(["sg", "timeline", "s1", "--grep", "foo"]);
        assert_parses(["sg", "timeline", "s1", "--regex", "bar"]);
    }

    #[test]
    fn timeline_accepts_session_local_seq_bounds() {
        assert_parses([
            "sg",
            "timeline",
            "claude:s1",
            "--seq-from",
            "2",
            "--seq-to",
            "5",
        ]);
        assert!(validate_seq_bounds(Some(5), Some(2)).is_err());
    }

    #[test]
    fn evidence_accepts_session_id_preview_chars_and_format() {
        assert_parses([
            "sg",
            "evidence",
            "claude:s1",
            "--preview-chars",
            "80",
            "--format",
            "json",
        ]);
    }
}
