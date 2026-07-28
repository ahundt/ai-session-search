//! `messages` command group: search, read, and timeline per-message rows.
//!
//! Thin command glue over [`crate::db::Db`] + [`crate::render`], so `cli.rs` stays a
//! dispatcher. Literal/regex `--limit 0` means unlimited; fuzzy requires a positive finite page.
//! Date filtering (`--since/--until/--when`) is the shared [`crate::dates::DateRange`],
//! which accepts EDTF / ISO / duration / natural language.

use std::collections::{BTreeMap, HashSet};
use std::io::{self, Write};

use anyhow::{bail, Result};
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::config::Config;
use crate::dates::DateRange;
use crate::db::Db;
use crate::inspect::{inspection_rows, InspectionOptions};
use crate::message_search::{
    ContextWindow, LineWindow, MatchWindow, MessageContentExtent, MessageQuery, MessageSearchHit,
    MessageSearchRequest, MessageSearchResponse, MessageTarget, PurposeSelection, ReceiptLevel,
    RequestedExtent, RequestedTimeRange, ResolvedExtent, SearchSurface, SequenceRange,
    MESSAGE_SEARCH_RESPONSE_SCHEMA_VERSION,
};
use crate::models::{
    MessageFilters, MessageHit, MessageKind, MessageSearchMode, Provider, Role, SearchField,
};
use crate::refs::{extract_refs_from_text, ref_summary, MessageRef};
use crate::render::{render, OutputFormat, Row};
use crate::service::{CatalogService, MessageService};
use crate::util::{select_message_lines, truncate_for_display};

const LINES_PER_MESSAGE_HELP: &str = "Limit each returned message's displayed content: positive keeps its first N lines, negative keeps its last N lines, and 0 keeps its complete content. This presentation window does not change matches, ranking, result count, pagination, context membership, or reference extraction. Use it to keep many search hits or long tool outputs skimmable without discarding hits. Omit it to use [cli].lines_per_message; use aise show --transcript-lines to window one whole session transcript.";

/// Max characters of content shown in tabular formats (json/jsonl keep full content).
const TABLE_CONTENT_CHARS: usize = 120;

/// Which end of the session a `--limit` row window is taken from. `oldest` keeps the first N
/// by sequence, `newest` keeps the last N; both are then printed oldest-first. Order drives
/// SELECTION, not just display, so `--order newest --limit 5` is the last 5 messages, not the
/// first 5 shown backwards (the `git log --reverse` trap). A signed `--limit` is deliberately
/// NOT used: a negative count is a CLI parser hazard and unprecedented among leading tools;
/// the sign convention is reserved for per-item depth (--lines-per-message, --summary-items).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ReadOrder {
    #[default]
    Oldest,
    Newest,
}

impl ReadOrder {
    fn to_message_order(self) -> crate::db::MessageOrder {
        match self {
            ReadOrder::Oldest => crate::db::MessageOrder::OldestFirst,
            ReadOrder::Newest => crate::db::MessageOrder::NewestFirst,
        }
    }
}

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

impl Row for MessageSearchHit {
    fn headers() -> &'static [&'static str] {
        &[
            "session", "provider", "seq", "role", "tool", "ts", "match", "content",
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
            self.match_evidence()
                .map(format_match_evidence)
                .unwrap_or_default(),
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
    #[serde(skip_serializing_if = "Option::is_none")]
    match_evidence: Option<crate::message_search::MessageMatchEvidence>,
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
            self.match_evidence
                .as_ref()
                .map(format_match_evidence)
                .unwrap_or_else(|| {
                    if self.is_match {
                        "*".into()
                    } else {
                        String::new()
                    }
                }),
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
            match_evidence: None,
        }
    }

    fn with_match_evidence(
        mut self,
        evidence: Option<crate::message_search::MessageMatchEvidence>,
    ) -> Self {
        self.match_evidence = evidence;
        self
    }
}

#[derive(Serialize)]
struct MessageHitWithRefs {
    #[serde(flatten)]
    hit: MessageHit,
    ref_summary: String,
    refs: Vec<MessageRef>,
}

#[derive(Serialize)]
struct MessageSearchHitWithRefs {
    #[serde(flatten)]
    hit: MessageSearchHit,
    ref_summary: String,
    refs: Vec<MessageRef>,
}

impl Row for MessageSearchHitWithRefs {
    fn headers() -> &'static [&'static str] {
        &[
            "session", "provider", "seq", "role", "tool", "ts", "match", "refs", "content",
        ]
    }

    fn cells(&self) -> Vec<String> {
        let mut cells = self.hit.cells();
        cells.insert(cells.len() - 1, self.ref_summary.clone());
        cells
    }
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
            self.row
                .match_evidence
                .as_ref()
                .map(format_match_evidence)
                .unwrap_or_else(|| {
                    if self.row.is_match {
                        "*".into()
                    } else {
                        String::new()
                    }
                }),
            self.ref_summary.clone(),
            truncate_for_display(&self.row.hit.content, TABLE_CONTENT_CHARS),
        ]
    }
}

fn format_match_evidence(evidence: &crate::message_search::MessageMatchEvidence) -> String {
    let (focus_start, focus_end, boundary) = match &evidence.markers {
        crate::message_search::MessageMatchViewMarkers::Characters { ranges, .. } => {
            let first = ranges
                .first()
                .copied()
                .unwrap_or(crate::message_search::ViewCharRange {
                    view_start_char: 0,
                    view_end_char_exclusive: 0,
                });
            (first.view_start_char, first.view_end_char_exclusive, None)
        }
        crate::message_search::MessageMatchViewMarkers::Boundary { view_at_char } => {
            (*view_at_char, *view_at_char, Some(*view_at_char))
        }
    };
    let total = evidence.view_text.chars().count();
    let caret_chars = usize::from(boundary.is_some());
    let rendered = if total + caret_chars <= TABLE_CONTENT_CHARS {
        let mut rendered = evidence.view_text.clone();
        if let Some(boundary) = boundary {
            insert_caret(&mut rendered, boundary);
        }
        rendered
    } else {
        // Reserve room for the caret and both possible ASCII omission markers. The selected range
        // or boundary stays visible even when the configured evidence excerpt exceeds this
        // terminal column's width.
        let visible_chars = TABLE_CONTENT_CHARS.saturating_sub(caret_chars + 6).max(1);
        let focus_width = focus_end.saturating_sub(focus_start);
        let start = focus_start
            .saturating_sub(visible_chars.saturating_sub(focus_width) / 2)
            .min(total.saturating_sub(visible_chars));
        let end = (start + visible_chars).min(total);
        let mut rendered = evidence
            .view_text
            .chars()
            .skip(start)
            .take(end - start)
            .collect::<String>();
        if let Some(boundary) = boundary {
            insert_caret(&mut rendered, boundary.saturating_sub(start));
        }
        if start > 0 {
            rendered.insert_str(0, "...");
        }
        if end < total {
            rendered.push_str("...");
        }
        rendered
    };
    crate::util::compact_whitespace(&rendered)
}

