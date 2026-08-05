// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use chrono::{DateTime, Utc};
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as NucleoConfig, Matcher as NucleoMatcher, Utf32Str};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Serialize, Serializer};
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;

use crate::hashing::FramedSha256;
use crate::models::{MessageKind, MessageSearchMode, Provider, Role, SearchField, SessionMeta};
use crate::refs::MessageRef;

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
            Self::Literal(_) => Some(MessageSearchMode::Literal),
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

/// Preset for the amount of one selected message value returned to a caller.
///
/// This changes presentation only. Result membership, ordering, context, includes, and receipts
/// are controlled independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum DetailLevel {
    Compact,
    Full,
}

/// Character budget for the boundary/full-value view of one selected message field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FieldViewBudget {
    NoCharLimit,
    MaxChars { max_chars: NonZeroUsize },
}

impl FieldViewBudget {
    pub fn max_chars(max_chars: usize) -> Result<Self, MessageSearchError> {
        NonZeroUsize::new(max_chars)
            .map(|max_chars| Self::MaxChars { max_chars })
            .ok_or_else(|| MessageSearchError::InvalidParameter {
                parameter: "field_view",
                reason: "max_chars must be a positive character count; use kind=no_char_limit to apply no additional character limit after line selection".into(),
            })
    }
}

/// Character budget for the independently match-centered view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MatchViewBudget {
    MinimalSpan,
    MaxChars { max_chars: NonZeroUsize },
}

/// Optional payload groups added to the stable message-search semantic core.
///
/// Context and receipt metadata intentionally are not include groups: their dedicated parameters
/// remain the single owners of those payloads.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "snake_case")]
pub enum MessageSearchInclude {
    NormalizedSessionMetadata,
    ParsedReferences,
    RawProviderMetadata,
    RuntimeDiagnostics,
}

impl MatchViewBudget {
    pub fn max_chars(max_chars: usize) -> Result<Self, MessageSearchError> {
        NonZeroUsize::new(max_chars)
            .map(|max_chars| Self::MaxChars { max_chars })
            .ok_or_else(|| MessageSearchError::InvalidParameter {
                parameter: "match_view",
                reason: "max_chars must be a positive character count; use kind=minimal_span for only the match span".into(),
            })
    }
}

fn decode_view_max_chars(
    object: &serde_json::Map<String, serde_json::Value>,
    parameter: &str,
    maximum: usize,
) -> Result<usize, MessageSearchError> {
    let raw = object
        .get("max_chars")
        .ok_or_else(|| MessageSearchError::InvalidParameter {
            parameter: "view_budget",
            reason: format!("{parameter}.max_chars is required when kind=max_chars"),
        })?;
    let raw = raw
        .as_u64()
        .ok_or_else(|| MessageSearchError::InvalidParameter {
            parameter: "view_budget",
            reason: format!("{parameter}.max_chars must be a positive integer"),
        })?;
    let value = usize::try_from(raw).map_err(|_| MessageSearchError::InvalidParameter {
        parameter: "view_budget",
        reason: format!("{parameter}.max_chars exceeds this platform's supported integer range"),
    })?;
    if value == 0 || value > maximum {
        return Err(MessageSearchError::InvalidParameter {
            parameter: "view_budget",
            reason: format!(
                "{parameter}.max_chars must be an integer from 1 through {maximum}; got {value}"
            ),
        });
    }
    Ok(value)
}

fn reject_view_unknown_fields(
    object: &serde_json::Map<String, serde_json::Value>,
    parameter: &str,
    accepts_max_chars: bool,
) -> Result<(), MessageSearchError> {
    if let Some(key) = object
        .keys()
        .find(|key| *key != "kind" && !(accepts_max_chars && *key == "max_chars"))
    {
        return Err(MessageSearchError::InvalidParameter {
            parameter: "view_budget",
            reason: format!(
                "{parameter} contains unknown field {key:?}; accepted fields are {}",
                if accepts_max_chars {
                    "kind and max_chars"
                } else {
                    "kind"
                }
            ),
        });
    }
    Ok(())
}

/// Decode the object-shaped field-view contract shared by MCP and Python adapters.
pub fn decode_field_view_budget(
    value: &serde_json::Value,
    maximum: usize,
) -> Result<FieldViewBudget, MessageSearchError> {
    let object = value
        .as_object()
        .ok_or_else(|| MessageSearchError::InvalidParameter {
            parameter: "field_view",
            reason: "must be an object with a kind field".into(),
        })?;
    match object.get("kind").and_then(serde_json::Value::as_str) {
        Some("no_char_limit") => {
            reject_view_unknown_fields(object, "field_view", false)?;
            Ok(FieldViewBudget::NoCharLimit)
        }
        Some("max_chars") => {
            reject_view_unknown_fields(object, "field_view", true)?;
            FieldViewBudget::max_chars(decode_view_max_chars(object, "field_view", maximum)?)
        }
        Some(other) => Err(MessageSearchError::InvalidParameter {
            parameter: "field_view",
            reason: format!("kind must be no_char_limit or max_chars; got {other:?}"),
        }),
        None => Err(MessageSearchError::InvalidParameter {
            parameter: "field_view",
            reason: "kind is required".into(),
        }),
    }
}

