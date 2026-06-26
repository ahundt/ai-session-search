use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
    Cursor,
    Antigravity,
    Pi,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::Antigravity => "antigravity",
            Self::Pi => "pi",
        }
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
            "codex" => Ok(Self::Codex),
            "cursor" => Ok(Self::Cursor),
            "antigravity" => Ok(Self::Antigravity),
            "pi" => Ok(Self::Pi),
            other => Err(format!("unsupported provider: {other}")),
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
            other => Err(format!("unknown role: {other}")),
        }
    }
}

/// A single conversation turn persisted per session (keystone for analytics).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub seq: i64,
    pub role: Role,
    pub ts: Option<DateTime<Utc>>,
    pub tool_name: Option<String>,
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
        Self { old: old.into(), new: new.into(), replace_all: false }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedSession {
    pub session: SessionRecord,
    pub transcript_text: String,
    /// Per-message rows persisted to the `messages` table (keystone).
    pub messages: Vec<Message>,
    /// File-mutating tool calls persisted to the `file_edits` table (Phase 5 recovery).
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

/// Filters for message-level search (`messages search`, analytics). `limit == 0` means
/// unlimited (consistent with the analytics default; avoids the session `--limit 25` trap).
#[derive(Debug, Clone, Default)]
pub struct MessageFilters {
    pub role: Option<Role>,
    pub session: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    /// Optional Rust regex applied to message content (linear-time; no ReDoS guard needed).
    pub regex: Option<String>,
    pub no_compaction: bool,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageHit {
    pub session_id: String,
    pub provider: Provider,
    pub seq: i64,
    pub role: Role,
    pub ts: Option<DateTime<Utc>>,
    pub content: String,
}

/// A user message that matched a correction pattern (port of aise's CorrectionMatch).
#[derive(Debug, Clone, Serialize)]
pub struct CorrectionMatch {
    pub session_id: String,
    pub provider: Provider,
    pub ts: Option<DateTime<Utc>>,
    pub category: String,
    pub matched_pattern: String,
    pub content: String,
}

/// Aggregate slash-command usage (port of aise's planning frequency).
#[derive(Debug, Clone, Serialize)]
pub struct PlanningCount {
    pub command: String,
    pub count: i64,
    pub unique_sessions: i64,
    pub unique_projects: i64,
}

/// Structured filters for the `files` query surface (search / cross-ref).
/// `pattern` is a glob (`*`/`?`) over the basename, or over the full path when it
/// contains a `/`. `limit == 0` means unlimited.
#[derive(Debug, Clone, Default)]
pub struct FileQuery {
    pub pattern: Option<String>,
    pub session: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub min_edits: Option<i64>,
    pub max_edits: Option<i64>,
    pub limit: usize,
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

#[derive(Debug, Clone)]
pub struct ProviderHealth {
    pub provider: Provider,
    pub binary_found: bool,
    pub roots: Vec<String>,
    pub discovered_files: usize,
    pub sample_resume: String,
}
