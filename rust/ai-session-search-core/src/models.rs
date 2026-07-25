use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum)]
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

/// How a request selects message classes in SQL. Two shapes rather than one because they are
/// not equivalent over rows whose stored `kind` is outside the current enum: `AllExcept`
/// keeps them, `Only` does not. See [`MessageFilters::kind_predicate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KindPredicate {
    /// Caller named no set: return everything except these classes.
    AllExcept(Vec<MessageKind>),
    /// Caller named a set: return exactly these classes.
    Only(Vec<MessageKind>),
}

/// A single conversation turn persisted per session (the unit of message-level analytics).
///
/// PATTERN: adding a variant here is the whole cost of adding a message class. It reaches the
/// MCP schema through `mcp_server::message_kind_values`, the CLI through clap, and the default
/// result set through [`MessageKind::default_search_set`], all derived from these variants.
/// Do not pair a new variant with an `include_<variant>` flag or an `is_<variant>` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "kebab-case")]
pub enum MessageKind {
    Conversation,
    Compaction,
    ToolCall,
    ToolResult,
    /// A message the harness injected into the transcript, addressed to the agent rather
    /// than written by the user or the model: Stop-hook feedback, PreToolUse blocks,
    /// local-command caveats and stdout, task notifications. Not user prose, so it is
    /// excluded from results by default, but it is the ONLY record of what a hook told an
    /// agent and what the agent did next, so it is stored rather than discarded.
    HarnessNotice,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum)]
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
            Self::HarnessNotice => "harness_notice",
            Self::Unknown => "unknown",
        }
    }

    /// Classes returned when a caller names no set: everything the user or the model wrote,
    /// plus tool traffic. `HarnessNotice` is excluded because it is the harness talking to the
    /// agent, so including it by default would change every existing result and skew the
    /// user-prose analytics (`corrections`, `repeats`, `vocab`) built on message content.
    pub fn default_search_set() -> Vec<Self> {
        use clap::ValueEnum;
        Self::value_variants()
            .iter()
            .copied()
            .filter(|kind| *kind != Self::HarnessNotice)
            .collect()
    }

    /// Parse a `kind` value read back from the index, mapping anything unrecognized to
    /// [`MessageKind::Unknown`].
    ///
    /// DELIBERATELY UNLIKE [`Provider::from_db_str`] and [`Role::from_db_str`], which
    /// `debug_assert!` on an unrecognized spelling because theirs can only come from corruption.
    /// This column is different in kind: `Unknown` is a real variant that exists precisely to
    /// absorb spellings this build does not know, and older index versions legitimately store
    /// values outside the current [`MessageKind::as_str`] set — reading them is exactly what the
    /// self-healing migration path does. Asserting here turns "opened an older index" into a
    /// panic; a proven case is `cli_search_self_heals_v4_hybrid_missing_trigram_from_intact_messages`
    /// against rows storing `kind = 'message'`.
    ///
    /// What this still fixes is the duplication: delegating to [`FromStr`] removes the second,
    /// hand-maintained match list where a newly added variant could silently fall through to
    /// `Unknown` forever. The round-trip test over every variant is what guards that.
    pub fn from_db_str(value: &str) -> Self {
        value.parse().unwrap_or(Self::Unknown)
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
            "harness_notice" => Ok(Self::HarnessNotice),
            "unknown" => Ok(Self::Unknown),
            other => Err(format!(
                "unknown message kind: {other} — must be one of \"conversation\", \"compaction\", \"tool_call\", \"tool_result\", \"harness_notice\", \"unknown\""
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
    /// The [`SessionRecord::id`] of the session that spawned this one, when this session is a
    /// subagent run.
    ///
    /// `None` for an ordinary top-level session. Every provider marks subagent runs
    /// differently (claude by a `subagents` directory under its parent, codex in a
    /// `thread_spawn` payload, cursor and pi by directory layout), so detection stays
    /// provider-specific while this field is the one shape they all produce. Typed rather than
    /// buried in `raw_metadata_json` so "every subagent of this session" is a query, which is
    /// what made codex's richer spawn data unusable.
    ///
    /// Holds the whole id, provider prefix included, so it reads as the same value the parent
    /// row carries in `id` and needs no rule about stripping a prefix before comparing.
    ///
    /// PATTERN: this names a row that need not exist. Providers keep a spawned run's transcript
    /// after the parent conversation is rotated away — in one project directory, 549 runs
    /// referenced 69 distinct parents and not one of those parent transcripts was still on
    /// disk. Recovering exactly that otherwise-unreachable work is a reason this is indexed, so
    /// do NOT add `references sessions(id)` to this column: the constraint would reject every
    /// such run at insert, and a cascade would delete the runs when a parent is reindexed away.
    pub parent_session_id: Option<String>,
    /// Human-meaningful name for the spawned agent when the provider records one: claude's
    /// `agentType` (`Explore`, `general-purpose`), codex's `agent_nickname`, or the agent's
    /// own directory or file name where that is all a provider records. Display and grouping
    /// only; the link is `parent_session_id`.
    pub agent_label: Option<String>,
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

/// Who started a session — the session-level counterpart of [`MessageKind`], selected the same
/// way through a set rather than a flag per class.
///
/// The two spellings are the providers' own. Codex stores this exact enum on its session
/// metadata as `thread_source`, with the values `user` and `subagent` (167 and 247 of the 414
/// rollouts this was checked against). `subagent` is what every provider that marks the concept
/// calls it: codex's `thread_source` value and `source.subagent` key, the `subagents`
/// directory claude and cursor both write, claude's `subagent_type` Task parameter, and
/// gemini-cli's `subagent_N` author labels. `user` is the counterpart because it is the one
/// reading no provider contradicts — and because a subagent IS an agent, so naming the other
/// class `agent` would make `[agent]` unpredictable, while nothing about a subagent is a user.
///
/// Derived from [`SessionRecord::parent_session_id`] rather than stored, so the classes are
/// exhaustive by construction and no row can fall outside them. That is why selection needs no
/// `AllExcept` shape the way [`KindPredicate`] does: there are no rows of an unknown class.
///
/// PATTERN: adding a variant here is the whole cost of adding a session class. It reaches the
/// CLI through clap's `ValueEnum`, the MCP schema through `mcp_server::session_kind_values`,
/// and the default result set through [`SessionKind::default_search_set`], all derived from
/// these variants. Do not pair a new variant with an `include_<variant>` boolean — one was
/// tried for `MessageKind::HarnessNotice` and reverted; see [`MessageFilters::kinds`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "kebab-case")]
pub enum SessionKind {
    /// A session a person started. Codex's `thread_source: "user"`; claude's
    /// `isSidechain: false`, which held on 143,220 of 143,224 records in top-level transcripts.
    User,
    /// A run some other session spawned, with [`SessionRecord::parent_session_id`] naming the
    /// spawner and [`SessionRecord::agent_label`] naming the kind of agent. Codex's
    /// `thread_source: "subagent"`; claude's `isSidechain: true`. On this machine they
    /// outnumber user-started claude sessions roughly five to one, which is why selecting by
    /// class is worth a parameter.
    Subagent,
}

impl SessionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Subagent => "subagent",
        }
    }

    /// Every class, in declaration order. Derived from the enum so a new variant is included
    /// without editing a second list.
    pub fn all() -> Vec<Self> {
        use clap::ValueEnum;
        Self::value_variants().to_vec()
    }

    /// The classes a request returns when it names none: all of them.
    ///
    /// Unlike [`MessageKind::default_search_set`], which drops harness notices because they are
    /// harness bookkeeping rather than prose, a spawned run holds real work — it is the record
    /// of what a subagent was asked and what it found. Hiding that by default would answer
    /// "what did I do about X" with only half the evidence. A class that IS noise by default
    /// belongs here as an exclusion, the same way `HarnessNotice` is one.
    pub fn default_search_set() -> Vec<Self> {
        Self::all()
    }

    /// SQL that is true for exactly this class, over a `sessions` row bound to `alias`.
    /// Colocated with the variant so adding a class cannot leave its predicate unwritten.
    pub fn sql_predicate(self, alias: &str) -> String {
        match self {
            Self::User => format!("{alias}.parent_session_id is null"),
            Self::Subagent => format!("{alias}.parent_session_id is not null"),
        }
    }
}