fn insert_caret(rendered: &mut String, at_char: usize) {
    let byte = rendered
        .char_indices()
        .nth(at_char)
        .map(|(byte, _)| byte)
        .unwrap_or(rendered.len());
    rendered.insert(byte, '^');
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum CliMessageQueryMode {
    #[default]
    Literal,
    Regex,
    Fuzzy,
}

#[derive(Debug, Args)]
pub struct MessageSearchArgs {
    /// Text to find in the selected field. Literal mode is the default; choose regex or fuzzy with
    /// --query-mode. Punctuation is significant in literal mode. Omit the query to list all. Pass
    /// a leading-dash query via `-e` or after `--`.
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
    /// Restrict by semantic message kind; tool calls and results are distinct. One-value alias
    /// for --kinds.
    #[arg(long, value_enum)]
    pub kind: Option<MessageKind>,
    /// Message classes to return. Omit for every class except harness-notice, which is what the
    /// harness told the agent (Stop-hook feedback, PreToolUse blocks, local-command caveats,
    /// task notifications) rather than what the user wrote. Pass harness-notice to answer why an
    /// agent stopped, looped, or was blocked. This is the single class filter; --kind selects one.
    #[arg(long, value_enum, num_args = 1.., value_delimiter = ',', conflicts_with = "kind")]
    pub kinds: Vec<MessageKind>,
    /// QUERY searches only this field: content, canonical tool name, or one tool-argument path.
    #[arg(long, value_enum, default_value_t = SearchField::Content)]
    pub field: SearchField,
    /// RFC 6901 JSON pointer relative to tool-call args, e.g. /cmd or /request/path.
    #[arg(long)]
    pub argument_path: Option<String>,
    /// Restrict to one indexed session source; omit to include all eight.
    #[arg(long, value_enum)]
    pub provider: Option<Provider>,
    /// Interpret QUERY as a literal substring, Rust regex, or bounded fuzzy pattern.
    #[arg(long, value_enum, default_value_t = CliMessageQueryMode::Literal)]
    pub query_mode: CliMessageQueryMode,
    /// Scope to one exact session id or unique prefix.
    #[arg(long)]
    pub session_id: Option<String>,
    /// Restrict by session working directory or repository root.
    #[arg(long)]
    pub workspace_path: Option<String>,
    /// Restrict by transcript storage path.
    #[arg(long)]
    pub transcript_path: Option<String>,
    /// Exclude a session working-directory or repository-root prefix. Repeatable.
    #[arg(long = "exclude-workspace-path")]
    pub exclude_workspace_paths: Vec<String>,
    /// Exclude a transcript storage prefix. Repeatable.
    #[arg(long = "exclude-transcript-path")]
    pub exclude_transcript_paths: Vec<String>,
    /// Exclude one exact session id. Repeat to exclude multiple sessions.
    #[arg(long = "exclude-session")]
    pub exclude_sessions: Vec<String>,
    /// Also require canonical tool_name to contain this case-insensitive substring, independent
    /// of --field (e.g. `exec` matches Codex `exec_command`; `edit` matches Claude `Edit`).
    #[arg(long)]
    pub tool_name_contains: Option<String>,
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
    /// Include extracted URL references in output. Bare --include-refs means true; pass
    /// --include-refs=false to override a purpose that enables references.
    #[arg(long, num_args = 0..=1, default_missing_value = "true", action = clap::ArgAction::Set)]
    pub include_refs: Option<bool>,
    /// Include context-compaction messages. Pass an explicit boolean.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub include_compaction: bool,
    /// Select the earliest or latest bounded matches. Latest requires one session.
    #[arg(long, value_enum)]
    pub match_window: Option<MatchWindow>,
    /// Select a configured purpose bundle.
    #[arg(long)]
    pub purpose: Option<String>,
    /// Require a specific configured purpose version.
    #[arg(long, requires = "purpose")]
    pub purpose_version: Option<std::num::NonZeroU32>,
    /// Select receipt detail: none omits diagnostics, summary includes planner diagnostics, and
    /// full adds resolved parameter origins.
    #[arg(long, value_enum)]
    pub receipt_level: Option<ReceiptLevel>,
    /// Show this many neighboring messages (0 or greater) on both sides of each match;
    /// 0 (the default) shows only the match.
    #[arg(long, value_parser = parse_context_count)]
    pub context: Option<i64>,
    /// Show this many neighboring messages (0 or greater) before each match
    /// (overrides --context for before).
    #[arg(long, value_parser = parse_context_count)]
    pub context_before: Option<i64>,
    /// Show this many neighboring messages (0 or greater) after each match
    /// (overrides --context for after).
    #[arg(long, value_parser = parse_context_count)]
    pub context_after: Option<i64>,
    /// Positive page size. Literal and regex select earliest matches unless --match-window latest
    /// is used with one session. With no configured operation/purpose default, omission returns
    /// every literal, regex, or no-text CLI match; MCP alone supplies an implicit finite page.
    /// Fuzzy ranks every eligible match and requires an explicit/configured finite page.
    /// Use --all-results to state an unbounded read explicitly in scripts.
    #[arg(long, conflicts_with = "all_results")]
    pub limit: Option<usize>,
    /// Return every literal, regex, or no-text match. Fuzzy search is always bounded.
    #[arg(long, conflicts_with = "limit")]
    pub all_results: bool,
    /// Skip this many matching messages before returning results. Fuzzy offsets apply after the
    /// deterministic relevance order on the complete eligible corpus.
    #[arg(long, default_value_t = 0)]
    pub offset: usize,
    /// Limit each returned message's displayed content without changing which messages return.
    #[arg(long, allow_hyphen_values = true, long_help = LINES_PER_MESSAGE_HELP)]
    pub lines_per_message: Option<i64>,
    /// Maximum Unicode scalar characters in the automatic selected-field match excerpt.
    /// This bounds presentation only; it never changes matching, ranking, or pagination.
    #[arg(long)]
    pub match_evidence_max_chars: Option<std::num::NonZeroUsize>,
    /// Output format. Without --include-refs, search has 8 fields:
    /// session, provider, seq, role, tool, ts, match, content. `plain` is headerless and
    /// tab-separated; `csv` includes the same header as `table`. --include-refs inserts refs
    /// before content. Content is always last. JSON/JSONL keep complete content unless
    /// --lines-per-message caps it.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct MessageGetArgs {
    /// Session id or prefix.
    pub id: String,
    /// Select one message by its 0-based sequence number and return a focused window instead of
    /// the whole session. A sequence past the session's end returns no rows.
    #[arg(long)]
    pub seq: Option<i64>,
    /// With --seq, include this many neighboring messages (0 or greater) before and after the
    /// selected seq; 0 (the default) shows only the selected message.
    #[arg(long, default_value_t = 0, value_parser = parse_context_count)]
    pub context: i64,
    /// Include extracted URL references in output for the focused --seq window or whole session.
    #[arg(long)]
    pub refs: bool,
    /// Filter by role.
    #[arg(long = "role", value_enum)]
    pub role: Option<Role>,
    #[command(flatten)]
    pub dates: DateRange,
    /// Maximum messages to return; 0 (the default) returns all. Selection is oldest-first by
    /// sequence unless --order newest, so `--limit 75 --order newest` reads the 75 most recent
    /// messages. Pair with --seq-from/--seq-to or --offset to page.
    #[arg(long, default_value_t = 0)]
    pub limit: usize,
    /// Which end the --limit window is taken from: oldest (first N by sequence, the default) or
    /// newest (last N). Results are always printed oldest-first; order selects, it does not just
    /// reverse display.
    #[arg(long, value_enum, default_value_t = ReadOrder::Oldest)]
    pub order: ReadOrder,
    /// Skip this many messages from the leading edge of the window before returning (count
    /// pagination companion to --limit). For guaranteed non-overlapping chunked reads prefer
    /// --seq-from/--seq-to, which pin absolute sequence numbers rather than a moving offset.
    #[arg(long, default_value_t = 0)]
    pub offset: usize,
    /// Lower inclusive sequence bound (sequences are 0-based). With --seq-to this reads an
    /// absolute \[from,to\] range, so successive chunks (0..499, then 500..999) never re-read
    /// the same messages.
    #[arg(long)]
    pub seq_from: Option<i64>,
    /// Upper inclusive sequence bound. See --seq-from.
    #[arg(long)]
    pub seq_to: Option<i64>,
    /// Limit each returned message's displayed content without changing which messages return.
    #[arg(long, allow_hyphen_values = true, long_help = LINES_PER_MESSAGE_HELP)]
    pub lines_per_message: Option<i64>,
    /// Output format. Without --refs, get has 7 fields:
    /// session, provider, seq, role, tool, ts, content. `plain` is headerless and
    /// tab-separated; `csv` includes the same header as `table`. --refs inserts refs before
    /// content. Content is always last. JSON/JSONL keep complete content unless
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
    /// Message classes to show. Omit for every class except harness-notice, which is what the
    /// harness told the agent rather than what the user wrote. This is the single class filter.
    #[arg(long, value_enum, num_args = 1.., value_delimiter = ',')]
    pub kinds: Vec<MessageKind>,
    #[command(flatten)]
    pub dates: DateRange,
    /// Maximum messages to return; 0 (the default) returns all. Selection is oldest-first by
    /// sequence unless --order newest. To read a long session in chunks, advance --seq-from
    /// (next chunk starts at the last seq + 1) rather than growing --limit, which re-sends what
    /// you already read.
    #[arg(long, default_value_t = 0)]
    pub limit: usize,
    /// Which end the --limit window is taken from: oldest (first N by sequence, the default) or
    /// newest (last N). Results are always printed oldest-first; order selects, it does not just
    /// reverse display.
    #[arg(long, value_enum, default_value_t = ReadOrder::Oldest)]
    pub order: ReadOrder,
    /// Skip this many messages from the leading edge of the window (count pagination companion to
    /// --limit). Prefer --seq-from/--seq-to for guaranteed non-overlapping chunked reads.
    #[arg(long, default_value_t = 0)]
    pub offset: usize,
    /// Limit each returned message's displayed content without changing which messages return.
    #[arg(long, allow_hyphen_values = true, long_help = LINES_PER_MESSAGE_HELP)]
    pub lines_per_message: Option<i64>,
    /// Output format. Without --refs, timeline has 7 fields:
    /// session, provider, seq, role, tool, ts, content. `plain` is headerless and
    /// tab-separated; `csv` includes the same header as `table`. --refs inserts refs before
    /// content. Content is always last. JSON/JSONL keep complete content unless
    /// --lines-per-message caps it.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct MessageEvidenceArgs {
    /// Session id or unique prefix.
    pub id: String,
    /// Maximum characters per preview in the compact summary (1 or greater). Omit to use
    /// `[cli].evidence_preview_chars` from config.
    #[arg(long, value_parser = crate::cli::parse_positive_usize)]
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

pub fn run(db: &Db, cmd: &MessagesCmd, config: &Config) -> Result<()> {
    let cli = &config.cli;
    match cmd {
        MessagesCmd::Search(args) => run_search(db, args, config),
        MessagesCmd::Get(args) => {
            let lines_per_message = args.lines_per_message.unwrap_or(cli.lines_per_message);
            let session = db.resolve_session_record(&args.id)?;
            if let Some(seq) = args.seq {
                if args.role.is_some()
                    || args.limit > 0
                    || args.order != ReadOrder::Oldest
                    || args.offset > 0
                    || args.seq_from.is_some()
                    || args.seq_to.is_some()
                    || args.dates.since.is_some()
                    || args.dates.until.is_some()
                    || args.dates.when.is_some()
                {
                    bail!(
                        "--seq selects one message by sequence, so it takes only --context; \
                         drop --role, --limit, --order, --offset, --seq-from, --seq-to, --since, \
                         --until, and --when, or omit --seq to read a range of messages"
                    );
                }
                let context = args.context;
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
            validate_seq_bounds(args.seq_from, args.seq_to)?;
            let (since, until) = args.dates.resolve_now()?;
            let filters = MessageFilters {
                role: args.role,
                session_id: Some(session.id),
                since,
                until,
                seq_from: args.seq_from,
                seq_to: args.seq_to,
                limit: args.limit,
                offset: args.offset,
                ..Default::default()
            };
            let hits = db.read_session_messages(&filters, args.order.to_message_order())?;
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
                limit: args.limit,
                offset: args.offset,
                match_mode: if args.regex.is_some() {
                    MessageSearchMode::Regex
                } else {
                    MessageSearchMode::Literal
                },
                no_compaction: args.no_compaction,
                kinds: (!args.kinds.is_empty()).then(|| args.kinds.clone()),
                ..Default::default()
            };
            // Order selects which N (oldest vs newest); a newest-first fetch is reversed back to
            // chronological so the timeline always prints oldest→newest.
            let order = args.order.to_message_order();
            let mut hits = db.search_messages_ordered(query, &filters, order)?;
            if order == crate::db::MessageOrder::NewestFirst {
                hits.reverse();
            }
            emit_message_hits(
                &hits,
                args.refs,
                args.format,
                args.lines_per_message.unwrap_or(cli.lines_per_message),
            )
        }
        MessagesCmd::Evidence(args) => {
            let options = InspectionOptions {
                preview_chars: args
                    .preview_chars
                    .unwrap_or(cli.evidence_preview_chars)
                    .max(1),
                evidence_window: crate::inspect::EvidenceWindow::from_signed_items(
                    args.summary_items.unwrap_or(cli.summary_items),
                )?,
                include_time_profile: args.include.contains(&EvidenceInclude::TimeProfile),
            };
            let inspection = CatalogService::new(db).inspect(&args.id, options)?;
            emit_inspection(&inspection, options, args.format)
        }
    }
}

fn run_search(db: &Db, args: &MessageSearchArgs, config: &Config) -> Result<()> {
    let (since, until) = args.dates.resolve_now()?;
    let query_text = args
        .query_arg
        .as_deref()
        .or(args.positional_query.as_deref())
        .unwrap_or("");
    let query = match (args.query_mode, query_text.is_empty()) {
        (CliMessageQueryMode::Literal, true) => MessageQuery::All,
        (CliMessageQueryMode::Literal, false) => MessageQuery::literal(query_text)?,
        (CliMessageQueryMode::Regex, false) => MessageQuery::regex(query_text)?,
        (CliMessageQueryMode::Fuzzy, false) => MessageQuery::fuzzy(query_text)?,
        (mode, true) => bail!("--query-mode {mode:?} requires QUERY or --query <QUERY>"),
    };
    let target = match args.field {
        SearchField::Content => MessageTarget::content(),
        SearchField::ToolName => MessageTarget::tool_name(),
        SearchField::ToolArgument => {
            MessageTarget::tool_argument(args.argument_path.clone().unwrap_or_default())?
        }
    };
    let mut builder = MessageSearchRequest::builder(query, target)
        .time(RequestedTimeRange::new(since, until)?)
        .include_compaction(args.include_compaction)
        .extent(if args.all_results {
            RequestedExtent::all_results_from(args.offset)
        } else {
            RequestedExtent::page(args.limit, args.offset)?
        });
    if let Some(role) = args.role {
        builder = builder.role(role);
    }
    // clap enforces that --kind and --kinds are not both given, so this cannot silently
    // discard one: they are two spellings of one selection, not two filters that combine.
    if let Some(kind) = args.kind {
        builder = builder.kind(kind);
    } else if !args.kinds.is_empty() {
        builder = builder.kinds(args.kinds.clone());
    }
    if let Some(provider) = args.provider {
        builder = builder.provider(provider);
    }
    if let Some(session) = &args.session_id {
        builder = builder.session_id(session)?;
    }
    if let Some(path) = &args.workspace_path {
        builder = builder.workspace_path_prefix(path)?;
    }
    if let Some(path) = &args.transcript_path {
        builder = builder.transcript_path_prefix(path)?;
    }
    for path in &args.exclude_workspace_paths {
        builder = builder.exclude_workspace_path_prefix(path)?;
    }
    for path in &args.exclude_transcript_paths {
        builder = builder.exclude_transcript_path_prefix(path)?;
    }
    for session in &args.exclude_sessions {
        builder = builder.exclude_session_id(session)?;
    }
    if args.seq_from.is_some() || args.seq_to.is_some() {
        builder = builder.sequence(SequenceRange::new(args.seq_from, args.seq_to)?);
    }
    if let Some(tool) = &args.tool_name_contains {
        builder = builder.tool_name_contains(tool)?;
    }
    if let Some(window) = args.match_window {
        builder = builder.match_window(window);
    }
    if args.context.is_some() || args.context_before.is_some() || args.context_after.is_some() {
        let symmetric = args.context.unwrap_or(0) as usize;
        builder = builder.context(ContextWindow::new(
            args.context_before
                .map(|value| value as usize)
                .unwrap_or(symmetric),
            args.context_after
                .map(|value| value as usize)
                .unwrap_or(symmetric),
        ));
    }
    if let Some(include_refs) = args.include_refs {
        builder = builder.include_refs(include_refs);
    }
    if let Some(lines) = args.lines_per_message {
        builder = builder.message_lines(LineWindow::from_signed(lines)?);
    }
    if let Some(maximum) = args.match_evidence_max_chars {
        builder = builder.match_evidence_max_chars(maximum);
    }
    if let Some(purpose) = &args.purpose {
        builder = builder.purpose(PurposeSelection::new(purpose, args.purpose_version)?);
    }
    if let Some(receipt) = args.receipt_level {
        builder = builder.receipt_level(receipt);
    }

    let response = MessageService::new(config, db, SearchSurface::Cli).search(builder.build()?)?;
    if let Some(explain) = response.search_explanation() {
        let has_content_query = !query_text.is_empty();
        eprintln!("{}", explain.summary(has_content_query));
    }
    if let Some(origins) = response.parameter_origins() {
        eprintln!("[origins] {}", serde_json::to_string(origins)?);
    }
    if let Some(next_offset) = response.page().next_offset() {
        if args.query_mode == CliMessageQueryMode::Fuzzy {
            eprintln!(
                "[more] additional fuzzy matches are available; rerun the same search with \
                 --offset {next_offset}"
            );
        } else {
            eprintln!(
                "[more] additional matches are available; rerun the same search with \
                 --offset {next_offset}, or pass --all-results to return every eligible match"
            );
        }
    }
    if matches!(args.format, OutputFormat::Json | OutputFormat::Jsonl) {
        return emit_message_search_machine_response(&response, args.format);
    }
    let hits = response.hits();
    let include_refs = response.presentation().include_refs();
    let lines_per_message = response.presentation().message_lines().to_signed()?;
    if response.context_windows().is_empty() {
        return emit_message_search_hits(hits, include_refs, args.format, lines_per_message);
    }

    let matched: HashSet<(String, i64)> =
        hits.iter().map(|h| (h.session_id.clone(), h.seq)).collect();
    let match_evidence = hits
        .iter()
        .filter_map(|hit| {
            hit.match_evidence()
                .cloned()
                .map(|evidence| ((hit.session_id.clone(), hit.seq), evidence))
        })
        .collect::<BTreeMap<_, _>>();
    if include_refs {
        let mut rows: BTreeMap<(String, i64), ContextRowWithRefs> = BTreeMap::new();
        for window in response.context_windows() {
            for ctx in window.iter().cloned() {
                let key = (ctx.session_id.clone(), ctx.seq);
                let evidence = match_evidence.get(&key).cloned();
                rows.entry(key).or_insert_with(|| {
                    let mut row = ContextRowWithRefs::from_hit(ctx, &matched, lines_per_message);
                    row.row = row.row.with_match_evidence(evidence);
                    row
                });
            }
        }
        let windowed: Vec<ContextRowWithRefs> = rows.into_values().collect();
        emit(&windowed, args.format)
    } else {
        let mut rows: BTreeMap<(String, i64), ContextRow> = BTreeMap::new();
        for window in response.context_windows() {
            for ctx in window.iter().cloned() {
                let key = (ctx.session_id.clone(), ctx.seq);
                let evidence = match_evidence.get(&key).cloned();
                rows.entry(key).or_insert_with(|| {
                    ContextRow::from_hit(ctx, &matched, lines_per_message)
                        .with_match_evidence(evidence)
                });
            }
        }
        let windowed: Vec<ContextRow> = rows.into_values().collect();
        emit(&windowed, args.format)
    }
}

fn presented_message_value(
    hit: &MessageHit,
    lines_per_message: i64,
    include_refs: bool,
) -> Result<serde_json::Value> {
    let content = select_message_lines(&hit.content, lines_per_message);
    let extent =
        MessageContentExtent::describe(&hit.content, &content, &content, lines_per_message, false);
    let mut value = serde_json::to_value(hit)?;
    value["content"] = serde_json::json!(content);
    value["content_extent"] = serde_json::to_value(extent)?;
    if include_refs {
        let refs = extract_refs_from_text(&hit.content, hit.tool_name.as_deref());
        value["ref_summary"] = serde_json::json!(ref_summary(&refs));
        value["refs"] = serde_json::to_value(refs)?;
    }
    Ok(value)
}

fn presented_search_hit_value(
    hit: &MessageSearchHit,
    lines_per_message: i64,
    include_refs: bool,
) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(hit)?;
    let presented = presented_message_value(&hit.message, lines_per_message, include_refs)?;
    let object = value
        .as_object_mut()
        .expect("MessageSearchHit always serializes as an object");
    let presented = presented
        .as_object()
        .expect("MessageHit always serializes as an object");
    for (key, field) in presented {
        object.insert(key.clone(), field.clone());
    }
    Ok(value)
}

/// Structured CLI formats return self-describing output rather than a bare row array.
///
/// JSONL has explicit metadata, row, and terminal records. A consumer that does not observe the
/// terminal record cannot mistake an interrupted stream for a complete page.
fn emit_message_search_machine_response(
    response: &MessageSearchResponse,
    format: OutputFormat,
) -> Result<()> {
    let lines_per_message = response.presentation().message_lines().to_signed()?;
    let include_refs = response.presentation().include_refs();
    let hits = response
        .hits()
        .iter()
        .map(|hit| presented_search_hit_value(hit, lines_per_message, include_refs))
        .collect::<Result<Vec<_>>>()?;
    let context_windows = response
        .context_windows()
        .iter()
        .map(|window| {
            window
                .iter()
                .map(|hit| presented_message_value(hit, lines_per_message, include_refs))
                .collect::<Result<Vec<_>>>()
        })
        .collect::<Result<Vec<_>>>()?;
    let (limit, offset) = match response.page().extent() {
        ResolvedExtent::Page { limit, offset } => (Some(limit.get()), offset),
        ResolvedExtent::AllResults { offset } => (None, offset),
    };
    let query_mode = match response.match_mode() {
        Some(MessageSearchMode::Literal) => "literal",
        Some(MessageSearchMode::Regex) => "regex",
        Some(MessageSearchMode::Fuzzy) => "fuzzy",
        None => "all",
    };
    let line_selection = match lines_per_message.cmp(&0) {
        std::cmp::Ordering::Greater => "first",
        std::cmp::Ordering::Less => "last",
        std::cmp::Ordering::Equal => "all",
    };
    let record = serde_json::json!({
        "response_schema_version": MESSAGE_SEARCH_RESPONSE_SCHEMA_VERSION,
        "query": response.query(),
        "query_mode": query_mode,
        "match_target": response.match_target(),
        "returned": hits.len(),
        "has_more": response.page().next_offset().is_some(),
        "next_offset": response.page().next_offset(),
        "pagination": {
            "limit": limit,
            "offset": offset,
            "ordering": response.page().ordering(),
            "consistency": "per-call",
        },
        "presentation": {
            "line_selection": line_selection,
            "lines_per_message": lines_per_message,
            "character_selection": "all",
            "whitespace_compacted": false,
        },
        "search_explanation": response.search_explanation(),
        "origins": response.parameter_origins(),
        "hits": hits,
        "context_windows": context_windows,
    });
    let stdout = io::stdout();
    let mut out = stdout.lock();
    match format {
        OutputFormat::Json => serde_json::to_writer_pretty(&mut out, &record)?,
        OutputFormat::Jsonl => {
            let metadata = serde_json::json!({
                "type": "search_metadata",
                "response_schema_version": MESSAGE_SEARCH_RESPONSE_SCHEMA_VERSION,
                "query": record["query"],
                "query_mode": record["query_mode"],
                "match_target": record["match_target"],
                "pagination": record["pagination"],
                "presentation": record["presentation"],
                "search_explanation": record["search_explanation"],
                "origins": record["origins"],
            });
            serde_json::to_writer(&mut out, &metadata)?;
            writeln!(out)?;
            for (index, hit) in record["hits"]
                .as_array()
                .expect("machine hits are always an array")
                .iter()
                .enumerate()
            {
                let context = record["context_windows"]
                    .as_array()
                    .and_then(|windows| windows.get(index))
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!([]));
                let row = serde_json::json!({
                    "type": "hit",
                    "index": index,
                    "hit": hit,
                    "context": context,
                });
                serde_json::to_writer(&mut out, &row)?;
                writeln!(out)?;
            }
            let terminal = serde_json::json!({
                "type": "search_complete",
                "response_schema_version": MESSAGE_SEARCH_RESPONSE_SCHEMA_VERSION,
                "returned": record["returned"],
                "has_more": record["has_more"],
                "next_offset": record["next_offset"],
            });
            serde_json::to_writer(&mut out, &terminal)?;
        }
        _ => unreachable!("machine response is only used for JSON and JSONL"),
    }
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

fn validate_seq_bounds(seq_from: Option<i64>, seq_to: Option<i64>) -> Result<()> {
    if let (Some(from), Some(to)) = (seq_from, seq_to) {
        if from > to {
            bail!(
                "--seq-from must be <= --seq-to, got {from} > {to}; \
                 swap the bounds or raise --seq-to to at least {from}"
            );
        }
    }
    Ok(())
}

/// Clap value parser for context counts: 0 or greater, matching the MCP schema
/// (`minimum: 0`) and the Python binding, which both reject negatives. A negative
/// context has no meaning, so clamping it quietly would hide a caller mistake.
fn parse_context_count(raw: &str) -> std::result::Result<i64, String> {
    let value: i64 = raw
        .parse()
        .map_err(|_| String::from("must be an integer 0 or greater"))?;
    if value < 0 {
        return Err(String::from(
            "must be 0 or greater; pass how many neighboring messages to include, \
             or 0 for the match alone",
        ));
    }
    Ok(value)
}

fn emit<T: Serialize + Row>(rows: &[T], format: OutputFormat) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    render(rows, format, &mut out)?;
    out.flush()?;
    Ok(())
}

