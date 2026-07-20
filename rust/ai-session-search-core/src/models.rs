use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub(crate) const MAX_FUZZY_RESULT_WINDOW: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    #[serde(rename = "claude-desktop")]
    #[clap(name = "claude-desktop", alias = "claude_desktop")]
    ClaudeDesktop,
    Codex,
    Cursor,
    Antigravity,
    Pi,
    #[serde(rename = "aistudio")]
    #[clap(name = "aistudio", alias = "ai-studio")]
    AiStudio,
    #[serde(rename = "gemini-cli")]
    #[clap(name = "gemini-cli", alias = "gemini_cli")]
    GeminiCli,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::ClaudeDesktop => "claude-desktop",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::Antigravity => "antigravity",
            Self::Pi => "pi",
            Self::AiStudio => "aistudio",
            Self::GeminiCli => "gemini-cli",
        }
    }

    /// Human-readable session-source name for CLI, MCP, and documentation surfaces.
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::ClaudeDesktop => "Claude Desktop local agent",
            Self::Codex => "Codex",
            Self::Cursor => "Cursor",
            Self::Antigravity => "Antigravity",
            Self::Pi => "Pi coding agent",
            Self::AiStudio => "Google AI Studio",
            Self::GeminiCli => "Gemini CLI",
        }
    }

    /// Whether this source has a native CLI command that reopens a recorded session.
    pub const fn supports_native_resume(self) -> bool {
        matches!(self, Self::Claude | Self::Codex | Self::Pi)
    }

    /// Parse a `provider` value read back from the index. These columns are written from
    /// [`Provider::as_str`], so a parse failure means index corruption or a variant added without a
    /// migration — a "can't happen unless there's a bug" case. `debug_assert!` makes that loud in
    /// dev/test (and CI) while release degrades to `Claude` rather than aborting a whole query over
    /// one bad row. Prefer this over `parse().unwrap_or(...)` so the invariant is not silent.
    pub fn from_db_str(value: &str) -> Self {
        value.parse().unwrap_or_else(|_| {
            debug_assert!(false, "unrecognized provider in index: {value:?}");
            Self::Claude
        })
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Provider {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "claude" => Ok(Self::Claude),
            "claude-desktop" | "claude_desktop" | "claudedesktop" => Ok(Self::ClaudeDesktop),
            "codex" => Ok(Self::Codex),
            "cursor" => Ok(Self::Cursor),
            "antigravity" => Ok(Self::Antigravity),
            "pi" => Ok(Self::Pi),
            "aistudio" | "ai-studio" | "ai_studio" => Ok(Self::AiStudio),
            "gemini-cli" | "gemini_cli" | "geminicli" => Ok(Self::GeminiCli),
            // Canonical spellings only: the hyphen/underscore aliases accepted above are
            // conveniences, so listing them here would imply four names for one provider.
            other => Err(format!(
                "unsupported provider: {other} — must be one of \"claude\", \"claude-desktop\", \"codex\", \"cursor\", \"antigravity\", \"pi\", \"aistudio\", \"gemini-cli\""
            )),
        }
    }
}

/// Normalized, closed message-role vocabulary shared by every provider adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    Tool,
    Slash,
    Compaction,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
            Self::Slash => "slash",
            Self::Compaction => "compaction",
        }
    }

    /// Parse a `role` value read back from the index. Written from [`Role::as_str`], so a failure
    /// means index corruption or a variant added without a migration. `debug_assert!` makes that
    /// loud in dev/test/CI; release degrades to `User` rather than aborting a whole query over one
    /// bad row. Prefer over `parse().unwrap_or(...)` so the round-trip invariant is not silent.
    pub fn from_db_str(value: &str) -> Self {
        value.parse().unwrap_or_else(|_| {
            debug_assert!(false, "unrecognized role in index: {value:?}");
            Self::User
        })
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Role {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "user" => Ok(Self::User),
            "assistant" => Ok(Self::Assistant),
            "tool" => Ok(Self::Tool),
            "slash" => Ok(Self::Slash),
            "compaction" => Ok(Self::Compaction),
            // Name the accepted values: this error reaches Python callers directly, where there is
            // no schema enum or clap suggestion to fall back on.
            other => Err(format!(
                "unknown role: {other} — must be one of \"user\", \"assistant\", \"tool\", \"slash\", \"compaction\""
            )),
        }
    }
}

