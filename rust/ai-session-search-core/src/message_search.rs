use std::borrow::Cow;
use std::num::{NonZeroU32, NonZeroUsize};

use chrono::{DateTime, Utc};
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as NucleoConfig, Matcher as NucleoMatcher, Utf32Str};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;

use crate::models::{MessageKind, MessageSearchMode, Provider, Role, SearchField};

pub const DEFAULT_MATCH_EVIDENCE_MAX_CHARS: usize = 220;
/// Version of the cross-surface structured message-search response contract.
///
/// This is intentionally independent of the SQLite schema version.
pub const MESSAGE_SEARCH_RESPONSE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MessageSearchError {
    #[error("{kind} query must not be empty")]
    EmptyQuery { kind: &'static str },
    #[error("fuzzy query must contain at least 3 Unicode scalar values")]
    ShortFuzzyQuery,
    #[error("fuzzy query must contain a non-whitespace character")]
    BlankFuzzyQuery,
    #[error("invalid regex: {0}")]
    InvalidRegex(String),
    #[error("invalid RFC 6901 JSON pointer: {0}")]
    InvalidJsonPointer(String),
    #[error("{parameter}: {reason}")]
    InvalidParameter {
        parameter: &'static str,
        reason: String,
    },
    #[error("conflicting message-search parameters: {0}")]
    Conflict(String),
}

impl MessageSearchError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::EmptyQuery { .. }
            | Self::ShortFuzzyQuery
            | Self::BlankFuzzyQuery
            | Self::InvalidRegex(_) => "invalid-query",
            Self::InvalidJsonPointer(_) => "invalid-json-pointer",
            Self::InvalidParameter { .. } => "invalid-parameter",
            Self::Conflict(_) => "parameter-conflict",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct LiteralQuery(String);

impl LiteralQuery {
    pub fn new(value: impl Into<String>) -> Result<Self, MessageSearchError> {
        nonempty_query(value, "literal").map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct FuzzyQuery(String);

impl FuzzyQuery {
    pub fn new(value: impl Into<String>) -> Result<Self, MessageSearchError> {
        let value = nonempty_query(value, "fuzzy")?;
        if value.trim().is_empty() {
            return Err(MessageSearchError::BlankFuzzyQuery);
        }
        if value.chars().take(3).count() < 3 {
            return Err(MessageSearchError::ShortFuzzyQuery);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct ValidatedRegex(String);

impl ValidatedRegex {
    pub fn new(value: impl Into<String>) -> Result<Self, MessageSearchError> {
        let value = nonempty_query(value, "regex")?;
        regex::Regex::new(&value)
            .map_err(|error| MessageSearchError::InvalidRegex(error.to_string()))?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn nonempty_query(
    value: impl Into<String>,
    kind: &'static str,
) -> Result<String, MessageSearchError> {
    let value = value.into();
    if value.is_empty() {
        return Err(MessageSearchError::EmptyQuery { kind });
    }
    Ok(value)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "mode", content = "query", rename_all = "kebab-case")]
pub enum MessageQuery {
    All,
    Literal(LiteralQuery),
    Regex(ValidatedRegex),
    Fuzzy(FuzzyQuery),
}

impl MessageQuery {
    pub fn literal(value: impl Into<String>) -> Result<Self, MessageSearchError> {
        LiteralQuery::new(value).map(Self::Literal)
    }

    pub fn regex(value: impl Into<String>) -> Result<Self, MessageSearchError> {
        ValidatedRegex::new(value).map(Self::Regex)
    }

    pub fn fuzzy(value: impl Into<String>) -> Result<Self, MessageSearchError> {
        FuzzyQuery::new(value).map(Self::Fuzzy)
    }

    pub fn text(&self) -> Option<&str> {
        match self {
            Self::All => None,
            Self::Literal(value) => Some(value.as_str()),
            Self::Regex(value) => Some(value.as_str()),
            Self::Fuzzy(value) => Some(value.as_str()),
        }
    }

    pub const fn mode(&self) -> Option<MessageSearchMode> {
        match self {
            Self::All => None,
            Self::Literal(_) => Some(MessageSearchMode::Exact),
            Self::Regex(_) => Some(MessageSearchMode::Regex),
            Self::Fuzzy(_) => Some(MessageSearchMode::Fuzzy),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct JsonPointer(String);

impl JsonPointer {
    pub fn new(value: impl Into<String>) -> Result<Self, MessageSearchError> {
        let value = value.into();
        if !value.is_empty() && !value.starts_with('/') {
            return Err(MessageSearchError::InvalidJsonPointer(
                "a nonempty pointer must start with '/'".into(),
            ));
        }
        let bytes = value.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'~' {
                if !matches!(bytes.get(index + 1), Some(b'0' | b'1')) {
                    return Err(MessageSearchError::InvalidJsonPointer(
                        "'~' must be followed by '0' or '1'".into(),
                    ));
                }
                index += 1;
            }
            index += 1;
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct MessageTarget {
    field: SearchField,
    argument_path: Option<JsonPointer>,
}

impl MessageTarget {
    pub const fn content() -> Self {
        Self {
            field: SearchField::Content,
            argument_path: None,
        }
    }

    pub const fn tool_name() -> Self {
        Self {
            field: SearchField::ToolName,
            argument_path: None,
        }
    }

    pub fn tool_argument(path: impl Into<String>) -> Result<Self, MessageSearchError> {
        Ok(Self {
            field: SearchField::ToolArgument,
            argument_path: Some(JsonPointer::new(path)?),
        })
    }

    pub const fn field(&self) -> SearchField {
        self.field
    }

    pub fn argument_path(&self) -> Option<&JsonPointer> {
        self.argument_path.as_ref()
    }
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "kebab-case")]
pub enum MatchWindow {
    #[default]
    Earliest,
    Latest,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "kebab-case")]
pub enum ReceiptLevel {
    #[default]
    None,
    Summary,
    Full,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContextWindow {
    before: usize,
    after: usize,
}

impl ContextWindow {
    pub const fn new(before: usize, after: usize) -> Self {
        Self { before, after }
    }

    pub const fn symmetric(count: usize) -> Self {
        Self::new(count, count)
    }

    pub const fn before(self) -> usize {
        self.before
    }

    pub const fn after(self) -> usize {
        self.after
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LineWindow {
    #[default]
    Full,
    Head(NonZeroUsize),
    Tail(NonZeroUsize),
}

impl LineWindow {
    pub fn from_signed(value: i64) -> Result<Self, MessageSearchError> {
        match value.cmp(&0) {
            std::cmp::Ordering::Equal => Ok(Self::Full),
            std::cmp::Ordering::Greater => usize::try_from(value)
                .ok()
                .and_then(NonZeroUsize::new)
                .map(Self::Head)
                .ok_or_else(|| MessageSearchError::InvalidParameter {
                    parameter: "lines_per_message",
                    reason: "positive value exceeds usize".into(),
                }),
            std::cmp::Ordering::Less => value
                .checked_abs()
                .and_then(|absolute| usize::try_from(absolute).ok())
                .and_then(NonZeroUsize::new)
                .map(Self::Tail)
                .ok_or_else(|| MessageSearchError::InvalidParameter {
                    parameter: "lines_per_message",
                    reason: "negative magnitude exceeds usize".into(),
                }),
        }
    }

    pub fn to_signed(self) -> Result<i64, MessageSearchError> {
        match self {
            Self::Full => Ok(0),
            Self::Head(lines) => {
                i64::try_from(lines.get()).map_err(|_| MessageSearchError::InvalidParameter {
                    parameter: "lines_per_message",
                    reason: "head line count exceeds i64".into(),
                })
            }
            Self::Tail(lines) => i64::try_from(lines.get()).map(|value| -value).map_err(|_| {
                MessageSearchError::InvalidParameter {
                    parameter: "lines_per_message",
                    reason: "tail line count exceeds i64".into(),
                }
            }),
        }
    }
}

/// Requested message-hit extent before surface defaults are resolved.
///
/// `Page { limit: None, .. }` deliberately has surface-specific meaning: Rust, CLI, and Python
/// resolve it to all literal, regex, or no-text matches, while MCP resolves it to its configured
/// finite page. Fuzzy search always requires a finite resolved page. `AllResults` is the explicit
/// cross-surface override and is never silently converted into a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum RequestedExtent {
    Page {
        limit: Option<NonZeroUsize>,
        offset: usize,
    },
    AllResults {
        offset: usize,
    },
}

impl Default for RequestedExtent {
    fn default() -> Self {
        Self::Page {
            limit: None,
            offset: 0,
        }
    }
}

impl RequestedExtent {
    pub fn page(limit: Option<usize>, offset: usize) -> Result<Self, MessageSearchError> {
        let limit = match limit {
            Some(0) => {
                return Err(MessageSearchError::InvalidParameter {
                    parameter: "limit",
                    reason: "use all_results instead of zero".into(),
                })
            }
            Some(value) => NonZeroUsize::new(value),
            None => None,
        };
        Ok(Self::Page { limit, offset })
    }

    pub const fn all_results() -> Self {
        Self::AllResults { offset: 0 }
    }

    pub const fn all_results_from(offset: usize) -> Self {
        Self::AllResults { offset }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
struct NonEmptyValue(String);

impl NonEmptyValue {
    pub fn new(
        parameter: &'static str,
        value: impl Into<String>,
    ) -> Result<Self, MessageSearchError> {
        let value = value.into();
        if value.is_empty() {
            return Err(MessageSearchError::InvalidParameter {
                parameter,
                reason: "must not be empty".into(),
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct SequenceRange {
    from: Option<i64>,
    to: Option<i64>,
}

impl SequenceRange {
    pub fn new(from: Option<i64>, to: Option<i64>) -> Result<Self, MessageSearchError> {
        if from.is_some_and(|value| value < 0) || to.is_some_and(|value| value < 0) {
            return Err(MessageSearchError::InvalidParameter {
                parameter: "sequence",
                reason: "bounds must be nonnegative".into(),
            });
        }
        if let (Some(from), Some(to)) = (from, to) {
            if from > to {
                return Err(MessageSearchError::InvalidParameter {
                    parameter: "sequence",
                    reason: format!("seq_from {from} exceeds seq_to {to}"),
                });
            }
        }
        Ok(Self { from, to })
    }

    pub const fn from(&self) -> Option<i64> {
        self.from
    }

    pub const fn to(&self) -> Option<i64> {
        self.to
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize)]
pub struct RequestedTimeRange {
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
}

impl RequestedTimeRange {
    pub fn new(
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<Self, MessageSearchError> {
        if let (Some(since), Some(until)) = (since, until) {
            if since > until {
                return Err(MessageSearchError::InvalidParameter {
                    parameter: "time",
                    reason: "since must not be later than until".into(),
                });
            }
        }
        Ok(Self { since, until })
    }

    pub const fn since(&self) -> Option<DateTime<Utc>> {
        self.since
    }

    pub const fn until(&self) -> Option<DateTime<Utc>> {
        self.until
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct MessagePredicates {
    role: Option<Role>,
    kinds: Option<Vec<MessageKind>>,
    provider: Option<Provider>,
    session: Option<NonEmptyValue>,
    workspace_path_prefix: Option<NonEmptyValue>,
    transcript_path_prefix: Option<NonEmptyValue>,
    exclude_workspace_path_prefixes: Vec<NonEmptyValue>,
    exclude_transcript_path_prefixes: Vec<NonEmptyValue>,
    exclude_session_ids: Vec<NonEmptyValue>,
    time: RequestedTimeRange,
    sequence: Option<SequenceRange>,
    tool_name_contains: Option<NonEmptyValue>,
    include_compaction: bool,
}

impl Default for MessagePredicates {
    fn default() -> Self {
        Self {
            role: None,
            kinds: None,
            provider: None,
            session: None,
            workspace_path_prefix: None,
            transcript_path_prefix: None,
            exclude_workspace_path_prefixes: Vec::new(),
            exclude_transcript_path_prefixes: Vec::new(),
            exclude_session_ids: Vec::new(),
            time: RequestedTimeRange::default(),
            sequence: None,
            tool_name_contains: None,
            include_compaction: true,
        }
    }
}

impl MessagePredicates {
    pub const fn role(&self) -> Option<Role> {
        self.role
    }

    pub fn kinds(&self) -> Option<&[MessageKind]> {
        self.kinds.as_deref()
    }

    pub const fn provider(&self) -> Option<Provider> {
        self.provider
    }

    pub fn session(&self) -> Option<&str> {
        self.session.as_ref().map(NonEmptyValue::as_str)
    }

    pub fn workspace_path_prefix(&self) -> Option<&str> {
        self.workspace_path_prefix
            .as_ref()
            .map(NonEmptyValue::as_str)
    }

    pub fn transcript_path_prefix(&self) -> Option<&str> {
        self.transcript_path_prefix
            .as_ref()
            .map(NonEmptyValue::as_str)
    }

    pub fn exclude_workspace_path_prefixes(&self) -> impl Iterator<Item = &str> {
        self.exclude_workspace_path_prefixes
            .iter()
            .map(NonEmptyValue::as_str)
    }

    pub fn exclude_transcript_path_prefixes(&self) -> impl Iterator<Item = &str> {
        self.exclude_transcript_path_prefixes
            .iter()
            .map(NonEmptyValue::as_str)
    }

    pub fn exclude_session_ids(&self) -> impl Iterator<Item = &str> {
        self.exclude_session_ids.iter().map(NonEmptyValue::as_str)
    }

    pub const fn time(&self) -> RequestedTimeRange {
        self.time
    }

    pub const fn sequence(&self) -> Option<SequenceRange> {
        self.sequence
    }

    pub fn tool_name_contains(&self) -> Option<&str> {
        self.tool_name_contains.as_ref().map(NonEmptyValue::as_str)
    }

    pub const fn include_compaction(&self) -> bool {
        self.include_compaction
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessagePresentation {
    include_refs: Option<bool>,
    message_lines: Option<LineWindow>,
    match_evidence_max_chars: Option<NonZeroUsize>,
}

impl MessagePresentation {
    pub const fn include_refs(&self) -> Option<bool> {
        self.include_refs
    }

    pub const fn message_lines(&self) -> Option<LineWindow> {
        self.message_lines
    }

    pub const fn match_evidence_max_chars(&self) -> Option<NonZeroUsize> {
        self.match_evidence_max_chars
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct PurposeSelection {
    name: NonEmptyValue,
    version: Option<NonZeroU32>,
}

impl PurposeSelection {
    pub fn new(
        name: impl Into<String>,
        version: Option<NonZeroU32>,
    ) -> Result<Self, MessageSearchError> {
        let name = NonEmptyValue::new("purpose", name)?;
        if !is_dash_separated_phrase(name.as_str()) {
            return Err(MessageSearchError::InvalidParameter {
                parameter: "purpose",
                reason: "must be a lowercase dash-separated phrase".into(),
            });
        }
        Ok(Self { name, version })
    }

    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    pub const fn version(&self) -> Option<NonZeroU32> {
        self.version
    }
}

pub(crate) fn is_dash_separated_phrase(value: &str) -> bool {
    !value.is_empty()
        && value
            .split('-')
            .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_lowercase()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SearchSurface {
    Rust,
    Cli,
    Mcp,
    Python,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "source", rename_all = "kebab-case")]
pub enum ValueOrigin {
    Explicit,
    Purpose { name: String, version: NonZeroU32 },
    SurfaceConfig { surface: SearchSurface },
    OperationConfig,
    TypedDefault,
    Derived,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MessageSearchOrigins {
    pub(crate) limit: ValueOrigin,
    pub(crate) context_before: ValueOrigin,
    pub(crate) context_after: ValueOrigin,
    pub(crate) include_refs: ValueOrigin,
    pub(crate) message_lines: ValueOrigin,
    pub(crate) match_evidence_max_chars: ValueOrigin,
    pub(crate) receipt_level: ValueOrigin,
    pub(crate) ordering: ValueOrigin,
}

impl MessageSearchOrigins {
    pub fn limit(&self) -> &ValueOrigin {
        &self.limit
    }

    pub fn context_before(&self) -> &ValueOrigin {
        &self.context_before
    }

    pub fn context_after(&self) -> &ValueOrigin {
        &self.context_after
    }

    pub fn include_refs(&self) -> &ValueOrigin {
        &self.include_refs
    }

    pub fn message_lines(&self) -> &ValueOrigin {
        &self.message_lines
    }

    pub fn match_evidence_max_chars(&self) -> &ValueOrigin {
        &self.match_evidence_max_chars
    }

    pub fn receipt_level(&self) -> &ValueOrigin {
        &self.receipt_level
    }

    pub fn ordering(&self) -> &ValueOrigin {
        &self.ordering
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionOrder {
    SessionSequence,
    FuzzyRelevance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum ResolvedExtent {
    Page { limit: NonZeroUsize, offset: usize },
    AllResults { offset: usize },
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedMessagePredicates {
    pub(crate) role: Option<Role>,
    pub(crate) kinds: Option<Vec<MessageKind>>,
    pub(crate) provider: Option<Provider>,
    pub(crate) session_id: Option<String>,
    pub(crate) workspace_path_prefix: Option<String>,
    pub(crate) transcript_path_prefix: Option<String>,
    pub(crate) exclude_workspace_path_prefixes: Vec<String>,
    pub(crate) exclude_transcript_path_prefixes: Vec<String>,
    pub(crate) exclude_session_ids: Vec<String>,
    pub(crate) time: RequestedTimeRange,
    pub(crate) sequence: Option<SequenceRange>,
    pub(crate) tool_name_contains: Option<String>,
    pub(crate) include_compaction: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct MessageRetrievalPlan {
    pub(crate) query: MessageQuery,
    pub(crate) target: MessageTarget,
    pub(crate) predicates: ResolvedMessagePredicates,
    pub(crate) match_window: Option<MatchWindow>,
    pub(crate) ordering: ExecutionOrder,
    pub(crate) extent: ResolvedExtent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ResolvedMessagePresentation {
    pub(crate) include_refs: bool,
    pub(crate) message_lines: LineWindow,
    pub(crate) match_evidence_max_chars: NonZeroUsize,
}

impl ResolvedMessagePresentation {
    pub const fn include_refs(&self) -> bool {
        self.include_refs
    }

    pub const fn message_lines(&self) -> LineWindow {
        self.message_lines
    }

    pub const fn match_evidence_max_chars(&self) -> NonZeroUsize {
        self.match_evidence_max_chars
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct MessageResponsePlan {
    pub(crate) context: ContextWindow,
    pub(crate) presentation: ResolvedMessagePresentation,
}

#[derive(Debug, Clone)]
pub struct MessageSearchPlan {
    pub(crate) retrieval: MessageRetrievalPlan,
    pub(crate) response: MessageResponsePlan,
    pub(crate) receipt: ReceiptLevel,
    pub(crate) origins: MessageSearchOrigins,
}

impl MessageSearchPlan {
    pub const fn ordering(&self) -> ExecutionOrder {
        self.retrieval.ordering
    }

    pub const fn extent(&self) -> ResolvedExtent {
        self.retrieval.extent
    }

    pub const fn context(&self) -> ContextWindow {
        self.response.context
    }

    pub const fn presentation(&self) -> ResolvedMessagePresentation {
        self.response.presentation
    }

    pub const fn receipt_level(&self) -> ReceiptLevel {
        self.receipt
    }

    pub fn origins(&self) -> &MessageSearchOrigins {
        &self.origins
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PageInfo {
    extent: ResolvedExtent,
    next_offset: Option<usize>,
    ordering: ExecutionOrder,
}

impl PageInfo {
    pub(crate) const fn new(
        extent: ResolvedExtent,
        next_offset: Option<usize>,
        ordering: ExecutionOrder,
    ) -> Self {
        Self {
            extent,
            next_offset,
            ordering,
        }
    }

    pub const fn extent(&self) -> ResolvedExtent {
        self.extent
    }

    pub const fn next_offset(&self) -> Option<usize> {
        self.next_offset
    }

    pub const fn ordering(&self) -> ExecutionOrder {
        self.ordering
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MessageMatchCharRange {
    pub start_char: usize,
    pub end_char: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum MessageMatchMarkers {
    Characters {
        ranges: Vec<MessageMatchCharRange>,
        matched_chars_total: usize,
        matched_chars_shown: usize,
    },
    Boundary {
        at_char: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MessageMatchEvidence {
    pub excerpt: String,
    pub excerpt_start_char: usize,
    pub selected_field_chars: usize,
    pub markers: MessageMatchMarkers,
}

/// Complete source occurrence for literal mode.
///
/// The independently bounded evidence excerpt can omit part of a long literal. This record keeps
/// the exact matched source text and absolute character coordinates without expanding regex or
/// fuzzy evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MessageLiteralMatch {
    pub text: String,
    pub start_char: usize,
    pub end_char: usize,
}

/// Honest description of the `content` string returned by one adapter.
///
/// Original totals are present only when the returned string is byte-for-byte complete. This
/// avoids a second full-input scan merely to populate metadata for a shortened row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MessageContentExtent {
    pub complete: bool,
    pub omitted_start: bool,
    pub omitted_end: bool,
    pub returned_chars: usize,
    pub returned_lines: usize,
    pub original_chars: Option<usize>,
    pub original_lines: Option<usize>,
}

impl MessageContentExtent {
    pub fn describe(
        original: &str,
        line_selected: &str,
        returned: &str,
        lines_per_message: i64,
        character_truncated: bool,
    ) -> Self {
        let line_truncated = line_selected != original;
        let complete = !character_truncated && returned == original;
        let returned_chars = returned.chars().count();
        let returned_lines = returned.lines().count();
        Self {
            complete,
            omitted_start: line_truncated && lines_per_message < 0,
            omitted_end: (line_truncated && lines_per_message > 0) || character_truncated,
            returned_chars,
            returned_lines,
            original_chars: complete.then_some(returned_chars),
            original_lines: complete.then_some(returned_lines),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageSearchHit {
    #[serde(flatten)]
    pub message: crate::models::MessageHit,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_evidence: Option<MessageMatchEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub literal_match: Option<MessageLiteralMatch>,
}

impl MessageSearchHit {
    pub fn message(&self) -> &crate::models::MessageHit {
        &self.message
    }

    pub fn match_evidence(&self) -> Option<&MessageMatchEvidence> {
        self.match_evidence.as_ref()
    }

    pub fn literal_match(&self) -> Option<&MessageLiteralMatch> {
        self.literal_match.as_ref()
    }
}

impl std::ops::Deref for MessageSearchHit {
    type Target = crate::models::MessageHit;

    fn deref(&self) -> &Self::Target {
        &self.message
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageSearchResponse {
    query: Option<String>,
    match_target: Option<MessageTarget>,
    match_mode: Option<MessageSearchMode>,
    hits: Vec<MessageSearchHit>,
    context_windows: Vec<Vec<crate::models::MessageHit>>,
    page: PageInfo,
    context: ContextWindow,
    presentation: ResolvedMessagePresentation,
    planner: Option<crate::models::SearchExplain>,
    origins: Option<MessageSearchOrigins>,
}

impl MessageSearchResponse {
    pub(crate) fn new(
        match_details: Option<(MessageTarget, MessageSearchMode)>,
        hits: Vec<MessageSearchHit>,
        context_windows: Vec<Vec<crate::models::MessageHit>>,
        page: PageInfo,
        response: MessageResponsePlan,
        planner: Option<crate::models::SearchExplain>,
        origins: Option<MessageSearchOrigins>,
    ) -> Self {
        let (match_target, match_mode) = match match_details {
            Some((target, mode)) => (Some(target), Some(mode)),
            None => (None, None),
        };
        Self {
            query: None,
            match_target,
            match_mode,
            hits,
            context_windows,
            page,
            context: response.context,
            presentation: response.presentation,
            planner,
            origins,
        }
    }

    pub(crate) fn with_query(mut self, query: Option<String>) -> Self {
        self.query = query;
        self
    }

    pub fn query(&self) -> Option<&str> {
        self.query.as_deref()
    }

    pub fn hits(&self) -> &[MessageSearchHit] {
        &self.hits
    }

    pub fn into_hits(self) -> Vec<MessageSearchHit> {
        self.hits
    }

    /// Consume returned rows without cloning their potentially large content strings.
    pub fn into_rows(self) -> (Vec<MessageSearchHit>, Vec<Vec<crate::models::MessageHit>>) {
        (self.hits, self.context_windows)
    }

    pub fn match_target(&self) -> Option<&MessageTarget> {
        self.match_target.as_ref()
    }

    pub const fn match_mode(&self) -> Option<MessageSearchMode> {
        self.match_mode
    }

    /// Context windows aligned by index with [`MessageSearchResponse::hits`]. Empty when the
    /// resolved context is zero on both sides.
    pub fn context_windows(&self) -> &[Vec<crate::models::MessageHit>] {
        &self.context_windows
    }

    pub const fn page(&self) -> PageInfo {
        self.page
    }

    pub const fn context(&self) -> ContextWindow {
        self.context
    }

    pub const fn presentation(&self) -> ResolvedMessagePresentation {
        self.presentation
    }

    pub fn planner(&self) -> Option<&crate::models::SearchExplain> {
        self.planner.as_ref()
    }

    pub fn origins(&self) -> Option<&MessageSearchOrigins> {
        self.origins.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct MessageSearchRequest {
    query: MessageQuery,
    target: MessageTarget,
    predicates: MessagePredicates,
    match_window: Option<MatchWindow>,
    context: Option<ContextWindow>,
    presentation: MessagePresentation,
    extent: RequestedExtent,
    purpose: Option<PurposeSelection>,
    receipt: Option<ReceiptLevel>,
}

impl MessageSearchRequest {
    pub fn builder(query: MessageQuery, target: MessageTarget) -> MessageSearchRequestBuilder {
        MessageSearchRequestBuilder {
            request: Self {
                query,
                target,
                predicates: MessagePredicates::default(),
                match_window: None,
                context: None,
                presentation: MessagePresentation::default(),
                extent: RequestedExtent::default(),
                purpose: None,
                receipt: None,
            },
        }
    }

    pub fn query(&self) -> &MessageQuery {
        &self.query
    }

    pub fn target(&self) -> &MessageTarget {
        &self.target
    }

    pub fn predicates(&self) -> &MessagePredicates {
        &self.predicates
    }

    pub const fn match_window(&self) -> Option<MatchWindow> {
        self.match_window
    }

    pub const fn context(&self) -> Option<ContextWindow> {
        self.context
    }

    pub const fn presentation(&self) -> MessagePresentation {
        self.presentation
    }

    pub const fn extent(&self) -> RequestedExtent {
        self.extent
    }

    pub fn purpose(&self) -> Option<&PurposeSelection> {
        self.purpose.as_ref()
    }

    pub const fn receipt_level(&self) -> Option<ReceiptLevel> {
        self.receipt
    }

    fn validate(&self) -> Result<(), MessageSearchError> {
        if self.predicates.sequence.is_some() && self.predicates.session.is_none() {
            return Err(MessageSearchError::Conflict(
                "sequence bounds require one session".into(),
            ));
        }
        // Validate the RESOLVED set, not each parameter alone: `kinds` and include_compaction
        // both narrow the same set, and every conflict found in this area passed
        // per-parameter checks while producing a request that could not match anything.
        // An unsatisfiable request must error, never return silently empty.
        let mut effective = self
            .predicates
            .kinds
            .clone()
            .unwrap_or_else(MessageKind::default_search_set);
        if !self.predicates.include_compaction {
            effective.retain(|kind| *kind != MessageKind::Compaction);
        }
        if effective.is_empty() {
            return Err(MessageSearchError::Conflict(
                "the selected kinds exclude every message class, so nothing can match".into(),
            ));
        }
        if self.predicates.role == Some(Role::Compaction)
            && !effective.contains(&MessageKind::Compaction)
        {
            return Err(MessageSearchError::Conflict(
                "role=compaction requires compaction among the selected kinds; \
                 include_compaction=false or a kinds set without it removes every match"
                    .into(),
            ));
        }
        if self.target.field == SearchField::ToolArgument
            && self
                .predicates
                .kinds
                .as_ref()
                .is_some_and(|kinds| !kinds.contains(&MessageKind::ToolCall))
        {
            return Err(MessageSearchError::Conflict(
                "tool-argument target requires tool_call among the selected kinds".into(),
            ));
        }
        if self.match_window.is_some() && matches!(self.query, MessageQuery::Fuzzy(_)) {
            return Err(MessageSearchError::Conflict(
                "match_window does not apply to fuzzy queries".into(),
            ));
        }
        if self.match_window == Some(MatchWindow::Latest) && self.predicates.session.is_none() {
            return Err(MessageSearchError::Conflict(
                "match_window=latest requires one session".into(),
            ));
        }
        if matches!(self.query, MessageQuery::All)
            && self.presentation.match_evidence_max_chars.is_some()
        {
            return Err(MessageSearchError::Conflict(
                "match_evidence_max_chars requires a literal, regex, or fuzzy query".into(),
            ));
        }
        if let MessageQuery::Fuzzy(_) = self.query {
            match self.extent {
                RequestedExtent::Page { .. } => {}
                RequestedExtent::AllResults { .. } => {
                    return Err(MessageSearchError::Conflict(
                        "fuzzy search does not support all_results".into(),
                    ))
                }
            }
        }
        Ok(())
    }
}

pub struct MessageSearchRequestBuilder {
    request: MessageSearchRequest,
}

impl MessageSearchRequestBuilder {
    pub fn role(mut self, role: Role) -> Self {
        self.request.predicates.role = Some(role);
        self
    }

    /// Select exactly one class. Convenience over [`Self::kinds`] for the common case.
    pub fn kind(mut self, kind: MessageKind) -> Self {
        self.request.predicates.kinds = Some(vec![kind]);
        self
    }

    /// Select the classes to return, replacing the default set (everything except
    /// `HarnessNotice`). This is the single mechanism for class selection.
    pub fn kinds(mut self, kinds: Vec<MessageKind>) -> Self {
        self.request.predicates.kinds = Some(kinds);
        self
    }

    pub fn provider(mut self, provider: Provider) -> Self {
        self.request.predicates.provider = Some(provider);
        self
    }

    pub fn session_id(mut self, session: impl Into<String>) -> Result<Self, MessageSearchError> {
        self.request.predicates.session = Some(NonEmptyValue::new("session_id", session)?);
        Ok(self)
    }

    pub fn workspace_path_prefix(
        mut self,
        path: impl Into<String>,
    ) -> Result<Self, MessageSearchError> {
        self.request.predicates.workspace_path_prefix =
            Some(NonEmptyValue::new("workspace_path_prefix", path)?);
        Ok(self)
    }

    pub fn transcript_path_prefix(
        mut self,
        path: impl Into<String>,
    ) -> Result<Self, MessageSearchError> {
        self.request.predicates.transcript_path_prefix =
            Some(NonEmptyValue::new("transcript_path_prefix", path)?);
        Ok(self)
    }

    pub fn exclude_workspace_path_prefix(
        mut self,
        path: impl Into<String>,
    ) -> Result<Self, MessageSearchError> {
        self.request
            .predicates
            .exclude_workspace_path_prefixes
            .push(NonEmptyValue::new("exclude_workspace_path_prefix", path)?);
        Ok(self)
    }

    pub fn exclude_transcript_path_prefix(
        mut self,
        path: impl Into<String>,
    ) -> Result<Self, MessageSearchError> {
        self.request
            .predicates
            .exclude_transcript_path_prefixes
            .push(NonEmptyValue::new("exclude_transcript_path_prefix", path)?);
        Ok(self)
    }

    pub fn exclude_session_id(
        mut self,
        session: impl Into<String>,
    ) -> Result<Self, MessageSearchError> {
        self.request
            .predicates
            .exclude_session_ids
            .push(NonEmptyValue::new("exclude_session_id", session)?);
        Ok(self)
    }

    pub fn time(mut self, time: RequestedTimeRange) -> Self {
        self.request.predicates.time = time;
        self
    }

    pub fn sequence(mut self, sequence: SequenceRange) -> Self {
        self.request.predicates.sequence = Some(sequence);
        self
    }

    pub fn tool_name_contains(
        mut self,
        value: impl Into<String>,
    ) -> Result<Self, MessageSearchError> {
        self.request.predicates.tool_name_contains =
            Some(NonEmptyValue::new("tool_name_contains", value)?);
        Ok(self)
    }

    pub fn include_compaction(mut self, include: bool) -> Self {
        self.request.predicates.include_compaction = include;
        self
    }

    pub fn match_window(mut self, window: MatchWindow) -> Self {
        self.request.match_window = Some(window);
        self
    }

    pub fn context(mut self, context: ContextWindow) -> Self {
        self.request.context = Some(context);
        self
    }

    pub fn include_refs(mut self, include: bool) -> Self {
        self.request.presentation.include_refs = Some(include);
        self
    }

    pub fn message_lines(mut self, window: LineWindow) -> Self {
        self.request.presentation.message_lines = Some(window);
        self
    }

    pub fn match_evidence_max_chars(mut self, maximum: NonZeroUsize) -> Self {
        self.request.presentation.match_evidence_max_chars = Some(maximum);
        self
    }

    pub fn extent(mut self, extent: RequestedExtent) -> Self {
        self.request.extent = extent;
        self
    }

    pub fn purpose(mut self, purpose: PurposeSelection) -> Self {
        self.request.purpose = Some(purpose);
        self
    }

    pub fn receipt_level(mut self, level: ReceiptLevel) -> Self {
        self.request.receipt = Some(level);
        self
    }

    pub fn build(self) -> Result<MessageSearchRequest, MessageSearchError> {
        self.request.validate()?;
        Ok(self.request)
    }
}

pub(crate) fn selected_message_field<'a>(
    hit: &'a crate::models::MessageHit,
    target: &MessageTarget,
) -> Option<Cow<'a, str>> {
    selected_message_field_parts(
        hit,
        target.field(),
        target.argument_path().map(JsonPointer::as_str),
    )
}

pub(crate) fn selected_message_field_parts<'a>(
    hit: &'a crate::models::MessageHit,
    field: SearchField,
    argument_path: Option<&str>,
) -> Option<Cow<'a, str>> {
    match field {
        SearchField::Content => Some(Cow::Borrowed(&hit.content)),
        SearchField::ToolName => hit.tool_name.as_deref().map(Cow::Borrowed),
        SearchField::ToolArgument => {
            let envelope: serde_json::Value = serde_json::from_str(&hit.content).ok()?;
            let args = envelope.get("args")?;
            let value = match argument_path.unwrap_or("") {
                "" => args,
                pointer => args.pointer(pointer)?,
            };
            Some(Cow::Owned(match value {
                serde_json::Value::String(value) => value.clone(),
                other => serde_json::to_string(other).ok()?,
            }))
        }
    }
}

pub(crate) fn attach_match_evidence(
    query: &MessageQuery,
    target: &MessageTarget,
    maximum_chars: NonZeroUsize,
    hits: Vec<crate::models::MessageHit>,
) -> anyhow::Result<Vec<MessageSearchHit>> {
    let mut prepared = PreparedMatchEvidence::new(query)?;
    hits.into_iter()
        .map(|message| {
            let (match_evidence, literal_match) = match query {
                MessageQuery::All => (None, None),
                _ => {
                    let selected = selected_message_field(&message, target).ok_or_else(|| {
                        anyhow::anyhow!(
                            "message-search match evidence cannot project {:?} for {} sequence {}",
                            target.field(),
                            message.session_id,
                            message.seq
                        )
                    })?;
                    let evidence = prepared.build(&selected, maximum_chars).ok_or_else(|| {
                            anyhow::anyhow!(
                                "message-search match evidence disagrees with {:?} membership for {} sequence {}",
                                target.field(),
                                message.session_id,
                                message.seq
                            )
                        })?;
                    let literal_match = match query {
                        MessageQuery::Literal(_) => {
                            let range = match &evidence.markers {
                                MessageMatchMarkers::Characters {
                                    ranges,
                                    matched_chars_total,
                                    ..
                                } => ranges.first().map(|range| {
                                    let start_char =
                                        evidence.excerpt_start_char + range.start_char;
                                    MessageMatchCharRange {
                                        start_char,
                                        end_char: start_char + matched_chars_total,
                                    }
                                }),
                                MessageMatchMarkers::Boundary { .. } => None,
                            }
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "literal match evidence has no character range for {} sequence {}",
                                    message.session_id,
                                    message.seq
                                )
                            })?;
                            Some(MessageLiteralMatch {
                                text: selected
                                    .chars()
                                    .skip(range.start_char)
                                    .take(range.end_char - range.start_char)
                                    .collect(),
                                start_char: range.start_char,
                                end_char: range.end_char,
                            })
                        }
                        _ => None,
                    };
                    (Some(evidence), literal_match)
                }
            };
            Ok(MessageSearchHit {
                message,
                match_evidence,
                literal_match,
            })
        })
        .collect()
}

enum PreparedMatchEvidence {
    All,
    Literal {
        lowered_query: String,
    },
    Regex(regex::Regex),
    Fuzzy {
        pattern: Pattern,
        matcher: NucleoMatcher,
        utf32_buf: Vec<char>,
        indices: Vec<u32>,
    },
}

impl PreparedMatchEvidence {
    fn new(query: &MessageQuery) -> anyhow::Result<Self> {
        Ok(match query {
            MessageQuery::All => Self::All,
            MessageQuery::Literal(query) => Self::Literal {
                lowered_query: query.as_str().to_lowercase(),
            },
            MessageQuery::Regex(query) => Self::Regex(regex::Regex::new(query.as_str())?),
            MessageQuery::Fuzzy(query) => Self::Fuzzy {
                pattern: Pattern::new(
                    query.as_str(),
                    CaseMatching::Ignore,
                    Normalization::Smart,
                    AtomKind::Fuzzy,
                ),
                matcher: NucleoMatcher::new(NucleoConfig::DEFAULT),
                utf32_buf: Vec::new(),
                indices: Vec::new(),
            },
        })
    }

    fn build(
        &mut self,
        selected: &str,
        maximum_chars: NonZeroUsize,
    ) -> Option<MessageMatchEvidence> {
        let selected_field_chars = selected.chars().count();
        let maximum = maximum_chars.get().min(selected_field_chars.max(1));
        match self {
            Self::All => None,
            Self::Literal { lowered_query } => {
                let range = literal_char_range(selected, lowered_query)?;
                Some(character_evidence(
                    selected,
                    selected_field_chars,
                    maximum,
                    vec![range],
                    false,
                ))
            }
            Self::Regex(regex) => {
                let matched = regex.find(selected)?;
                let start = selected[..matched.start()].chars().count();
                let end = start + matched.as_str().chars().count();
                if start == end {
                    Some(boundary_evidence(
                        selected,
                        selected_field_chars,
                        maximum,
                        start,
                    ))
                } else {
                    Some(character_evidence(
                        selected,
                        selected_field_chars,
                        maximum,
                        vec![MessageMatchCharRange {
                            start_char: start,
                            end_char: end,
                        }],
                        false,
                    ))
                }
            }
            Self::Fuzzy {
                pattern,
                matcher,
                utf32_buf,
                indices,
            } => {
                utf32_buf.clear();
                indices.clear();
                let matcher_input = Utf32Str::new(selected, utf32_buf);
                pattern.indices(matcher_input, matcher, indices)?;
                let matcher_unit_ranges = scalar_ranges_by_matcher_unit(selected, matcher_input);
                let ranges = indices
                    .iter()
                    .map(|index| matcher_unit_ranges.get(*index as usize).copied())
                    .collect::<Option<Vec<_>>>()?;
                let ranges = coalesce_character_ranges(ranges);
                Some(character_evidence(
                    selected,
                    selected_field_chars,
                    maximum,
                    ranges,
                    true,
                ))
            }
        }
    }
}

fn literal_char_range(selected: &str, lowered_query: &str) -> Option<MessageMatchCharRange> {
    let mut lowered = String::new();
    let mut original_char_for_lowered = Vec::new();
    for (original_char, value) in selected.chars().enumerate() {
        for lowered_char in value.to_lowercase() {
            lowered.push(lowered_char);
            original_char_for_lowered.push(original_char);
        }
    }
    let start_byte = lowered.find(lowered_query)?;
    let start_lowered = lowered[..start_byte].chars().count();
    let end_lowered = start_lowered + lowered_query.chars().count();
    Some(MessageMatchCharRange {
        start_char: original_char_for_lowered[start_lowered],
        end_char: original_char_for_lowered[end_lowered - 1] + 1,
    })
}

fn scalar_ranges_by_grapheme(selected: &str) -> Vec<MessageMatchCharRange> {
    let mut scalar_start = 0;
    selected
        .graphemes(true)
        .map(|grapheme| {
            let scalar_end = scalar_start + grapheme.chars().count();
            let range = MessageMatchCharRange {
                start_char: scalar_start,
                end_char: scalar_end,
            };
            scalar_start = scalar_end;
            range
        })
        .collect()
}

fn scalar_ranges_by_matcher_unit(
    selected: &str,
    matcher_input: Utf32Str<'_>,
) -> Vec<MessageMatchCharRange> {
    match matcher_input {
        Utf32Str::Unicode(_) => scalar_ranges_by_grapheme(selected),
        Utf32Str::Ascii(_) => {
            let mut scalar_start = 0;
            let mut ranges = Vec::with_capacity(selected.len());
            for grapheme in selected.graphemes(true) {
                let scalar_end = scalar_start + grapheme.chars().count();
                let range = MessageMatchCharRange {
                    start_char: scalar_start,
                    end_char: scalar_end,
                };
                ranges.extend(std::iter::repeat_n(range, grapheme.len()));
                scalar_start = scalar_end;
            }
            ranges
        }
    }
}

fn coalesce_character_ranges(mut ranges: Vec<MessageMatchCharRange>) -> Vec<MessageMatchCharRange> {
    ranges.sort_by_key(|range| (range.start_char, range.end_char));
    let mut coalesced = Vec::new();
    for range in ranges {
        match coalesced.last_mut() {
            Some(MessageMatchCharRange { end_char, .. }) if range.start_char <= *end_char => {
                *end_char = (*end_char).max(range.end_char);
            }
            _ => coalesced.push(range),
        }
    }
    coalesced
}

fn character_evidence(
    selected: &str,
    selected_field_chars: usize,
    maximum: usize,
    ranges: Vec<MessageMatchCharRange>,
    densest_window: bool,
) -> MessageMatchEvidence {
    let matched_chars_total = ranges
        .iter()
        .map(|range| range.end_char - range.start_char)
        .sum();
    let excerpt_start_char = if selected_field_chars <= maximum {
        0
    } else if densest_window {
        densest_excerpt_start(&ranges, maximum, selected_field_chars)
    } else {
        let first = ranges[0];
        let width = first.end_char - first.start_char;
        first
            .start_char
            .saturating_sub(maximum.saturating_sub(width) / 2)
            .min(selected_field_chars - maximum)
    };
    let excerpt_end_char = (excerpt_start_char + maximum).min(selected_field_chars);
    let shown = ranges
        .iter()
        .filter_map(|range| {
            let start = range.start_char.max(excerpt_start_char);
            let end = range.end_char.min(excerpt_end_char);
            (start < end).then_some(MessageMatchCharRange {
                start_char: start - excerpt_start_char,
                end_char: end - excerpt_start_char,
            })
        })
        .collect::<Vec<_>>();
    let matched_chars_shown = shown
        .iter()
        .map(|range| range.end_char - range.start_char)
        .sum();
    MessageMatchEvidence {
        excerpt: selected
            .chars()
            .skip(excerpt_start_char)
            .take(excerpt_end_char - excerpt_start_char)
            .collect(),
        excerpt_start_char,
        selected_field_chars,
        markers: MessageMatchMarkers::Characters {
            ranges: shown,
            matched_chars_total,
            matched_chars_shown,
        },
    }
}

fn densest_excerpt_start(
    ranges: &[MessageMatchCharRange],
    maximum: usize,
    selected_field_chars: usize,
) -> usize {
    let indices = ranges
        .iter()
        .flat_map(|range| range.start_char..range.end_char)
        .collect::<Vec<_>>();
    let mut best = (0, 0);
    let mut right = 0;
    for left in 0..indices.len() {
        while right < indices.len() && indices[right] < indices[left] + maximum {
            right += 1;
        }
        let candidate = (
            right - left,
            indices[left].min(selected_field_chars - maximum),
        );
        if candidate.0 > best.0 || (candidate.0 == best.0 && candidate.1 < best.1) {
            best = candidate;
        }
    }
    best.1
}

fn boundary_evidence(
    selected: &str,
    selected_field_chars: usize,
    maximum: usize,
    boundary: usize,
) -> MessageMatchEvidence {
    let excerpt_start_char = boundary
        .saturating_sub(maximum / 2)
        .min(selected_field_chars.saturating_sub(maximum));
    let excerpt_end_char = (excerpt_start_char + maximum).min(selected_field_chars);
    MessageMatchEvidence {
        excerpt: selected
            .chars()
            .skip(excerpt_start_char)
            .take(excerpt_end_char - excerpt_start_char)
            .collect(),
        excerpt_start_char,
        selected_field_chars,
        markers: MessageMatchMarkers::Boundary {
            at_char: boundary - excerpt_start_char,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn hit(content: impl Into<String>) -> crate::models::MessageHit {
        crate::models::MessageHit {
            session_id: "claude:evidence".into(),
            provider: Provider::Claude,
            seq: 7,
            role: Role::Tool,
            kind: MessageKind::ToolCall,
            ts: None,
            tool_name: Some("exec_command".into()),
            tool_call_id: Some("tool-7".into()),
            fuzzy_score: None,
            content: content.into(),
        }
    }

    #[test]
    fn query_and_target_constructors_validate_input() {
        assert_eq!(
            MessageQuery::literal("").unwrap_err().code(),
            "invalid-query"
        );
        assert_eq!(
            MessageQuery::regex("[").unwrap_err().code(),
            "invalid-query"
        );
        assert_eq!(
            MessageQuery::fuzzy("ab").unwrap_err().code(),
            "invalid-query"
        );
        assert!(MessageQuery::fuzzy("a🦀b").is_ok());
        assert!(MessageTarget::tool_argument("").is_ok());
        assert!(MessageTarget::tool_argument("/request/path").is_ok());
        assert_eq!(
            MessageTarget::tool_argument("request/path")
                .unwrap_err()
                .code(),
            "invalid-json-pointer"
        );
        assert!(MessageTarget::tool_argument("/~0key/~1path").is_ok());
        assert!(MessageTarget::tool_argument("/~bad").is_err());
        assert_eq!(
            MessageQuery::fuzzy(" \t\n").unwrap_err().code(),
            "invalid-query"
        );
    }

    #[test]
    fn builder_rejects_cross_field_conflicts() {
        let sequence = SequenceRange::new(Some(1), Some(2)).unwrap();
        let error = MessageSearchRequest::builder(
            MessageQuery::literal("needle").unwrap(),
            MessageTarget::content(),
        )
        .sequence(sequence)
        .build()
        .unwrap_err();
        assert_eq!(error.code(), "parameter-conflict");

        let error = MessageSearchRequest::builder(
            MessageQuery::All,
            MessageTarget::tool_argument("/cmd").unwrap(),
        )
        .kind(MessageKind::Conversation)
        .build()
        .unwrap_err();
        assert_eq!(error.code(), "parameter-conflict");

        let error = MessageSearchRequest::builder(
            MessageQuery::literal("needle").unwrap(),
            MessageTarget::content(),
        )
        .role(Role::Compaction)
        .include_compaction(false)
        .build()
        .unwrap_err();
        assert_eq!(error.code(), "parameter-conflict");

        let error = MessageSearchRequest::builder(MessageQuery::All, MessageTarget::content())
            .match_evidence_max_chars(NonZeroUsize::new(20).unwrap())
            .build()
            .unwrap_err();
        assert!(error.to_string().contains("requires a literal"));
    }

    #[test]
    fn selected_tool_argument_evidence_exposes_a_match_beyond_raw_boundaries() {
        let command = format!("{}Trash{}", "x".repeat(855), "y".repeat(400));
        let content = serde_json::json!({"args": {"command": command}}).to_string();
        let evidence = attach_match_evidence(
            &MessageQuery::literal("trash").unwrap(),
            &MessageTarget::tool_argument("/command").unwrap(),
            NonZeroUsize::new(40).unwrap(),
            vec![hit(content)],
        )
        .unwrap();

        let evidence = evidence[0].match_evidence().unwrap();
        assert!(evidence.excerpt.to_lowercase().contains("trash"));
        assert!(evidence.excerpt_start_char > 220);
        assert_eq!(evidence.excerpt.chars().count(), 40);
        assert_eq!(evidence.selected_field_chars, 1_260);
    }

    #[test]
    fn long_literal_keeps_complete_source_occurrence_beside_bounded_evidence() {
        let literal = "Needle".repeat(80);
        let content = format!("prefix {literal} suffix");
        let hits = attach_match_evidence(
            &MessageQuery::literal(literal.to_lowercase()).unwrap(),
            &MessageTarget::content(),
            NonZeroUsize::new(40).unwrap(),
            vec![hit(content)],
        )
        .unwrap();

        let hit = &hits[0];
        assert_eq!(hit.match_evidence().unwrap().excerpt.chars().count(), 40);
        let source = hit.literal_match().expect("literal source occurrence");
        assert_eq!(source.text, literal);
        assert_eq!(source.start_char, 7);
        assert_eq!(source.end_char, 7 + literal.chars().count());
    }

    #[test]
    fn content_extent_distinguishes_complete_head_tail_and_character_omissions() {
        let original = "alpha\nbeta\ngamma";

        assert_eq!(
            MessageContentExtent::describe(original, original, original, 0, false),
            MessageContentExtent {
                complete: true,
                omitted_start: false,
                omitted_end: false,
                returned_chars: 16,
                returned_lines: 3,
                original_chars: Some(16),
                original_lines: Some(3),
            }
        );

        let head = "alpha\n";
        let head_extent = MessageContentExtent::describe(original, head, head, 1, false);
        assert!(!head_extent.complete);
        assert!(!head_extent.omitted_start);
        assert!(head_extent.omitted_end);
        assert_eq!(head_extent.original_chars, None);

        let tail = "gamma";
        let tail_extent = MessageContentExtent::describe(original, tail, tail, -1, false);
        assert!(!tail_extent.complete);
        assert!(tail_extent.omitted_start);
        assert!(!tail_extent.omitted_end);

        let preview = "alp...";
        let preview_extent = MessageContentExtent::describe(original, original, preview, 0, true);
        assert!(!preview_extent.complete);
        assert!(!preview_extent.omitted_start);
        assert!(preview_extent.omitted_end);
        assert_eq!(preview_extent.returned_chars, 6);
    }

    #[test]
    fn regex_zero_width_and_fuzzy_matches_have_typed_markers() {
        let regex = attach_match_evidence(
            &MessageQuery::regex(r"(?m)^").unwrap(),
            &MessageTarget::content(),
            NonZeroUsize::new(8).unwrap(),
            vec![hit("abcdefghijk")],
        )
        .unwrap();
        assert!(matches!(
            regex[0].match_evidence().unwrap().markers,
            MessageMatchMarkers::Boundary { at_char: 0 }
        ));

        let fuzzy = attach_match_evidence(
            &MessageQuery::fuzzy("tst").unwrap(),
            &MessageTarget::content(),
            NonZeroUsize::new(12).unwrap(),
            vec![hit("prefix test suffix")],
        )
        .unwrap();
        let evidence = fuzzy[0].match_evidence().unwrap();
        assert!(matches!(
            evidence.markers,
            MessageMatchMarkers::Characters {
                matched_chars_total: 3,
                matched_chars_shown: 3,
                ..
            }
        ));
    }

    #[test]
    fn fuzzy_grapheme_indices_map_to_complete_scalar_ranges() {
        for (selected, expected) in [
            (
                "e\u{301} prefix test",
                vec![
                    MessageMatchCharRange {
                        start_char: 10,
                        end_char: 11,
                    },
                    MessageMatchCharRange {
                        start_char: 12,
                        end_char: 14,
                    },
                ],
            ),
            (
                "é e\u{301} prefix test",
                vec![
                    MessageMatchCharRange {
                        start_char: 12,
                        end_char: 13,
                    },
                    MessageMatchCharRange {
                        start_char: 14,
                        end_char: 16,
                    },
                ],
            ),
            (
                "👩‍💻 test",
                vec![
                    MessageMatchCharRange {
                        start_char: 4,
                        end_char: 5,
                    },
                    MessageMatchCharRange {
                        start_char: 6,
                        end_char: 8,
                    },
                ],
            ),
        ] {
            let fuzzy = attach_match_evidence(
                &MessageQuery::fuzzy("tst").unwrap(),
                &MessageTarget::content(),
                NonZeroUsize::new(30).unwrap(),
                vec![hit(selected)],
            )
            .unwrap();
            let MessageMatchMarkers::Characters { ranges, .. } =
                &fuzzy[0].match_evidence().unwrap().markers
            else {
                panic!("fuzzy evidence must use character ranges");
            };
            assert_eq!(ranges, &expected);
        }
    }

    #[test]
    fn queryless_hits_omit_evidence_without_projecting_selected_fields() {
        let hits = attach_match_evidence(
            &MessageQuery::All,
            &MessageTarget::tool_argument("/missing").unwrap(),
            NonZeroUsize::new(20).unwrap(),
            vec![hit("not-json")],
        )
        .unwrap();
        assert!(hits[0].match_evidence().is_none());
    }

    #[test]
    fn fuzzy_extent_and_latest_window_rules_are_explicit() {
        let fuzzy = MessageQuery::fuzzy("needle").unwrap();
        assert!(
            MessageSearchRequest::builder(fuzzy.clone(), MessageTarget::content())
                .build()
                .is_ok()
        );
        assert!(
            MessageSearchRequest::builder(fuzzy.clone(), MessageTarget::content())
                .extent(RequestedExtent::page(Some(10), 1).unwrap())
                .build()
                .is_ok(),
            "deterministically ranked fuzzy results support numeric offset pages"
        );
        assert!(
            MessageSearchRequest::builder(fuzzy, MessageTarget::content())
                .extent(RequestedExtent::page(Some(10), 0).unwrap())
                .build()
                .is_ok()
        );

        let latest = MessageSearchRequest::builder(
            MessageQuery::regex("needle").unwrap(),
            MessageTarget::content(),
        )
        .match_window(MatchWindow::Latest)
        .build()
        .unwrap_err();
        assert!(latest.to_string().contains("requires one session"));

        assert!(
            MessageSearchRequest::builder(MessageQuery::All, MessageTarget::content())
                .session_id("claude:session")
                .unwrap()
                .match_window(MatchWindow::Latest)
                .build()
                .is_ok()
        );
    }

    #[test]
    fn signed_line_window_preserves_full_head_and_tail() {
        assert_eq!(LineWindow::from_signed(0).unwrap(), LineWindow::Full);
        assert_eq!(LineWindow::from_signed(3).unwrap().to_signed().unwrap(), 3);
        assert_eq!(
            LineWindow::from_signed(-3).unwrap().to_signed().unwrap(),
            -3
        );
        assert!(LineWindow::from_signed(i64::MIN).is_err());
    }

    #[test]
    fn complete_valid_request_preserves_every_orthogonal_concept() {
        let time = RequestedTimeRange::new(
            Some(Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap()),
            Some(Utc.with_ymd_and_hms(2026, 7, 22, 0, 0, 0).unwrap()),
        )
        .unwrap();
        let request = MessageSearchRequest::builder(
            MessageQuery::literal("cargo test").unwrap(),
            MessageTarget::tool_argument("/cmd").unwrap(),
        )
        .role(Role::Tool)
        .kind(MessageKind::ToolCall)
        .provider(Provider::Claude)
        .session_id("claude:session")
        .unwrap()
        .workspace_path_prefix("/workspace")
        .unwrap()
        .transcript_path_prefix("/transcripts")
        .unwrap()
        .exclude_workspace_path_prefix("/workspace/vendor")
        .unwrap()
        .exclude_transcript_path_prefix("/transcripts/archive")
        .unwrap()
        .exclude_session_id("claude:excluded")
        .unwrap()
        .time(time)
        .sequence(SequenceRange::new(Some(2), Some(8)).unwrap())
        .tool_name_contains("exec")
        .unwrap()
        .include_compaction(true)
        .match_window(MatchWindow::Latest)
        .context(ContextWindow::new(1, 3))
        .include_refs(true)
        .message_lines(LineWindow::from_signed(-8).unwrap())
        .extent(RequestedExtent::page(Some(25), 5).unwrap())
        .purpose(PurposeSelection::new("historical-audit", NonZeroU32::new(2)).unwrap())
        .receipt_level(ReceiptLevel::Full)
        .build()
        .unwrap();

        assert_eq!(request.query().text(), Some("cargo test"));
        assert_eq!(request.target().field(), SearchField::ToolArgument);
        assert_eq!(
            request.target().argument_path().map(JsonPointer::as_str),
            Some("/cmd")
        );
        assert_eq!(request.predicates().session(), Some("claude:session"));
        assert_eq!(request.predicates().sequence().unwrap().from(), Some(2));
        assert_eq!(request.context(), Some(ContextWindow::new(1, 3)));
        assert_eq!(request.presentation().include_refs(), Some(true));
        assert_eq!(request.match_window(), Some(MatchWindow::Latest));
        assert_eq!(request.receipt_level(), Some(ReceiptLevel::Full));
        assert_eq!(request.purpose().unwrap().name(), "historical-audit");
    }

    #[test]
    fn range_extent_and_purpose_failures_have_stable_codes() {
        assert_eq!(
            RequestedExtent::page(Some(0), 0).unwrap_err().code(),
            "invalid-parameter"
        );
        assert_eq!(
            SequenceRange::new(Some(2), Some(1)).unwrap_err().code(),
            "invalid-parameter"
        );
        assert_eq!(
            RequestedTimeRange::new(
                Some(Utc.with_ymd_and_hms(2026, 7, 2, 0, 0, 0).unwrap()),
                Some(Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap()),
            )
            .unwrap_err()
            .code(),
            "invalid-parameter"
        );
        assert_eq!(
            PurposeSelection::new("hard2parse", None)
                .unwrap_err()
                .code(),
            "invalid-parameter"
        );
    }
}