impl std::fmt::Display for SessionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SessionKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().replace('-', "_").as_str() {
            "user" => Ok(Self::User),
            "subagent" => Ok(Self::Subagent),
            other => Err(format!(
                "unknown session kind: {other} — must be one of \"user\" (a session you started) or \"subagent\" (a run one of those spawned)"
            )),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchFilters {
    pub provider: Option<Provider>,
    /// Which classes of session to return. `None` selects the default set
    /// ([`SessionKind::default_search_set`]), which is every class.
    ///
    /// PATTERN: this set is the ONLY mechanism for session-class selection, mirroring
    /// [`MessageFilters::kinds`] one level up. Adding a class means adding a [`SessionKind`]
    /// variant, never an `include_<class>` boolean or a `<class>_only` flag beside it.
    pub session_kinds: Option<Vec<SessionKind>>,
    pub path_prefix: Option<String>,
    pub exclude_path_prefixes: Vec<String>,
    pub exclude_session_ids: Vec<String>,
    /// Return only sessions spawned by this exact session id. The link
    /// [`SessionRecord::parent_session_id`] stores is typed precisely so "every subagent of
    /// this session" is an equality match rather than a JSON scan.
    pub parent_session_id: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub limit: usize,
    pub warnings_only: bool,
}

impl SearchFilters {
    /// The session classes this request returns, with the default applied. Every reader goes
    /// through here so a caller-named set and the default cannot diverge.
    pub fn effective_session_kinds(&self) -> Vec<SessionKind> {
        self.session_kinds
            .clone()
            .unwrap_or_else(SessionKind::default_search_set)
    }
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
    /// Which semantic message classes to return. `None` selects the default set
    /// ([`MessageKind::default_search_set`]): every class except `HarnessNotice`, which is
    /// harness output rather than user or model prose.
    ///
    /// PATTERN: this set is the ONLY mechanism for class selection. Adding a class means
    /// adding a `MessageKind` variant, never an `include_<class>` boolean beside it. A
    /// boolean was tried and reverted: it duplicated this field, needed tie-breaking code so
    /// `kind=harness_notice` would not self-cancel into an empty result, and would have cost
    /// one parameter per future class. Related: `no_compaction` below is legacy sugar that
    /// resolves through [`MessageFilters::effective_kinds`], and the `is_compaction` column in
    /// `db.rs` is the same redundancy left in the schema.
    pub kinds: Option<Vec<MessageKind>>,
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
    /// Restrict by session cwd or repository root only. New typed requests use this instead of
    /// the legacy broad `path_prefix` while prerelease adapters are cut over.
    #[doc(hidden)]
    pub workspace_path_prefix: Option<String>,
    /// Restrict by transcript storage path only.
    #[doc(hidden)]
    pub transcript_path_prefix: Option<String>,
    #[doc(hidden)]
    pub exclude_workspace_path_prefixes: Vec<String>,
    #[doc(hidden)]
    pub exclude_transcript_path_prefixes: Vec<String>,
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
    /// uses Rust regex syntax, and Fuzzy exhaustively scores the structurally filtered corpus with
    /// Nucleo's fzf-style sequence score. Fuzzy is sequence matching, not edit distance.
    pub match_mode: MessageSearchMode,
    /// Optional case-insensitive substring filter on a tool message's canonical `tool_name`,
    /// independent of `field` (e.g. `exec` matches Codex `exec_command`; `edit` matches Claude
    /// `Edit` and `MultiEdit`).
    pub tool: Option<String>,
    /// Drop `Compaction` from the effective class set. Retained as a convenience over
    /// [`MessageFilters::kinds`]; both resolve through [`MessageFilters::effective_kinds`], so
    /// they cannot disagree.
    pub no_compaction: bool,
    /// Which session classes the messages may come from. `None` = every class, matching what
    /// `messages search` and `list` already return by default.
    ///
    /// Mirrors [`SearchFilters::session_kinds`] rather than introducing a second spelling, and is
    /// a SET for the reason stated on [`SessionKind`]: an `include_subagent` boolean was the
    /// shape explicitly rejected there. `Some(vec![])` selects no class and therefore matches
    /// nothing, exactly as it already does on `search`.
    ///
    /// `corrections` is the one operation that narrows this by default -- see
    /// [`crate::db::Db::find_corrections`], which forces `User` for the same reason it forces
    /// `Role::User`.
    pub session_kinds: Option<Vec<SessionKind>>,
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
                !query.trim().is_empty(),
                "fuzzy query must contain a non-whitespace character"
            );
            ensure!(
                self.limit > 0,
                "fuzzy search requires a finite non-zero limit; exact search supports unlimited results"
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
            crate::message_search::JsonPointer::new(pointer.to_string()).map_err(|error| {
                anyhow::anyhow!("argument_path must be an RFC 6901 JSON pointer: {error}")
            })?;
            // A named set may restrict further, but it must leave tool_call reachable:
            // searching a tool argument in a set that excludes tool calls matches nothing.
            ensure!(
                self.kinds
                    .as_ref()
                    .is_none_or(|kinds| kinds.contains(&MessageKind::ToolCall)),
                "tool-argument search requires tool_call among the selected kinds"
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
            || self.kinds.is_some()
            || self.provider.is_some()
            || self.session_id.is_some()
            || self.path_prefix.is_some()
            || !self.exclude_path_prefixes.is_empty()
            || self.workspace_path_prefix.is_some()
            || self.transcript_path_prefix.is_some()
            || !self.exclude_workspace_path_prefixes.is_empty()
            || !self.exclude_transcript_path_prefixes.is_empty()
            || !self.exclude_session_ids.is_empty()
            || self.since.is_some()
            || self.until.is_some()
            || self.seq_from.is_some()
            || self.seq_to.is_some()
            || self.tool.is_some()
            || self.no_compaction
            || self.kinds.is_some()
    }

    /// The message classes this filter actually returns, resolved once so every caller and the
    /// SQL builder agree. `kinds` names the set (defaulting to
    /// [`MessageKind::default_search_set`]); `no_compaction` then removes one member. Because
    /// both flow through here, a request cannot ask for a class and exclude it at the same
    /// time, which is what made the earlier per-class booleans return silently empty results.
    pub fn effective_kinds(&self) -> Vec<MessageKind> {
        let mut kinds = self
            .kinds
            .clone()
            .unwrap_or_else(MessageKind::default_search_set);
        if self.no_compaction {
            kinds.retain(|kind| *kind != MessageKind::Compaction);
        }
        kinds
    }

    /// How to express class selection in SQL.
    ///
    /// PATTERN: naming no set must EXCLUDE the unwanted classes, never INCLUDE the known ones.
    /// An inclusion list silently drops any row whose stored `kind` is not a current enum
    /// variant, which is every row written by an older or newer build. That was shipped
    /// briefly and caught by a test inserting `kind = 'message'`: it reintroduced the same
    /// silent omission this filter exists to remove. An explicit set is an inclusion because
    /// the caller named exactly what they want.
    pub fn kind_predicate(&self) -> KindPredicate {
        if self.kinds.is_some() {
            return KindPredicate::Only(self.effective_kinds());
        }
        let mut excluded = vec![MessageKind::HarnessNotice];
        if self.no_compaction {
            excluded.push(MessageKind::Compaction);
        }
        KindPredicate::AllExcept(excluded)
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
    /// Discovered files that produced no session row at all, counted by set difference against
    /// the indexed source paths. This is deliberately NOT `discovered_files - indexed_sessions`:
    /// retained sessions make indexed exceed discovered, so that subtraction measures something
    /// else. A non-zero value means search results are incomplete for those sources.
    pub unindexed_files: i64,
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

/// Cost breakdown for `messages search --explain`. For literal/regex search, `candidates` is the
/// prefilter output that requires verification. For fuzzy search, it is every row that matched
/// during complete-corpus scoring, independent of the requested result page.
#[derive(Debug, Clone, Serialize)]
pub struct SearchExplain {
    /// Trigram query derived from literal text or regex literals. `None` means the query has no
    /// >=3-char literal anchor, so it must scan the structurally-filtered corpus.
    pub prefilter: Option<String>,
    /// Rows requiring literal/regex verification after prefiltering, or rows that matched complete
    /// fuzzy scoring. `None` when no candidate count is available.
    pub candidates: Option<i64>,
    /// Why a prefilter was skipped or was not applicable, including complete fuzzy scoring.
    pub prefilter_skipped: Option<String>,
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
                format!(
                    "[explain] trigram prefilter: {prefilter}\n\
                     [explain] candidates: {candidates} / {} corpus rows ({pct:.1}%) to verify{hint}",
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
    /// The exact substring that matched, not the rule that matched it: this is
    /// `Regex::find(..).as_str()` from the classifier (`db.rs`), so for the rule
    /// `\byou forgot\b` the value is `you forgot`.
    pub matched_text: String,
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
    /// Discovered files for this provider that produced no session row. `discovered_files` and
    /// `indexed_sessions` above come from different subsystems (filesystem discovery and the
    /// index) and are NOT two ends of one subtraction: retained sessions make the second exceed
    /// the first. This field is the reconciliation, computed by set difference.
    pub unindexed_files: i64,
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

    /// Naming no set must EXCLUDE unwanted classes, never INCLUDE the known ones. An inclusion
    /// list drops every row whose stored `kind` is outside the current enum, which is any row
    /// written by another build. That shipped briefly and was caught by an existing test
    /// inserting `kind = 'message'`; this pins the distinction so it cannot return.
    #[test]
    fn default_class_selection_excludes_rather_than_enumerates() {
        let default = MessageFilters::default();
        assert_eq!(
            default.kind_predicate(),
            KindPredicate::AllExcept(vec![MessageKind::HarnessNotice]),
            "the default must be an exclusion so unrecognized kinds survive"
        );

        let no_compaction = MessageFilters {
            no_compaction: true,
            ..Default::default()
        };
        assert_eq!(
            no_compaction.kind_predicate(),
            KindPredicate::AllExcept(vec![MessageKind::HarnessNotice, MessageKind::Compaction]),
            "no_compaction narrows the same exclusion rather than switching to an inclusion"
        );
    }

    /// The session-class spellings are the providers' own, not invented here, and the
    /// alternatives considered are locked out rather than silently accepted.
    ///
    /// Evidence, gathered from live data before choosing:
    /// - codex stores this exact enum as `thread_source` on its session metadata, valued
    ///   `user` (167 rollouts) and `subagent` (247) — the same partition, already named.
    /// - `subagent` is unanimous across every provider that marks the concept: codex's
    ///   `thread_source` value and `source.subagent` key, the `subagents` directory claude and
    ///   cursor both write, claude's `subagent_type` Task parameter (in 15 of 401 transcripts
    ///   scanned), and gemini-cli's `subagent_N` author labels.
    ///
    /// `agent` is rejected on purpose: a subagent IS an agent, so `[agent]` could be read as
    /// "every session" or "every session that is not a subagent", and the caller cannot tell
    /// which from the name. Nothing about a subagent is a user, so `user` has no such reading.
    /// `top_level` is rejected as a different vocabulary from its sibling — it answers where a
    /// session sits rather than who started it. Asserting they do NOT parse, because a
    /// presence-only test would let either come back beside the good spelling.
    #[test]
    fn session_class_spellings_match_the_providers_and_exclude_the_ambiguous_ones() {
        use std::str::FromStr;

        assert_eq!(SessionKind::User.as_str(), "user");
        assert_eq!(SessionKind::Subagent.as_str(), "subagent");
        for kind in SessionKind::all() {
            assert_eq!(
                SessionKind::from_str(kind.as_str()),
                Ok(kind),
                "every spelling this emits must be one it accepts"
            );
        }

        for rejected in [
            "agent",
            "top_level",
            "top-level",
            "sidechain",
            "child",
            "spawned",
        ] {
            let error = SessionKind::from_str(rejected)
                .expect_err(&format!("{rejected} must not be an accepted spelling"));
            assert!(
                error.contains("user") && error.contains("subagent"),
                "a rejection must name both accepted values: {error}"
            );
        }

        // Case and separator tolerance, matching MessageKind's parser.
        assert_eq!(SessionKind::from_str("SubAgent"), Ok(SessionKind::Subagent));
    }

    /// `..SearchFilters::default()` is used throughout the db, service, and tail-parse tests,
    /// which is only sound while the default filters nothing: a field defaulting to a
    /// restrictive value would silently narrow every one of those tests rather than failing
    /// one of them.
    ///
    /// The destructuring is the point. Adding a field to [`SearchFilters`] breaks THIS test to
    /// compile — "pattern does not mention field" — which is the signal the explicit
    /// field-by-field literals in those tests used to give, concentrated in the one place that
    /// can say what to do about it: assert here what the new field's default selects, then
    /// leave the other tests alone.
    #[test]
    fn the_default_session_filter_selects_every_session() {
        let SearchFilters {
            provider,
            session_kinds,
            path_prefix,
            exclude_path_prefixes,
            exclude_session_ids,
            parent_session_id,
            since,
            until,
            limit,
            warnings_only,
        } = SearchFilters::default();

        assert_eq!(provider, None, "no provider named means every provider");
        assert_eq!(session_kinds, None, "no class named means every class");
        assert_eq!(path_prefix, None);
        assert!(exclude_path_prefixes.is_empty());
        assert!(exclude_session_ids.is_empty());
        assert_eq!(parent_session_id, None, "not restricted to one spawner");
        assert_eq!(since, None);
        assert_eq!(until, None);
        assert_eq!(limit, 0, "0 is the unbounded page; callers state their own");
        assert!(!warnings_only);
    }

    /// The default returns both classes. Subagent runs are real work — 4,051 of them against
    /// 858 user-started claude sessions here — so unlike `MessageKind::HarnessNotice`, which
    /// is harness bookkeeping and stays out by default, hiding these would answer "what did I
    /// do about X" with half the evidence.
    #[test]
    fn the_default_session_class_set_is_every_class() {
        assert_eq!(
            SearchFilters::default().effective_session_kinds(),
            vec![SessionKind::User, SessionKind::Subagent]
        );
        assert_eq!(SessionKind::default_search_set(), SessionKind::all());

        // A named set is honored exactly, including the empty one.
        let only_runs = SearchFilters {
            session_kinds: Some(vec![SessionKind::Subagent]),
            ..SearchFilters::default()
        };
        assert_eq!(
            only_runs.effective_session_kinds(),
            vec![SessionKind::Subagent]
        );
        let none = SearchFilters {
            session_kinds: Some(Vec::new()),
            ..SearchFilters::default()
        };
        assert!(
            none.effective_session_kinds().is_empty(),
            "an empty set is a caller who deselected every class, not an absent one"
        );
    }

    /// A named set is an inclusion, because the caller stated exactly what they want.
    #[test]
    fn a_named_class_set_selects_exactly_those_classes() {
        let notices = MessageFilters {
            kinds: Some(vec![MessageKind::HarnessNotice]),
            ..Default::default()
        };
        assert_eq!(
            notices.kind_predicate(),
            KindPredicate::Only(vec![MessageKind::HarnessNotice]),
            "asking for harness notices must return them, not self-cancel against the default"
        );

        // no_compaction still narrows a named set, so the two cannot disagree.
        let conflict = MessageFilters {
            kinds: Some(vec![MessageKind::Compaction]),
            no_compaction: true,
            ..Default::default()
        };
        assert_eq!(
            conflict.kind_predicate(),
            KindPredicate::Only(Vec::new()),
            "an emptied set is reported as empty so the caller gets an error, not silence"
        );
    }

    /// The default set is derived, so a new variant joins it automatically and a deliberate
    /// exclusion is visible in the diff rather than hidden in a hand-written list.
    #[test]
    fn the_default_search_set_is_every_class_except_harness_notices() {
        let set = MessageKind::default_search_set();
        assert!(!set.contains(&MessageKind::HarnessNotice));
        for kind in [
            MessageKind::Conversation,
            MessageKind::Compaction,
            MessageKind::ToolCall,
            MessageKind::ToolResult,
            MessageKind::Unknown,
        ] {
            assert!(
                set.contains(&kind),
                "{kind:?} must be searchable by default"
            );
        }
    }

    #[test]
    fn every_message_kind_round_trips_through_its_database_spelling() {
        use clap::ValueEnum;

        for kind in MessageKind::value_variants() {
            assert_eq!(
                MessageKind::from_db_str(kind.as_str()),
                *kind,
                "{} must not decode as a different kind after a database read",
                kind.as_str()
            );
        }
    }

    // PATTERN: every `from_db_str` must be LOUD about index corruption rather than silently
    // degrading. `Provider` and `Role` state the rule in their own doc comments — "Prefer this
    // over `parse().unwrap_or(...)` so the round-trip invariant is not silent" — and back it with
    // `debug_assert!`. The rule was documented but never tested, so a refactor to the plain
    // `unwrap_or` form would have passed CI. These three tests pin the contract for all of them.
    //
    // `debug_assert!` compiles out under `--release`, so guard on `debug_assertions`; the repo
    // runs `cargo test` in debug (run_ci_local.sh:349, ci.yml:58) and these are skipped, not
    // failed, if that ever changes.

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "unrecognized provider in index")]
    fn unrecognized_provider_spelling_is_loud_in_debug() {
        let _ = Provider::from_db_str("not_a_provider");
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "unrecognized role in index")]
    fn unrecognized_role_spelling_is_loud_in_debug() {
        let _ = Role::from_db_str("not_a_role");
    }

    // `MessageKind` is the deliberate exception to the rule above, and this test pins the
    // exception so nobody "fixes" it into consistency. An earlier attempt to give it the same
    // `debug_assert!` broke
    // `cli_search_self_heals_v4_hybrid_missing_trigram_from_intact_messages`, which reads rows
    // storing `kind = 'message'`: older indexes legitimately hold spellings this build does not
    // know, and absorbing them is what `Unknown` is FOR. Panicking would turn "opened an older
    // index" into a crash on the very path that exists to heal it.
    #[test]
    fn unrecognized_message_kind_decodes_quietly_because_older_indexes_hold_other_spellings() {
        assert_eq!(
            MessageKind::from_db_str("message"),
            MessageKind::Unknown,
            "a legacy on-disk spelling must decode, not panic"
        );
        assert_eq!(
            MessageKind::from_db_str(MessageKind::Unknown.as_str()),
            MessageKind::Unknown,
            "\"unknown\" is itself a legitimate stored spelling"
        );
    }

    #[test]
    fn programmatic_search_field_accepts_cli_spelling_but_serializes_canonically() {
        assert_eq!(
            "tool-argument".parse::<SearchField>().unwrap(),
            SearchField::ToolArgument
        );
        assert_eq!(
            "tool_argument".parse::<SearchField>().unwrap(),
            SearchField::ToolArgument
        );
        assert_eq!(SearchField::ToolArgument.as_str(), "tool_argument");
        assert_eq!(
            serde_json::to_string(&SearchField::ToolArgument).unwrap(),
            "\"tool_argument\""
        );
    }

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
    fn fuzzy_message_validation_requires_a_finite_page_without_an_arbitrary_window_cap() {
        let unlimited = MessageFilters {
            match_mode: MessageSearchMode::Fuzzy,
            ..Default::default()
        };
        assert!(unlimited
            .validate("query")
            .unwrap_err()
            .to_string()
            .contains("finite non-zero limit"));

        let large_valid_page = MessageFilters {
            match_mode: MessageSearchMode::Fuzzy,
            offset: 10_000,
            limit: 1,
            ..Default::default()
        };
        assert!(large_valid_page.validate("query").is_ok());

        let overflowing = MessageFilters {
            match_mode: MessageSearchMode::Fuzzy,
            offset: usize::MAX,
            limit: 1,
            ..Default::default()
        };
        assert!(overflowing
            .validate("query")
            .unwrap_err()
            .to_string()
            .contains("signed 64-bit"));

        let whitespace = MessageFilters {
            match_mode: MessageSearchMode::Fuzzy,
            limit: 5,
            ..Default::default()
        };
        assert!(whitespace
            .validate(" \t\n")
            .unwrap_err()
            .to_string()
            .contains("non-whitespace"));
    }

    #[test]
    fn legacy_tool_argument_filter_rejects_malformed_json_pointer_escapes() {
        let filters = MessageFilters {
            field: Some(SearchField::ToolArgument),
            argument_path: Some("/~bad".into()),
            ..Default::default()
        };
        let error = filters.validate("needle").unwrap_err().to_string();
        assert!(error.contains("RFC 6901"), "{error}");
        assert!(error.contains("'~' must be followed"), "{error}");
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
            corpus: 25,
        };
        let s = ex.summary(true);
        assert!(s.contains("trigram prefilter available"), "{s}");
        assert!(s.contains("skipped trigram prefilter"), "{s}");
        assert!(s.contains("direct scan of 25 corpus rows"), "{s}");
    }
}