/// A single conversation turn persisted per session (the unit of message-level analytics).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "kebab-case")]
pub enum MessageKind {
    Conversation,
    Compaction,
    ToolCall,
    ToolResult,
    Unknown,
}

/// Content-matching strategy shared by programmatic message-search clients.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum MessageSearchMode {
    #[default]
    Exact,
    Regex,
    Fuzzy,
}

impl MessageSearchMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Regex => "regex",
            Self::Fuzzy => "fuzzy",
        }
    }
}

impl std::str::FromStr for MessageSearchMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "exact" => Ok(Self::Exact),
            "regex" => Ok(Self::Regex),
            "fuzzy" => Ok(Self::Fuzzy),
            other => Err(format!(
                "unknown message search mode: {other}; expected exact, regex, or fuzzy"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "kebab-case")]
pub enum SearchField {
    Content,
    ToolName,
    ToolArgument,
}

impl SearchField {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Content => "content",
            Self::ToolName => "tool_name",
            Self::ToolArgument => "tool_argument",
        }
    }
}

impl std::str::FromStr for SearchField {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().replace('-', "_").as_str() {
            "content" => Ok(Self::Content),
            "tool_name" => Ok(Self::ToolName),
            "tool_argument" => Ok(Self::ToolArgument),
            other => Err(format!(
                "unknown message search field: {other} — must be one of \"content\", \"tool_name\", \"tool_argument\""
            )),
        }
    }
}

impl MessageKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Compaction => "compaction",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_db_str(value: &str) -> Self {
        match value {
            "conversation" => Self::Conversation,
            "compaction" => Self::Compaction,
            "tool_call" => Self::ToolCall,
            "tool_result" => Self::ToolResult,
            _ => Self::Unknown,
        }
    }
}

impl std::fmt::Display for MessageKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for MessageKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().replace('-', "_").as_str() {
            "conversation" => Ok(Self::Conversation),
            "compaction" => Ok(Self::Compaction),
            "tool_call" => Ok(Self::ToolCall),
            "tool_result" => Ok(Self::ToolResult),
            "unknown" => Ok(Self::Unknown),
            other => Err(format!(
                "unknown message kind: {other} — must be one of \"conversation\", \"compaction\", \"tool_call\", \"tool_result\", \"unknown\""
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub seq: i64,
    pub role: Role,
    pub ts: Option<DateTime<Utc>>,
    pub tool_name: Option<String>,
    pub kind: MessageKind,
    pub tool_call_id: Option<String>,
    pub is_compaction: bool,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    pub provider: Provider,
    pub provider_session_id: String,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub cwd: Option<String>,
    pub repo_root: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub last_message_at: Option<DateTime<Utc>>,
    pub preview_text: String,
    pub source_path: String,
    pub message_count: Option<i64>,
    pub parse_version: String,
    pub raw_metadata_json: Option<String>,
    pub parse_warning: Option<String>,
    pub discovery_source: String,
}

/// Opaque keyset cursor for a bounded session-analysis document scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AnalysisCursor(String);

impl AnalysisCursor {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn after(session_id: String) -> Self {
        Self(session_id)
    }
}

/// Provider-normalized input document for outward analysis pipelines.
#[derive(Debug, Clone, Serialize)]
pub struct AnalysisDocument {
    pub session: SessionRecord,
    pub user_text: String,
    pub first_user_text: Option<String>,
    pub message_count: i64,
    pub user_message_count: i64,
}

/// One bounded keyset page of provider-normalized analysis documents.
#[derive(Debug, Clone, Serialize)]
pub struct AnalysisDocumentPage {
    pub documents: Vec<AnalysisDocument>,
    pub next_cursor: Option<AnalysisCursor>,
}

/// A single file-mutating tool call (`Write`/`Edit`/`MultiEdit`/`NotebookEdit`)
/// extracted from an assistant turn. Threaded through [`ParsedSession`] like
/// [`Message`], persisted to the `file_edits` table, and replayed to reconstruct
/// historical file content (`files extract`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEdit {
    /// Monotonic order within the session (independent of message seq).
    pub seq: i64,
    pub ts: Option<DateTime<Utc>>,
    /// Originating tool name (`Write`|`Edit`|`MultiEdit`|`NotebookEdit`).
    pub tool: String,
    pub file_path: String,
    /// Basename of `file_path`, denormalized for fast glob/search.
    pub file_name: String,
    /// Full file content — present only for `Write` (a full snapshot / replay base).
    pub new_content: Option<String>,
    /// `old_string`→`new_string` replacements for `Edit`/`MultiEdit`; empty otherwise.
    pub edits: Vec<EditOp>,
}

/// One `old_string`→`new_string` replacement from an `Edit`/`MultiEdit` tool call.
/// `replace_all` mirrors Claude's `Edit` flag: when true the replacement is applied to
/// every occurrence, otherwise only the first (which is also the only one, since a
/// non-`replace_all` `Edit` requires `old_string` to be unique).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditOp {
    pub old: String,
    pub new: String,
    /// Replace every occurrence (Claude `Edit`/`MultiEdit` `replace_all: true`).
    #[serde(default)]
    pub replace_all: bool,
}