fn emit_message_search_hits(
    hits: &[MessageSearchHit],
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
                hit.message.content = select_message_lines(&hit.message.content, lines_per_message);
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
            hit.message.content = select_message_lines(&hit.message.content, lines_per_message);
            MessageSearchHitWithRefs {
                hit,
                ref_summary: ref_summary(&refs),
                refs,
            }
        })
        .collect::<Vec<_>>();
    emit(&rows, format)
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
    crate::render::render_record(
        inspection,
        &inspection_rows(inspection, options),
        format,
        &mut out,
    )?;
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

    fn sample_hit(seq: i64, content: &str) -> MessageHit {
        MessageHit {
            session_id: "claude:s1".to_string(),
            provider: Provider::Claude,
            seq,
            role: Role::User,
            kind: MessageKind::Conversation,
            ts: None,
            tool_name: None,
            tool_call_id: None,
            fuzzy_score: None,
            content: content.to_string(),
        }
    }

    #[test]
    fn render_message_hit_uses_documented_columns_across_formats() {
        let hit = sample_hit(7, "hello world");

        // CSV leads with the header row; content is the last column.
        let mut buf = Vec::new();
        render(std::slice::from_ref(&hit), OutputFormat::Csv, &mut buf).unwrap();
        let csv = String::from_utf8(buf).unwrap();
        assert_eq!(
            csv.lines().next().unwrap(),
            "session,provider,seq,role,tool,ts,content"
        );
        assert!(csv.trim_end().ends_with("hello world"));

        // Plain is headerless, tab-separated, seven fields with content last.
        let mut buf = Vec::new();
        render(std::slice::from_ref(&hit), OutputFormat::Plain, &mut buf).unwrap();
        let plain = String::from_utf8(buf).unwrap();
        let fields: Vec<&str> = plain.trim_end().split('\t').collect();
        assert_eq!(
            fields,
            ["claude:s1", "claude", "7", "user", "", "", "hello world"]
        );

        // JSON keeps the serde field names and full untruncated content.
        let mut buf = Vec::new();
        render(std::slice::from_ref(&hit), OutputFormat::Json, &mut buf).unwrap();
        let json = String::from_utf8(buf).unwrap();
        assert!(json.contains("\"session_id\"") && json.contains("hello world"));

        // Table prints the header row then the padded body.
        let mut buf = Vec::new();
        render(std::slice::from_ref(&hit), OutputFormat::Table, &mut buf).unwrap();
        let table = String::from_utf8(buf).unwrap();
        assert!(table.lines().next().unwrap().starts_with("session"));
        assert!(table.contains("hello world"));
    }

    #[test]
    fn render_context_and_refs_rows_expose_match_and_refs_columns() {
        let hit = sample_hit(7, "see https://example.com now");
        let mut matched = HashSet::new();
        matched.insert((hit.session_id.clone(), hit.seq));

        // ContextRow marks the matched row with `*` and a non-match with blank.
        let ctx = ContextRow::from_hit(hit.clone(), &matched, 0);
        assert_eq!(
            <ContextRow as Row>::headers(),
            ["session", "provider", "seq", "role", "tool", "ts", "match", "content"]
        );
        assert_eq!(ctx.cells()[6], "*");
        let other = ContextRow::from_hit(sample_hit(8, "context line"), &matched, 0);
        assert_eq!(other.cells()[6], "");

        // MessageHitWithRefs inserts a `refs` column before content.
        let refs = extract_refs_from_text(&hit.content, hit.tool_name.as_deref());
        let with_refs = MessageHitWithRefs {
            hit: hit.clone(),
            ref_summary: ref_summary(&refs),
            refs,
        };
        assert_eq!(<MessageHitWithRefs as Row>::headers()[6], "refs");
        let mut buf = Vec::new();
        render(
            std::slice::from_ref(&with_refs),
            OutputFormat::Csv,
            &mut buf,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap().lines().next().unwrap(),
            "session,provider,seq,role,tool,ts,refs,content"
        );

        // ContextRowWithRefs carries both the `match` and `refs` columns.
        let ctx_refs = ContextRowWithRefs::from_hit(hit.clone(), &matched, 0);
        assert_eq!(
            <ContextRowWithRefs as Row>::headers(),
            ["session", "provider", "seq", "role", "tool", "ts", "match", "refs", "content"]
        );
        assert_eq!(ctx_refs.cells()[6], "*");
    }

    #[test]
    fn structured_search_rows_disclose_head_and_tail_omissions() {
        let hit = sample_hit(7, "alpha\nbeta\ngamma");

        let head = presented_message_value(&hit, 1, false).unwrap();
        assert_eq!(head["content"], "alpha");
        assert_eq!(head["content_extent"]["complete"], false);
        assert_eq!(head["content_extent"]["omitted_start"], false);
        assert_eq!(head["content_extent"]["omitted_end"], true);

        let tail = presented_message_value(&hit, -1, false).unwrap();
        assert_eq!(tail["content"], "gamma");
        assert_eq!(tail["content_extent"]["complete"], false);
        assert_eq!(tail["content_extent"]["omitted_start"], true);
        assert_eq!(tail["content_extent"]["omitted_end"], false);
    }

    #[test]
    fn match_evidence_table_window_keeps_late_matches_and_boundaries_visible() {
        let excerpt = format!("{}NEED{}", "a".repeat(205), "z".repeat(10));
        let late_match = crate::message_search::MessageMatchEvidence {
            view_text: excerpt.clone(),
            field_start_char: 0,
            field_total_chars: excerpt.chars().count(),
            markers: crate::message_search::MessageMatchViewMarkers::Characters {
                ranges: vec![crate::message_search::ViewCharRange {
                    view_start_char: 205,
                    view_end_char_exclusive: 209,
                }],
                matched_chars_total: 4,
                matched_chars_shown: 4,
            },
        };
        let rendered = format_match_evidence(&late_match);
        assert!(
            rendered.contains("NEED"),
            "the late selected-field match must remain in the table window"
        );
        assert!(rendered.starts_with("..."));
        assert!(rendered.chars().count() <= TABLE_CONTENT_CHARS);

        for at_char in [0, 109, excerpt.chars().count()] {
            let boundary = crate::message_search::MessageMatchEvidence {
                view_text: excerpt.clone(),
                field_start_char: 0,
                field_total_chars: excerpt.chars().count(),
                markers: crate::message_search::MessageMatchViewMarkers::Boundary {
                    view_at_char: at_char,
                },
            };
            let rendered = format_match_evidence(&boundary);
            assert_eq!(
                rendered.matches('^').count(),
                1,
                "boundary {at_char} must remain visible"
            );
            assert!(rendered.chars().count() <= TABLE_CONTENT_CHARS);
        }
    }

    #[test]
    fn context_table_rows_render_the_same_bounded_match_evidence() {
        let hit = sample_hit(7, "full raw message content");
        let matched = HashSet::from([(hit.session_id.clone(), hit.seq)]);
        let evidence = crate::message_search::MessageMatchEvidence {
            view_text: "x".repeat(220),
            field_start_char: 0,
            field_total_chars: 220,
            markers: crate::message_search::MessageMatchViewMarkers::Boundary { view_at_char: 220 },
        };

        let row = ContextRow::from_hit(hit.clone(), &matched, 0)
            .with_match_evidence(Some(evidence.clone()));
        let row_with_refs = ContextRowWithRefs {
            row: ContextRow::from_hit(hit, &matched, 0).with_match_evidence(Some(evidence)),
            ref_summary: String::new(),
            refs: Vec::new(),
        };

        assert!(row.cells()[6].contains('^'));
        assert!(row_with_refs.cells()[6].contains('^'));
        assert!(row.cells()[6].chars().count() <= TABLE_CONTENT_CHARS);
        assert!(row_with_refs.cells()[6].chars().count() <= TABLE_CONTENT_CHARS);
    }

    #[test]
    fn match_evidence_cells_compact_control_whitespace_without_changing_structured_evidence() {
        let excerpt = format!("{}\r\n\tNEED\n{}", "a".repeat(180), "z".repeat(40));
        let evidence = crate::message_search::MessageMatchEvidence {
            view_text: excerpt.clone(),
            field_start_char: 0,
            field_total_chars: excerpt.chars().count(),
            markers: crate::message_search::MessageMatchViewMarkers::Characters {
                ranges: vec![crate::message_search::ViewCharRange {
                    view_start_char: 183,
                    view_end_char_exclusive: 187,
                }],
                matched_chars_total: 4,
                matched_chars_shown: 4,
            },
        };
        let rendered = format_match_evidence(&evidence);
        assert!(rendered.contains("NEED"));
        assert!(!rendered.contains(['\r', '\n', '\t']));
        assert!(rendered.chars().count() <= TABLE_CONTENT_CHARS);
        assert_eq!(
            evidence.view_text, excerpt,
            "structured JSON evidence must retain its original selected-field text"
        );

        let hit = sample_hit(7, "raw content");
        let matched = HashSet::from([(hit.session_id.clone(), hit.seq)]);
        let search_hit = MessageSearchHit::from_parts(hit.clone(), Some(evidence.clone()), None);
        let context = ContextRow::from_hit(hit.clone(), &matched, 0)
            .with_match_evidence(Some(evidence.clone()));
        let context_with_refs = ContextRowWithRefs {
            row: ContextRow::from_hit(hit, &matched, 0).with_match_evidence(Some(evidence.clone())),
            ref_summary: String::new(),
            refs: Vec::new(),
        };
        for cell in [
            format_match_evidence(&evidence),
            context.cells()[6].clone(),
            context_with_refs.cells()[6].clone(),
        ] {
            assert!(!cell.contains(['\r', '\n', '\t']));
            assert!(cell.chars().count() <= TABLE_CONTENT_CHARS);
        }

        let mut plain = Vec::new();
        render(
            std::slice::from_ref(&search_hit),
            OutputFormat::Plain,
            &mut plain,
        )
        .unwrap();
        let plain = String::from_utf8(plain).unwrap();
        assert_eq!(plain.lines().count(), 1);
        assert_eq!(plain.trim_end().split('\t').count(), 8);

        let structured = serde_json::to_value(search_hit).unwrap();
        assert_eq!(structured["match_evidence"]["view_text"], excerpt);
    }

    #[test]
    fn search_query_mode_uses_one_closed_interpretation_axis() {
        assert_parses(["sg", "search", "foo"]);
        assert_parses(["sg", "search", "foo", "--query-mode", "regex"]);
        assert_parses(["sg", "search", "--query", "foo", "--query-mode", "regex"]);
        assert_parses(["sg", "search", "--query-mode", "regex", "bar"]);
        assert_parses([
            "sg",
            "search",
            "TODO|FIXME",
            "--query-mode",
            "regex",
            "--role",
            "user",
        ]);
        assert_rejects(["sg", "search", "foo", "--query-mode", "unknown"]);
    }

    #[test]
    fn search_match_evidence_bound_is_positive_and_search_only() {
        assert_parses(["sg", "search", "needle", "--match-evidence-max-chars", "80"]);
        assert_rejects(["sg", "search", "needle", "--match-evidence-max-chars", "0"]);
        assert_rejects(["sg", "get", "claude:s1", "--match-evidence-max-chars", "80"]);
    }

    #[test]
    fn search_include_refs_accepts_explicit_false_for_purpose_override() {
        for (argument, expected) in [("--include-refs", true), ("--include-refs=false", false)] {
            let parsed = TestCli::try_parse_from(["sg", "search", "needle", argument]).unwrap();
            let MessagesCmd::Search(args) = parsed.cmd else {
                panic!("expected messages search command");
            };
            assert_eq!(args.include_refs, Some(expected));
        }
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
    fn get_accepts_full_read_flag_combination_and_validates_order() {
        // All range/paging/order read flags coexist on one invocation.
        assert_parses([
            "sg",
            "get",
            "claude:s1",
            "--role",
            "user",
            "--limit",
            "20",
            "--order",
            "newest",
            "--offset",
            "10",
            "--seq-from",
            "100",
            "--seq-to",
            "200",
        ]);
        // --order is a closed enum: only oldest/newest, never a free value or a sign.
        assert_rejects(["sg", "get", "claude:s1", "--order", "recent"]);
        assert_rejects(["sg", "get", "claude:s1", "--order", "-1"]);
    }

    #[test]
    fn get_selects_trailing_window_via_order_not_a_signed_limit() {
        // Newest N is `--limit N --order newest`, NOT a signed limit. A negative --limit is a
        // parser hazard and unprecedented among leading CLIs, so it must be rejected.
        assert_parses([
            "sg",
            "get",
            "claude:s1",
            "--limit",
            "75",
            "--order",
            "newest",
        ]);
        assert_parses([
            "sg",
            "get",
            "claude:s1",
            "--limit",
            "75",
            "--order",
            "oldest",
        ]);
        assert_parses(["sg", "get", "claude:s1", "--limit", "75"]);
        assert_rejects(["sg", "get", "claude:s1", "--limit", "-75"]);
        assert_rejects(["sg", "get", "claude:s1", "--limit=-75"]);
    }

    #[test]
    fn read_order_maps_to_db_message_order_and_defaults_oldest() {
        use crate::db::MessageOrder;
        // The CLI ReadOrder enum drives the db read direction; verify the mapping,
        // not just that the flag parses. oldest -> OldestFirst, newest -> NewestFirst.
        assert_eq!(
            ReadOrder::Oldest.to_message_order(),
            MessageOrder::OldestFirst
        );
        assert_eq!(
            ReadOrder::Newest.to_message_order(),
            MessageOrder::NewestFirst
        );
        // The default direction is oldest-first, matching --order's default and the
        // documented "oldest-first unless --order newest" contract.
        assert_eq!(ReadOrder::default(), ReadOrder::Oldest);
        assert_eq!(
            ReadOrder::default().to_message_order(),
            MessageOrder::OldestFirst
        );
    }

    #[test]
    fn parse_context_count_accepts_zero_and_rejects_negative_and_noninteger() {
        // 0 is the load-bearing value: the match alone, no neighbors.
        assert_eq!(parse_context_count("0").unwrap(), 0);
        assert_eq!(parse_context_count("5").unwrap(), 5);
        // A large value is accepted; saturating to session bounds is the reader's job.
        assert_eq!(parse_context_count("1000000").unwrap(), 1_000_000);
        // Negative is rejected with actionable guidance that names the 0 case.
        let neg = parse_context_count("-1").unwrap_err();
        assert!(neg.contains("0 or greater"), "{neg}");
        assert!(neg.contains("0 for the match alone"), "{neg}");
        // Non-integer and empty are rejected as integers, never silently coerced to 0.
        assert!(parse_context_count("abc")
            .unwrap_err()
            .contains("integer 0 or greater"));
        assert!(parse_context_count("")
            .unwrap_err()
            .contains("integer 0 or greater"));
        assert!(parse_context_count("3.5")
            .unwrap_err()
            .contains("integer 0 or greater"));
    }

    #[test]
    fn timeline_accepts_limit_offset_and_order_for_paged_single_session_reads() {
        assert_parses([
            "sg",
            "timeline",
            "claude:s1",
            "--limit",
            "50",
            "--order",
            "newest",
        ]);
        assert_parses([
            "sg",
            "timeline",
            "claude:s1",
            "--limit",
            "50",
            "--offset",
            "50",
        ]);
        // Negative limit is the parser hazard; newest-N is --order newest, not a sign.
        assert_rejects(["sg", "timeline", "claude:s1", "--limit", "-50"]);
    }

    #[test]
    fn get_accepts_seq_range_and_offset_for_chunked_reads() {
        assert_parses([
            "sg",
            "get",
            "claude:s1",
            "--seq-from",
            "501",
            "--seq-to",
            "1000",
        ]);
        assert_parses([
            "sg",
            "get",
            "claude:s1",
            "--limit",
            "500",
            "--offset",
            "500",
        ]);
    }

    #[test]
    fn message_commands_accept_refs_enrichment_flag() {
        assert_parses(["sg", "search", "https://example.com", "--include-refs"]);
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
