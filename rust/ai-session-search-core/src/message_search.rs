use std::num::{NonZeroU32, NonZeroUsize};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::models::{MessageKind, Provider, Role, SearchField};

pub const MAX_FUZZY_RESULT_WINDOW: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MessageSearchError {
    #[error("{kind} query must not be empty")]
    EmptyQuery { kind: &'static str },
    #[error("fuzzy query must contain at least 3 Unicode scalar values")]
    ShortFuzzyQuery,
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
            Self::EmptyQuery { .. } | Self::ShortFuzzyQuery | Self::InvalidRegex(_) => {
                "invalid-query"
            }
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MatchWindow {
    #[default]
    Earliest,
    Latest,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum RequestedExtent {
    Page {
        limit: Option<NonZeroUsize>,
        offset: usize,
    },
    AllResults,
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
        Self::AllResults
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
    kind: Option<MessageKind>,
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
            kind: None,
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

    pub const fn kind(&self) -> Option<MessageKind> {
        self.kind
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
    include_refs: bool,
    message_lines: LineWindow,
}

impl MessagePresentation {
    pub const fn include_refs(&self) -> bool {
        self.include_refs
    }

    pub const fn message_lines(&self) -> LineWindow {
        self.message_lines
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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct MessageSearchRequest {
    query: MessageQuery,
    target: MessageTarget,
    predicates: MessagePredicates,
    match_window: Option<MatchWindow>,
    context: ContextWindow,
    presentation: MessagePresentation,
    extent: RequestedExtent,
    purpose: Option<PurposeSelection>,
    receipt: ReceiptLevel,
}

impl MessageSearchRequest {
    pub fn builder(query: MessageQuery, target: MessageTarget) -> MessageSearchRequestBuilder {
        MessageSearchRequestBuilder {
            request: Self {
                query,
                target,
                predicates: MessagePredicates::default(),
                match_window: None,
                context: ContextWindow::default(),
                presentation: MessagePresentation::default(),
                extent: RequestedExtent::default(),
                purpose: None,
                receipt: ReceiptLevel::None,
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

    pub const fn context(&self) -> ContextWindow {
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

    pub const fn receipt_level(&self) -> ReceiptLevel {
        self.receipt
    }

    fn validate(&self) -> Result<(), MessageSearchError> {
        if self.predicates.sequence.is_some() && self.predicates.session.is_none() {
            return Err(MessageSearchError::Conflict(
                "sequence bounds require one session".into(),
            ));
        }
        if !self.predicates.include_compaction
            && (self.predicates.role == Some(Role::Compaction)
                || self.predicates.kind == Some(MessageKind::Compaction))
        {
            return Err(MessageSearchError::Conflict(
                "include_compaction=false conflicts with a compaction role or kind".into(),
            ));
        }
        if self.target.field == SearchField::ToolArgument
            && self
                .predicates
                .kind
                .is_some_and(|kind| kind != MessageKind::ToolCall)
        {
            return Err(MessageSearchError::Conflict(
                "tool-argument target permits only kind=tool-call".into(),
            ));
        }
        if self.match_window.is_some()
            && matches!(self.query, MessageQuery::All | MessageQuery::Fuzzy(_))
        {
            return Err(MessageSearchError::Conflict(
                "match_window applies only to literal or regex queries".into(),
            ));
        }
        if self.match_window == Some(MatchWindow::Latest) && self.predicates.session.is_none() {
            return Err(MessageSearchError::Conflict(
                "match_window=latest requires one session".into(),
            ));
        }
        if let MessageQuery::Fuzzy(_) = self.query {
            match self.extent {
                RequestedExtent::Page {
                    limit: Some(limit),
                    offset: 0,
                } if limit.get() <= MAX_FUZZY_RESULT_WINDOW => {}
                RequestedExtent::Page {
                    limit: None,
                    offset: 0,
                } => {}
                RequestedExtent::Page { offset, .. } if offset != 0 => {
                    return Err(MessageSearchError::InvalidParameter {
                        parameter: "offset",
                        reason: "fuzzy search requires offset 0".into(),
                    })
                }
                RequestedExtent::Page {
                    limit: Some(limit), ..
                } => {
                    return Err(MessageSearchError::InvalidParameter {
                        parameter: "limit",
                        reason: format!(
                            "fuzzy page size must be at most {MAX_FUZZY_RESULT_WINDOW}, got {}",
                            limit.get()
                        ),
                    })
                }
                RequestedExtent::Page { limit: None, .. } => {
                    return Err(MessageSearchError::InvalidParameter {
                        parameter: "offset",
                        reason: "fuzzy search requires offset 0".into(),
                    })
                }
                RequestedExtent::AllResults => {
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

    pub fn kind(mut self, kind: MessageKind) -> Self {
        self.request.predicates.kind = Some(kind);
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
        self.request.context = context;
        self
    }

    pub fn include_refs(mut self, include: bool) -> Self {
        self.request.presentation.include_refs = include;
        self
    }

    pub fn message_lines(mut self, window: LineWindow) -> Self {
        self.request.presentation.message_lines = window;
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
        self.request.receipt = level;
        self
    }

    pub fn build(self) -> Result<MessageSearchRequest, MessageSearchError> {
        self.request.validate()?;
        Ok(self.request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

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
                .is_err()
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
        assert_eq!(request.context(), ContextWindow::new(1, 3));
        assert!(request.presentation().include_refs());
        assert_eq!(request.match_window(), Some(MatchWindow::Latest));
        assert_eq!(request.receipt_level(), ReceiptLevel::Full);
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