/// Decode the object-shaped match-view contract shared by MCP and Python adapters.
pub fn decode_match_view_budget(
    value: &serde_json::Value,
    maximum: usize,
) -> Result<MatchViewBudget, MessageSearchError> {
    let object = value
        .as_object()
        .ok_or_else(|| MessageSearchError::InvalidParameter {
            parameter: "match_view",
            reason: "must be an object with a kind field".into(),
        })?;
    match object.get("kind").and_then(serde_json::Value::as_str) {
        Some("minimal_span") => {
            reject_view_unknown_fields(object, "match_view", false)?;
            Ok(MatchViewBudget::MinimalSpan)
        }
        Some("max_chars") => {
            reject_view_unknown_fields(object, "match_view", true)?;
            MatchViewBudget::max_chars(decode_view_max_chars(object, "match_view", maximum)?)
        }
        Some(other) => Err(MessageSearchError::InvalidParameter {
            parameter: "match_view",
            reason: format!("kind must be minimal_span or max_chars; got {other:?}"),
        }),
        None => Err(MessageSearchError::InvalidParameter {
            parameter: "match_view",
            reason: "kind is required".into(),
        }),
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContextWindow {
    messages_before: usize,
    messages_after: usize,
}

impl ContextWindow {
    pub const fn new(messages_before: usize, messages_after: usize) -> Self {
        Self {
            messages_before,
            messages_after,
        }
    }

    pub const fn symmetric(count: usize) -> Self {
        Self::new(count, count)
    }

    pub const fn messages_before(self) -> usize {
        self.messages_before
    }

    pub const fn messages_after(self) -> usize {
        self.messages_after
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
    providers: Option<Vec<Provider>>,
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
            providers: None,
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

    /// Selected session sources in canonical identifier order, or `None` for every source.
    pub fn providers(&self) -> Option<&[Provider]> {
        self.providers.as_deref()
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
    message_lines: Option<LineWindow>,
    detail: Option<DetailLevel>,
    field_view: Option<FieldViewBudget>,
    match_view: Option<MatchViewBudget>,
}

impl MessagePresentation {
    pub const fn message_lines(&self) -> Option<LineWindow> {
        self.message_lines
    }

    pub const fn detail(&self) -> Option<DetailLevel> {
        self.detail
    }

    pub const fn field_view(&self) -> Option<FieldViewBudget> {
        self.field_view
    }

    pub const fn match_view(&self) -> Option<MatchViewBudget> {
        self.match_view
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum SearchSurface {
    Rust,
    Cli,
    Mcp,
    Python,
}

impl SearchSurface {
    pub const ALL: &'static [Self] = &[Self::Rust, Self::Cli, Self::Mcp, Self::Python];
}

/// Stable conceptual identity for one message-search input.
///
/// Adapters may project a concept into idiomatic syntax, but they must not create a second
/// semantic owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageSearchParameter {
    Query,
    QueryMode,
    Field,
    ArgumentPath,
    Role,
    Kinds,
    Providers,
    SessionId,
    WorkspacePathPrefix,
    TranscriptPathPrefix,
    ExcludeWorkspacePathPrefixes,
    ExcludeTranscriptPathPrefixes,
    ExcludeSessionIds,
    Since,
    Until,
    Sequence,
    ToolNameContains,
    IncludeCompaction,
    MatchWindow,
    Context,
    ResultExtent,
    Detail,
    LinesPerMessage,
    FieldView,
    MatchView,
    Purpose,
    ReceiptLevel,
    Include,
}

impl MessageSearchParameter {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::QueryMode => "query_mode",
            Self::Field => "field",
            Self::ArgumentPath => "argument_path",
            Self::Role => "role",
            Self::Kinds => "kinds",
            Self::Providers => "providers",
            Self::SessionId => "session_id",
            Self::WorkspacePathPrefix => "workspace_path_prefix",
            Self::TranscriptPathPrefix => "transcript_path_prefix",
            Self::ExcludeWorkspacePathPrefixes => "exclude_workspace_path_prefixes",
            Self::ExcludeTranscriptPathPrefixes => "exclude_transcript_path_prefixes",
            Self::ExcludeSessionIds => "exclude_session_ids",
            Self::Since => "since",
            Self::Until => "until",
            Self::Sequence => "sequence",
            Self::ToolNameContains => "tool_name_contains",
            Self::IncludeCompaction => "include_compaction",
            Self::MatchWindow => "match_window",
            Self::Context => "context",
            Self::ResultExtent => "result_extent",
            Self::Detail => "detail",
            Self::LinesPerMessage => "lines_per_message",
            Self::FieldView => "field_view",
            Self::MatchView => "match_view",
            Self::Purpose => "purpose",
            Self::ReceiptLevel => "receipt_level",
            Self::Include => "include",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageSearchObjectFieldDomain {
    PositiveCharacterCount,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MessageSearchObjectFieldSpec {
    name: &'static str,
    required: bool,
    domain: MessageSearchObjectFieldDomain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MessageSearchObjectVariantSpec {
    value: &'static str,
    selects: &'static str,
    fields: Vec<MessageSearchObjectFieldSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageSearchParameterDomain {
    Text {
        non_empty: bool,
    },
    Boolean,
    Enum {
        accepted_values: Vec<String>,
    },
    NonEmptySet {
        accepted_values: Vec<String>,
    },
    NonNegativeCount,
    SignedEdgeCount,
    TimeBound,
    SequenceRange,
    ContextWindow,
    ResultExtent,
    FieldView {
        discriminator: &'static str,
        accepted_variants: Vec<MessageSearchObjectVariantSpec>,
    },
    MatchView {
        discriminator: &'static str,
        accepted_variants: Vec<MessageSearchObjectVariantSpec>,
    },
    PurposeSelection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageSearchOmission {
    AllEligible,
    TypedDefault,
    SurfacePolicy,
    NoAdditionalFilter,
    QuerylessSearch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MessageSearchParameterSpec {
    parameter: MessageSearchParameter,
    selects: &'static str,
    domain: MessageSearchParameterDomain,
    omission: MessageSearchOmission,
    surfaces: &'static [SearchSurface],
}

impl MessageSearchParameterSpec {
    pub const fn parameter(&self) -> MessageSearchParameter {
        self.parameter
    }

    pub const fn selects(&self) -> &'static str {
        self.selects
    }

    pub const fn domain(&self) -> &MessageSearchParameterDomain {
        &self.domain
    }

    pub const fn omission(&self) -> MessageSearchOmission {
        self.omission
    }

    pub const fn surfaces(&self) -> &'static [SearchSurface] {
        self.surfaces
    }

    pub fn accepted_values(&self) -> &[String] {
        match &self.domain {
            MessageSearchParameterDomain::Enum { accepted_values }
            | MessageSearchParameterDomain::NonEmptySet { accepted_values } => accepted_values,
            _ => &[],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueOriginKind {
    Explicit,
    DetailPreset,
    Purpose,
    OperationConfig,
    SurfaceConfig,
    TypedDefault,
    Derived,
}

/// Executable cross-parameter rule identity shared by request validation and caller specifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageSearchRule {
    DetailOwnsPresentationBudgets,
    SequenceRequiresSession,
    KindsMustRemainSatisfiable,
    CompactionRoleRequiresCompactionKind,
    ToolArgumentRequiresToolCallKind,
    MatchViewRequiresQuery,
    FuzzyRejectsMatchWindow,
    LatestWindowRequiresSession,
    FuzzyRejectsAllResults,
}

impl MessageSearchRule {
    pub const ALL: &'static [Self] = &[
        Self::DetailOwnsPresentationBudgets,
        Self::SequenceRequiresSession,
        Self::KindsMustRemainSatisfiable,
        Self::CompactionRoleRequiresCompactionKind,
        Self::ToolArgumentRequiresToolCallKind,
        Self::MatchViewRequiresQuery,
        Self::FuzzyRejectsMatchWindow,
        Self::LatestWindowRequiresSession,
        Self::FuzzyRejectsAllResults,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DetailOwnsPresentationBudgets => "detail_owns_presentation_budgets",
            Self::SequenceRequiresSession => "sequence_requires_session",
            Self::KindsMustRemainSatisfiable => "kinds_must_remain_satisfiable",
            Self::CompactionRoleRequiresCompactionKind => {
                "compaction_role_requires_compaction_kind"
            }
            Self::ToolArgumentRequiresToolCallKind => "tool_argument_requires_tool_call_kind",
            Self::MatchViewRequiresQuery => "match_view_requires_query",
            Self::FuzzyRejectsMatchWindow => "fuzzy_rejects_match_window",
            Self::LatestWindowRequiresSession => "latest_window_requires_session",
            Self::FuzzyRejectsAllResults => "fuzzy_rejects_all_results",
        }
    }

    pub const fn message(self) -> &'static str {
        match self {
            Self::DetailOwnsPresentationBudgets => {
                "detail conflicts with lines_per_message, field_view, and match_view; omit detail to compose custom presentation budgets"
            }
            Self::SequenceRequiresSession => "sequence bounds require one session",
            Self::KindsMustRemainSatisfiable => {
                "the selected kinds exclude every message class, so nothing can match"
            }
            Self::CompactionRoleRequiresCompactionKind => {
                "role=compaction requires compaction among the selected kinds; include_compaction=false or a kinds set without it removes every match"
            }
            Self::ToolArgumentRequiresToolCallKind => {
                "tool-argument target requires tool_call among the selected kinds"
            }
            Self::MatchViewRequiresQuery => {
                "match_view requires a literal, regex, or fuzzy query"
            }
            Self::FuzzyRejectsMatchWindow => "match_window does not apply to fuzzy queries",
            Self::LatestWindowRequiresSession => "match_window=latest requires one session",
            Self::FuzzyRejectsAllResults => "fuzzy search does not support all_results",
        }
    }

    fn error(self) -> MessageSearchError {
        MessageSearchError::Conflict(format!("{}: {}", self.as_str(), self.message()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MessageSearchRuleDescriptor {
    rule: MessageSearchRule,
    message: &'static str,
}

impl MessageSearchRuleDescriptor {
    const fn new(rule: MessageSearchRule) -> Self {
        Self {
            rule,
            message: rule.message(),
        }
    }

    pub const fn rule(self) -> MessageSearchRule {
        self.rule
    }

    pub const fn message(self) -> &'static str {
        self.message
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MessageSearchParameterRegistry {
    purpose: &'static str,
    parameters: Vec<MessageSearchParameterSpec>,
    precedence: &'static [ValueOriginKind],
    #[serde(skip)]
    rules: &'static [MessageSearchRule],
    #[serde(rename = "rules")]
    rule_descriptors: Vec<MessageSearchRuleDescriptor>,
}

impl MessageSearchParameterRegistry {
    /// Return the process-wide immutable input catalogue.
    ///
    /// Construction is `O(P + V)` time and memory for bounded parameter and vocabulary counts and
    /// occurs once per process. Search calls never clone or attach this catalogue to responses.
    pub fn current() -> &'static Self {
        static REGISTRY: std::sync::OnceLock<MessageSearchParameterRegistry> =
            std::sync::OnceLock::new();
        REGISTRY.get_or_init(Self::build)
    }

    pub const fn purpose(&self) -> &'static str {
        self.purpose
    }

    pub fn parameters(&self) -> &[MessageSearchParameterSpec] {
        &self.parameters
    }

    pub fn parameter(
        &self,
        parameter: MessageSearchParameter,
    ) -> Option<&MessageSearchParameterSpec> {
        self.parameters
            .iter()
            .find(|candidate| candidate.parameter == parameter)
    }

    pub const fn precedence(&self) -> &'static [ValueOriginKind] {
        self.precedence
    }

    pub const fn rules(&self) -> &'static [MessageSearchRule] {
        self.rules
    }

    pub fn rule_descriptors(&self) -> &[MessageSearchRuleDescriptor] {
        &self.rule_descriptors
    }

    fn build() -> Self {
        fn serialized_variants<T>() -> Vec<String>
        where
            T: clap::ValueEnum + Copy + Serialize + 'static,
        {
            T::value_variants()
                .iter()
                .map(|value| {
                    serde_json::to_value(*value)
                        .expect("message-search enum serializes")
                        .as_str()
                        .expect("message-search enum serializes as a string")
                        .to_owned()
                })
                .collect()
        }

        let enum_domain = |accepted_values| MessageSearchParameterDomain::Enum { accepted_values };
        let set_domain =
            |accepted_values| MessageSearchParameterDomain::NonEmptySet { accepted_values };
        let max_chars_field = || MessageSearchObjectFieldSpec {
            name: "max_chars",
            required: true,
            domain: MessageSearchObjectFieldDomain::PositiveCharacterCount,
        };
        let parameter = |parameter, selects, domain, omission| MessageSearchParameterSpec {
            parameter,
            selects,
            domain,
            omission,
            surfaces: SearchSurface::ALL,
        };
        let parameters = vec![
            parameter(
                MessageSearchParameter::Query,
                "Text to match.",
                MessageSearchParameterDomain::Text { non_empty: false },
                MessageSearchOmission::QuerylessSearch,
            ),
            parameter(
                MessageSearchParameter::QueryMode,
                "How query is matched.",
                enum_domain(serialized_variants::<MessageSearchMode>()),
                MessageSearchOmission::TypedDefault,
            ),
            parameter(
                MessageSearchParameter::Field,
                "Which field to match.",
                enum_domain(serialized_variants::<SearchField>()),
                MessageSearchOmission::TypedDefault,
            ),
            parameter(
                MessageSearchParameter::ArgumentPath,
                "RFC 6901 pointer into tool arguments; needs field=tool_argument",
                MessageSearchParameterDomain::Text { non_empty: true },
                MessageSearchOmission::NoAdditionalFilter,
            ),
            parameter(
                MessageSearchParameter::Role,
                "Author role.",
                enum_domain(serialized_variants::<Role>()),
                MessageSearchOmission::AllEligible,
            ),
            parameter(
                MessageSearchParameter::Kinds,
                "Message classes to return.",
                set_domain(serialized_variants::<MessageKind>()),
                MessageSearchOmission::TypedDefault,
            ),
            parameter(
                MessageSearchParameter::Providers,
                "Session sources.",
                set_domain(
                    crate::source::PROVIDERS
                        .iter()
                        .map(|provider| provider.as_str().to_owned())
                        .collect(),
                ),
                MessageSearchOmission::AllEligible,
            ),
            parameter(
                MessageSearchParameter::SessionId,
                "One session by ID or prefix.",
                MessageSearchParameterDomain::Text { non_empty: true },
                MessageSearchOmission::AllEligible,
            ),
            parameter(
                MessageSearchParameter::WorkspacePathPrefix,
                "Match cwd or repo-root prefix.",
                MessageSearchParameterDomain::Text { non_empty: true },
                MessageSearchOmission::AllEligible,
            ),
            parameter(
                MessageSearchParameter::TranscriptPathPrefix,
                "Match transcript-path prefix.",
                MessageSearchParameterDomain::Text { non_empty: true },
                MessageSearchOmission::AllEligible,
            ),
            parameter(
                MessageSearchParameter::ExcludeWorkspacePathPrefixes,
                "Excluded cwd/repo-root prefixes.",
                MessageSearchParameterDomain::Text { non_empty: true },
                MessageSearchOmission::NoAdditionalFilter,
            ),
            parameter(
                MessageSearchParameter::ExcludeTranscriptPathPrefixes,
                "Excluded transcript prefixes.",
                MessageSearchParameterDomain::Text { non_empty: true },
                MessageSearchOmission::NoAdditionalFilter,
            ),
            parameter(
                MessageSearchParameter::ExcludeSessionIds,
                "Excluded session IDs.",
                MessageSearchParameterDomain::Text { non_empty: true },
                MessageSearchOmission::NoAdditionalFilter,
            ),
            parameter(
                MessageSearchParameter::Since,
                "Lower time bound.",
                MessageSearchParameterDomain::TimeBound,
                MessageSearchOmission::NoAdditionalFilter,
            ),
            parameter(
                MessageSearchParameter::Until,
                "Upper time bound.",
                MessageSearchParameterDomain::TimeBound,
                MessageSearchOmission::NoAdditionalFilter,
            ),
            parameter(
                MessageSearchParameter::Sequence,
                "Sequence bound.",
                MessageSearchParameterDomain::SequenceRange,
                MessageSearchOmission::NoAdditionalFilter,
            ),
            parameter(
                MessageSearchParameter::ToolNameContains,
                "Substring in the tool name.",
                MessageSearchParameterDomain::Text { non_empty: true },
                MessageSearchOmission::NoAdditionalFilter,
            ),
            parameter(
                MessageSearchParameter::IncludeCompaction,
                "Keep compaction summaries.",
                MessageSearchParameterDomain::Boolean,
                MessageSearchOmission::TypedDefault,
            ),
            parameter(
                MessageSearchParameter::MatchWindow,
                "Which match per message.",
                enum_domain(serialized_variants::<MatchWindow>()),
                MessageSearchOmission::TypedDefault,
            ),
            parameter(
                MessageSearchParameter::Context,
                "Neighbours each side per hit.",
                MessageSearchParameterDomain::ContextWindow,
                MessageSearchOmission::TypedDefault,
            ),
            parameter(
                MessageSearchParameter::ResultExtent,
                "Page size.",
                MessageSearchParameterDomain::ResultExtent,
                MessageSearchOmission::SurfacePolicy,
            ),
            parameter(
                MessageSearchParameter::Detail,
                "Presentation preset.",
                enum_domain(serialized_variants::<DetailLevel>()),
                MessageSearchOmission::SurfacePolicy,
            ),
            parameter(
                MessageSearchParameter::LinesPerMessage,
                "Line window.",
                MessageSearchParameterDomain::SignedEdgeCount,
                MessageSearchOmission::SurfacePolicy,
            ),
            parameter(
                MessageSearchParameter::FieldView,
                "Character budget for the field.",
                MessageSearchParameterDomain::FieldView {
                    discriminator: "kind",
                    accepted_variants: vec![
                        MessageSearchObjectVariantSpec {
                            value: "no_char_limit",
                            selects: "No additional character limit after line selection.",
                            fields: vec![],
                        },
                        MessageSearchObjectVariantSpec {
                            value: "max_chars",
                            selects: "At most max_chars Unicode-scalar characters from the field boundary.",
                            fields: vec![max_chars_field()],
                        },
                    ],
                },
                MessageSearchOmission::SurfacePolicy,
            ),
            parameter(
                MessageSearchParameter::MatchView,
                "Character budget around the match.",
                MessageSearchParameterDomain::MatchView {
                    discriminator: "kind",
                    accepted_variants: vec![
                        MessageSearchObjectVariantSpec {
                            value: "minimal_span",
                            selects: "Only the complete selected match span.",
                            fields: vec![],
                        },
                        MessageSearchObjectVariantSpec {
                            value: "max_chars",
                            selects: "Up to max_chars Unicode-scalar characters centered on the complete match.",
                            fields: vec![max_chars_field()],
                        },
                    ],
                },
                MessageSearchOmission::TypedDefault,
            ),
            parameter(
                MessageSearchParameter::Purpose,
                "Configured preference bundle.",
                MessageSearchParameterDomain::PurposeSelection,
                MessageSearchOmission::NoAdditionalFilter,
            ),
            parameter(
                MessageSearchParameter::ReceiptLevel,
                "Diagnostics.",
                enum_domain(serialized_variants::<ReceiptLevel>()),
                MessageSearchOmission::TypedDefault,
            ),
            parameter(
                MessageSearchParameter::Include,
                "Payload groups; a supplied set replaces the default",
                set_domain(serialized_variants::<MessageSearchInclude>()),
                MessageSearchOmission::TypedDefault,
            ),
        ];
        Self {
            purpose: "Search indexed AI-session messages while separating result selection, context, presentation, optional payloads, and receipts.",
            parameters,
            precedence: &[
                ValueOriginKind::Explicit,
                ValueOriginKind::DetailPreset,
                ValueOriginKind::Purpose,
                ValueOriginKind::OperationConfig,
                ValueOriginKind::SurfaceConfig,
                ValueOriginKind::TypedDefault,
                ValueOriginKind::Derived,
            ],
            rules: MessageSearchRule::ALL,
            rule_descriptors: MessageSearchRule::ALL
                .iter()
                .copied()
                .map(MessageSearchRuleDescriptor::new)
                .collect(),
        }
    }
}

/// Static caller catalogue plus the real planner's configured default semantic request.
///
/// The registry is borrowed from the process-wide cache. The configured request is produced by
/// [`crate::service::MessageService::plan`], so this DTO cannot drift into a second precedence
/// resolver. Construction is independent of indexed result count and message bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MessageSearchSpecification {
    registry: &'static MessageSearchParameterRegistry,
    configured_default: ResolvedMessageSearchRequest,
}

impl MessageSearchSpecification {
    pub(crate) fn new(configured_default: ResolvedMessageSearchRequest) -> Self {
        Self {
            registry: MessageSearchParameterRegistry::current(),
            configured_default,
        }
    }

    pub const fn registry(&self) -> &'static MessageSearchParameterRegistry {
        self.registry
    }

    pub const fn configured_default(&self) -> &ResolvedMessageSearchRequest {
        &self.configured_default
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "source", rename_all = "kebab-case")]
pub enum ValueOrigin {
    Explicit,
    DetailPreset { detail: DetailLevel },
    Purpose { name: String, version: NonZeroU32 },
    SurfaceConfig { surface: SearchSurface },
    OperationConfig,
    TypedDefault,
    Derived,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MessageSearchOrigins {
    pub(crate) result_extent: ValueOrigin,
    pub(crate) context_messages_before: ValueOrigin,
    pub(crate) context_messages_after: ValueOrigin,
    pub(crate) includes: ValueOrigin,
    pub(crate) detail: ValueOrigin,
    pub(crate) lines_per_message: ValueOrigin,
    pub(crate) field_view: ValueOrigin,
    pub(crate) match_view: ValueOrigin,
    pub(crate) receipt_level: ValueOrigin,
    pub(crate) result_order: ValueOrigin,
}

impl MessageSearchOrigins {
    pub fn result_extent(&self) -> &ValueOrigin {
        &self.result_extent
    }

    pub fn context_messages_before(&self) -> &ValueOrigin {
        &self.context_messages_before
    }

    pub fn context_messages_after(&self) -> &ValueOrigin {
        &self.context_messages_after
    }

    pub fn includes(&self) -> &ValueOrigin {
        &self.includes
    }

    pub fn detail(&self) -> &ValueOrigin {
        &self.detail
    }

    pub fn lines_per_message(&self) -> &ValueOrigin {
        &self.lines_per_message
    }

    pub fn field_view(&self) -> &ValueOrigin {
        &self.field_view
    }

    pub fn match_view(&self) -> &ValueOrigin {
        &self.match_view
    }

    pub fn receipt_level(&self) -> &ValueOrigin {
        &self.receipt_level
    }

    pub fn result_order(&self) -> &ValueOrigin {
        &self.result_order
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
    pub(crate) providers: Option<Vec<Provider>>,
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
    pub(crate) detail: Option<DetailLevel>,
    pub(crate) field_view: FieldViewBudget,
    pub(crate) match_view: MatchViewBudget,
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

    pub const fn detail(&self) -> Option<DetailLevel> {
        self.detail
    }

    pub const fn field_view(&self) -> FieldViewBudget {
        self.field_view
    }

    pub const fn match_view(&self) -> MatchViewBudget {
        self.match_view
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct MessageResponsePlan {
    pub(crate) context: ContextWindow,
    pub(crate) presentation: ResolvedMessagePresentation,
    pub(crate) includes: Vec<MessageSearchInclude>,
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

    pub fn includes(&self) -> &[MessageSearchInclude] {
        &self.response.includes
    }

    pub const fn receipt_level(&self) -> ReceiptLevel {
        self.receipt
    }

    pub fn origins(&self) -> &MessageSearchOrigins {
        &self.origins
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderScope {
    All,
    Selected { providers: Vec<Provider> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResolvedRequestExtent {
    Page { limit: NonZeroUsize, offset: usize },
    AllResults { offset: usize },
}

impl From<ResolvedExtent> for ResolvedRequestExtent {
    fn from(value: ResolvedExtent) -> Self {
        match value {
            ResolvedExtent::Page { limit, offset } => Self::Page { limit, offset },
            ResolvedExtent::AllResults { offset } => Self::AllResults { offset },
        }
    }
}

/// Public query-mode vocabulary shared by CLI, Python, MCP, and response documents.
///
/// Public query-mode vocabulary shared by all search surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedQueryMode {
    Literal,
    Regex,
    Fuzzy,
}

impl From<MessageSearchMode> for ResolvedQueryMode {
    fn from(value: MessageSearchMode) -> Self {
        match value {
            MessageSearchMode::Literal => Self::Literal,
            MessageSearchMode::Regex => Self::Regex,
            MessageSearchMode::Fuzzy => Self::Fuzzy,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedRequestPresentation {
    lines_per_message: i64,
    field_view: FieldViewBudget,
    match_view: MatchViewBudget,
}

impl ResolvedRequestPresentation {
    pub const fn lines_per_message(&self) -> i64 {
        self.lines_per_message
    }

    pub const fn field_view(&self) -> FieldViewBudget {
        self.field_view
    }

    pub const fn match_view(&self) -> MatchViewBudget {
        self.match_view
    }
}

/// Effective semantic choices returned with every response.
///
/// Parameter origins and planner evidence are optional receipt data; this compact record is always
/// present so a caller can interpret result and presentation extent without conversational state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedMessageSearchRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    query_mode: Option<ResolvedQueryMode>,
    target: MessageTarget,
    provider_scope: ProviderScope,
    extent: ResolvedRequestExtent,
    #[serde(skip_serializing_if = "Option::is_none")]
    match_window: Option<MatchWindow>,
    context: ContextWindow,
    presentation: ResolvedRequestPresentation,
    include: Vec<MessageSearchInclude>,
    receipt_level: ReceiptLevel,
}

impl ResolvedMessageSearchRequest {
    pub(crate) fn from_plan(plan: &MessageSearchPlan) -> Result<Self, MessageSearchError> {
        Ok(Self {
            query: plan.retrieval.query.text().map(str::to_owned),
            query_mode: plan.retrieval.query.mode().map(ResolvedQueryMode::from),
            target: plan.retrieval.target.clone(),
            provider_scope: plan
                .retrieval
                .predicates
                .providers
                .clone()
                .map_or(ProviderScope::All, |providers| ProviderScope::Selected {
                    providers,
                }),
            extent: plan.retrieval.extent.into(),
            match_window: (!matches!(plan.retrieval.query, MessageQuery::Fuzzy(_)))
                .then_some(plan.retrieval.match_window.unwrap_or_default()),
            context: plan.response.context,
            presentation: ResolvedRequestPresentation {
                lines_per_message: plan.response.presentation.message_lines.to_signed()?,
                field_view: plan.response.presentation.field_view,
                match_view: plan.response.presentation.match_view,
            },
            include: plan.response.includes.clone(),
            receipt_level: plan.receipt,
        })
    }

    pub fn query(&self) -> Option<&str> {
        self.query.as_deref()
    }

    pub const fn query_mode(&self) -> Option<ResolvedQueryMode> {
        self.query_mode
    }

    pub const fn target(&self) -> &MessageTarget {
        &self.target
    }

    pub const fn provider_scope(&self) -> &ProviderScope {
        &self.provider_scope
    }

    pub const fn extent(&self) -> ResolvedRequestExtent {
        self.extent
    }

    pub const fn match_window(&self) -> Option<MatchWindow> {
        self.match_window
    }

    pub const fn context(&self) -> ContextWindow {
        self.context
    }

    pub const fn presentation(&self) -> &ResolvedRequestPresentation {
        &self.presentation
    }

    pub fn include(&self) -> &[MessageSearchInclude] {
        &self.include
    }

    pub const fn receipt_level(&self) -> ReceiptLevel {
        self.receipt_level
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PageInfo {
    extent: ResolvedExtent,
    returned: usize,
    next_offset: Option<usize>,
    ordering: ExecutionOrder,
    earlier_results: PageSide,
    result_set_extent: ResultSetExtent,
}

impl PageInfo {
    pub(crate) const fn new(
        extent: ResolvedExtent,
        returned: usize,
        next_offset: Option<usize>,
        ordering: ExecutionOrder,
    ) -> Self {
        let offset = match extent {
            ResolvedExtent::Page { offset, .. } | ResolvedExtent::AllResults { offset } => offset,
        };
        let earlier_results = if offset == 0 {
            PageSide::None
        } else if returned > 0 {
            // A nonempty page after an offset proves that matching rows were skipped.
            PageSide::Present
        } else {
            // A positive offset alone does not distinguish no matches from a beyond-end page.
            PageSide::Unknown
        };
        let result_set_extent = match (earlier_results, next_offset.is_some()) {
            (PageSide::Present, _) | (_, true) => ResultSetExtent::Partial,
            (PageSide::None, false) => ResultSetExtent::All,
            _ => ResultSetExtent::Unknown,
        };
        Self {
            extent,
            returned,
            next_offset,
            ordering,
            earlier_results,
            result_set_extent,
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

    pub const fn returned(&self) -> usize {
        self.returned
    }

    pub const fn earlier_results(&self) -> PageSide {
        self.earlier_results
    }

    pub const fn result_set_extent(&self) -> ResultSetExtent {
        self.result_set_extent
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PageSide {
    None,
    Present,
    Unknown,
}

impl PageSide {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Present => "present",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultSetExtent {
    All,
    Partial,
    Unknown,
}

impl ResultSetExtent {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Partial => "partial",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ViewCharRange {
    pub view_start_char: usize,
    pub view_end_char_exclusive: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum MessageMatchViewMarkers {
    Characters {
        ranges: Vec<ViewCharRange>,
        matched_chars_total: usize,
        matched_chars_shown: usize,
    },
    Boundary {
        view_at_char: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MessageMatchEvidence {
    pub view_text: String,
    pub field_start_char: usize,
    pub field_total_chars: usize,
    pub markers: MessageMatchViewMarkers,
}

/// Complete selected-field occurrence for literal mode.
///
/// The independently bounded evidence excerpt can omit part of a long literal. This record keeps
/// the exact matched field text and absolute character coordinates without expanding regex or
/// fuzzy evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MessageLiteralMatch {
    pub text: String,
    pub field_start_char: usize,
    pub field_end_char_exclusive: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FieldCharRange {
    field_start_char: usize,
    field_end_char_exclusive: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CoordinateUnit {
    UnicodeScalar,
}

/// Which portion of the selected field is present in one returned view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdditionalFieldText {
    None,
    Before,
    After,
    BeforeAndAfter,
}

impl AdditionalFieldText {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Before => "before",
            Self::After => "after",
            Self::BeforeAndAfter => "before_and_after",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FieldViewExtent {
    additional_field_text: AdditionalFieldText,
    /// Full selected-field Unicode scalar count, or `None` when computing it would require an
    /// additional full-field scan solely for metadata.
    ///
    /// Deliberately per view rather than hoisted with the unit beside it. The two views are
    /// filled by different code paths -- `evidence_field_view` always knows the count while
    /// `selected_field_view` computes an `Option` -- so one can be `null` while the other is
    /// populated for the same result. Collapsing them would either invent a value or drop one,
    /// which `REQ046-preserve-boundary-results` names as silently dropping optional data.
    field_total_chars: Option<usize>,
    /// Not serialized: `CoordinateUnit` has a single variant, so this was the same constant
    /// written three times per result. It is stated once at the response root instead.
    #[serde(skip)]
    coordinate_unit: CoordinateUnit,
}

impl FieldViewExtent {
    pub const fn additional_field_text(&self) -> AdditionalFieldText {
        self.additional_field_text
    }

    pub const fn field_total_chars(&self) -> Option<usize> {
        self.field_total_chars
    }
}

/// Absolute Unicode-scalar range and remaining-text direction for returned message content.
///
/// The range length equals the returned `content` character count. `field_total_chars` stays
/// optional when obtaining it would require another full-field scan solely for metadata.
/// Completeness and both boundary conditions are derivable without parallel booleans:
/// `additional_field_text == none` means the range contains the complete field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MessageContentExtent {
    field_start_char: usize,
    field_end_char_exclusive: usize,
    #[serde(flatten)]
    extent: FieldViewExtent,
    /// Kept here, unlike the search response's views, because this shape is serialized into
    /// `get_session`, a different document with no shared root to hoist to. It also appears once
    /// per returned message rather than three times per result, so the repetition the search
    /// response had is not there to remove.
    coordinate_unit: CoordinateUnit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MessageFieldView {
    text: String,
    field_start_char: usize,
    field_end_char_exclusive: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    markers: Vec<ViewCharRange>,
    #[serde(flatten)]
    extent: FieldViewExtent,
}

impl MessageFieldView {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub const fn field_start_char(&self) -> usize {
        self.field_start_char
    }

    pub const fn field_end_char_exclusive(&self) -> usize {
        self.field_end_char_exclusive
    }

    pub fn markers(&self) -> &[ViewCharRange] {
        &self.markers
    }

    pub const fn extent(&self) -> FieldViewExtent {
        self.extent
    }

    pub(crate) fn into_content_and_extent(self) -> (String, MessageContentExtent) {
        (
            self.text,
            MessageContentExtent {
                field_start_char: self.field_start_char,
                field_end_char_exclusive: self.field_end_char_exclusive,
                extent: self.extent,
                coordinate_unit: CoordinateUnit::UnicodeScalar,
            },
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MessageResultRef<'a> {
    session_id: &'a str,
    message_seq: i64,
}

impl<'a> MessageResultRef<'a> {
    pub const fn session_id(self) -> &'a str {
        self.session_id
    }

    pub const fn message_seq(self) -> i64 {
        self.message_seq
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
    #[serde(skip_serializing)]
    field_view: Option<MessageFieldView>,
    #[serde(skip_serializing)]
    match_view: Option<MessageFieldView>,
    #[serde(skip_serializing)]
    parsed_references: Option<Vec<MessageRef>>,
}

impl MessageSearchHit {
    #[cfg(test)]
    pub(crate) fn from_parts(
        message: crate::models::MessageHit,
        match_evidence: Option<MessageMatchEvidence>,
        literal_match: Option<MessageLiteralMatch>,
    ) -> Self {
        let field_view = complete_field_view(&message.content);
        let match_view = match_evidence.as_ref().map(evidence_field_view);
        Self {
            message,
            match_evidence,
            literal_match,
            field_view: Some(field_view),
            match_view,
            parsed_references: None,
        }
    }

    pub fn message(&self) -> &crate::models::MessageHit {
        &self.message
    }

    pub fn match_evidence(&self) -> Option<&MessageMatchEvidence> {
        self.match_evidence.as_ref()
    }

    pub fn literal_match(&self) -> Option<&MessageLiteralMatch> {
        self.literal_match.as_ref()
    }

    pub fn message_ref(&self) -> MessageResultRef<'_> {
        MessageResultRef {
            session_id: &self.message.session_id,
            message_seq: self.message.seq,
        }
    }

    pub const fn field_view(&self) -> &MessageFieldView {
        self.field_view
            .as_ref()
            .expect("message presentation must be applied before returning search results")
    }

    pub fn match_view(&self) -> Option<&MessageFieldView> {
        self.match_view.as_ref()
    }

    pub fn parsed_references(&self) -> Option<&[MessageRef]> {
        self.parsed_references.as_deref()
    }

    pub(crate) fn set_parsed_references(&mut self, references: Vec<MessageRef>) {
        self.parsed_references = Some(references);
    }
}

impl std::ops::Deref for MessageSearchHit {
    type Target = crate::models::MessageHit;

    fn deref(&self) -> &Self::Target {
        &self.message
    }
}

#[derive(Debug, Clone)]
pub struct MessageSearchResponse {
    request: ResolvedMessageSearchRequest,
    query: Option<String>,
    match_target: Option<MessageTarget>,
    match_mode: Option<MessageSearchMode>,
    hits: Vec<MessageSearchHit>,
    context_windows: Vec<Vec<crate::models::MessageHit>>,
    page: PageInfo,
    context: ContextWindow,
    presentation: ResolvedMessagePresentation,
    includes: Vec<MessageSearchInclude>,
    planner: Option<crate::models::SearchExplain>,
    origins: Option<MessageSearchOrigins>,
    included: MessageSearchIncludedData,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageSearchRuntimeDiagnostics {
    package_version: &'static str,
    database_schema_version: i64,
    response_schema_version: u32,
    surface: SearchSurface,
    config_digest: String,
}

impl MessageSearchRuntimeDiagnostics {
    pub(crate) fn new(surface: SearchSurface, config_digest: String) -> Self {
        Self {
            package_version: env!("CARGO_PKG_VERSION"),
            database_schema_version: crate::db::SCHEMA_VERSION,
            response_schema_version: MESSAGE_SEARCH_RESPONSE_SCHEMA_VERSION,
            surface,
            config_digest,
        }
    }

    pub const fn package_version(&self) -> &'static str {
        self.package_version
    }

    pub const fn database_schema_version(&self) -> i64 {
        self.database_schema_version
    }

    pub const fn response_schema_version(&self) -> u32 {
        self.response_schema_version
    }

    pub const fn surface(&self) -> SearchSurface {
        self.surface
    }

    pub fn config_digest(&self) -> &str {
        &self.config_digest
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MessageSearchIncludedData {
    #[serde(skip_serializing_if = "Option::is_none")]
    normalized_session_metadata: Option<BTreeMap<String, SessionMeta>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_provider_metadata: Option<BTreeMap<String, Box<serde_json::value::RawValue>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    runtime_diagnostics: Option<MessageSearchRuntimeDiagnostics>,
}

impl MessageSearchIncludedData {
    pub(crate) fn new(
        normalized_session_metadata: Option<BTreeMap<String, SessionMeta>>,
        raw_provider_metadata: Option<BTreeMap<String, Box<serde_json::value::RawValue>>>,
        runtime_diagnostics: Option<MessageSearchRuntimeDiagnostics>,
    ) -> Self {
        Self {
            normalized_session_metadata,
            raw_provider_metadata,
            runtime_diagnostics,
        }
    }

    fn is_empty(&self) -> bool {
        self.normalized_session_metadata.is_none()
            && self.raw_provider_metadata.is_none()
            && self.runtime_diagnostics.is_none()
    }

    pub fn normalized_session_metadata(&self) -> Option<&BTreeMap<String, SessionMeta>> {
        self.normalized_session_metadata.as_ref()
    }

    pub fn raw_provider_metadata(
        &self,
    ) -> Option<&BTreeMap<String, Box<serde_json::value::RawValue>>> {
        self.raw_provider_metadata.as_ref()
    }

    pub const fn runtime_diagnostics(&self) -> Option<&MessageSearchRuntimeDiagnostics> {
        self.runtime_diagnostics.as_ref()
    }
}

pub(crate) struct MessageSearchResponseParts {
    pub request: ResolvedMessageSearchRequest,
    pub match_details: Option<(MessageTarget, MessageSearchMode)>,
    pub hits: Vec<MessageSearchHit>,
    pub context_windows: Vec<Vec<crate::models::MessageHit>>,
    pub page: PageInfo,
    pub response: MessageResponsePlan,
    pub planner: Option<crate::models::SearchExplain>,
    pub origins: Option<MessageSearchOrigins>,
    pub included: MessageSearchIncludedData,
}

impl MessageSearchResponse {
    pub(crate) fn new(parts: MessageSearchResponseParts) -> Self {
        let MessageSearchResponseParts {
            request,
            match_details,
            hits,
            context_windows,
            page,
            response,
            planner,
            origins,
            included,
        } = parts;
        let (match_target, match_mode) = match match_details {
            Some((target, mode)) => (Some(target), Some(mode)),
            None => (None, None),
        };
        Self {
            request,
            query: None,
            match_target,
            match_mode,
            hits,
            context_windows,
            page,
            context: response.context,
            presentation: response.presentation,
            includes: response.includes,
            planner,
            origins,
            included,
        }
    }

    pub(crate) fn with_query(mut self, query: Option<String>) -> Self {
        self.query = query;
        self
    }

    pub fn query(&self) -> Option<&str> {
        self.query.as_deref()
    }

    pub const fn request(&self) -> &ResolvedMessageSearchRequest {
        &self.request
    }

    pub fn hits(&self) -> &[MessageSearchHit] {
        &self.hits
    }

    /// Finalized semantic result records. `hits()` remains as the Rust compatibility spelling
    /// while adapters migrate; both access the same allocation and ordered identities.
    pub fn results(&self) -> &[MessageSearchHit] {
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

    pub fn includes(&self) -> &[MessageSearchInclude] {
        &self.includes
    }

    pub fn search_explanation(&self) -> Option<&crate::models::SearchExplain> {
        self.planner.as_ref()
    }

    pub fn parameter_origins(&self) -> Option<&MessageSearchOrigins> {
        self.origins.as_ref()
    }

    pub const fn included(&self) -> &MessageSearchIncludedData {
        &self.included
    }

    pub fn has_included_data(&self) -> bool {
        !self.included.is_empty()
    }

    /// Borrow the finalized cross-surface version-1 semantic document without cloning hit text.
    pub const fn document(&self) -> MessageSearchDocument<'_> {
        MessageSearchDocument {
            response: self,
            cancellation: None,
        }
    }

    /// Borrow the canonical document with cooperative cancellation between encoded results.
    ///
    /// Let `H` be returned results and `P` their encoded bytes. Successful serialization remains
    /// `O(H + P)` time with `O(P)` destination memory. Cancellation is observed in `O(1)` at every
    /// result boundary and during an optional ordered-digest pass; one result is the maximum
    /// non-interruptible unit because serializers receive each string value atomically.
    pub const fn document_cancellable<'a>(
        &'a self,
        cancellation: &'a AtomicBool,
    ) -> MessageSearchDocument<'a> {
        MessageSearchDocument {
            response: self,
            cancellation: Some(cancellation),
        }
    }

    /// Borrow one canonical result for incremental encoders such as CLI JSON Lines.
    ///
    /// Serialization is `O(result bytes)` time and retains no second result tree.
    pub fn result_document(&self, index: usize) -> Option<MessageSearchResultDocument<'_>> {
        self.hits.get(index).map(|hit| MessageSearchResultDocument {
            hit,
            target: Some(self.request.target()),
            context: self.context_windows.get(index).map(Vec::as_slice),
            presentation: self.presentation,
        })
    }

    /// Borrow canonical page metadata for incremental encoders.
    pub const fn page_document(&self) -> MessageSearchPageDocument {
        MessageSearchPageDocument(self.page)
    }

    /// Borrow the requested receipt without cloning response data.
    ///
    /// The ordered digest is computed only for a full receipt, matching [`MessageSearchDocument`].
    pub fn receipt_document(&self) -> Option<MessageSearchReceiptDocument<'_>> {
        (self.request.receipt_level != ReceiptLevel::None).then(|| MessageSearchReceiptDocument {
            search_explanation: self.planner.as_ref(),
            parameter_origins: self.origins.as_ref(),
            ordered_digest: (self.request.receipt_level == ReceiptLevel::Full)
                .then(|| self.ordered_digest()),
        })
    }

    /// Digest ordered semantic result identities, excluding presentation and output encoding.
    ///
    /// Let `H` be returned hits and `I` their identity/target bytes. Time is `O(H + I)` and
    /// retained memory is `O(1)` beyond the SHA-256 state and final string.
    pub fn ordered_digest(&self) -> String {
        self.ordered_digest_cancellable(None)
            .expect("uncancelled digest construction cannot fail")
    }

    fn ordered_digest_cancellable(
        &self,
        cancellation: Option<&AtomicBool>,
    ) -> Result<String, &'static str> {
        let mut digest =
            MessageSearchOrderedDigest::new(self.request.target().clone(), self.match_mode);
        for hit in &self.hits {
            ensure_message_search_serialization_active(cancellation)?;
            digest.update(hit);
        }
        ensure_message_search_serialization_active(cancellation)?;
        Ok(digest.finish())
    }
}

/// Incremental owner of the canonical ordered-result digest used by materialized and streamed
/// responses. Presentation, batch size, context, includes, and encoding never enter this digest.
pub(crate) struct MessageSearchOrderedDigest {
    digest: FramedSha256,
    target: MessageTarget,
    match_mode: Option<MessageSearchMode>,
}

impl MessageSearchOrderedDigest {
    pub(crate) fn new(target: MessageTarget, match_mode: Option<MessageSearchMode>) -> Self {
        let digest = FramedSha256::new(b"aise-message-search-ordered-digest-v1");
        Self {
            digest,
            target,
            match_mode,
        }
    }

    /// Add one already-enriched semantic result in `O(identity + target-path bytes)` time and
    /// `O(1)` retained state.
    pub(crate) fn update(&mut self, hit: &MessageSearchHit) {
        self.digest.update_bytes(hit.message.session_id.as_bytes());
        self.digest.update_i64(hit.message.seq);
        self.digest.update_u8(match self.target.field() {
            SearchField::Content => 0,
            SearchField::ToolName => 1,
            SearchField::ToolArgument => 2,
        });
        if let Some(path) = self.target.argument_path() {
            self.digest.update_u8(1);
            self.digest.update_bytes(path.as_str().as_bytes());
        } else {
            self.digest.update_u8(0);
        }
        self.digest.update_u8(match self.match_mode {
            Some(MessageSearchMode::Literal) => 0,
            Some(MessageSearchMode::Regex) => 1,
            Some(MessageSearchMode::Fuzzy) => 2,
            None => 255,
        });
        if let Some(score) = hit.message.fuzzy_score {
            self.digest.update_u8(1);
            self.digest.update_u32(score);
        } else {
            self.digest.update_u8(0);
        }
        if let Some(literal) = hit.literal_match.as_ref() {
            self.digest.update_u8(1);
            self.digest.update_u64(literal.field_start_char as u64);
            self.digest
                .update_u64(literal.field_end_char_exclusive as u64);
        } else {
            self.digest.update_u8(0);
        }
    }

    pub(crate) fn finish(self) -> String {
        self.digest.finish()
    }
}

impl Serialize for MessageSearchResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.document().serialize(serializer)
    }
}

/// Canonical structured response projection. CLI JSON, JSONL framing, MCP structured content, and
/// schema fixtures must derive from this owner rather than rebuilding response facts.
#[derive(Debug, Clone, Copy)]
pub struct MessageSearchDocument<'a> {
    response: &'a MessageSearchResponse,
    cancellation: Option<&'a AtomicBool>,
}

impl Serialize for MessageSearchDocument<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::Error as _;

        ensure_message_search_serialization_active(self.cancellation).map_err(S::Error::custom)?;
        let receipt_level = self.response.request.receipt_level;
        let mut entry_count = 5;
        if receipt_level != ReceiptLevel::None {
            entry_count += 1;
        }
        if !self.response.included.is_empty() {
            entry_count += 1;
        }
        let mut map = serializer.serialize_map(Some(entry_count))?;
        map.serialize_entry(
            "response_schema_version",
            &MESSAGE_SEARCH_RESPONSE_SCHEMA_VERSION,
        )?;
        // Every character offset in this document is in this unit. It used to be repeated at
        // three sites per result, always the same constant, because `CoordinateUnit` has one
        // variant. `REQ006-report-extent-honestly` requires the response to state it; stating it
        // once satisfies that and costs 34 characters instead of 34 times three times the page.
        map.serialize_entry("coordinate_unit", &CoordinateUnit::UnicodeScalar)?;
        map.serialize_entry("effective_request", &self.response.request)?;
        map.serialize_entry(
            "results",
            &SemanticResults {
                hits: &self.response.hits,
                target: Some(self.response.request.target()),
                context_windows: &self.response.context_windows,
                presentation: self.response.presentation,
                cancellation: self.cancellation,
            },
        )?;
        ensure_message_search_serialization_active(self.cancellation).map_err(S::Error::custom)?;
        map.serialize_entry("page", &self.response.page_document())?;
        if !self.response.included.is_empty() {
            ensure_message_search_serialization_active(self.cancellation)
                .map_err(S::Error::custom)?;
            map.serialize_entry("included", &self.response.included)?;
        }
        if receipt_level != ReceiptLevel::None {
            ensure_message_search_serialization_active(self.cancellation)
                .map_err(S::Error::custom)?;
            let ordered_digest = if receipt_level == ReceiptLevel::Full {
                Some(
                    self.response
                        .ordered_digest_cancellable(self.cancellation)
                        .map_err(S::Error::custom)?,
                )
            } else {
                None
            };
            map.serialize_entry(
                "receipt",
                &MessageSearchReceiptDocument {
                    search_explanation: self.response.planner.as_ref(),
                    parameter_origins: self.response.origins.as_ref(),
                    ordered_digest,
                },
            )?;
        }
        ensure_message_search_serialization_active(self.cancellation).map_err(S::Error::custom)?;
        map.end()
    }
}

/// Borrowed canonical result projection for incremental output encoders.
#[derive(Debug, Clone, Copy)]
pub struct MessageSearchResultDocument<'a> {
    hit: &'a MessageSearchHit,
    target: Option<&'a MessageTarget>,
    context: Option<&'a [crate::models::MessageHit]>,
    presentation: ResolvedMessagePresentation,
}

impl<'a> MessageSearchResultDocument<'a> {
    pub(crate) fn from_batch_parts(
        hit: &'a MessageSearchHit,
        request: &'a ResolvedMessageSearchRequest,
        context: Option<&'a [crate::models::MessageHit]>,
    ) -> Self {
        let resolved = request.presentation();
        Self {
            hit,
            target: Some(request.target()),
            context,
            presentation: ResolvedMessagePresentation {
                include_refs: request
                    .include()
                    .contains(&MessageSearchInclude::ParsedReferences),
                message_lines: LineWindow::from_signed(resolved.lines_per_message())
                    .expect("resolved request contains a validated line window"),
                match_evidence_max_chars: NonZeroUsize::new(DEFAULT_MATCH_EVIDENCE_MAX_CHARS)
                    .expect("typed default is positive"),
                detail: None,
                field_view: resolved.field_view(),
                match_view: resolved.match_view(),
            },
        }
    }
}

impl Serialize for MessageSearchResultDocument<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        SemanticResult {
            hit: self.hit,
            target: self.target,
            context: self.context,
            presentation: self.presentation,
        }
        .serialize(serializer)
    }
}

/// Borrowed canonical result-page projection for incremental output encoders.
#[derive(Debug, Clone, Copy)]
pub struct MessageSearchPageDocument(PageInfo);

impl MessageSearchPageDocument {
    pub(crate) const fn from_page(page: PageInfo) -> Self {
        Self(page)
    }
}

impl Serialize for MessageSearchPageDocument {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        SemanticPageFields::from(self.0).serialize(serializer)
    }
}

/// Borrowed canonical optional receipt for incremental output encoders.
#[derive(Debug)]
pub struct MessageSearchReceiptDocument<'a> {
    search_explanation: Option<&'a crate::models::SearchExplain>,
    parameter_origins: Option<&'a MessageSearchOrigins>,
    ordered_digest: Option<String>,
}

impl<'a> MessageSearchReceiptDocument<'a> {
    pub(crate) fn from_parts(
        receipt_level: ReceiptLevel,
        search_explanation: Option<&'a crate::models::SearchExplain>,
        parameter_origins: Option<&'a MessageSearchOrigins>,
        ordered_digest: Option<&'a str>,
    ) -> Option<Self> {
        (receipt_level != ReceiptLevel::None).then_some(Self {
            search_explanation,
            parameter_origins,
            ordered_digest: (receipt_level == ReceiptLevel::Full)
                .then_some(ordered_digest)
                .flatten()
                .map(str::to_owned),
        })
    }
}

impl Serialize for MessageSearchReceiptDocument<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        SemanticReceipt {
            search_explanation: self.search_explanation,
            parameter_origins: self.parameter_origins,
            ordered_digest: self.ordered_digest.clone(),
        }
        .serialize(serializer)
    }
}

struct SemanticResults<'a> {
    hits: &'a [MessageSearchHit],
    target: Option<&'a MessageTarget>,
    context_windows: &'a [Vec<crate::models::MessageHit>],
    presentation: ResolvedMessagePresentation,
    cancellation: Option<&'a AtomicBool>,
}

impl Serialize for SemanticResults<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::Error as _;

        let mut sequence = serializer.serialize_seq(Some(self.hits.len()))?;
        for (index, hit) in self.hits.iter().enumerate() {
            ensure_message_search_serialization_active(self.cancellation)
                .map_err(S::Error::custom)?;
            sequence.serialize_element(&SemanticResult {
                hit,
                target: self.target,
                context: self.context_windows.get(index).map(Vec::as_slice),
                presentation: self.presentation,
            })?;
        }
        sequence.end()
    }
}

fn ensure_message_search_serialization_active(
    cancellation: Option<&AtomicBool>,
) -> Result<(), &'static str> {
    if cancellation.is_some_and(|flag| flag.load(AtomicOrdering::Acquire)) {
        Err("message-search response serialization was cancelled")
    } else {
        Ok(())
    }
}

#[derive(Serialize)]
struct SemanticMessageRef<'a> {
    session_id: &'a str,
    message_seq: i64,
}

#[derive(Serialize)]
struct SemanticMessageMetadata {
    provider: Provider,
    role: Role,
    kind: MessageKind,
}

#[derive(Serialize)]
struct SemanticLiteralOccurrence<'a> {
    text: &'a str,
    field_start_char: usize,
    field_end_char_exclusive: usize,
}

#[derive(Serialize)]
struct SemanticMatch<'a> {
    field: SearchField,
    #[serde(skip_serializing_if = "Option::is_none")]
    argument_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fuzzy_score: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    literal_occurrence: Option<SemanticLiteralOccurrence<'a>>,
}

#[derive(Serialize)]
struct SemanticPresentation<'a> {
    field_view: SemanticFieldView<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    match_view: Option<SemanticFieldView<'a>>,
}

#[derive(Serialize)]
struct SemanticFieldView<'a> {
    text: &'a str,
    field_start_char: usize,
    field_end_char_exclusive: usize,
    #[serde(skip_serializing_if = "SemanticMarkers::is_empty")]
    markers: SemanticMarkers<'a>,
    #[serde(flatten)]
    extent: FieldViewExtent,
}

impl<'a> From<&'a MessageFieldView> for SemanticFieldView<'a> {
    fn from(view: &'a MessageFieldView) -> Self {
        Self {
            text: &view.text,
            field_start_char: view.field_start_char,
            field_end_char_exclusive: view.field_end_char_exclusive,
            markers: SemanticMarkers(&view.markers),
            extent: view.extent,
        }
    }
}

struct SemanticMarkers<'a>(&'a [ViewCharRange]);

impl SemanticMarkers<'_> {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Serialize for SemanticMarkers<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for marker in self.0 {
            sequence.serialize_element(&SemanticMarker {
                view_start_char: marker.view_start_char,
                view_end_char_exclusive: marker.view_end_char_exclusive,
            })?;
        }
        sequence.end()
    }
}