impl EditOp {
    /// Construct a first-occurrence (non-`replace_all`) edit.
    pub fn new(old: impl Into<String>, new: impl Into<String>) -> Self {
        Self {
            old: old.into(),
            new: new.into(),
            replace_all: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedSession {
    pub session: SessionRecord,
    pub transcript_text: String,
    /// Per-message rows persisted to the `messages` table.
    pub messages: Vec<Message>,
    /// File-mutating tool calls persisted to the `file_edits` table (file-version recovery).
    pub file_edits: Vec<FileEdit>,
}

#[derive(Debug, Clone)]
pub struct SourceFile {
    pub provider: Provider,
    pub path: std::path::PathBuf,
    pub mtime_ns: i64,
    pub size_bytes: i64,
}

#[derive(Debug, Clone)]
pub struct SearchFilters {
    pub provider: Option<Provider>,
    pub path_prefix: Option<String>,
    pub exclude_path_prefixes: Vec<String>,
    pub exclude_session_ids: Vec<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub limit: usize,
    pub warnings_only: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionWithTranscript {
    #[serde(flatten)]
    pub session: SessionRecord,
    pub transcript_text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    #[serde(flatten)]
    pub session: SessionRecord,
    pub score: i64,
    pub match_source: String,
    pub match_snippet: String,
}

/// Filters for message-level search (`messages search`, analytics). Exact/regex `limit == 0`
/// means unlimited; fuzzy validation requires a positive finite page.
#[derive(Debug, Clone, Default)]
pub struct MessageFilters {
    pub role: Option<Role>,
    pub kind: Option<MessageKind>,
    /// The query searches only `field`: message content, canonical `tool_name`, or the canonical
    /// tool argument selected by [`MessageFilters::argument_path`].
    pub field: Option<SearchField>,
    /// RFC 6901 JSON pointer relative to the canonical tool-call `args` value.
    pub argument_path: Option<String>,
    /// Restrict to one indexed session source: claude, claude-desktop, codex, cursor,
    /// antigravity, pi, aistudio, or gemini-cli.
    pub provider: Option<Provider>,
    /// Exact session id, used after CLI commands resolve a user-supplied id/prefix.
    /// This avoids substring filters accidentally merging sessions in `messages get`
    /// and `messages timeline`.
    pub session_id: Option<String>,
    /// Restrict to messages whose session's `cwd`, `repo_root`, or source transcript starts with this
    /// prefix — the message-level analogue of [`SearchFilters::path_prefix`]. Applied
    /// as a subquery against `sessions` in `append_message_filters` (the `sessions`
    /// table is tiny relative to `messages`, so no dedicated index is needed).
    pub path_prefix: Option<String>,
    /// Exclude messages whose session's `cwd`, `repo_root`, or source transcript path starts
    /// with any of these normalized prefixes. Applied before limits/context expansion.
    pub exclude_path_prefixes: Vec<String>,
    /// Exclude exact session ids. Applied before limits/context expansion.
    pub exclude_session_ids: Vec<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    /// Lower inclusive message sequence bound. Only meaningful within one or more scoped
    /// sessions because `seq` is local to each session.
    pub seq_from: Option<i64>,
    /// Upper inclusive message sequence bound. Only meaningful within one or more scoped
    /// sessions because `seq` is local to each session.
    pub seq_to: Option<i64>,
    /// How the separate query string is interpreted. Exact is a case-insensitive literal, Regex
    /// uses Rust regex syntax, and Fuzzy uses bounded candidate retrieval followed by Nucleo's
    /// fzf-style sequence score. Fuzzy is approximate retrieval, not exhaustive edit distance.
    pub match_mode: MessageSearchMode,
    /// Optional case-insensitive substring filter on a tool message's canonical `tool_name`,
    /// independent of `field` (e.g. `exec` matches Codex `exec_command`; `edit` matches Claude
    /// `Edit` and `MultiEdit`).
    pub tool: Option<String>,
    pub no_compaction: bool,
    pub limit: usize,
    pub offset: usize,
}

impl MessageFilters {
    /// Validate invariants shared by CLI, MCP, Rust, and language bindings.
    ///
    /// `query` is interpreted according to [`MessageFilters::match_mode`].
    pub fn validate(&self, query: &str) -> anyhow::Result<()> {
        use anyhow::{bail, ensure};

        if self.match_mode == MessageSearchMode::Fuzzy {
            ensure!(
                query.chars().take(3).count() >= 3,
                "fuzzy search requires at least 3 characters; use exact search for shorter text"
            );
            ensure!(
                self.limit > 0,
                "fuzzy search requires a finite non-zero limit; exact search supports unlimited results"
            );
            let window = self.offset.checked_add(self.limit).ok_or_else(|| {
                anyhow::anyhow!("fuzzy offset + limit exceeds the supported result window")
            })?;
            ensure!(
                window <= MAX_FUZZY_RESULT_WINDOW,
                "fuzzy offset + limit must be <= {MAX_FUZZY_RESULT_WINDOW}; narrow the page or use exact search"
            );
        }
        ensure!(
            !query.is_empty() || self.match_mode == MessageSearchMode::Exact,
            "match_mode={} requires a non-empty query",
            self.match_mode.as_str()
        );
        let offset = i64::try_from(self.offset)
            .map_err(|_| anyhow::anyhow!("offset exceeds SQLite's signed 64-bit limit"))?;
        if self.limit > 0 {
            let limit = i64::try_from(self.limit)
                .map_err(|_| anyhow::anyhow!("limit exceeds SQLite's signed 64-bit limit"))?;
            offset.checked_add(limit).ok_or_else(|| {
                anyhow::anyhow!("offset + limit exceeds SQLite's signed 64-bit limit")
            })?;
        }

        if self.seq_from.is_some() || self.seq_to.is_some() {
            if self.seq_from.is_some_and(|seq| seq < 0) || self.seq_to.is_some_and(|seq| seq < 0) {
                bail!("seq_from and seq_to must be non-negative");
            }
            if let (Some(from), Some(to)) = (self.seq_from, self.seq_to) {
                ensure!(from <= to, "seq_from must be <= seq_to");
            }
        }

        let field = self.field.unwrap_or(SearchField::Content);
        if field == SearchField::ToolArgument {
            let pointer = self
                .argument_path
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("tool-argument search requires argument_path"))?;
            ensure!(
                pointer.is_empty() || pointer.starts_with('/'),
                "argument_path must be an RFC 6901 JSON pointer starting with '/'"
            );
            ensure!(
                !self.kind.is_some_and(|kind| kind != MessageKind::ToolCall),
                "tool-argument search is only compatible with kind=tool_call"
            );
        } else if self.argument_path.is_some() {
            bail!("argument_path requires field=tool_argument");
        }
        Ok(())
    }

    /// True when at least one structural predicate (role / provider / session ID / path / time window /
    /// tool / no-compaction) restricts the SQL row set BEFORE content matching. `regex`, `rank`
    /// and `limit` are NOT structural — they filter/order content, not the scanned corpus. Used
    /// by `search_messages` to decide whether the content prefilter/scorer is worth querying:
    /// when a structural filter already narrows the corpus to a small slice, a direct scan of
    /// that slice beats intersecting against the whole-corpus trigram index.
    pub fn narrows_corpus(&self) -> bool {
        self.role.is_some()
            || self.kind.is_some()
            || self.provider.is_some()
            || self.session_id.is_some()
            || self.path_prefix.is_some()
            || !self.exclude_path_prefixes.is_empty()
            || !self.exclude_session_ids.is_empty()
            || self.since.is_some()
            || self.until.is_some()
            || self.seq_from.is_some()
            || self.seq_to.is_some()
            || self.tool.is_some()
            || self.no_compaction
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageHit {
    pub session_id: String,
    pub provider: Provider,
    pub seq: i64,
    pub role: Role,
    pub kind: MessageKind,
    pub ts: Option<DateTime<Utc>>,
    /// The tool that produced a `Role::Tool` message (e.g. `Bash`, `exec_command`), else None.
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fuzzy_score: Option<u32>,
    pub content: String,
}

/// Lightweight per-session metadata used to enrich message hits with human-readable
/// context (working dir / repo / title) in the MCP `search_messages` response, so an
/// agent can interpret and group results without a follow-up `get_session` per hit.
/// Kept off [`MessageHit`] so the CLI table rendering is unchanged; the MCP layer joins
/// it on via [`crate::db::Db::session_metadata`].
#[derive(Debug, Clone, Default, Serialize)]
pub struct SessionMeta {
    pub provider_session_id: Option<String>,
    pub cwd: Option<String>,
    pub repo_root: Option<String>,
    pub title: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
    pub last_message_at: Option<DateTime<Utc>>,
    pub message_count: Option<i64>,
    pub parse_warning: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderParserHealth {
    pub provider: Provider,
    pub expected_parse_version: String,
    pub indexed_sessions: i64,
    pub current_sessions: i64,
    pub stale_sessions: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ParserHealth {
    pub schema_version: i64,
    pub expected_schema_version: i64,
    pub schema_current: bool,
    pub indexed_sessions: i64,
    pub current_sessions: i64,
    pub stale_sessions: i64,
    pub parse_warnings: i64,
    pub providers: Vec<ProviderParserHealth>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexUpdateState {
    InProgress,
    AttentionRequired,
}

impl IndexUpdateState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::AttentionRequired => "attention_required",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexUpdateStatus {
    pub state: IndexUpdateState,
    pub started_at: DateTime<Utc>,
    pub message: String,
    pub next_command: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexStatus {
    pub parser_health: ParserHealth,
    /// Stale sessions whose source files are currently enabled and discoverable.
    pub repairable_stale_sessions: i64,
    /// Stale sessions retained in the index whose source files are not currently discoverable.
    pub unavailable_stale_sessions: i64,
    pub repair_commands: Vec<String>,
    /// Actionable automatic index-update state; normal completed work stays silent.
    pub index_update: Option<IndexUpdateStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionTimeProfile {
    pub messages: i64,
    pub timestamped_messages: i64,
    pub undated_messages: i64,
    pub first_timestamp: Option<DateTime<Utc>>,
    pub last_timestamp: Option<DateTime<Utc>>,
    pub observed_span_seconds: Option<i64>,
    pub max_message_gap_seconds: Option<i64>,
    pub tool_calls: i64,
    pub tool_results: i64,
}

/// Cost breakdown for `messages search --explain`: how much the trigram prefilter narrows
/// the scan before literal/regex verification. A `candidates` count close to `corpus`
/// explains a slow content query because the prefilter barely narrowed the scan.
#[derive(Debug, Clone)]
pub struct SearchExplain {
    /// Trigram query derived from literal text or regex literals. `None` means the query has no
    /// >=3-char literal anchor, so it must scan the structurally-filtered corpus.
    pub prefilter: Option<String>,
    /// Rows the literal/regex verifier must check after the trigram prefilter.
    /// `None` when there is no usable prefilter or the prefilter was intentionally skipped.
    pub candidates: Option<i64>,
    /// Why an available prefilter was intentionally skipped, usually because structured filters
    /// already narrowed the corpus enough that a direct scan is cheaper.
    pub prefilter_skipped: Option<String>,
    /// True when an indexed candidate source had more rows than its explicit admission budget.
    /// Results remain bounded and deterministic, but callers should narrow structural filters when
    /// they require better recall from a highly common fuzzy fragment.
    pub candidate_source_saturated: bool,
    /// Rows matching the structural filters (role/provider/session/date) — the
    /// selectivity denominator.
    pub corpus: i64,
}

impl SearchExplain {
    /// One-line (two for content search) human-readable selectivity summary for
    /// `messages search --explain`, written to stderr so it never pollutes the
    /// parseable stdout. `has_content_query` distinguishes a query with no usable
    /// >=3-char anchor from an empty search (structural filters only).
    pub fn summary(&self, has_content_query: bool) -> String {
        match (&self.prefilter, self.candidates) {
            (None, Some(candidates)) if has_content_query && self.prefilter_skipped.is_some() => {
                format!(
                    "[explain] {} / {} corpus rows matched after {}",
                    candidates,
                    self.corpus,
                    self.prefilter_skipped
                        .as_deref()
                        .unwrap_or("content scoring")
                )
            }
            (Some(prefilter), Some(candidates)) => {
                let pct = if self.corpus > 0 {
                    100.0 * candidates as f64 / self.corpus as f64
                } else {
                    0.0
                };
                let hint = if pct >= 50.0 {
                    "  — low selectivity; anchor the regex on a rarer literal substring"
                } else {
                    ""
                };
                let saturation = if self.candidate_source_saturated {
                    "\n[explain] candidate source reached its admission budget; narrow provider, session, path, role, kind, or date filters for better fuzzy recall"
                } else {
                    ""
                };
                format!(
                    "[explain] trigram prefilter: {prefilter}\n\
                     [explain] candidates: {candidates} / {} corpus rows ({pct:.1}%) to verify{hint}{saturation}",
                    self.corpus
                )
            }
            (Some(prefilter), None) if has_content_query && self.prefilter_skipped.is_some() => {
                format!(
                    "[explain] trigram prefilter available: {prefilter}\n\
                 [explain] skipped trigram prefilter: {}; direct scan of {} corpus rows",
                    self.prefilter_skipped.as_deref().unwrap_or("not used"),
                    self.corpus
                )
            }
            _ if has_content_query => format!(
                "[explain] query has no >=3-char literal anchor → full scan of {} corpus rows",
                self.corpus
            ),
            _ => format!(
                "[explain] {} corpus rows; no content query was provided",
                self.corpus
            ),
        }
    }
}

/// A user message that matched a correction pattern.
#[derive(Debug, Clone, Serialize)]
pub struct CorrectionMatch {
    pub session_id: String,
    pub provider: Provider,
    pub ts: Option<DateTime<Utc>>,
    pub category: String,
    pub matched_pattern: String,
    pub content: String,
}

/// Aggregate slash-command usage frequency.
#[derive(Debug, Clone, Serialize)]
pub struct PlanningCount {
    pub command: String,
    pub count: i64,
    pub unique_sessions: i64,
    pub unique_projects: i64,
}

/// Structured filters for the `files` query surface (search / cross-ref).
/// `pattern` is a glob (`*`/`?`) over the basename, or over the full path when it
/// contains a `/`. `limit == 0` means unlimited; `offset` skips rows in the surface's
/// deterministic order.
#[derive(Debug, Clone, Default)]
pub struct FileQuery {
    pub pattern: Option<String>,
    /// Restrict to one indexed session source.
    pub provider: Option<Provider>,
    /// Exact canonical session ID. Prefer this when chaining from session/message search output.
    pub session_id: Option<String>,
    /// Restrict to sessions whose cwd, repo root, or source transcript starts with this prefix.
    pub path_prefix: Option<String>,
    /// Exclude edits from sessions whose cwd, repo root, or source transcript starts with a prefix.
    /// Applied before grouping, reconstruction selection, and result limits.
    pub exclude_path_prefixes: Vec<String>,
    /// Exclude exact canonical session IDs before grouping, reconstruction selection, and limits.
    pub exclude_session_ids: Vec<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub min_edits: Option<i64>,
    pub max_edits: Option<i64>,
    /// Maximum rows returned by file search, cross-reference, and history. Zero explicitly means
    /// unlimited and may materialize the complete filtered result.
    pub limit: usize,
    /// Rows skipped after the surface's documented deterministic ordering.
    pub offset: usize,
}

/// One aggregate row per file across the filtered edit set (`files search`).
#[derive(Debug, Clone, Serialize)]
pub struct FileEditSummary {
    pub file_path: String,
    pub file_name: String,
    pub edits: i64,
    pub sessions: i64,
    pub last_edited: Option<DateTime<Utc>>,
}

/// One reconstructed version (edit) of a file within a session (`files history`).
#[derive(Debug, Clone, Serialize)]
pub struct FileVersion {
    pub session_id: String,
    pub provider: Provider,
    pub version: i64,
    pub tool: String,
    pub ts: Option<DateTime<Utc>>,
    pub lines: i64,
    pub file_path: String,
}

/// A file ↔ session linkage with that pair's edit count (`files cross-ref`).
#[derive(Debug, Clone, Serialize)]
pub struct FileCrossRef {
    pub file_path: String,
    pub session_id: String,
    pub provider: Provider,
    pub edits: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderHealth {
    pub provider: Provider,
    pub enabled: bool,
    pub cli_available: bool,
    pub roots: Vec<String>,
    pub discovered_files: usize,
    pub indexed_sessions: i64,
    pub expected_parse_version: String,
    pub current_sessions: i64,
    pub stale_sessions: i64,
    pub repairable_stale_sessions: i64,
    pub unavailable_stale_sessions: i64,
    pub resume_command: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticStatus {
    pub db_path: String,
    #[serde(flatten)]
    pub index_status: IndexStatus,
    pub providers: Vec<ProviderHealth>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_parsed_vocabulary_names_its_accepted_values_when_rejecting_input() {
        // These `FromStr` errors surface directly to Python callers, who have neither the MCP
        // schema's enum nor clap's `[possible values: ...]` to fall back on, so the accepted set
        // has to live in the message itself. Asserting all four together keeps a newly added
        // vocabulary from silently reverting to a bare rejection.
        for (rejected, accepted) in [
            (
                "usr".parse::<Role>().unwrap_err(),
                vec!["user", "assistant", "tool", "slash", "compaction"],
            ),
            (
                "contnet".parse::<SearchField>().unwrap_err(),
                vec!["content", "tool_name", "tool_argument"],
            ),
            (
                "chatgpt".parse::<Provider>().unwrap_err(),
                vec![
                    "claude",
                    "claude-desktop",
                    "codex",
                    "cursor",
                    "antigravity",
                    "pi",
                    "aistudio",
                    "gemini-cli",
                ],
            ),
            (
                "converstaion".parse::<MessageKind>().unwrap_err(),
                vec![
                    "conversation",
                    "compaction",
                    "tool_call",
                    "tool_result",
                    "unknown",
                ],
            ),
        ] {
            assert!(
                rejected.contains("must be one of"),
                "no accepted-value list in: {rejected}"
            );
            for value in accepted {
                assert!(
                    rejected.contains(&format!("{value:?}")),
                    "{value} missing from: {rejected}"
                );
            }
        }
    }

    #[test]
    fn fuzzy_message_validation_rejects_short_queries_before_database_work() {
        let filters = MessageFilters {
            match_mode: MessageSearchMode::Fuzzy,
            limit: 5,
            ..Default::default()
        };

        for query in ["", "a", "éx"] {
            let error = filters.validate(query).unwrap_err().to_string();
            assert!(
                error.contains("at least 3 characters"),
                "unexpected error for {query:?}: {error}"
            );
            assert!(error.contains("exact"), "{error}");
        }
        assert!(filters.validate("éx!").is_ok());
    }

    #[test]
    fn fuzzy_message_validation_requires_a_bounded_result_window() {
        let unlimited = MessageFilters {
            match_mode: MessageSearchMode::Fuzzy,
            ..Default::default()
        };
        assert!(unlimited
            .validate("query")
            .unwrap_err()
            .to_string()
            .contains("finite non-zero limit"));

        let oversized = MessageFilters {
            match_mode: MessageSearchMode::Fuzzy,
            offset: MAX_FUZZY_RESULT_WINDOW,
            limit: 1,
            ..Default::default()
        };
        assert!(oversized
            .validate("query")
            .unwrap_err()
            .to_string()
            .contains("must be <="));
    }

    #[test]
    fn message_page_validation_rejects_values_sqlite_cannot_bind() {
        let filters = MessageFilters {
            offset: i64::MAX as usize,
            limit: 1,
            ..Default::default()
        };
        assert!(filters
            .validate("")
            .unwrap_err()
            .to_string()
            .contains("offset + limit"));

        if let Some(too_large) = usize::try_from(i64::MAX)
            .ok()
            .and_then(|maximum| maximum.checked_add(1))
        {
            let filters = MessageFilters {
                offset: too_large,
                ..Default::default()
            };
            assert!(filters
                .validate("")
                .unwrap_err()
                .to_string()
                .contains("offset exceeds"));
        }
    }

    #[test]
    fn every_provider_has_a_concrete_display_name_and_resume_contract() {
        let providers = crate::source::PROVIDERS;
        assert!(providers
            .iter()
            .all(|provider| !provider.display_name().trim().is_empty()));
        assert_eq!(
            providers
                .into_iter()
                .filter(|provider| provider.supports_native_resume())
                .map(Provider::as_str)
                .collect::<Vec<_>>(),
            ["claude", "codex", "pi"]
        );
    }

    #[test]
    fn explain_summary_reports_prefilter_and_selectivity_pct() {
        let ex = SearchExplain {
            prefilter: Some("\"abc\"".to_string()),
            candidates: Some(80),
            prefilter_skipped: None,
            candidate_source_saturated: false,
            corpus: 100,
        };
        let s = ex.summary(true);
        assert!(s.contains("trigram prefilter: \"abc\""), "{s}");
        assert!(s.contains("80 / 100 corpus rows (80.0%)"), "{s}");
        // 80% candidates is non-selective → the slow-query hint must fire.
        assert!(s.contains("low selectivity"), "{s}");
    }

    #[test]
    fn explain_summary_omits_hint_when_prefilter_is_selective() {
        let ex = SearchExplain {
            prefilter: Some("\"rareword\"".to_string()),
            candidates: Some(2),
            prefilter_skipped: None,
            candidate_source_saturated: false,
            corpus: 1000,
        };
        let s = ex.summary(true);
        assert!(s.contains("2 / 1000 corpus rows (0.2%)"), "{s}");
        assert!(
            !s.contains("low selectivity"),
            "selective query gets no hint: {s}"
        );
    }

    #[test]
    fn explain_summary_flags_regex_without_literal_anchor() {
        let ex = SearchExplain {
            prefilter: None,
            candidates: None,
            prefilter_skipped: None,
            candidate_source_saturated: false,
            corpus: 500,
        };
        let s = ex.summary(true);
        assert!(s.contains("no >=3-char literal anchor"), "{s}");
        assert!(s.contains("full scan of 500 corpus rows"), "{s}");
    }

    #[test]
    fn explain_summary_notes_no_content_query_for_empty_searches() {
        let ex = SearchExplain {
            prefilter: None,
            candidates: None,
            prefilter_skipped: None,
            candidate_source_saturated: false,
            corpus: 42,
        };
        let s = ex.summary(false);
        assert!(s.contains("42 corpus rows"), "{s}");
        assert!(s.contains("no content query was provided"), "{s}");
    }

    #[test]
    fn explain_summary_handles_empty_corpus_without_dividing_by_zero() {
        let ex = SearchExplain {
            prefilter: Some("\"x\"".to_string()),
            candidates: Some(0),
            prefilter_skipped: None,
            candidate_source_saturated: false,
            corpus: 0,
        };
        let s = ex.summary(true);
        assert!(s.contains("0 / 0 corpus rows (0.0%)"), "{s}");
    }

    #[test]
    fn explain_summary_reports_intentional_prefilter_skip() {
        let ex = SearchExplain {
            prefilter: Some("\"rare\"".to_string()),
            candidates: None,
            prefilter_skipped: Some("corpus below configured threshold".to_string()),
            candidate_source_saturated: false,
            corpus: 25,
        };
        let s = ex.summary(true);
        assert!(s.contains("trigram prefilter available"), "{s}");
        assert!(s.contains("skipped trigram prefilter"), "{s}");
        assert!(s.contains("direct scan of 25 corpus rows"), "{s}");
    }

    #[test]
    fn explain_summary_reports_candidate_saturation_separately_from_prefilter_skip() {
        let ex = SearchExplain {
            prefilter: Some("SQLite word FTS + trigram-overlap union".to_string()),
            candidates: Some(1200),
            prefilter_skipped: None,
            candidate_source_saturated: true,
            corpus: 10_000,
        };

        let summary = ex.summary(true);

        assert!(
            summary.contains("reached its admission budget"),
            "{summary}"
        );
        assert!(summary.contains("better fuzzy recall"), "{summary}");
        assert!(!summary.contains("skipped trigram prefilter"), "{summary}");
    }
}
