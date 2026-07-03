use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
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
        }
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
            other => Err(format!("unknown role: {other}")),
        }
    }
}

/// A single conversation turn persisted per session (the unit of message-level analytics).
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

/// Filters for message-level search (`messages search`, analytics). `limit == 0` means
/// unlimited (consistent with the analytics default; avoids the session `--limit 25` trap).
#[derive(Debug, Clone, Default)]
pub struct MessageFilters {
    pub role: Option<Role>,
    /// Restrict to one harness (claude|claude-desktop|codex|cursor|antigravity|pi).
    pub provider: Option<Provider>,
    /// Exact session id, used after CLI commands resolve a user-supplied id/prefix.
    /// This avoids substring filters accidentally merging sessions in `messages get`
    /// and `messages timeline`.
    pub session_id: Option<String>,
    /// Substring/prefix session filter for exploratory search surfaces.
    pub session: Option<String>,
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
    /// Optional Rust regex applied to message content (linear-time; no ReDoS guard needed).
    pub regex: Option<String>,
    /// Optional case-insensitive substring filter on a tool message's `tool_name`
    /// (e.g. `exec` matches codex `exec_command`, `edit` matches claude `Edit`/`MultiEdit`).
    pub tool: Option<String>,
    pub no_compaction: bool,
    /// Compatibility flag for the old FTS-ranking behavior. Exact literal and regex message
    /// search keep deterministic session/seq order; ranking must never change the match set.
    pub rank: bool,
    pub limit: usize,
}

impl MessageFilters {
    /// True when at least one structural predicate (role / provider / session / path / time window /
    /// tool / no-compaction) restricts the SQL row set BEFORE content matching. `regex`, `rank`
    /// and `limit` are NOT structural — they filter/order content, not the scanned corpus. Used
    /// by `search_messages` to decide whether the content trigram prefilter is worth querying:
    /// when a structural filter already narrows the corpus to a small slice, a direct scan of
    /// that slice beats intersecting against the whole-corpus trigram index.
    pub fn narrows_corpus(&self) -> bool {
        self.role.is_some()
            || self.provider.is_some()
            || self.session_id.is_some()
            || self.session.is_some()
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
    pub ts: Option<DateTime<Utc>>,
    /// The tool that produced a `Role::Tool` message (e.g. `Bash`, `exec_command`), else None.
    pub tool_name: Option<String>,
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
/// contains a `/`. `limit == 0` means unlimited.
#[derive(Debug, Clone, Default)]
pub struct FileQuery {
    pub pattern: Option<String>,
    /// Exact canonical session id. Prefer this when chaining from session/message search output.
    pub session_id: Option<String>,
    /// Fuzzy substring session filter for exploratory file queries.
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

#[cfg(test)]
mod tests {
    use super::*;

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