#[derive(Serialize)]
struct SemanticMarker {
    view_start_char: usize,
    view_end_char_exclusive: usize,
}

struct SemanticResult<'a> {
    hit: &'a MessageSearchHit,
    target: Option<&'a MessageTarget>,
    context: Option<&'a [crate::models::MessageHit]>,
    presentation: ResolvedMessagePresentation,
}

impl Serialize for SemanticResult<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let hit = self.hit;
        let message_ref = SemanticMessageRef {
            session_id: &hit.message.session_id,
            message_seq: hit.message.seq,
        };
        let literal_occurrence =
            hit.literal_match
                .as_ref()
                .map(|literal| SemanticLiteralOccurrence {
                    text: &literal.text,
                    field_start_char: literal.field_start_char,
                    field_end_char_exclusive: literal.field_end_char_exclusive,
                });
        let field = self
            .target
            .map_or(SearchField::Content, MessageTarget::field);
        let has_match = hit.match_evidence.is_some() || hit.literal_match.is_some();
        let mut entry_count = if has_match { 4 } else { 3 };
        if hit.parsed_references.is_some() {
            entry_count += 1;
        }
        if self.context.is_some() {
            entry_count += 1;
        }
        let mut map = serializer.serialize_map(Some(entry_count))?;
        map.serialize_entry("message_ref", &message_ref)?;
        map.serialize_entry(
            "message_metadata",
            &SemanticMessageMetadata {
                provider: hit.message.provider,
                role: hit.message.role,
                kind: hit.message.kind,
            },
        )?;
        if has_match {
            map.serialize_entry(
                "match",
                &SemanticMatch {
                    field,
                    argument_path: self
                        .target
                        .and_then(MessageTarget::argument_path)
                        .map(JsonPointer::as_str),
                    fuzzy_score: hit.message.fuzzy_score,
                    literal_occurrence,
                },
            )?;
        }
        map.serialize_entry(
            "presentation",
            &SemanticPresentation {
                field_view: SemanticFieldView::from(hit.field_view()),
                match_view: hit.match_view.as_ref().map(SemanticFieldView::from),
            },
        )?;
        if let Some(parsed_references) = hit.parsed_references.as_ref() {
            map.serialize_entry("included", &SemanticResultIncluded { parsed_references })?;
        }
        if let Some(context) = self.context {
            map.serialize_entry(
                "context",
                &SemanticContext {
                    anchor_seq: hit.message.seq,
                    messages: context,
                    presentation: self.presentation,
                },
            )?;
        }
        map.end()
    }
}

#[derive(Serialize)]
struct SemanticResultIncluded<'a> {
    parsed_references: &'a [MessageRef],
}

struct SemanticContext<'a> {
    anchor_seq: i64,
    messages: &'a [crate::models::MessageHit],
    presentation: ResolvedMessagePresentation,
}

impl Serialize for SemanticContext<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let messages_before = self
            .messages
            .iter()
            .filter(|message| message.seq < self.anchor_seq)
            .map(|message| SemanticContextMessage {
                message,
                presentation: self.presentation,
            })
            .collect::<Vec<_>>();
        let messages_after = self
            .messages
            .iter()
            .filter(|message| message.seq > self.anchor_seq)
            .map(|message| SemanticContextMessage {
                message,
                presentation: self.presentation,
            })
            .collect::<Vec<_>>();
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("messages_before", &messages_before)?;
        map.serialize_entry("messages_after", &messages_after)?;
        map.end()
    }
}

struct SemanticContextMessage<'a> {
    message: &'a crate::models::MessageHit,
    presentation: ResolvedMessagePresentation,
}

impl Serialize for SemanticContextMessage<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::Error as _;

        let message = self.message;
        let field_view = selected_field_view(
            &message.content,
            self.presentation.message_lines,
            self.presentation.field_view,
            None,
        )
        .map_err(S::Error::custom)?;
        let mut map = serializer.serialize_map(Some(6))?;
        map.serialize_entry(
            "message_ref",
            &SemanticMessageRef {
                session_id: &message.session_id,
                message_seq: message.seq,
            },
        )?;
        map.serialize_entry(
            "message_metadata",
            &SemanticMessageMetadata {
                provider: message.provider,
                role: message.role,
                kind: message.kind,
            },
        )?;
        map.serialize_entry("timestamp", &message.ts)?;
        map.serialize_entry("tool_name", &message.tool_name)?;
        map.serialize_entry("tool_call_id", &message.tool_call_id)?;
        map.serialize_entry(
            "presentation",
            &SemanticPresentation {
                field_view: SemanticFieldView::from(&field_view),
                match_view: None,
            },
        )?;
        map.end()
    }
}

#[derive(Serialize)]
struct SemanticPageFields {
    returned: usize,
    limit: Option<usize>,
    offset: usize,
    has_more: bool,
    next_offset: Option<usize>,
    earlier_results: PageSide,
    result_set_extent: ResultSetExtent,
    ordering: ExecutionOrder,
    consistency: PageConsistency,
}

#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
enum PageConsistency {
    PerCall,
}

impl From<PageInfo> for SemanticPageFields {
    fn from(page: PageInfo) -> Self {
        let (limit, offset) = match page.extent {
            ResolvedExtent::Page { limit, offset } => (Some(limit.get()), offset),
            ResolvedExtent::AllResults { offset } => (None, offset),
        };
        Self {
            returned: page.returned,
            limit,
            offset,
            has_more: page.next_offset.is_some(),
            next_offset: page.next_offset,
            earlier_results: page.earlier_results,
            result_set_extent: page.result_set_extent,
            ordering: page.ordering,
            consistency: PageConsistency::PerCall,
        }
    }
}

#[derive(Serialize)]
struct SemanticReceipt<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    search_explanation: Option<&'a crate::models::SearchExplain>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameter_origins: Option<&'a MessageSearchOrigins>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ordered_digest: Option<String>,
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
    includes: Option<Vec<MessageSearchInclude>>,
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
                includes: None,
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

    pub fn includes(&self) -> Option<&[MessageSearchInclude]> {
        self.includes.as_deref()
    }

    fn validate(&self) -> Result<(), MessageSearchError> {
        if self.presentation.detail.is_some()
            && (self.presentation.message_lines.is_some()
                || self.presentation.field_view.is_some()
                || self.presentation.match_view.is_some())
        {
            return Err(MessageSearchRule::DetailOwnsPresentationBudgets.error());
        }
        if self.predicates.sequence.is_some() && self.predicates.session.is_none() {
            return Err(MessageSearchRule::SequenceRequiresSession.error());
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
            return Err(MessageSearchRule::KindsMustRemainSatisfiable.error());
        }
        if self.predicates.role == Some(Role::Compaction)
            && !effective.contains(&MessageKind::Compaction)
        {
            return Err(MessageSearchRule::CompactionRoleRequiresCompactionKind.error());
        }
        if self.target.field == SearchField::ToolArgument
            && self
                .predicates
                .kinds
                .as_ref()
                .is_some_and(|kinds| !kinds.contains(&MessageKind::ToolCall))
        {
            return Err(MessageSearchRule::ToolArgumentRequiresToolCallKind.error());
        }
        if matches!(self.query, MessageQuery::All) && self.presentation.match_view.is_some() {
            return Err(MessageSearchRule::MatchViewRequiresQuery.error());
        }
        if self.match_window.is_some() && matches!(self.query, MessageQuery::Fuzzy(_)) {
            return Err(MessageSearchRule::FuzzyRejectsMatchWindow.error());
        }
        if self.match_window == Some(MatchWindow::Latest) && self.predicates.session.is_none() {
            return Err(MessageSearchRule::LatestWindowRequiresSession.error());
        }
        if let MessageQuery::Fuzzy(_) = self.query {
            match self.extent {
                RequestedExtent::Page { .. } => {}
                RequestedExtent::AllResults { .. } => {
                    return Err(MessageSearchRule::FuzzyRejectsAllResults.error())
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
        self.request.predicates.providers = Some(vec![provider]);
        self
    }

    /// Select one or more session sources.
    ///
    /// This is set semantics: duplicates are removed and the stored order is canonical, so
    /// equivalent requests produce byte-identical effective-request metadata. An empty set is an
    /// invalid scope rather than a successful search that misleadingly reports no matches.
    pub fn providers(mut self, mut providers: Vec<Provider>) -> Result<Self, MessageSearchError> {
        if providers.is_empty() {
            return Err(MessageSearchError::InvalidParameter {
                parameter: "providers",
                reason: "must contain at least one provider".into(),
            });
        }
        providers.sort_unstable_by_key(|provider| provider.as_str());
        providers.dedup();
        self.request.predicates.providers = Some(providers);
        Ok(self)
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

    pub fn message_lines(mut self, window: LineWindow) -> Self {
        self.request.presentation.message_lines = Some(window);
        self
    }

    pub fn detail(mut self, detail: DetailLevel) -> Self {
        self.request.presentation.detail = Some(detail);
        self
    }

    pub fn field_view(mut self, budget: FieldViewBudget) -> Self {
        self.request.presentation.field_view = Some(budget);
        self
    }

    pub fn match_view(mut self, budget: MatchViewBudget) -> Self {
        self.request.presentation.match_view = Some(budget);
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

    /// Replace optional payload groups. An empty collection explicitly requests the semantic core.
    pub fn includes(mut self, includes: impl IntoIterator<Item = MessageSearchInclude>) -> Self {
        let mut includes = includes.into_iter().collect::<Vec<_>>();
        includes.sort_unstable();
        includes.dedup();
        self.request.includes = Some(includes);
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

#[cfg(test)]
pub(crate) fn attach_match_evidence(
    query: &MessageQuery,
    target: &MessageTarget,
    match_view: MatchViewBudget,
    hits: Vec<crate::models::MessageHit>,
) -> anyhow::Result<Vec<MessageSearchHit>> {
    attach_match_evidence_cancellable(query, target, match_view, hits, || Ok(()))
}

pub(crate) fn attach_match_evidence_cancellable(
    query: &MessageQuery,
    target: &MessageTarget,
    match_view: MatchViewBudget,
    hits: Vec<crate::models::MessageHit>,
    mut check_active: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<Vec<MessageSearchHit>> {
    let mut prepared = PreparedMatchEvidence::new(query)?;
    let mut enriched = Vec::with_capacity(hits.len());
    for message in hits {
        check_active()?;
        let (match_evidence, literal_match) = match query {
            MessageQuery::All => {
                // Queryless search selects rows by field presence and deliberately does not
                // parse or reconstruct a selected value merely to prove match evidence.
                (None, None)
            }
            _ => {
                let selected = selected_message_field(&message, target).ok_or_else(|| {
                    anyhow::anyhow!(
                        "message-search match evidence cannot project {:?} for {} sequence {}",
                        target.field(),
                        message.session_id,
                        message.seq
                    )
                })?;
                let evidence = prepared.build(&selected, match_view).ok_or_else(|| {
                    anyhow::anyhow!(
                        "message-search match evidence disagrees with {:?} membership for {} sequence {}",
                        target.field(),
                        message.session_id,
                        message.seq
                    )
                })?;
                check_active()?;
                let literal_match = match query {
                    MessageQuery::Literal(_) => {
                        let range = match &evidence.markers {
                            MessageMatchViewMarkers::Characters {
                                ranges,
                                matched_chars_total,
                                ..
                            } => ranges.first().map(|range| {
                                let field_start_char =
                                    evidence.field_start_char + range.view_start_char;
                                FieldCharRange {
                                    field_start_char,
                                    field_end_char_exclusive: field_start_char
                                        + matched_chars_total,
                                }
                            }),
                            MessageMatchViewMarkers::Boundary { .. } => None,
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
                                .skip(range.field_start_char)
                                .take(range.field_end_char_exclusive - range.field_start_char)
                                .collect(),
                            field_start_char: range.field_start_char,
                            field_end_char_exclusive: range.field_end_char_exclusive,
                        })
                    }
                    _ => None,
                };
                (Some(evidence), literal_match)
            }
        };
        enriched.push(MessageSearchHit {
            message,
            match_evidence,
            literal_match,
            field_view: None,
            match_view: None,
            parsed_references: None,
        });
    }
    Ok(enriched)
}

#[cfg(test)]
fn complete_field_view(field_text: &str) -> MessageFieldView {
    let returned_chars = field_text.chars().count();
    MessageFieldView {
        text: field_text.to_owned(),
        field_start_char: 0,
        field_end_char_exclusive: returned_chars,
        markers: Vec::new(),
        extent: FieldViewExtent {
            additional_field_text: AdditionalFieldText::None,
            field_total_chars: Some(returned_chars),
            coordinate_unit: CoordinateUnit::UnicodeScalar,
        },
    }
}

fn evidence_field_view(evidence: &MessageMatchEvidence) -> MessageFieldView {
    let markers = match &evidence.markers {
        MessageMatchViewMarkers::Characters { ranges, .. } => ranges.clone(),
        MessageMatchViewMarkers::Boundary { view_at_char } => vec![ViewCharRange {
            view_start_char: *view_at_char,
            view_end_char_exclusive: *view_at_char,
        }],
    };
    let returned_chars = evidence.view_text.chars().count();
    let has_text_before = evidence.field_start_char > 0;
    let has_text_after =
        evidence.field_start_char.saturating_add(returned_chars) < evidence.field_total_chars;
    MessageFieldView {
        text: evidence.view_text.clone(),
        field_start_char: evidence.field_start_char,
        field_end_char_exclusive: evidence.field_start_char.saturating_add(returned_chars),
        markers,
        extent: FieldViewExtent {
            additional_field_text: additional_field_text(has_text_before, has_text_after),
            field_total_chars: Some(evidence.field_total_chars),
            coordinate_unit: CoordinateUnit::UnicodeScalar,
        },
    }
}

/// Project a fully classified message into bounded field and match views without changing the
/// text used for classification.
///
/// Time is `O(D + V)`, where `D` is authoritative message characters and `V` is returned view
/// characters. Retained memory is `O(V)`. Exact match coordinates come from the classifier, so a
/// repeated substring cannot move the presentation marker to a different occurrence.
pub(crate) fn classification_presentation(
    content: &str,
    match_start_char: usize,
    match_end_char_exclusive: usize,
    field_budget: FieldViewBudget,
    match_budget: MatchViewBudget,
) -> anyhow::Result<(MessageFieldView, MessageFieldView)> {
    let field_total_chars = content.chars().count();
    if match_start_char >= match_end_char_exclusive || match_end_char_exclusive > field_total_chars
    {
        anyhow::bail!(
            "classification match range {match_start_char}..{match_end_char_exclusive} is outside \
             a message containing {field_total_chars} Unicode scalar characters"
        );
    }
    let range = FieldCharRange {
        field_start_char: match_start_char,
        field_end_char_exclusive: match_end_char_exclusive,
    };
    let maximum = match_view_max_chars(
        match_budget,
        field_total_chars,
        std::slice::from_ref(&range),
    );
    let evidence = character_evidence(content, field_total_chars, maximum, vec![range], false);
    Ok((
        selected_field_view(
            content,
            LineWindow::Full,
            field_budget,
            Some(field_total_chars),
        )?,
        evidence_field_view(&evidence),
    ))
}

/// Serialize one bounded classification result using the same recovery-oriented shape for CLI
/// JSON and MCP. The full classified content remains in the typed core report; bounded adapters
/// return its stable message reference and extent instead of duplicating large text.
pub(crate) fn classification_presentation_document(
    matched: &crate::models::MessageClassificationMatch,
    field_budget: FieldViewBudget,
    match_budget: MatchViewBudget,
) -> anyhow::Result<serde_json::Value> {
    let (field_view, match_view) = classification_presentation(
        &matched.content,
        matched.match_start_char,
        matched.match_end_char_exclusive,
        field_budget,
        match_budget,
    )?;
    Ok(serde_json::json!({
        "message_ref": {
            "session_id": matched.session_id,
            "message_seq": matched.message_seq
        },
        "message_metadata": {
            "provider": matched.provider,
            "timestamp": matched.ts
        },
        "classification": {
            "policy_name": matched.policy_name,
            "category": matched.category,
            "matched_text": matched.matched_text,
            "field_start_char": matched.match_start_char,
            "field_end_char_exclusive": matched.match_end_char_exclusive,
            "coordinate_unit": "unicode_scalar"
        },
        "presentation": {
            "field_view": field_view,
            "match_view": match_view
        }
    }))
}

/// Apply presentation after retrieval and match proof have been finalized.
///
/// Let `V` be returned view characters and `D` the selected-field characters inspected to locate
/// a tail boundary. Head/full presentation is `O(V)` when the original size is not needed; tail
/// selection is `O(D)`. This function never changes hit membership, order, score, or page identity.
pub(crate) fn apply_message_presentation_cancellable(
    target: &MessageTarget,
    presentation: ResolvedMessagePresentation,
    hits: &mut [MessageSearchHit],
    mut check_active: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    for hit in hits {
        check_active()?;
        let selected = match selected_message_field(&hit.message, target) {
            Some(selected) => selected,
            None if hit.match_evidence.is_none() => Cow::Borrowed(hit.message.content.as_str()),
            None => {
                return Err(anyhow::anyhow!(
                    "message-search presentation cannot project {:?} for {} sequence {}",
                    target.field(),
                    hit.message.session_id,
                    hit.message.seq
                ))
            }
        };
        hit.field_view = Some(selected_field_view(
            &selected,
            presentation.message_lines,
            presentation.field_view,
            hit.match_evidence
                .as_ref()
                .map(|evidence| evidence.field_total_chars),
        )?);
        hit.match_view = hit.match_evidence.as_ref().map(evidence_field_view);
        check_active()?;
    }
    Ok(())
}

pub(crate) fn selected_field_view(
    original: &str,
    lines: LineWindow,
    budget: FieldViewBudget,
    known_original_chars: Option<usize>,
) -> Result<MessageFieldView, MessageSearchError> {
    let signed_lines = lines.to_signed()?;
    let line_selected = if signed_lines == 0 {
        Cow::Borrowed(original)
    } else {
        Cow::Owned(crate::util::select_message_lines(original, signed_lines))
    };
    let line_start = if matches!(lines, LineWindow::Tail(_)) {
        let selected_chars = line_selected.chars().count();
        known_original_chars
            .map(|total| total.saturating_sub(selected_chars))
            .or_else(|| {
                original
                    .rfind(line_selected.as_ref())
                    .map(|byte| original[..byte].chars().count())
            })
            .unwrap_or_else(|| original.chars().count().saturating_sub(selected_chars))
    } else {
        0
    };
    let line_has_text_before = line_start > 0;
    let line_end = line_start.saturating_add(line_selected.chars().count());
    let tail_has_text_after =
        matches!(lines, LineWindow::Tail(_)) && !original.ends_with(line_selected.as_ref());
    let original_chars_if_counted = match (known_original_chars, lines, budget) {
        (Some(total), _, _) => Some(total),
        (None, LineWindow::Full, FieldViewBudget::NoCharLimit) => Some(original.chars().count()),
        (None, LineWindow::Tail(_), _) => None,
        (None, LineWindow::Head(_), _) if line_selected == original => Some(line_end),
        _ => None,
    };
    let line_has_text_after = original_chars_if_counted.is_some_and(|total| line_end < total)
        || (matches!(lines, LineWindow::Head(_)) && line_selected != original)
        || tail_has_text_after;

    let (text, field_start_char, budget_has_text_before, budget_has_text_after) = match budget {
        FieldViewBudget::NoCharLimit => (line_selected.into_owned(), line_start, false, false),
        FieldViewBudget::MaxChars { max_chars } => {
            let maximum = max_chars.get();
            match lines {
                LineWindow::Tail(_) => {
                    let selected_chars = line_selected.chars().count();
                    if selected_chars <= maximum {
                        (line_selected.into_owned(), line_start, false, false)
                    } else {
                        let skipped = selected_chars - maximum;
                        let start_byte = line_selected
                            .char_indices()
                            .nth(skipped)
                            .map_or(line_selected.len(), |(byte, _)| byte);
                        (
                            line_selected[start_byte..].to_owned(),
                            line_start + skipped,
                            true,
                            false,
                        )
                    }
                }
                LineWindow::Full | LineWindow::Head(_) => {
                    let mut characters = line_selected.chars();
                    let text = characters.by_ref().take(maximum).collect::<String>();
                    let truncated = characters.next().is_some();
                    (text, line_start, false, truncated)
                }
            }
        }
    };
    let returned_chars = text.chars().count();
    let has_text_before = line_has_text_before || budget_has_text_before;
    let has_text_after = line_has_text_after || budget_has_text_after;
    let field_total_chars = original_chars_if_counted;
    Ok(MessageFieldView {
        text,
        field_start_char,
        field_end_char_exclusive: field_start_char.saturating_add(returned_chars),
        markers: Vec::new(),
        extent: FieldViewExtent {
            additional_field_text: additional_field_text(has_text_before, has_text_after),
            field_total_chars,
            coordinate_unit: CoordinateUnit::UnicodeScalar,
        },
    })
}

const fn additional_field_text(has_text_before: bool, has_text_after: bool) -> AdditionalFieldText {
    match (has_text_before, has_text_after) {
        (false, false) => AdditionalFieldText::None,
        (false, true) => AdditionalFieldText::After,
        (true, false) => AdditionalFieldText::Before,
        (true, true) => AdditionalFieldText::BeforeAndAfter,
    }
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
        match_view: MatchViewBudget,
    ) -> Option<MessageMatchEvidence> {
        let field_total_chars = selected.chars().count();
        match self {
            Self::All => None,
            Self::Literal { lowered_query } => {
                let range = literal_char_range(selected, lowered_query)?;
                let maximum = match_view_max_chars(
                    match_view,
                    field_total_chars,
                    std::slice::from_ref(&range),
                );
                Some(character_evidence(
                    selected,
                    field_total_chars,
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
                    let maximum = match match_view {
                        MatchViewBudget::MinimalSpan => 0,
                        MatchViewBudget::MaxChars { max_chars } => {
                            max_chars.get().min(field_total_chars.max(1))
                        }
                    };
                    Some(boundary_evidence(
                        selected,
                        field_total_chars,
                        maximum,
                        start,
                    ))
                } else {
                    let range = FieldCharRange {
                        field_start_char: start,
                        field_end_char_exclusive: end,
                    };
                    let maximum = match_view_max_chars(
                        match_view,
                        field_total_chars,
                        std::slice::from_ref(&range),
                    );
                    Some(character_evidence(
                        selected,
                        field_total_chars,
                        maximum,
                        vec![range],
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
                let maximum = match_view_max_chars(match_view, field_total_chars, &ranges);
                Some(character_evidence(
                    selected,
                    field_total_chars,
                    maximum,
                    ranges,
                    true,
                ))
            }
        }
    }
}

fn match_view_max_chars(
    budget: MatchViewBudget,
    field_total_chars: usize,
    ranges: &[FieldCharRange],
) -> usize {
    match budget {
        MatchViewBudget::MaxChars { max_chars } => max_chars.get().min(field_total_chars.max(1)),
        MatchViewBudget::MinimalSpan => ranges
            .first()
            .zip(ranges.last())
            .map(|(first, last)| {
                last.field_end_char_exclusive
                    .saturating_sub(first.field_start_char)
            })
            .unwrap_or(0),
    }
}

fn literal_char_range(selected: &str, lowered_query: &str) -> Option<FieldCharRange> {
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
    Some(FieldCharRange {
        field_start_char: original_char_for_lowered[start_lowered],
        field_end_char_exclusive: original_char_for_lowered[end_lowered - 1] + 1,
    })
}

fn scalar_ranges_by_grapheme(selected: &str) -> Vec<FieldCharRange> {
    let mut scalar_start = 0;
    selected
        .graphemes(true)
        .map(|grapheme| {
            let scalar_end = scalar_start + grapheme.chars().count();
            let range = FieldCharRange {
                field_start_char: scalar_start,
                field_end_char_exclusive: scalar_end,
            };
            scalar_start = scalar_end;
            range
        })
        .collect()
}

fn scalar_ranges_by_matcher_unit(
    selected: &str,
    matcher_input: Utf32Str<'_>,
) -> Vec<FieldCharRange> {
    match matcher_input {
        Utf32Str::Unicode(_) => scalar_ranges_by_grapheme(selected),
        Utf32Str::Ascii(_) => {
            let mut scalar_start = 0;
            let mut ranges = Vec::with_capacity(selected.len());
            for grapheme in selected.graphemes(true) {
                let scalar_end = scalar_start + grapheme.chars().count();
                let range = FieldCharRange {
                    field_start_char: scalar_start,
                    field_end_char_exclusive: scalar_end,
                };
                ranges.extend(std::iter::repeat_n(range, grapheme.len()));
                scalar_start = scalar_end;
            }
            ranges
        }
    }
}

fn coalesce_character_ranges(mut ranges: Vec<FieldCharRange>) -> Vec<FieldCharRange> {
    ranges.sort_by_key(|range| (range.field_start_char, range.field_end_char_exclusive));
    let mut coalesced = Vec::new();
    for range in ranges {
        match coalesced.last_mut() {
            Some(FieldCharRange {
                field_end_char_exclusive,
                ..
            }) if range.field_start_char <= *field_end_char_exclusive => {
                *field_end_char_exclusive =
                    (*field_end_char_exclusive).max(range.field_end_char_exclusive);
            }
            _ => coalesced.push(range),
        }
    }
    coalesced
}

fn character_evidence(
    selected: &str,
    field_total_chars: usize,
    maximum: usize,
    ranges: Vec<FieldCharRange>,
    densest_window: bool,
) -> MessageMatchEvidence {
    let matched_chars_total = ranges
        .iter()
        .map(|range| range.field_end_char_exclusive - range.field_start_char)
        .sum();
    let field_start_char = if field_total_chars <= maximum {
        0
    } else if densest_window {
        densest_excerpt_start(&ranges, maximum, field_total_chars)
    } else {
        let first = ranges[0];
        let width = first.field_end_char_exclusive - first.field_start_char;
        first
            .field_start_char
            .saturating_sub(maximum.saturating_sub(width) / 2)
            .min(field_total_chars - maximum)
    };
    let field_end_char_exclusive = (field_start_char + maximum).min(field_total_chars);
    let shown = ranges
        .iter()
        .filter_map(|range| {
            let start = range.field_start_char.max(field_start_char);
            let end = range.field_end_char_exclusive.min(field_end_char_exclusive);
            (start < end).then_some(ViewCharRange {
                view_start_char: start - field_start_char,
                view_end_char_exclusive: end - field_start_char,
            })
        })
        .collect::<Vec<_>>();
    let matched_chars_shown = shown
        .iter()
        .map(|range| range.view_end_char_exclusive - range.view_start_char)
        .sum();
    MessageMatchEvidence {
        view_text: selected
            .chars()
            .skip(field_start_char)
            .take(field_end_char_exclusive - field_start_char)
            .collect(),
        field_start_char,
        field_total_chars,
        markers: MessageMatchViewMarkers::Characters {
            ranges: shown,
            matched_chars_total,
            matched_chars_shown,
        },
    }
}

fn densest_excerpt_start(
    ranges: &[FieldCharRange],
    maximum: usize,
    field_total_chars: usize,
) -> usize {
    let indices = ranges
        .iter()
        .flat_map(|range| range.field_start_char..range.field_end_char_exclusive)
        .collect::<Vec<_>>();
    let mut best = (0, 0);
    let mut right = 0;
    for left in 0..indices.len() {
        while right < indices.len() && indices[right] < indices[left] + maximum {
            right += 1;
        }
        let candidate = (right - left, indices[left].min(field_total_chars - maximum));
        if candidate.0 > best.0 || (candidate.0 == best.0 && candidate.1 < best.1) {
            best = candidate;
        }
    }
    best.1
}

fn boundary_evidence(
    selected: &str,
    field_total_chars: usize,
    maximum: usize,
    boundary: usize,
) -> MessageMatchEvidence {
    let field_start_char = boundary
        .saturating_sub(maximum / 2)
        .min(field_total_chars.saturating_sub(maximum));
    let field_end_char_exclusive = (field_start_char + maximum).min(field_total_chars);
    MessageMatchEvidence {
        view_text: selected
            .chars()
            .skip(field_start_char)
            .take(field_end_char_exclusive - field_start_char)
            .collect(),
        field_start_char,
        field_total_chars,
        markers: MessageMatchViewMarkers::Boundary {
            view_at_char: boundary - field_start_char,
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
            .match_view(MatchViewBudget::max_chars(20).unwrap())
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
            MatchViewBudget::MaxChars {
                max_chars: NonZeroUsize::new(40).unwrap(),
            },
            vec![hit(content)],
        )
        .unwrap();

        let evidence = evidence[0].match_evidence().unwrap();
        assert!(evidence.view_text.to_lowercase().contains("trash"));
        assert!(evidence.field_start_char > 220);
        assert_eq!(evidence.view_text.chars().count(), 40);
        assert_eq!(evidence.field_total_chars, 1_260);
    }

    #[test]
    fn batch_enrichment_checks_cancellation_between_hits_and_phases() {
        let query = MessageQuery::literal("needle").unwrap();
        let target = MessageTarget::content();
        let source_hits = vec![hit("needle zero"), hit("needle one"), hit("needle two")];
        let mut evidence_checks = 0;
        let evidence_error = attach_match_evidence_cancellable(
            &query,
            &target,
            MatchViewBudget::MinimalSpan,
            source_hits.clone(),
            || {
                evidence_checks += 1;
                anyhow::ensure!(evidence_checks < 3, "injected evidence cancellation");
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(evidence_checks, 3);
        assert!(evidence_error
            .to_string()
            .contains("injected evidence cancellation"));

        let mut hits =
            attach_match_evidence(&query, &target, MatchViewBudget::MinimalSpan, source_hits)
                .unwrap();
        let presentation = ResolvedMessagePresentation {
            include_refs: false,
            message_lines: LineWindow::Full,
            match_evidence_max_chars: NonZeroUsize::new(32).unwrap(),
            detail: None,
            field_view: FieldViewBudget::NoCharLimit,
            match_view: MatchViewBudget::MinimalSpan,
        };
        let mut presentation_checks = 0;
        let presentation_error =
            apply_message_presentation_cancellable(&target, presentation, &mut hits, || {
                presentation_checks += 1;
                anyhow::ensure!(
                    presentation_checks < 3,
                    "injected presentation cancellation"
                );
                Ok(())
            })
            .unwrap_err();
        assert_eq!(presentation_checks, 3);
        assert!(presentation_error
            .to_string()
            .contains("injected presentation cancellation"));
    }

    #[test]
    fn long_literal_keeps_complete_field_occurrence_beside_bounded_evidence() {
        let literal = "Needle".repeat(80);
        let content = format!("prefix {literal} suffix");
        let hits = attach_match_evidence(
            &MessageQuery::literal(literal.to_lowercase()).unwrap(),
            &MessageTarget::content(),
            MatchViewBudget::MaxChars {
                max_chars: NonZeroUsize::new(40).unwrap(),
            },
            vec![hit(content)],
        )
        .unwrap();

        let hit = &hits[0];
        assert_eq!(hit.match_evidence().unwrap().view_text.chars().count(), 40);
        let occurrence = hit.literal_match().expect("literal field occurrence");
        assert_eq!(occurrence.text, literal);
        assert_eq!(occurrence.field_start_char, 7);
        assert_eq!(
            occurrence.field_end_char_exclusive,
            7 + literal.chars().count()
        );
    }

    #[test]
    fn minimal_match_view_contains_the_entire_literal_regex_or_fuzzy_span() {
        let literal = "Needle".repeat(80);
        let content = format!("prefix {literal} suffix");
        for query in [
            MessageQuery::literal(literal.to_lowercase()).unwrap(),
            MessageQuery::regex(r"(?:Needle){80}").unwrap(),
        ] {
            let hits = attach_match_evidence(
                &query,
                &MessageTarget::content(),
                MatchViewBudget::MinimalSpan,
                vec![hit(content.clone())],
            )
            .unwrap();
            assert_eq!(hits[0].match_evidence().unwrap().view_text, literal);
        }

        let fuzzy = attach_match_evidence(
            &MessageQuery::fuzzy("tst").unwrap(),
            &MessageTarget::content(),
            MatchViewBudget::MinimalSpan,
            vec![hit("prefix test suffix")],
        )
        .unwrap();
        assert_eq!(fuzzy[0].match_evidence().unwrap().view_text, "test");

        let boundary = attach_match_evidence(
            &MessageQuery::regex(r"(?m)^").unwrap(),
            &MessageTarget::content(),
            MatchViewBudget::MinimalSpan,
            vec![hit("line")],
        )
        .unwrap();
        assert_eq!(boundary[0].match_evidence().unwrap().view_text, "");
    }

    #[test]
    fn field_view_range_extent_and_count_cover_full_start_end_middle_and_unicode() {
        let full =
            selected_field_view("aé🙂", LineWindow::Full, FieldViewBudget::NoCharLimit, None)
                .unwrap();
        assert_eq!(full.text(), "aé🙂");
        assert_eq!(
            (full.field_start_char(), full.field_end_char_exclusive()),
            (0, 3)
        );
        assert_eq!(
            full.extent().additional_field_text(),
            AdditionalFieldText::None
        );
        assert_eq!(full.extent().field_total_chars(), Some(3));

        let start = selected_field_view(
            "alpha\nbeta",
            LineWindow::Head(NonZeroUsize::new(1).unwrap()),
            FieldViewBudget::NoCharLimit,
            None,
        )
        .unwrap();
        assert_eq!(start.text(), "alpha");
        assert_eq!(
            (start.field_start_char(), start.field_end_char_exclusive()),
            (0, 5)
        );
        assert_eq!(
            start.extent().additional_field_text(),
            AdditionalFieldText::After
        );
        assert_eq!(start.extent().field_total_chars(), None);

        let end = selected_field_view(
            "alpha\nbeta",
            LineWindow::Tail(NonZeroUsize::new(1).unwrap()),
            FieldViewBudget::NoCharLimit,
            None,
        )
        .unwrap();
        assert_eq!(end.text(), "beta");
        assert_eq!(
            (end.field_start_char(), end.field_end_char_exclusive()),
            (6, 10)
        );
        assert_eq!(
            end.extent().additional_field_text(),
            AdditionalFieldText::Before
        );
        assert_eq!(end.extent().field_total_chars(), None);

        let middle = evidence_field_view(&MessageMatchEvidence {
            view_text: "cd".into(),
            field_start_char: 2,
            field_total_chars: 6,
            markers: MessageMatchViewMarkers::Characters {
                ranges: vec![ViewCharRange {
                    view_start_char: 0,
                    view_end_char_exclusive: 2,
                }],
                matched_chars_total: 2,
                matched_chars_shown: 2,
            },
        });
        assert_eq!(
            (middle.field_start_char(), middle.field_end_char_exclusive()),
            (2, 4)
        );
        assert_eq!(
            middle.extent().additional_field_text(),
            AdditionalFieldText::BeforeAndAfter
        );
        assert_eq!(middle.extent().field_total_chars(), Some(6));

        let empty =
            selected_field_view("", LineWindow::Full, FieldViewBudget::NoCharLimit, None).unwrap();
        assert_eq!(
            (empty.field_start_char(), empty.field_end_char_exclusive()),
            (0, 0)
        );
        assert_eq!(
            empty.extent().additional_field_text(),
            AdditionalFieldText::None
        );
        assert_eq!(empty.extent().field_total_chars(), Some(0));

        for view in [&full, &start, &end, &middle, &empty] {
            assert_eq!(
                view.field_end_char_exclusive() - view.field_start_char(),
                view.text().chars().count(),
                "absolute field range must equal returned Unicode scalar count"
            );
        }
    }

    #[test]
    fn content_extent_uses_absolute_ranges_and_one_additional_text_direction() {
        let original = "alpha\nbeta\ngamma";
        let cases = [
            (
                LineWindow::Full,
                FieldViewBudget::NoCharLimit,
                "alpha\nbeta\ngamma",
                0,
                16,
                "none",
                Some(16),
            ),
            (
                LineWindow::Head(NonZeroUsize::new(1).unwrap()),
                FieldViewBudget::NoCharLimit,
                "alpha",
                0,
                5,
                "after",
                None,
            ),
            (
                LineWindow::Tail(NonZeroUsize::new(1).unwrap()),
                FieldViewBudget::NoCharLimit,
                "gamma",
                11,
                16,
                "before",
                None,
            ),
            (
                LineWindow::Full,
                FieldViewBudget::max_chars(6).unwrap(),
                "alpha\n",
                0,
                6,
                "after",
                None,
            ),
        ];
        for (lines, budget, text, start, end, additional, total) in cases {
            let view = selected_field_view(original, lines, budget, None).unwrap();
            let (actual_text, extent) = view.into_content_and_extent();
            let extent = serde_json::to_value(extent).unwrap();
            assert_eq!(actual_text, text);
            assert_eq!(extent["field_start_char"], start);
            assert_eq!(extent["field_end_char_exclusive"], end);
            assert_eq!(extent["additional_field_text"], additional);
            assert_eq!(extent["field_total_chars"], serde_json::json!(total));
            // This shape is serialized into get_session, which has no shared root to hoist to,
            // so it keeps the unit. The search response's views do not.
            assert_eq!(extent["coordinate_unit"], "unicode_scalar");
            for rejected in [
                "complete",
                "omitted_start",
                "omitted_end",
                "returned_chars",
                "original_chars",
            ] {
                assert!(
                    extent.get(rejected).is_none(),
                    "{rejected} leaked: {extent}"
                );
            }
        }

        let trailing_newline = selected_field_view(
            "alpha\nbeta\n",
            LineWindow::Tail(NonZeroUsize::new(1).unwrap()),
            FieldViewBudget::NoCharLimit,
            None,
        )
        .unwrap();
        let (text, extent) = trailing_newline.into_content_and_extent();
        let extent = serde_json::to_value(extent).unwrap();
        assert_eq!(text, "beta");
        assert_eq!(extent["field_start_char"], 6);
        assert_eq!(extent["field_end_char_exclusive"], 10);
        assert_eq!(extent["additional_field_text"], "before_and_after");
    }

    #[test]
    fn regex_zero_width_and_fuzzy_matches_have_typed_markers() {
        let regex = attach_match_evidence(
            &MessageQuery::regex(r"(?m)^").unwrap(),
            &MessageTarget::content(),
            MatchViewBudget::MaxChars {
                max_chars: NonZeroUsize::new(8).unwrap(),
            },
            vec![hit("abcdefghijk")],
        )
        .unwrap();
        assert!(matches!(
            regex[0].match_evidence().unwrap().markers,
            MessageMatchViewMarkers::Boundary { view_at_char: 0 }
        ));

        let fuzzy = attach_match_evidence(
            &MessageQuery::fuzzy("tst").unwrap(),
            &MessageTarget::content(),
            MatchViewBudget::MaxChars {
                max_chars: NonZeroUsize::new(12).unwrap(),
            },
            vec![hit("prefix test suffix")],
        )
        .unwrap();
        let evidence = fuzzy[0].match_evidence().unwrap();
        assert!(matches!(
            evidence.markers,
            MessageMatchViewMarkers::Characters {
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
                    ViewCharRange {
                        view_start_char: 10,
                        view_end_char_exclusive: 11,
                    },
                    ViewCharRange {
                        view_start_char: 12,
                        view_end_char_exclusive: 14,
                    },
                ],
            ),
            (
                "é e\u{301} prefix test",
                vec![
                    ViewCharRange {
                        view_start_char: 12,
                        view_end_char_exclusive: 13,
                    },
                    ViewCharRange {
                        view_start_char: 14,
                        view_end_char_exclusive: 16,
                    },
                ],
            ),
            (
                "👩‍💻 test",
                vec![
                    ViewCharRange {
                        view_start_char: 4,
                        view_end_char_exclusive: 5,
                    },
                    ViewCharRange {
                        view_start_char: 6,
                        view_end_char_exclusive: 8,
                    },
                ],
            ),
        ] {
            let fuzzy = attach_match_evidence(
                &MessageQuery::fuzzy("tst").unwrap(),
                &MessageTarget::content(),
                MatchViewBudget::MaxChars {
                    max_chars: NonZeroUsize::new(30).unwrap(),
                },
                vec![hit(selected)],
            )
            .unwrap();
            let MessageMatchViewMarkers::Characters { ranges, .. } =
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
            MatchViewBudget::MaxChars {
                max_chars: NonZeroUsize::new(20).unwrap(),
            },
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
    fn provider_sets_are_non_empty_deduplicated_and_canonically_ordered() {
        let empty = MessageSearchRequest::builder(
            MessageQuery::literal("needle").unwrap(),
            MessageTarget::content(),
        )
        .providers(Vec::new())
        .err()
        .expect("an empty provider set must fail");
        assert!(empty.to_string().contains("providers"), "{empty}");
        assert!(empty.to_string().contains("at least one"), "{empty}");

        let request = MessageSearchRequest::builder(
            MessageQuery::literal("needle").unwrap(),
            MessageTarget::content(),
        )
        .providers(vec![
            Provider::Codex,
            Provider::Claude,
            Provider::Codex,
            Provider::GeminiCli,
        ])
        .unwrap()
        .build()
        .unwrap();
        assert_eq!(
            request.predicates().providers(),
            Some([Provider::Claude, Provider::Codex, Provider::GeminiCli].as_slice())
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
        .includes([MessageSearchInclude::ParsedReferences])
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
        assert_eq!(
            request.includes(),
            Some([MessageSearchInclude::ParsedReferences].as_slice())
        );
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

    #[test]
    fn registered_rules_reject_the_named_parameter_combinations() {
        let literal = || {
            MessageSearchRequest::builder(
                MessageQuery::literal("needle").unwrap(),
                MessageTarget::content(),
            )
        };
        let cases = [
            (
                MessageSearchRule::DetailOwnsPresentationBudgets,
                literal()
                    .detail(DetailLevel::Full)
                    .message_lines(LineWindow::Full)
                    .build(),
            ),
            (
                MessageSearchRule::SequenceRequiresSession,
                literal()
                    .sequence(SequenceRange::new(Some(1), None).unwrap())
                    .build(),
            ),
            (
                MessageSearchRule::KindsMustRemainSatisfiable,
                literal().kinds(Vec::new()).build(),
            ),
            (
                MessageSearchRule::CompactionRoleRequiresCompactionKind,
                literal()
                    .role(Role::Compaction)
                    .kinds(vec![MessageKind::Conversation])
                    .build(),
            ),
            (
                MessageSearchRule::ToolArgumentRequiresToolCallKind,
                MessageSearchRequest::builder(
                    MessageQuery::literal("needle").unwrap(),
                    MessageTarget::tool_argument("/cmd").unwrap(),
                )
                .kinds(vec![MessageKind::Conversation])
                .build(),
            ),
            (
                MessageSearchRule::MatchViewRequiresQuery,
                MessageSearchRequest::builder(MessageQuery::All, MessageTarget::content())
                    .match_view(MatchViewBudget::MinimalSpan)
                    .build(),
            ),
            (
                MessageSearchRule::FuzzyRejectsMatchWindow,
                MessageSearchRequest::builder(
                    MessageQuery::fuzzy("needle").unwrap(),
                    MessageTarget::content(),
                )
                .match_window(MatchWindow::Earliest)
                .extent(RequestedExtent::page(Some(1), 0).unwrap())
                .build(),
            ),
            (
                MessageSearchRule::LatestWindowRequiresSession,
                literal().match_window(MatchWindow::Latest).build(),
            ),
            (
                MessageSearchRule::FuzzyRejectsAllResults,
                MessageSearchRequest::builder(
                    MessageQuery::fuzzy("needle").unwrap(),
                    MessageTarget::content(),
                )
                .extent(RequestedExtent::all_results())
                .build(),
            ),
        ];

        assert_eq!(
            MessageSearchParameterRegistry::current().rules(),
            MessageSearchRule::ALL
        );
        for (rule, result) in cases {
            let error = result.unwrap_err();
            assert_eq!(error.code(), "parameter-conflict");
            assert!(
                error.to_string().contains(rule.as_str()),
                "{} must identify executable rule {}",
                error,
                rule.as_str()
            );
            assert!(error.to_string().contains(rule.message()));
        }
    }

    #[test]
    fn parameter_registry_derives_closed_vocabularies_and_names_surface_availability() {
        fn serialized_variants<T>() -> Vec<String>
        where
            T: clap::ValueEnum + Copy + Serialize + 'static,
        {
            T::value_variants()
                .iter()
                .map(|value| {
                    serde_json::to_value(*value)
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .to_owned()
                })
                .collect()
        }

        let registry = MessageSearchParameterRegistry::current();
        for (parameter, expected) in [
            (
                MessageSearchParameter::QueryMode,
                serialized_variants::<MessageSearchMode>(),
            ),
            (
                MessageSearchParameter::Field,
                serialized_variants::<SearchField>(),
            ),
            (MessageSearchParameter::Role, serialized_variants::<Role>()),
            (
                MessageSearchParameter::Kinds,
                serialized_variants::<MessageKind>(),
            ),
            (
                MessageSearchParameter::MatchWindow,
                serialized_variants::<MatchWindow>(),
            ),
            (
                MessageSearchParameter::Detail,
                serialized_variants::<DetailLevel>(),
            ),
            (
                MessageSearchParameter::Include,
                serialized_variants::<MessageSearchInclude>(),
            ),
            (
                MessageSearchParameter::ReceiptLevel,
                serialized_variants::<ReceiptLevel>(),
            ),
        ] {
            assert_eq!(
                registry
                    .parameter(parameter)
                    .unwrap_or_else(|| panic!("missing registry parameter {}", parameter.as_str()))
                    .accepted_values(),
                expected,
                "{} must derive its vocabulary from the executable enum",
                parameter.as_str()
            );
        }
        assert_eq!(
            registry
                .parameter(MessageSearchParameter::Providers)
                .unwrap()
                .accepted_values(),
            crate::source::PROVIDERS
                .iter()
                .map(|provider| provider.as_str().to_owned())
                .collect::<Vec<_>>(),
            "provider vocabulary and canonical order must come from source::PROVIDERS"
        );
        assert_eq!(
            registry
                .parameter(MessageSearchParameter::Query)
                .unwrap()
                .domain(),
            &MessageSearchParameterDomain::Text { non_empty: false },
            "empty is the cross-surface queryless-search spelling and must not be advertised as invalid"
        );
        assert_eq!(
            registry
                .parameter(MessageSearchParameter::Detail)
                .unwrap()
                .surfaces(),
            SearchSurface::ALL,
            "the finalized registry must not freeze provisional CLI/Python presentation gaps"
        );
        assert_eq!(registry.rule_descriptors().len(), registry.rules().len());
        for (descriptor, rule) in registry.rule_descriptors().iter().zip(registry.rules()) {
            assert_eq!(descriptor.rule(), *rule);
            assert_eq!(descriptor.message(), rule.message());
        }
    }
}
