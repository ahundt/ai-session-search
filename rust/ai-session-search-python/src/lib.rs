// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use ai_session_search::analysis_pipeline::{
    AnalysisPolicy as RustAnalysisPolicy, AnalysisPolicySpec as RustAnalysisPolicySpec,
    AnalysisResult, AnalyzedSession, ClassificationMatch, ClassificationRuleSpec,
    ClassificationTarget, PhraseFrequency, PhraseTextMode, PhraseVocabularyPolicySpec,
    RelationshipHint, RelationshipKind, RelationshipResolution, RelationshipRuleSpec, SessionGraph,
    SessionGraphEdge, SessionGraphGroup, SessionGraphNode,
};
use ai_session_search::analysis_publication::{
    AnalysisArtifact as RustAnalysisArtifact,
    AnalysisPublicationFormat as RustAnalysisPublicationFormat,
    AnalysisPublicationPlan as RustAnalysisPublicationPlan,
    AnalysisPublicationReceipt as RustAnalysisPublicationReceipt,
    PublishedAnalysisArtifact as RustPublishedAnalysisArtifact,
};
use ai_session_search::config::{Config, ConfigOverrides};
use ai_session_search::indexer::AutoReindexOutcome;
use ai_session_search::message_search::{
    ContextWindow as CoreContextWindow, DetailLevel as CoreDetailLevel,
    FieldViewBudget as CoreFieldViewBudget, LineWindow as CoreLineWindow,
    MatchViewBudget as CoreMatchViewBudget, MatchWindow as CoreMatchWindow,
    MessageQuery as CoreMessageQuery, MessageSearchError as CoreMessageSearchError,
    MessageSearchInclude as CoreMessageSearchInclude,
    MessageSearchRequest as CoreMessageSearchRequest,
    MessageSearchRuntimeDiagnostics as CoreMessageSearchRuntimeDiagnostics,
    MessageTarget as CoreMessageTarget, PurposeSelection as CorePurposeSelection,
    ReceiptLevel as CoreReceiptLevel, RequestedExtent as CoreRequestedExtent,
    RequestedTimeRange as CoreRequestedTimeRange, SearchSurface as CoreSearchSurface,
    SequenceRange as CoreSequenceRange,
};
use ai_session_search::models::{
    AnalysisCursor, AnalysisDocument, AnalysisDocumentPage, AnalysisRequest as CoreAnalysisRequest,
    AnalysisSessionSelection as CoreAnalysisSessionSelection, FileCrossRef, FileEditSummary,
    FileQuery as CoreFileQuery, FileVersion, IndexReadinessStatus, IndexRefreshStatus, IndexStatus,
    MessageFilters, MessageHit, MessageKind, ParserHealth, Provider, ProviderHealth,
    ProviderParserHealth, Role, SearchField, SearchFilters, SearchHit, SessionKind, SessionRecord,
};
use ai_session_search::service::{
    AnalysisReceipt as CoreAnalysisReceipt, CompactOutcome,
    ReceiptedAnalysis as CoreReceiptedAnalysis, SessionSearch as CoreSessionSearch,
};
use ai_session_search::{
    MessageSearchBatch as CoreMessageSearchBatch, MessageSearchBatches as CoreMessageSearchBatches,
    MessageSearchCompletion as CoreMessageSearchCompletion,
};
use pyo3::exceptions::{PyBrokenPipeError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;

fn runtime_error(error: impl std::fmt::Display) -> PyErr {
    // The alternate flag renders an anyhow chain as "top: cause1: cause2" on one line instead
    // of just the top-level `.context(...)` message, so a wrapped failure (e.g. a migration
    // error) still surfaces the underlying error text to the caller instead of swallowing it.
    PyRuntimeError::new_err(format!("{error:#}"))
}

fn value_error(error: impl std::fmt::Display) -> PyErr {
    // Same rationale as runtime_error: the alternate flag preserves any anyhow context chain
    // instead of only the top-level message. Harmless no-op for the flat MessageSearchError
    // callers (no source chain to render differently) and a real fix for anyhow::Error callers.
    PyValueError::new_err(format!("{error:#}"))
}

fn python_view_budget_value(
    parameter: &'static str,
    value: Option<&Bound<'_, PyDict>>,
) -> PyResult<Option<serde_json::Value>> {
    value
        .map(|mapping| {
            let mut object = serde_json::Map::new();
            for (key, value) in mapping {
                let key = key.extract::<String>().map_err(|_| {
                    PyValueError::new_err(format!("{parameter} keys must be strings"))
                })?;
                let value = if let Ok(text) = value.extract::<String>() {
                    serde_json::Value::String(text)
                } else if let Ok(integer) = value.extract::<u64>() {
                    serde_json::Value::from(integer)
                } else {
                    return Err(PyValueError::new_err(format!(
                        "{parameter}.{key} must be a string or non-negative integer"
                    )));
                };
                object.insert(key, value);
            }
            Ok(serde_json::Value::Object(object))
        })
        .transpose()
}

fn core_message_query(query: String, query_mode: &str) -> PyResult<(CoreMessageQuery, bool)> {
    let has_content_query = !query.is_empty();
    let query = match (query_mode, query.is_empty()) {
        ("literal", true) => Ok(CoreMessageQuery::All),
        ("literal", false) => CoreMessageQuery::literal(query),
        ("regex", false) => CoreMessageQuery::regex(query),
        ("fuzzy", false) => CoreMessageQuery::fuzzy(query),
        ("regex" | "fuzzy", true) => {
            return Err(PyValueError::new_err(format!(
                "query_mode={query_mode} requires a nonempty query"
            )));
        }
        _ => {
            return Err(PyValueError::new_err(
                "query_mode must be 'literal', 'regex', or 'fuzzy'",
            ));
        }
    }
    .map_err(value_error)?;
    Ok((query, has_content_query))
}

/// Preserve caller-vs-runtime failure semantics across the Rust/Python boundary.
///
/// `MessageSearchError` is the closed set of query and parameter validation failures. Database,
/// indexing, I/O, and execution errors remain `RuntimeError`; no message-string classification is
/// used. Downcasting through anyhow context keeps this correct when a service adds operation
/// context around the typed source.
fn python_message_search_error(error: anyhow::Error) -> PyErr {
    if error.downcast_ref::<CoreMessageSearchError>().is_some() {
        value_error(error)
    } else {
        runtime_error(error)
    }
}

fn python_batch_open_error(error: anyhow::Error) -> PyErr {
    let caller_input = error.downcast_ref::<CoreMessageSearchError>().is_some();
    let rendered = format!("{error:#}").replace("search()", "search_messages()");
    if caller_input {
        PyValueError::new_err(rendered)
    } else {
        PyRuntimeError::new_err(rendered)
    }
}

fn message_search_include_name(include: CoreMessageSearchInclude) -> &'static str {
    match include {
        CoreMessageSearchInclude::NormalizedSessionMetadata => "normalized_session_metadata",
        CoreMessageSearchInclude::ParsedReferences => "parsed_references",
        CoreMessageSearchInclude::RawProviderMetadata => "raw_provider_metadata",
        CoreMessageSearchInclude::RuntimeDiagnostics => "runtime_diagnostics",
    }
}

fn parse_message_search_includes(
    includes: Option<Vec<String>>,
) -> PyResult<Option<Vec<CoreMessageSearchInclude>>> {
    includes
        .map(|includes| {
            includes
                .into_iter()
                .map(|include| match include.as_str() {
                    "normalized_session_metadata" => {
                        Ok(CoreMessageSearchInclude::NormalizedSessionMetadata)
                    }
                    "parsed_references" => Ok(CoreMessageSearchInclude::ParsedReferences),
                    "raw_provider_metadata" => Ok(CoreMessageSearchInclude::RawProviderMetadata),
                    "runtime_diagnostics" => Ok(CoreMessageSearchInclude::RuntimeDiagnostics),
                    _ => Err(PyValueError::new_err(format!(
                        "unknown include {include:?}; accepted values are normalized_session_metadata, parsed_references, raw_provider_metadata, and runtime_diagnostics"
                    ))),
                })
                .collect()
        })
        .transpose()
}

fn json_compatible<'py, T: serde::Serialize>(
    py: Python<'py>,
    value: &T,
) -> PyResult<Bound<'py, PyAny>> {
    let encoded = serde_json::to_string(value).map_err(runtime_error)?;
    py.import("json")?.call_method1("loads", (encoded,))
}

fn json_compatible_with_loads<T: serde::Serialize>(
    loads: &Bound<'_, PyAny>,
    value: &T,
) -> PyResult<Py<PyAny>> {
    let encoded = serde_json::to_string(value).map_err(runtime_error)?;
    Ok(loads.call1((encoded,))?.unbind())
}

fn python_message_classification_definition(
    value: Option<&Bound<'_, PyDict>>,
) -> PyResult<Option<ai_session_search::MessageClassificationDefinition>> {
    value
        .map(|mapping| {
            let encoded = mapping
                .py()
                .import("json")?
                .call_method1("dumps", (mapping,))?
                .extract::<String>()?;
            serde_json::from_str(&encoded).map_err(|error| {
                PyValueError::new_err(format!(
                    "definition must contain only categories with name and patterns fields: \
                     {error}"
                ))
            })
        })
        .transpose()
}

/// Serve the AI Session Search MCP protocol over standard input and output until EOF.
#[pyfunction]
fn serve_mcp(py: Python<'_>) -> PyResult<()> {
    py.detach(|| ai_session_search::mcp_server::serve().map_err(runtime_error))
}

#[pyfunction]
fn _run_cli_command(py: Python<'_>, args: Vec<String>) -> PyResult<i32> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(OsString::from("aise"));
    argv.extend(args.into_iter().map(OsString::from));
    py.detach(move || {
        ai_session_search::run_cli_from(argv).map_err(|error| {
            if ai_session_search::is_broken_pipe_error(&error) {
                PyBrokenPipeError::new_err(format!("{error:#}"))
            } else {
                runtime_error(error)
            }
        })
    })
}

/// Parse session-class names into the typed set.
///
/// `None` keeps the default set (every class); an empty list is a caller who deselected every
/// class and gets no rows, which is why the two are not collapsed. The rejection message is the
/// enum's own, so it names the accepted values and cannot drift from what parses.
fn parse_session_kinds(values: Option<Vec<String>>) -> PyResult<Option<Vec<SessionKind>>> {
    values
        .map(|values| {
            values
                .into_iter()
                .map(|value| value.parse::<SessionKind>().map_err(PyValueError::new_err))
                .collect::<PyResult<Vec<_>>>()
        })
        .transpose()
}

fn parse_provider(value: Option<String>) -> PyResult<Option<Provider>> {
    value
        .map(|value| {
            // Surface the parser's message unwrapped: it already opens with "unsupported
            // provider: <value>" and names every accepted provider, so an "invalid provider: "
            // prefix here only produced "invalid provider: unsupported provider: chatgpt — ...".
            value.parse().map_err(PyValueError::new_err)
        })
        .transpose()
}

fn parse_provider_set(values: Option<Vec<String>>) -> PyResult<Option<Vec<Provider>>> {
    values
        .map(|values| {
            if values.is_empty() {
                return Err(PyValueError::new_err(
                    "providers must contain at least one provider; omit providers to search all sources",
                ));
            }
            let mut providers = values
                .into_iter()
                .map(|value| value.parse().map_err(PyValueError::new_err))
                .collect::<PyResult<Vec<Provider>>>()?;
            providers.sort_unstable_by_key(|provider| provider.as_str());
            providers.dedup();
            Ok(providers)
        })
        .transpose()
}

/// A paging argument accepted from Python, carrying the guidance its rejection message needs.
///
/// An enum rather than a `&str` name so a call site cannot introduce an unlabelled parameter that
/// falls through to generic wording.
#[derive(Clone, Copy)]
enum PagingArgument {
    Limit,
    Offset,
}

impl PagingArgument {
    const fn name(self) -> &'static str {
        match self {
            Self::Limit => "limit",
            Self::Offset => "offset",
        }
    }

    /// What the caller should pass instead.
    ///
    /// Deliberately names no unrelated parameter. This helper serves query types with different
    /// presentation controls, so redirecting a paging error to one of those controls would be
    /// misleading.
    const fn guidance(self) -> &'static str {
        match self {
            Self::Limit => "pass a positive count, or 0 for every match",
            Self::Offset => "pass the number of results to skip, or 0 to start at the first",
        }
    }
}

/// Convert a caller-supplied `limit`/`offset` to `usize`, rejecting negatives with the parameter
/// name, the rejected value, the bound, and what to pass instead.
///
/// Taking these as `i64` rather than `usize` keeps the rejection in this crate: PyO3's own `usize`
/// extraction raises `OverflowError: can't convert negative int to unsigned`, naming neither the
/// parameter nor the bound. Naming the bound alone is still not enough to act on, because `0` is
/// not simply the floor — `limit=0` returns every match while `offset=0` starts at the first
/// result — so the message states that distinction instead of leaving the caller to find it in
/// the schema or docstring.
fn paging_argument(argument: PagingArgument, value: i64) -> PyResult<usize> {
    usize::try_from(value).map_err(|_| {
        PyValueError::new_err(format!(
            "{} must be 0 or greater, got {value}; {}",
            argument.name(),
            argument.guidance()
        ))
    })
}

const fn classification_target_name(target: ClassificationTarget) -> &'static str {
    match target {
        ClassificationTarget::Title => "title",
        ClassificationTarget::Summary => "summary",
        ClassificationTarget::FirstUserText => "first_user_text",
        ClassificationTarget::UserText => "user_text",
        ClassificationTarget::Any => "any",
    }
}

const fn relationship_kind_name(kind: RelationshipKind) -> &'static str {
    match kind {
        RelationshipKind::Branch => "branch",
        RelationshipKind::Copy => "copy",
        RelationshipKind::Version => "version",
    }
}

/// Canonical indexed session metadata and source provenance.
#[pyclass(name = "SessionRecord", module = "ai_session_search._native", frozen)]
struct NativeSessionRecord {
    #[pyo3(get)]
    id: String,
    #[pyo3(get)]
    provider: String,
    #[pyo3(get)]
    provider_session_id: String,
    #[pyo3(get)]
    title: Option<String>,
    #[pyo3(get)]
    summary: Option<String>,
    #[pyo3(get)]
    cwd: Option<String>,
    #[pyo3(get)]
    repo_root: Option<String>,
    #[pyo3(get)]
    created_at: Option<String>,
    #[pyo3(get)]
    updated_at: Option<String>,
    #[pyo3(get)]
    last_message_at: Option<String>,
    #[pyo3(get)]
    preview_text: String,
    #[pyo3(get)]
    source_path: String,
    #[pyo3(get)]
    message_count: Option<i64>,
    #[pyo3(get)]
    parse_warning: Option<String>,
    /// The session that spawned this one when it is a subagent run, otherwise None.
    #[pyo3(get)]
    parent_session_id: Option<String>,
    /// Provider-recorded name for the spawned agent, otherwise None.
    #[pyo3(get)]
    agent_label: Option<String>,
}

impl From<SessionRecord> for NativeSessionRecord {
    fn from(session: SessionRecord) -> Self {
        Self {
            id: session.id,
            provider: session.provider.as_str().to_string(),
            provider_session_id: session.provider_session_id,
            title: session.title,
            summary: session.summary,
            cwd: session.cwd,
            repo_root: session.repo_root,
            created_at: session.created_at.map(|value| value.to_rfc3339()),
            updated_at: session.updated_at.map(|value| value.to_rfc3339()),
            last_message_at: session.last_message_at.map(|value| value.to_rfc3339()),
            preview_text: session.preview_text,
            source_path: session.source_path,
            message_count: session.message_count,
            parse_warning: session.parse_warning,
            parent_session_id: session.parent_session_id,
            agent_label: session.agent_label,
        }
    }
}

#[derive(Clone)]
/// Opaque keyset cursor for the next non-overlapping analysis document page.
#[pyclass(
    name = "AnalysisCursor",
    module = "ai_session_search._native",
    frozen,
    from_py_object
)]
struct NativeAnalysisCursor {
    inner: AnalysisCursor,
}

/// One indexed session and its normalized user-message text for analysis.
#[pyclass(
    name = "AnalysisDocument",
    module = "ai_session_search._native",
    frozen
)]
struct NativeAnalysisDocument {
    #[pyo3(get)]
    session: Py<NativeSessionRecord>,
    #[pyo3(get)]
    user_text: String,
    #[pyo3(get)]
    first_user_text: Option<String>,
    #[pyo3(get)]
    message_count: i64,
    #[pyo3(get)]
    user_message_count: i64,
}

impl NativeAnalysisDocument {
    fn from_document(py: Python<'_>, document: AnalysisDocument) -> PyResult<Self> {
        Ok(Self {
            session: Py::new(py, NativeSessionRecord::from(document.session))?,
            user_text: document.user_text,
            first_user_text: document.first_user_text,
            message_count: document.message_count,
            user_message_count: document.user_message_count,
        })
    }
}

/// Bounded analysis document page with an optional continuation cursor.
#[pyclass(
    name = "AnalysisDocumentPage",
    module = "ai_session_search._native",
    frozen
)]
struct NativeAnalysisDocumentPage {
    #[pyo3(get)]
    documents: Vec<Py<NativeAnalysisDocument>>,
    #[pyo3(get)]
    next_cursor: Option<Py<NativeAnalysisCursor>>,
}

impl NativeAnalysisDocumentPage {
    fn from_page(py: Python<'_>, page: AnalysisDocumentPage) -> PyResult<Self> {
        Ok(Self {
            documents: page
                .documents
                .into_iter()
                .map(|document| {
                    NativeAnalysisDocument::from_document(py, document)
                        .and_then(|document| Py::new(py, document))
                })
                .collect::<PyResult<Vec<_>>>()?,
            next_cursor: page
                .next_cursor
                .map(|inner| Py::new(py, NativeAnalysisCursor { inner }))
                .transpose()?,
        })
    }
}

#[derive(Clone)]
/// One weighted regex classification applied to a selected session text field.
#[pyclass(module = "ai_session_search._native", frozen, from_py_object)]
struct ClassificationRule {
    inner: ClassificationRuleSpec,
}

#[pymethods]
impl ClassificationRule {
    #[new]
    #[pyo3(signature = (dimension, label, pattern, *, target="user_text", weight=0))]
    fn new(
        dimension: String,
        label: String,
        pattern: String,
        target: &str,
        weight: i64,
    ) -> PyResult<Self> {
        let target = match target {
            "title" => ClassificationTarget::Title,
            "summary" => ClassificationTarget::Summary,
            "first_user_text" => ClassificationTarget::FirstUserText,
            "user_text" => ClassificationTarget::UserText,
            "any" => ClassificationTarget::Any,
            value => {
                return Err(PyValueError::new_err(format!(
                    "invalid classification target '{value}'; expected title, summary, first_user_text, user_text, or any"
                )))
            }
        };
        let inner = ClassificationRuleSpec {
            dimension,
            label,
            target,
            pattern,
            weight,
        };
        RustAnalysisPolicy::compile(vec![inner.clone()], Vec::new()).map_err(value_error)?;
        Ok(Self { inner })
    }

    #[getter]
    fn dimension(&self) -> &str {
        &self.inner.dimension
    }

    #[getter]
    fn label(&self) -> &str {
        &self.inner.label
    }

    #[getter]
    fn pattern(&self) -> &str {
        &self.inner.pattern
    }

    #[getter]
    fn target(&self) -> &'static str {
        classification_target_name(self.inner.target)
    }

    #[getter]
    fn weight(&self) -> i64 {
        self.inner.weight
    }
}

#[derive(Clone)]
/// One regex rule that identifies a branch, copy, or version relationship between sessions.
#[pyclass(module = "ai_session_search._native", frozen, from_py_object)]
struct RelationshipRule {
    inner: RelationshipRuleSpec,
}

#[derive(Clone)]
/// Bounded recurring-phrase extraction policy for analyzed user messages.
#[pyclass(module = "ai_session_search._native", frozen, from_py_object)]
struct PhraseVocabulary {
    spec: PhraseVocabularyPolicySpec,
}

#[pymethods]
impl PhraseVocabulary {
    #[new]
    #[pyo3(signature = (widths, max_unique_phrases, *, min_document_tokens=0, excluded_tokens=None, exclude_numeric_tokens=true, prose_only=false))]
    fn new(
        widths: Vec<usize>,
        max_unique_phrases: usize,
        min_document_tokens: usize,
        excluded_tokens: Option<Vec<String>>,
        exclude_numeric_tokens: bool,
        prose_only: bool,
    ) -> PyResult<Self> {
        let spec = PhraseVocabularyPolicySpec {
            widths,
            max_unique_phrases,
            min_document_tokens,
            excluded_tokens: excluded_tokens.unwrap_or_default(),
            exclude_numeric_tokens,
            text_mode: if prose_only {
                PhraseTextMode::ProseOnly
            } else {
                PhraseTextMode::UserText
            },
        };
        spec.compile().map_err(value_error)?;
        Ok(Self { spec })
    }

    #[getter]
    fn widths(&self) -> Vec<usize> {
        self.spec.widths.clone()
    }

    #[getter]
    fn max_unique_phrases(&self) -> usize {
        self.spec.max_unique_phrases
    }

    #[getter]
    fn min_document_tokens(&self) -> usize {
        self.spec.min_document_tokens
    }

    #[getter]
    fn excluded_tokens(&self) -> Vec<String> {
        self.spec.excluded_tokens.clone()
    }

    #[getter]
    fn exclude_numeric_tokens(&self) -> bool {
        self.spec.exclude_numeric_tokens
    }

    #[getter]
    fn prose_only(&self) -> bool {
        self.spec.text_mode == PhraseTextMode::ProseOnly
    }
}

#[derive(Clone)]
/// Validated classification, relationship, and optional phrase-analysis policy.
#[pyclass(module = "ai_session_search._native", frozen, from_py_object)]
struct AnalysisPolicy {
    inner: RustAnalysisPolicy,
}

#[pymethods]
impl AnalysisPolicy {
    #[new]
    #[pyo3(signature = (*, classification_rules=None, relationship_rules=None, phrase_vocabulary=None, max_classification_chars=None))]
    fn new(
        classification_rules: Option<Vec<ClassificationRule>>,
        relationship_rules: Option<Vec<RelationshipRule>>,
        phrase_vocabulary: Option<PhraseVocabulary>,
        max_classification_chars: Option<usize>,
    ) -> PyResult<Self> {
        let spec = RustAnalysisPolicySpec {
            classification_rules: classification_rules
                .unwrap_or_default()
                .into_iter()
                .map(|rule| rule.inner)
                .collect(),
            relationship_rules: relationship_rules
                .unwrap_or_default()
                .into_iter()
                .map(|rule| rule.inner)
                .collect(),
            phrase_vocabulary: phrase_vocabulary.map(|vocabulary| vocabulary.spec),
            max_classification_chars,
        };
        let inner = spec.compile().map_err(value_error)?;
        Ok(Self { inner })
    }
}

#[pymethods]
impl RelationshipRule {
    #[new]
    fn new(id: String, kind: &str, pattern: String) -> PyResult<Self> {
        let kind = match kind {
            "branch" => RelationshipKind::Branch,
            "copy" => RelationshipKind::Copy,
            "version" => RelationshipKind::Version,
            value => {
                return Err(PyValueError::new_err(format!(
                    "invalid relationship kind '{value}'; expected branch, copy, or version"
                )))
            }
        };
        let inner = RelationshipRuleSpec { id, kind, pattern };
        RustAnalysisPolicy::compile(Vec::new(), vec![inner.clone()]).map_err(value_error)?;
        Ok(Self { inner })
    }

    #[getter]
    fn id(&self) -> &str {
        &self.inner.id
    }

    #[getter]
    fn kind(&self) -> &'static str {
        relationship_kind_name(self.inner.kind)
    }

    #[getter]
    fn pattern(&self) -> &str {
        &self.inner.pattern
    }
}

/// One classification label and weight matched in a session.
#[pyclass(
    name = "ClassificationMatch",
    module = "ai_session_search._native",
    frozen
)]
struct NativeClassificationMatch {
    #[pyo3(get)]
    dimension: String,
    #[pyo3(get)]
    label: String,
    #[pyo3(get)]
    target: String,
    #[pyo3(get)]
    weight: i64,
}

impl From<ClassificationMatch> for NativeClassificationMatch {
    fn from(value: ClassificationMatch) -> Self {
        Self {
            dimension: value.dimension,
            label: value.label,
            target: classification_target_name(value.target).into(),
            weight: value.weight,
        }
    }
}

/// Resolved, ambiguous, or unresolved relationship inferred for a session.
#[pyclass(
    name = "RelationshipHint",
    module = "ai_session_search._native",
    frozen
)]
struct NativeRelationshipHint {
    #[pyo3(get)]
    rule_id: String,
    #[pyo3(get)]
    kind: String,
    #[pyo3(get)]
    parent_title: String,
    #[pyo3(get)]
    status: String,
    #[pyo3(get)]
    resolved_session_id: Option<String>,
    #[pyo3(get)]
    candidate_session_ids: Vec<String>,
}

impl From<RelationshipHint> for NativeRelationshipHint {
    fn from(value: RelationshipHint) -> Self {
        let (status, resolved_session_id, candidate_session_ids) = match value.resolution {
            RelationshipResolution::Unresolved => ("unresolved", None, Vec::new()),
            RelationshipResolution::Resolved { session_id } => {
                ("resolved", Some(session_id), Vec::new())
            }
            RelationshipResolution::Ambiguous { session_ids } => ("ambiguous", None, session_ids),
        };
        Self {
            rule_id: value.rule_id,
            kind: relationship_kind_name(value.kind).into(),
            parent_title: value.parent_title,
            status: status.into(),
            resolved_session_id,
            candidate_session_ids,
        }
    }
}

/// One session with its analysis score, classifications, and relationship hints.
#[pyclass(name = "AnalyzedSession", module = "ai_session_search._native", frozen)]
struct NativeAnalyzedSession {
    #[pyo3(get)]
    session: Py<NativeSessionRecord>,
    #[pyo3(get)]
    classifications: Vec<Py<NativeClassificationMatch>>,
    #[pyo3(get)]
    score: i64,
    #[pyo3(get)]
    relationship_hints: Vec<Py<NativeRelationshipHint>>,
    #[pyo3(get)]
    has_user_text: bool,
    #[pyo3(get)]
    message_count: i64,
    #[pyo3(get)]
    user_message_count: i64,
}

impl NativeAnalyzedSession {
    fn from_session(py: Python<'_>, value: AnalyzedSession) -> PyResult<Self> {
        Ok(Self {
            session: Py::new(py, NativeSessionRecord::from(value.session))?,
            classifications: value
                .classifications
                .into_iter()
                .map(|item| Py::new(py, NativeClassificationMatch::from(item)))
                .collect::<PyResult<Vec<_>>>()?,
            score: value.score,
            relationship_hints: value
                .relationship_hints
                .into_iter()
                .map(|item| Py::new(py, NativeRelationshipHint::from(item)))
                .collect::<PyResult<Vec<_>>>()?,
            has_user_text: value.has_user_text,
            message_count: value.message_count,
            user_message_count: value.user_message_count,
        })
    }
}

/// Recurring normalized phrase with document and occurrence counts.
#[pyclass(name = "PhraseFrequency", module = "ai_session_search._native", frozen)]
struct NativePhraseFrequency {
    #[pyo3(get)]
    phrase: String,
    #[pyo3(get)]
    words: usize,
    #[pyo3(get)]
    documents: u64,
    #[pyo3(get)]
    occurrences: u64,
}

impl From<PhraseFrequency> for NativePhraseFrequency {
    fn from(value: PhraseFrequency) -> Self {
        Self {
            phrase: value.phrase,
            words: value.words,
            documents: value.documents,
            occurrences: value.occurrences,
        }
    }
}

/// Typed classifications, relationships, vocabulary, and graph for analyzed sessions.
#[pyclass(name = "AnalysisResult", module = "ai_session_search._native", frozen)]
struct NativeAnalysisResult {
    inner: Arc<CoreReceiptedAnalysis>,
    graph: OnceLock<SessionGraph>,
}

impl NativeAnalysisResult {
    fn from_receipted(value: Arc<CoreReceiptedAnalysis>) -> Self {
        Self {
            inner: value,
            graph: OnceLock::new(),
        }
    }

    fn result(&self) -> &AnalysisResult {
        &self.inner.result
    }
}

#[pymethods]
impl NativeAnalysisResult {
    #[getter]
    fn sessions(&self, py: Python<'_>) -> PyResult<BTreeMap<String, Py<NativeAnalyzedSession>>> {
        self.result()
            .sessions
            .iter()
            .map(|(id, session)| {
                NativeAnalyzedSession::from_session(py, session.clone())
                    .and_then(|session| Py::new(py, session))
                    .map(|session| (id.clone(), session))
            })
            .collect()
    }

    #[getter]
    fn vocabulary(&self, py: Python<'_>) -> PyResult<Vec<Py<NativePhraseFrequency>>> {
        self.result()
            .vocabulary
            .iter()
            .cloned()
            .map(|item| Py::new(py, NativePhraseFrequency::from(item)))
            .collect()
    }

    #[getter]
    fn graph(&self, py: Python<'_>) -> PyResult<Py<NativeSessionGraph>> {
        let graph = self
            .graph
            .get_or_init(|| self.result().session_graph())
            .clone();
        Py::new(py, NativeSessionGraph::from_graph(py, graph)?)
    }
}

/// Same-snapshot selection, count, and digest facts for one analysis result.
#[pyclass(name = "AnalysisReceipt", module = "ai_session_search._native", frozen)]
struct NativeAnalysisReceipt {
    inner: CoreAnalysisReceipt,
}

#[pymethods]
impl NativeAnalysisReceipt {
    #[getter]
    fn selection_kind(&self) -> &'static str {
        match self.inner.selection {
            CoreAnalysisSessionSelection::AllEligible => "all_eligible",
            CoreAnalysisSessionSelection::FirstCanonicalSessions { .. } => {
                "first_canonical_sessions"
            }
        }
    }

    #[getter]
    fn max_selected_sessions(&self) -> Option<usize> {
        match self.inner.selection {
            CoreAnalysisSessionSelection::AllEligible => None,
            CoreAnalysisSessionSelection::FirstCanonicalSessions { max_sessions } => {
                Some(max_sessions.get())
            }
        }
    }

    #[getter]
    fn database_schema_version(&self) -> i64 {
        self.inner.database_schema_version
    }

    #[getter]
    fn selected_sessions(&self) -> u64 {
        self.inner.selected_sessions
    }

    #[getter]
    fn messages_in_selected_sessions(&self) -> u64 {
        self.inner.messages_in_selected_sessions
    }

    #[getter]
    fn analyzed_user_messages(&self) -> u64 {
        self.inner.analyzed_user_messages
    }

    #[getter]
    fn has_more(&self) -> bool {
        self.inner.has_more
    }

    #[getter]
    fn last_selected_session_id(&self) -> Option<String> {
        self.inner.last_selected_session_id.clone()
    }

    #[getter]
    fn max_selected_session_updated_at(&self) -> Option<String> {
        self.inner.max_selected_session_updated_at.clone()
    }

    #[getter]
    fn policy_digest(&self) -> &str {
        &self.inner.policy_digest
    }

    #[getter]
    fn corpus_digest(&self) -> &str {
        &self.inner.corpus_digest
    }

    #[getter]
    fn result_digest(&self) -> &str {
        &self.inner.result_digest
    }
}

/// Analysis data paired with the exact same-snapshot selection and digest receipt.
#[pyclass(
    name = "ReceiptedAnalysis",
    module = "ai_session_search._native",
    frozen
)]
struct NativeReceiptedAnalysis {
    inner: Arc<CoreReceiptedAnalysis>,
}

#[pymethods]
impl NativeReceiptedAnalysis {
    #[getter]
    fn result(&self) -> NativeAnalysisResult {
        NativeAnalysisResult::from_receipted(Arc::clone(&self.inner))
    }

    #[getter]
    fn receipt(&self) -> NativeAnalysisReceipt {
        NativeAnalysisReceipt {
            inner: self.inner.receipt.clone(),
        }
    }
}

/// Rendered analysis artifact held in memory before publication.
#[pyclass(
    name = "AnalysisArtifact",
    module = "ai_session_search._native",
    frozen
)]
struct NativeAnalysisArtifact {
    #[pyo3(get)]
    name: String,
    #[pyo3(get)]
    content: String,
    #[pyo3(get)]
    sha256: String,
    #[pyo3(get)]
    bytes: usize,
}

impl From<RustAnalysisArtifact> for NativeAnalysisArtifact {
    fn from(value: RustAnalysisArtifact) -> Self {
        Self {
            name: value.name().to_owned(),
            content: value.content().to_owned(),
            sha256: value.sha256().to_owned(),
            bytes: value.bytes(),
        }
    }
}

/// Name, byte count, and SHA-256 digest of one published artifact.
#[pyclass(
    name = "PublishedAnalysisArtifact",
    module = "ai_session_search._native",
    frozen
)]
struct NativePublishedAnalysisArtifact {
    #[pyo3(get)]
    name: String,
    #[pyo3(get)]
    bytes: u64,
    #[pyo3(get)]
    sha256: String,
}

impl From<RustPublishedAnalysisArtifact> for NativePublishedAnalysisArtifact {
    fn from(value: RustPublishedAnalysisArtifact) -> Self {
        Self {
            name: value.name,
            bytes: value.bytes,
            sha256: value.sha256,
        }
    }
}

/// Receipt for an atomically published immutable analysis bundle.
#[pyclass(
    name = "AnalysisPublicationReceipt",
    module = "ai_session_search._native",
    frozen
)]
struct NativeAnalysisPublicationReceipt {
    #[pyo3(get)]
    destination: PathBuf,
    #[pyo3(get)]
    artifacts: Vec<Py<NativePublishedAnalysisArtifact>>,
}

impl NativeAnalysisPublicationReceipt {
    fn from_receipt(py: Python<'_>, value: RustAnalysisPublicationReceipt) -> PyResult<Self> {
        Ok(Self {
            destination: value.destination,
            artifacts: value
                .artifacts
                .into_iter()
                .map(|artifact| Py::new(py, NativePublishedAnalysisArtifact::from(artifact)))
                .collect::<PyResult<Vec<_>>>()?,
        })
    }
}

#[derive(Clone)]
/// Immutable, no-replace publication plan for JSON and Markdown analysis artifacts.
///
/// Every bundle includes format-independent `analysis-receipt.v1.json` and `manifest.v1.json`
/// control artifacts.
#[pyclass(
    module = "ai_session_search._native",
    name = "AnalysisPublicationPlan",
    frozen,
    from_py_object
)]
struct NativeAnalysisPublicationPlan {
    inner: RustAnalysisPublicationPlan,
}

fn parse_publication_formats(
    formats: Option<Vec<String>>,
) -> PyResult<Vec<RustAnalysisPublicationFormat>> {
    formats
        .unwrap_or_else(|| vec!["json".to_owned(), "markdown".to_owned()])
        .into_iter()
        .map(|format| match format.as_str() {
            "json" => Ok(RustAnalysisPublicationFormat::Json),
            "markdown" => Ok(RustAnalysisPublicationFormat::Markdown),
            _ => Err(PyValueError::new_err(format!(
                "unknown analysis publication format '{format}'; expected json or markdown"
            ))),
        })
        .collect()
}

#[pymethods]
impl NativeAnalysisPublicationPlan {
    #[new]
    #[pyo3(signature = (destination, formats=None))]
    fn new(destination: PathBuf, formats: Option<Vec<String>>) -> PyResult<Self> {
        let formats = parse_publication_formats(formats)?;
        RustAnalysisPublicationPlan::new(destination, formats)
            .map(|inner| Self { inner })
            .map_err(value_error)
    }

    #[getter]
    fn destination(&self) -> PathBuf {
        self.inner.destination().to_path_buf()
    }

    #[getter]
    fn formats(&self) -> Vec<&'static str> {
        self.inner
            .formats()
            .map(|format| match format {
                RustAnalysisPublicationFormat::Json => "json",
                RustAnalysisPublicationFormat::Markdown => "markdown",
            })
            .collect()
    }

    fn render(
        &self,
        py: Python<'_>,
        analysis: PyRef<'_, NativeReceiptedAnalysis>,
    ) -> PyResult<Vec<Py<NativeAnalysisArtifact>>> {
        let plan = self.inner.clone();
        let analysis = Arc::clone(&analysis.inner);
        let artifacts = py.detach(move || plan.render(&analysis).map_err(runtime_error))?;
        artifacts
            .into_iter()
            .map(|artifact| Py::new(py, NativeAnalysisArtifact::from(artifact)))
            .collect()
    }

    fn publish(
        &self,
        py: Python<'_>,
        analysis: PyRef<'_, NativeReceiptedAnalysis>,
    ) -> PyResult<NativeAnalysisPublicationReceipt> {
        let plan = self.inner.clone();
        let analysis = Arc::clone(&analysis.inner);
        let receipt = py.detach(move || plan.publish(&analysis).map_err(runtime_error))?;
        NativeAnalysisPublicationReceipt::from_receipt(py, receipt)
    }
}

/// Graph node containing one analyzed session identity and classifications.
#[pyclass(
    name = "SessionGraphNode",
    module = "ai_session_search._native",
    frozen
)]
struct NativeSessionGraphNode {
    #[pyo3(get)]
    session_id: String,
    #[pyo3(get)]
    provider: String,
    #[pyo3(get)]
    title: Option<String>,
    #[pyo3(get)]
    cwd: Option<String>,
    #[pyo3(get)]
    repo_root: Option<String>,
    #[pyo3(get)]
    created_at: Option<String>,
    #[pyo3(get)]
    updated_at: Option<String>,
    #[pyo3(get)]
    score: i64,
    #[pyo3(get)]
    classifications: Vec<Py<NativeClassificationMatch>>,
}

impl NativeSessionGraphNode {
    fn from_node(py: Python<'_>, value: SessionGraphNode) -> PyResult<Self> {
        Ok(Self {
            session_id: value.session_id,
            provider: value.provider,
            title: value.title,
            cwd: value.cwd,
            repo_root: value.repo_root,
            created_at: value.created_at,
            updated_at: value.updated_at,
            score: value.score,
            classifications: value
                .classifications
                .into_iter()
                .map(|item| Py::new(py, NativeClassificationMatch::from(item)))
                .collect::<PyResult<Vec<_>>>()?,
        })
    }
}

/// One resolved directed relationship between two session IDs.
#[pyclass(
    name = "SessionGraphEdge",
    module = "ai_session_search._native",
    frozen
)]
struct NativeSessionGraphEdge {
    #[pyo3(get)]
    source_session_id: String,
    #[pyo3(get)]
    target_session_id: String,
    #[pyo3(get)]
    kind: String,
    #[pyo3(get)]
    rule_id: String,
}

impl From<SessionGraphEdge> for NativeSessionGraphEdge {
    fn from(value: SessionGraphEdge) -> Self {
        Self {
            source_session_id: value.source_session_id,
            target_session_id: value.target_session_id,
            kind: relationship_kind_name(value.kind).into(),
            rule_id: value.rule_id,
        }
    }
}

/// Session IDs sharing one classification dimension and label.
#[pyclass(
    name = "SessionGraphGroup",
    module = "ai_session_search._native",
    frozen
)]
struct NativeSessionGraphGroup {
    #[pyo3(get)]
    dimension: String,
    #[pyo3(get)]
    key: String,
    #[pyo3(get)]
    session_ids: Vec<String>,
}

impl From<SessionGraphGroup> for NativeSessionGraphGroup {
    fn from(value: SessionGraphGroup) -> Self {
        Self {
            dimension: value.dimension,
            key: value.key,
            session_ids: value.session_ids,
        }
    }
}

/// Deterministic nodes, resolved edges, and classification groups for analyzed sessions.
#[pyclass(name = "SessionGraph", module = "ai_session_search._native", frozen)]
struct NativeSessionGraph {
    #[pyo3(get)]
    nodes: BTreeMap<String, Py<NativeSessionGraphNode>>,
    #[pyo3(get)]
    edges: Vec<Py<NativeSessionGraphEdge>>,
    #[pyo3(get)]
    groups: Vec<Py<NativeSessionGraphGroup>>,
}

impl NativeSessionGraph {
    fn from_graph(py: Python<'_>, value: SessionGraph) -> PyResult<Self> {
        Ok(Self {
            nodes: value
                .nodes
                .into_iter()
                .map(|(id, node)| {
                    NativeSessionGraphNode::from_node(py, node)
                        .and_then(|node| Py::new(py, node))
                        .map(|node| (id, node))
                })
                .collect::<PyResult<BTreeMap<_, _>>>()?,
            edges: value
                .edges
                .into_iter()
                .map(|edge| Py::new(py, NativeSessionGraphEdge::from(edge)))
                .collect::<PyResult<Vec<_>>>()?,
            groups: value
                .groups
                .into_iter()
                .map(|group| Py::new(py, NativeSessionGraphGroup::from(group)))
                .collect::<PyResult<Vec<_>>>()?,
        })
    }
}

/// Ranked session search result with score and matched-field preview.
#[pyclass(name = "SearchHit", module = "ai_session_search._native", frozen)]
struct NativeSessionSearchHit {
    #[pyo3(get)]
    session: Py<NativeSessionRecord>,
    #[pyo3(get)]
    score: i64,
    #[pyo3(get)]
    match_source: String,
    #[pyo3(get)]
    match_snippet: String,
}

impl NativeSessionSearchHit {
    fn from_hit(py: Python<'_>, hit: SearchHit) -> PyResult<Self> {
        Ok(Self {
            session: Py::new(py, NativeSessionRecord::from(hit.session))?,
            score: hit.score,
            match_source: hit.match_source,
            match_snippet: hit.match_snippet,
        })
    }
}

/// Bounded message preview with its exact expansion command.
#[pyclass(name = "MessagePreview", module = "ai_session_search._native", frozen)]
struct NativeMessagePreview {
    #[pyo3(get)]
    seq: i64,
    #[pyo3(get)]
    timestamp: Option<String>,
    #[pyo3(get)]
    chars: usize,
    #[pyo3(get)]
    preview: String,
    #[pyo3(get)]
    expand_command: String,
}

impl From<ai_session_search::inspect::MessagePreview> for NativeMessagePreview {
    fn from(preview: ai_session_search::inspect::MessagePreview) -> Self {
        Self {
            seq: preview.seq,
            timestamp: preview.ts,
            chars: preview.chars,
            preview: preview.preview,
            expand_command: preview.expand_command,
        }
    }
}

/// Bounded tool-call or tool-result evidence with an exact expansion command.
#[pyclass(name = "ToolActivity", module = "ai_session_search._native", frozen)]
struct NativeToolActivity {
    #[pyo3(get)]
    seq: i64,
    #[pyo3(get)]
    timestamp: Option<String>,
    #[pyo3(get)]
    tool_name: Option<String>,
    #[pyo3(get)]
    kind: String,
    #[pyo3(get)]
    chars: usize,
    #[pyo3(get)]
    preview: String,
    #[pyo3(get)]
    expand_command: String,
}

impl From<ai_session_search::inspect::ToolActivity> for NativeToolActivity {
    fn from(activity: ai_session_search::inspect::ToolActivity) -> Self {
        Self {
            seq: activity.seq,
            timestamp: activity.ts,
            tool_name: activity.tool_name,
            kind: activity.kind,
            chars: activity.chars,
            preview: activity.preview,
            expand_command: activity.expand_command,
        }
    }
}

/// One normalized URL-like reference extracted from a message.
#[pyclass(name = "MessageRef", module = "ai_session_search._native", frozen)]
struct NativeMessageRef {
    #[pyo3(get)]
    kind: String,
    #[pyo3(get)]
    value: String,
    #[pyo3(get)]
    normalized_value: Option<String>,
    #[pyo3(get)]
    host: Option<String>,
    #[pyo3(get)]
    source_tool: Option<String>,
    #[pyo3(get)]
    source_field: Option<String>,
    #[pyo3(get)]
    confidence: String,
    #[pyo3(get)]
    span_start: usize,
    #[pyo3(get)]
    span_end: usize,
}

impl From<ai_session_search::refs::MessageRef> for NativeMessageRef {
    fn from(reference: ai_session_search::refs::MessageRef) -> Self {
        Self {
            kind: reference.kind,
            value: reference.value,
            normalized_value: reference.normalized_value,
            host: reference.host,
            source_tool: reference.source_tool,
            source_field: reference.source_field,
            confidence: reference.confidence,
            span_start: reference.span_start,
            span_end: reference.span_end,
        }
    }
}

/// Message preview and normalized references used as session evidence.
#[pyclass(name = "RefEvidence", module = "ai_session_search._native", frozen)]
struct NativeRefEvidence {
    #[pyo3(get)]
    seq: i64,
    #[pyo3(get)]
    role: String,
    #[pyo3(get)]
    tool_name: Option<String>,
    #[pyo3(get)]
    ref_summary: String,
    #[pyo3(get)]
    refs: Vec<Py<NativeMessageRef>>,
    #[pyo3(get)]
    preview: String,
    #[pyo3(get)]
    expand_command: String,
}

impl NativeRefEvidence {
    fn from_evidence(
        py: Python<'_>,
        evidence: ai_session_search::inspect::RefEvidence,
    ) -> PyResult<Self> {
        Ok(Self {
            seq: evidence.seq,
            role: evidence.role,
            tool_name: evidence.tool_name,
            ref_summary: evidence.ref_summary,
            refs: evidence
                .refs
                .into_iter()
                .map(|reference| Py::new(py, NativeMessageRef::from(reference)))
                .collect::<PyResult<Vec<_>>>()?,
            preview: evidence.preview,
            expand_command: evidence.expand_command,
        })
    }
}

/// Aggregate edit count and expansion command for one changed file.
#[pyclass(
    name = "ChangedFileEvidence",
    module = "ai_session_search._native",
    frozen
)]
struct NativeChangedFileEvidence {
    #[pyo3(get)]
    file_path: String,
    #[pyo3(get)]
    provider: String,
    #[pyo3(get)]
    edits: i64,
    #[pyo3(get)]
    follow_up_command: String,
}

impl From<ai_session_search::inspect::ChangedFileEvidence> for NativeChangedFileEvidence {
    fn from(evidence: ai_session_search::inspect::ChangedFileEvidence) -> Self {
        Self {
            file_path: evidence.file_path,
            provider: evidence.provider,
            edits: evidence.edits,
            follow_up_command: evidence.follow_up_command,
        }
    }
}

/// Observed timestamp span, gaps, and tool/message counts for one session.
#[pyclass(
    name = "SessionTimeProfile",
    module = "ai_session_search._native",
    frozen
)]
struct NativeSessionTimeProfile {
    #[pyo3(get)]
    messages: i64,
    #[pyo3(get)]
    timestamped_messages: i64,
    #[pyo3(get)]
    undated_messages: i64,
    #[pyo3(get)]
    first_timestamp: Option<String>,
    #[pyo3(get)]
    last_timestamp: Option<String>,
    #[pyo3(get)]
    observed_span_seconds: Option<i64>,
    #[pyo3(get)]
    max_message_gap_seconds: Option<i64>,
    #[pyo3(get)]
    tool_calls: i64,
    #[pyo3(get)]
    tool_results: i64,
}

impl From<ai_session_search::models::SessionTimeProfile> for NativeSessionTimeProfile {
    fn from(profile: ai_session_search::models::SessionTimeProfile) -> Self {
        Self {
            messages: profile.messages,
            timestamped_messages: profile.timestamped_messages,
            undated_messages: profile.undated_messages,
            first_timestamp: profile.first_timestamp.map(|value| value.to_rfc3339()),
            last_timestamp: profile.last_timestamp.map(|value| value.to_rfc3339()),
            observed_span_seconds: profile.observed_span_seconds,
            max_message_gap_seconds: profile.max_message_gap_seconds,
            tool_calls: profile.tool_calls,
            tool_results: profile.tool_results,
        }
    }
}

/// Compact purpose, activity, reference, file, and optional timing evidence for one session.
#[pyclass(
    name = "SessionInspection",
    module = "ai_session_search._native",
    frozen
)]
struct NativeSessionInspection {
    #[pyo3(get)]
    session: Py<NativeSessionRecord>,
    #[pyo3(get)]
    user_intent: Vec<Py<NativeMessagePreview>>,
    #[pyo3(get)]
    tool_activity: Vec<Py<NativeToolActivity>>,
    #[pyo3(get)]
    refs: Vec<Py<NativeRefEvidence>>,
    #[pyo3(get)]
    changed_files: Vec<Py<NativeChangedFileEvidence>>,
    #[pyo3(get)]
    truncated_evidence: Vec<String>,
    #[pyo3(get)]
    time_profile: Option<Py<NativeSessionTimeProfile>>,
    #[pyo3(get)]
    next_commands: Vec<String>,
}

impl NativeSessionInspection {
    fn from_inspection(
        py: Python<'_>,
        inspection: ai_session_search::inspect::SessionInspection,
    ) -> PyResult<Self> {
        Ok(Self {
            session: Py::new(py, NativeSessionRecord::from(inspection.session))?,
            user_intent: inspection
                .user_intent
                .into_iter()
                .map(|preview| Py::new(py, NativeMessagePreview::from(preview)))
                .collect::<PyResult<Vec<_>>>()?,
            tool_activity: inspection
                .tool_activity
                .into_iter()
                .map(|activity| Py::new(py, NativeToolActivity::from(activity)))
                .collect::<PyResult<Vec<_>>>()?,
            refs: inspection
                .refs
                .into_iter()
                .map(|evidence| {
                    NativeRefEvidence::from_evidence(py, evidence)
                        .and_then(|value| Py::new(py, value))
                })
                .collect::<PyResult<Vec<_>>>()?,
            changed_files: inspection
                .changed_files
                .into_iter()
                .map(|evidence| Py::new(py, NativeChangedFileEvidence::from(evidence)))
                .collect::<PyResult<Vec<_>>>()?,
            truncated_evidence: inspection
                .truncated_evidence
                .into_iter()
                .map(|section| section.as_str().to_string())
                .collect(),
            time_profile: inspection
                .time_profile
                .map(|profile| Py::new(py, NativeSessionTimeProfile::from(profile)))
                .transpose()?,
            next_commands: inspection.next_commands,
        })
    }
}

/// Aggregate edit and session counts for one indexed file path.
#[pyclass(name = "FileEditSummary", module = "ai_session_search._native", frozen)]
struct NativeFileEditSummary {
    #[pyo3(get)]
    file_path: String,
    #[pyo3(get)]
    file_name: String,
    #[pyo3(get)]
    edits: i64,
    #[pyo3(get)]
    sessions: i64,
    #[pyo3(get)]
    last_edited: Option<String>,
}

impl From<FileEditSummary> for NativeFileEditSummary {
    fn from(summary: FileEditSummary) -> Self {
        Self {
            file_path: summary.file_path,
            file_name: summary.file_name,
            edits: summary.edits,
            sessions: summary.sessions,
            last_edited: summary.last_edited.map(|value| value.to_rfc3339()),
        }
    }
}

/// One causally ordered historical file version reconstructed from an edit.
#[pyclass(name = "FileVersion", module = "ai_session_search._native", frozen)]
struct NativeFileVersion {
    #[pyo3(get)]
    session_id: String,
    #[pyo3(get)]
    provider: String,
    #[pyo3(get)]
    version: i64,
    #[pyo3(get)]
    tool: String,
    #[pyo3(get)]
    timestamp: Option<String>,
    #[pyo3(get)]
    lines: i64,
    #[pyo3(get)]
    file_path: String,
}

impl From<FileVersion> for NativeFileVersion {
    fn from(version: FileVersion) -> Self {
        Self {
            session_id: version.session_id,
            provider: version.provider.as_str().to_string(),
            version: version.version,
            tool: version.tool,
            timestamp: version.ts.map(|value| value.to_rfc3339()),
            lines: version.lines,
            file_path: version.file_path,
        }
    }
}

/// One session-to-file edit relationship from indexed tool activity.
#[pyclass(name = "FileCrossRef", module = "ai_session_search._native", frozen)]
struct NativeFileCrossRef {
    #[pyo3(get)]
    file_path: String,
    #[pyo3(get)]
    session_id: String,
    #[pyo3(get)]
    provider: String,
    #[pyo3(get)]
    edits: i64,
}

impl From<FileCrossRef> for NativeFileCrossRef {
    fn from(reference: FileCrossRef) -> Self {
        Self {
            file_path: reference.file_path,
            session_id: reference.session_id,
            provider: reference.provider.as_str().to_string(),
            edits: reference.edits,
        }
    }
}

#[derive(Clone)]
/// One reconstructed historical file with provenance and complete content.
#[pyclass(
    name = "ReconstructedFile",
    module = "ai_session_search._native",
    frozen,
    from_py_object
)]
struct NativeReconstructedFile {
    #[pyo3(get)]
    session_id: String,
    provider: Provider,
    #[pyo3(get)]
    version: usize,
    #[pyo3(get)]
    file_path: String,
    #[pyo3(get)]
    content: String,
}

#[pymethods]
impl NativeReconstructedFile {
    #[getter]
    fn provider(&self) -> String {
        self.provider.as_str().to_string()
    }

    #[pyo3(signature = (*, output_dir=None))]
    fn restore(&self, py: Python<'_>, output_dir: Option<PathBuf>) -> PyResult<PathBuf> {
        let reconstructed = self.clone().into_core();
        py.detach(move || {
            ai_session_search::files::restore_reconstructed(&reconstructed, output_dir.as_deref())
                .map_err(runtime_error)
        })
    }
}

impl NativeReconstructedFile {
    fn into_core(self) -> ai_session_search::files::ReconstructedFile {
        ai_session_search::files::ReconstructedFile {
            session_id: self.session_id,
            provider: self.provider,
            version: self.version,
            file_path: self.file_path,
            content: self.content,
        }
    }
}

impl From<ai_session_search::files::ReconstructedFile> for NativeReconstructedFile {
    fn from(file: ai_session_search::files::ReconstructedFile) -> Self {
        Self {
            session_id: file.session_id,
            provider: file.provider,
            version: file.version,
            file_path: file.file_path,
            content: file.content,
        }
    }
}

/// Single-pass iterator over causally reconstructable file versions.
#[pyclass(
    name = "ReconstructedFileVersions",
    module = "ai_session_search._native"
)]
struct NativeReconstructedFileVersions {
    inner: ai_session_search::files::ReconstructedFileVersions,
}

/// Receipt for an atomically published directory of recovered file versions.
#[pyclass(
    name = "RecoveryPublicationReceipt",
    module = "ai_session_search._native",
    frozen
)]
struct NativeRecoveryPublicationReceipt {
    #[pyo3(get)]
    destination: PathBuf,
    #[pyo3(get)]
    files: Vec<PathBuf>,
}

impl From<ai_session_search::files::RecoveryPublicationReceipt>
    for NativeRecoveryPublicationReceipt
{
    fn from(receipt: ai_session_search::files::RecoveryPublicationReceipt) -> Self {
        Self {
            destination: receipt.destination,
            files: receipt.files,
        }
    }
}

#[pymethods]
impl NativeReconstructedFileVersions {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self) -> Option<NativeReconstructedFile> {
        self.inner.next().map(NativeReconstructedFile::from)
    }
}

/// Complete rendered session export in the requested format.
#[pyclass(name = "ExportDocument", module = "ai_session_search._native", frozen)]
struct NativeExportDocument {
    #[pyo3(get)]
    format: String,
    #[pyo3(get)]
    content: String,
}

/// Receipt for an atomically published session export bundle.
#[pyclass(
    name = "ExportPublicationReceipt",
    module = "ai_session_search._native",
    frozen
)]
struct NativeExportPublicationReceipt {
    #[pyo3(get)]
    destination: PathBuf,
    #[pyo3(get)]
    format: String,
    #[pyo3(get)]
    sessions: usize,
    #[pyo3(get)]
    files: Vec<PathBuf>,
}

impl From<ai_session_search::export::ExportPublicationReceipt> for NativeExportPublicationReceipt {
    fn from(receipt: ai_session_search::export::ExportPublicationReceipt) -> Self {
        Self {
            destination: receipt.destination,
            format: receipt.format,
            sessions: receipt.sessions,
            files: receipt.files,
        }
    }
}

impl From<ai_session_search::export::ExportDocument> for NativeExportDocument {
    fn from(document: ai_session_search::export::ExportDocument) -> Self {
        Self {
            format: document.format().as_str().to_string(),
            content: document.into_content(),
        }
    }
}

/// Enabled roots and discovered session-file count for one provider.
#[pyclass(
    name = "ProviderSourceStatus",
    module = "ai_session_search._native",
    frozen
)]
struct NativeProviderSourceStatus {
    #[pyo3(get)]
    provider: String,
    #[pyo3(get)]
    enabled: bool,
    #[pyo3(get)]
    roots: Vec<String>,
    #[pyo3(get)]
    discovered_files: usize,
    #[pyo3(get)]
    warnings: Vec<NativeProviderDiscoveryWarning>,
}

/// One message classified by a named capability rule category.
#[pyclass(
    name = "MessageClassificationMatch",
    module = "ai_session_search._native",
    frozen
)]
struct NativeMessageClassificationMatch {
    #[pyo3(get)]
    session_id: String,
    #[pyo3(get)]
    message_seq: i64,
    #[pyo3(get)]
    provider: String,
    #[pyo3(get)]
    timestamp: Option<String>,
    /// Which compiled classification policy produced this match. Only the name: version and
    /// digest are reported once per run on `MessageClassificationReport.policies` rather than
    /// repeated on every row.
    #[pyo3(get)]
    policy_name: String,
    #[pyo3(get)]
    category: String,
    #[pyo3(get)]
    matched_text: String,
    #[pyo3(get)]
    match_start_char: usize,
    #[pyo3(get)]
    match_end_char_exclusive: usize,
    #[pyo3(get)]
    content: String,
}

impl From<ai_session_search::models::MessageClassificationMatch>
    for NativeMessageClassificationMatch
{
    fn from(hit: ai_session_search::models::MessageClassificationMatch) -> Self {
        Self {
            session_id: hit.session_id,
            message_seq: hit.message_seq,
            provider: hit.provider.as_str().to_string(),
            timestamp: hit.ts.map(|value| value.to_rfc3339()),
            policy_name: hit.policy_name,
            category: hit.category,
            matched_text: hit.matched_text,
            match_start_char: hit.match_start_char,
            match_end_char_exclusive: hit.match_end_char_exclusive,
            content: hit.content,
        }
    }
}

/// Name, version, and digest of one message-classification capability evaluated for a report.
#[pyclass(
    name = "CapabilityReceipt",
    module = "ai_session_search._native",
    frozen
)]
struct NativeCapabilityReceipt {
    #[pyo3(get)]
    name: String,
    #[pyo3(get)]
    version: String,
    /// Digest of the exact resolved policy bytes. A name and version alone are not reproducible:
    /// a policy file can be edited without a version bump, and then two runs reporting the same
    /// version would disagree with no way to tell which rules produced which.
    #[pyo3(get)]
    sha256: String,
}

impl From<ai_session_search::CapabilityReceipt> for NativeCapabilityReceipt {
    fn from(receipt: ai_session_search::CapabilityReceipt) -> Self {
        Self {
            name: receipt.name,
            version: receipt.version,
            sha256: receipt.sha256,
        }
    }
}

/// Classified message matches together with the capabilities evaluated to produce them.
#[pyclass(
    name = "MessageClassificationReport",
    module = "ai_session_search._native",
    frozen
)]
struct NativeMessageClassificationReport {
    /// Every evaluated message-classification capability, INCLUDING any that matched nothing.
    /// Carried so an empty `matches` list is unambiguous: "these rules ran and found nothing" and
    /// "no rules ran" are different answers.
    #[pyo3(get)]
    policies: Vec<Py<NativeCapabilityReceipt>>,
    /// Matches newest first, after `offset` is skipped and `limit` taken.
    ///
    /// `Py<..>` rather than owned values, as `MessageSearchResponse.hits` already is: a getter
    /// over owned rows clones the entire list on every attribute access, which turns
    /// `len(report.matches)` into a full copy.
    #[pyo3(get)]
    matches: Vec<Py<NativeMessageClassificationMatch>>,
}

impl NativeMessageClassificationReport {
    fn from_report(
        py: Python<'_>,
        report: ai_session_search::MessageClassificationReport,
    ) -> PyResult<Self> {
        Ok(Self {
            policies: report
                .policies
                .into_iter()
                .map(|receipt| Py::new(py, NativeCapabilityReceipt::from(receipt)))
                .collect::<PyResult<Vec<_>>>()?,
            matches: report
                .matches
                .into_iter()
                .map(|hit| Py::new(py, NativeMessageClassificationMatch::from(hit)))
                .collect::<PyResult<Vec<_>>>()?,
        })
    }
}

/// Where the selected skill package was resolved.
#[pyclass(
    name = "SelectedSkillLocation",
    module = "ai_session_search._native",
    frozen
)]
struct NativeSelectedSkillLocation {
    #[pyo3(get)]
    kind: String,
    #[pyo3(get)]
    canonical_skill_md: Option<PathBuf>,
}

impl From<ai_session_search::SelectedSkillLocation> for NativeSelectedSkillLocation {
    fn from(location: ai_session_search::SelectedSkillLocation) -> Self {
        match location {
            ai_session_search::SelectedSkillLocation::Embedded => Self {
                kind: "embedded".to_string(),
                canonical_skill_md: None,
            },
            ai_session_search::SelectedSkillLocation::Path { canonical_skill_md } => Self {
                kind: "path".to_string(),
                canonical_skill_md: Some(canonical_skill_md),
            },
        }
    }
}

/// Where the deterministic capability bytes executed by a skill came from.
#[pyclass(
    name = "CapabilityExecutionSource",
    module = "ai_session_search._native",
    frozen
)]
struct NativeCapabilityExecutionSource {
    #[pyo3(get)]
    kind: String,
    #[pyo3(get)]
    canonical_capability_toml: Option<PathBuf>,
}

impl From<ai_session_search::CapabilityExecutionSource> for NativeCapabilityExecutionSource {
    fn from(source: ai_session_search::CapabilityExecutionSource) -> Self {
        match source {
            ai_session_search::CapabilityExecutionSource::Embedded => Self {
                kind: "embedded".to_string(),
                canonical_capability_toml: None,
            },
            ai_session_search::CapabilityExecutionSource::Path {
                canonical_capability_toml,
            } => Self {
                kind: "path".to_string(),
                canonical_capability_toml: Some(canonical_capability_toml),
            },
            ai_session_search::CapabilityExecutionSource::Inline => Self {
                kind: "inline".to_string(),
                canonical_capability_toml: None,
            },
        }
    }
}

/// Provenance for the package and capability selected by one skill run.
#[pyclass(
    name = "ResolvedSkillReceipt",
    module = "ai_session_search._native",
    frozen
)]
struct NativeResolvedSkillReceipt {
    #[pyo3(get)]
    name: String,
    #[pyo3(get)]
    package_version: Option<String>,
    #[pyo3(get)]
    selected_location: Py<NativeSelectedSkillLocation>,
    #[pyo3(get)]
    execution_source: Py<NativeCapabilityExecutionSource>,
}

/// Typed message-classification output nested inside a skill-run report.
#[pyclass(
    name = "MessageClassificationResult",
    module = "ai_session_search._native",
    frozen
)]
struct NativeMessageClassificationResult {
    #[pyo3(get)]
    receipt: Py<NativeCapabilityReceipt>,
    #[pyo3(get)]
    report: Py<NativeMessageClassificationReport>,
}

/// Result and provenance from one deterministic skill invocation.
#[pyclass(name = "SkillRunReport", module = "ai_session_search._native", frozen)]
struct NativeSkillRunReport {
    #[pyo3(get)]
    requested_selector: Py<NativeSkillSelector>,
    #[pyo3(get)]
    resolved_skill: Py<NativeResolvedSkillReceipt>,
    #[pyo3(get)]
    output: Py<NativeMessageClassificationResult>,
}

impl NativeSkillRunReport {
    fn from_report(py: Python<'_>, report: ai_session_search::SkillRunReport) -> PyResult<Self> {
        let resolved = report.resolved_skill;
        let resolved_skill = NativeResolvedSkillReceipt {
            name: resolved.name.as_str().to_string(),
            package_version: resolved.package_version,
            selected_location: Py::new(
                py,
                NativeSelectedSkillLocation::from(resolved.selected_location),
            )?,
            execution_source: Py::new(
                py,
                NativeCapabilityExecutionSource::from(resolved.execution_source),
            )?,
        };
        let ai_session_search::SkillCapabilityOutput::MessageClassification(output) = report.output;
        let result = NativeMessageClassificationResult {
            receipt: Py::new(py, NativeCapabilityReceipt::from(output.receipt))?,
            report: Py::new(
                py,
                NativeMessageClassificationReport::from_report(py, output.report)?,
            )?,
        };
        Ok(Self {
            requested_selector: Py::new(
                py,
                NativeSkillSelector::from_core(report.requested_selector),
            )?,
            resolved_skill: Py::new(py, resolved_skill)?,
            output: Py::new(py, result)?,
        })
    }
}

/// Slash-command usage count with distinct session and project counts.
#[pyclass(name = "PlanningCount", module = "ai_session_search._native", frozen)]
struct NativePlanningCount {
    #[pyo3(get)]
    command: String,
    #[pyo3(get)]
    count: i64,
    #[pyo3(get)]
    unique_sessions: i64,
    #[pyo3(get)]
    unique_projects: i64,
}

impl From<ai_session_search::models::PlanningCount> for NativePlanningCount {
    fn from(count: ai_session_search::models::PlanningCount) -> Self {
        Self {
            command: count.command,
            count: count.count,
            unique_sessions: count.unique_sessions,
            unique_projects: count.unique_projects,
        }
    }
}

/// Exact indexed message count for one normalized role.
#[pyclass(name = "RoleStat", module = "ai_session_search._native", frozen)]
struct NativeRoleStatistic {
    #[pyo3(get)]
    role: String,
    #[pyo3(get)]
    count: i64,
}

impl From<ai_session_search::analytics::RoleStat> for NativeRoleStatistic {
    fn from(statistic: ai_session_search::analytics::RoleStat) -> Self {
        Self {
            role: statistic.role,
            count: statistic.count,
        }
    }
}

impl From<ai_session_search::source::ProviderSourceStatus> for NativeProviderSourceStatus {
    fn from(status: ai_session_search::source::ProviderSourceStatus) -> Self {
        Self {
            provider: status.provider.as_str().to_string(),
            enabled: status.enabled,
            roots: status.roots,
            discovered_files: status.discovered_files,
            warnings: status.warnings.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Clone, Default)]
/// Date bounds parsed by the same Rust grammar used by the CLI and MCP server.
#[pyclass(module = "ai_session_search._native", frozen, from_py_object)]
struct DateRange {
    dates: ai_session_search::dates::DateRange,
}

#[derive(Clone)]
/// Concrete inclusive UTC bounds produced by resolving a DateRange.
#[pyclass(module = "ai_session_search._native", frozen, skip_from_py_object)]
struct ResolvedDateRange {
    #[pyo3(get)]
    since: Option<String>,
    #[pyo3(get)]
    until: Option<String>,
}

impl From<ai_session_search::dates::Bounds> for ResolvedDateRange {
    fn from((since, until): ai_session_search::dates::Bounds) -> Self {
        Self {
            since: since.map(|value| value.to_rfc3339()),
            until: until.map(|value| value.to_rfc3339()),
        }
    }
}

#[pymethods]
impl DateRange {
    #[new]
    #[pyo3(signature = (*, since=None, until=None, when=None))]
    fn new(since: Option<String>, until: Option<String>, when: Option<String>) -> PyResult<Self> {
        if when.is_some() && (since.is_some() || until.is_some()) {
            return Err(PyValueError::new_err(
                "when is mutually exclusive with since and until",
            ));
        }
        Ok(Self {
            dates: ai_session_search::dates::DateRange { since, until, when },
        })
    }

    #[getter]
    fn since(&self) -> Option<String> {
        self.dates.since.clone()
    }

    #[getter]
    fn until(&self) -> Option<String> {
        self.dates.until.clone()
    }

    #[getter]
    fn when(&self) -> Option<String> {
        self.dates.when.clone()
    }

    /// Resolve all expressions through the canonical Rust date parser.
    ///
    /// `reference_time` is optional in normal use and exists so callers can make
    /// relative expressions deterministic in tests and reproducible workflows.
    #[pyo3(signature = (*, reference_time=None))]
    fn resolve_bounds(&self, reference_time: Option<&str>) -> PyResult<ResolvedDateRange> {
        let bounds = match reference_time {
            Some(value) => {
                let now = chrono::DateTime::parse_from_rfc3339(value)
                    .map(|value| value.with_timezone(&chrono::Utc))
                    .map_err(|_| {
                        PyValueError::new_err(format!(
                            "reference_time must be an RFC 3339 timestamp, got {value:?}"
                        ))
                    })?;
                self.dates.resolve(now)
            }
            None => self.dates.resolve_now(),
        }
        .map_err(value_error)?;
        Ok(bounds.into())
    }
}

impl DateRange {
    fn resolve(&self) -> PyResult<ai_session_search::dates::Bounds> {
        self.dates.resolve_now().map_err(value_error)
    }
}

#[derive(Clone, Default)]
/// Reusable path-prefix and exact-session exclusions applied before result limits.
#[pyclass(module = "ai_session_search._native", frozen, from_py_object)]
struct QueryExclusions {
    #[pyo3(get)]
    path_prefixes: Vec<String>,
    #[pyo3(get)]
    session_ids: Vec<String>,
}

#[pymethods]
impl QueryExclusions {
    #[new]
    #[pyo3(signature = (*, path_prefixes=None, session_ids=None))]
    fn new(path_prefixes: Option<Vec<String>>, session_ids: Option<Vec<String>>) -> Self {
        Self {
            path_prefixes: path_prefixes.unwrap_or_default(),
            session_ids: session_ids.unwrap_or_default(),
        }
    }
}

impl QueryExclusions {
    fn into_filters(self) -> (Vec<String>, Vec<String>) {
        (
            self.path_prefixes
                .iter()
                .map(|path| ai_session_search::util::normalize_path_prefix(path))
                .collect(),
            self.session_ids,
        )
    }
}

#[derive(Clone)]
/// Session list/search filters; limit=0 explicitly selects every matching session.
/// `current_repo` overrides repository-aware ranking; omission honors `prefer_current_repo` and
/// derives the repository from the process working directory when search runs.
#[pyclass(module = "ai_session_search._native", frozen, from_py_object)]
struct SessionQuery {
    provider: Option<Provider>,
    path_prefix: Option<String>,
    exclusions: QueryExclusions,
    session_kinds: Option<Vec<SessionKind>>,
    #[pyo3(get)]
    parent_session_id: Option<String>,
    #[pyo3(get)]
    current_repo: Option<String>,
    dates: DateRange,
    #[pyo3(get)]
    limit: usize,
}

#[pymethods]
impl SessionQuery {
    #[new]
    // Independent session filters stay flat and keyword-only, matching MessageSearchRequest;
    // grouping them would restore the one-use wrapper types this API intentionally removed.
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (*, provider=None, path_prefix=None, exclusions=None, session_kinds=None, parent_session_id=None, current_repo=None, dates=None, limit=50))]
    fn new(
        provider: Option<String>,
        path_prefix: Option<String>,
        exclusions: Option<QueryExclusions>,
        session_kinds: Option<Vec<String>>,
        parent_session_id: Option<String>,
        current_repo: Option<String>,
        dates: Option<DateRange>,
        limit: i64,
    ) -> PyResult<Self> {
        Ok(Self {
            provider: parse_provider(provider)?,
            path_prefix,
            exclusions: exclusions.unwrap_or_default(),
            session_kinds: parse_session_kinds(session_kinds)?,
            parent_session_id,
            current_repo,
            dates: dates.unwrap_or_default(),
            limit: paging_argument(PagingArgument::Limit, limit)?,
        })
    }

    #[getter]
    fn provider(&self) -> Option<String> {
        self.provider.map(|provider| provider.as_str().to_string())
    }

    #[getter]
    fn session_kinds(&self) -> Option<Vec<String>> {
        self.session_kinds.as_ref().map(|kinds| {
            kinds
                .iter()
                .copied()
                .map(|kind| kind.as_str().to_string())
                .collect()
        })
    }

    #[getter]
    fn path_prefix(&self) -> Option<String> {
        self.path_prefix.clone()
    }

    #[getter]
    fn exclusions(&self) -> QueryExclusions {
        self.exclusions.clone()
    }

    #[getter]
    fn dates(&self) -> DateRange {
        self.dates.clone()
    }
}

impl Default for SessionQuery {
    fn default() -> Self {
        Self {
            provider: None,
            path_prefix: None,
            exclusions: QueryExclusions::default(),
            session_kinds: None,
            parent_session_id: None,
            current_repo: None,
            dates: DateRange::default(),
            limit: 50,
        }
    }
}

impl SessionQuery {
    fn into_filters(self) -> PyResult<(SearchFilters, Option<String>)> {
        let (since, until) = self.dates.resolve()?;
        let (exclude_path_prefixes, exclude_session_ids) = self.exclusions.into_filters();
        let filters = SearchFilters {
            provider: self.provider,
            path_prefix: self
                .path_prefix
                .as_deref()
                .map(ai_session_search::util::normalize_path_prefix),
            exclude_path_prefixes,
            exclude_session_ids,
            session_kinds: self.session_kinds,
            parent_session_id: self.parent_session_id,
            since,
            until,
            limit: self.limit,
            warnings_only: false,
        };
        filters.validate().map_err(value_error)?;
        Ok((filters, self.current_repo))
    }
}

#[derive(Clone, Default)]
/// Scope and explicit population strategy for longitudinal session analysis.
///
/// Omitted `first_canonical_sessions` analyzes every eligible session. A positive value selects
/// the first N eligible sessions in canonical session-ID order; it is not a recency sample,
/// representative sample, or message limit.
#[pyclass(
    name = "AnalysisRequest",
    module = "ai_session_search._native",
    frozen,
    from_py_object
)]
struct NativeAnalysisRequest {
    provider: Option<Provider>,
    path_prefix: Option<String>,
    exclusions: QueryExclusions,
    session_kinds: Option<Vec<SessionKind>>,
    #[pyo3(get)]
    parent_session_id: Option<String>,
    dates: DateRange,
    #[pyo3(get)]
    first_canonical_sessions: Option<usize>,
}

#[pymethods]
impl NativeAnalysisRequest {
    #[new]
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (*, provider=None, path_prefix=None, exclusions=None, session_kinds=None, parent_session_id=None, dates=None, first_canonical_sessions=None))]
    fn new(
        provider: Option<String>,
        path_prefix: Option<String>,
        exclusions: Option<QueryExclusions>,
        session_kinds: Option<Vec<String>>,
        parent_session_id: Option<String>,
        dates: Option<DateRange>,
        first_canonical_sessions: Option<i64>,
    ) -> PyResult<Self> {
        let first_canonical_sessions = first_canonical_sessions
            .map(|value| {
                usize::try_from(value)
                    .ok()
                    .and_then(NonZeroUsize::new)
                    .map(NonZeroUsize::get)
                    .ok_or_else(|| {
                        PyValueError::new_err(format!(
                            "first_canonical_sessions must be greater than zero; omit it to \
                             analyze every eligible session; got {value}"
                        ))
                    })
            })
            .transpose()?;
        Ok(Self {
            provider: parse_provider(provider)?,
            path_prefix,
            exclusions: exclusions.unwrap_or_default(),
            session_kinds: parse_session_kinds(session_kinds)?,
            parent_session_id,
            dates: dates.unwrap_or_default(),
            first_canonical_sessions,
        })
    }

    #[getter]
    fn provider(&self) -> Option<String> {
        self.provider.map(|provider| provider.as_str().to_owned())
    }

    #[getter]
    fn path_prefix(&self) -> Option<String> {
        self.path_prefix.clone()
    }

    #[getter]
    fn exclusions(&self) -> QueryExclusions {
        self.exclusions.clone()
    }

    #[getter]
    fn session_kinds(&self) -> Option<Vec<String>> {
        self.session_kinds
            .as_ref()
            .map(|kinds| kinds.iter().map(|kind| kind.as_str().to_owned()).collect())
    }

    #[getter]
    fn dates(&self) -> DateRange {
        self.dates.clone()
    }
}

impl NativeAnalysisRequest {
    fn into_core(self) -> PyResult<CoreAnalysisRequest> {
        let selection = self.first_canonical_sessions.map_or(
            CoreAnalysisSessionSelection::AllEligible,
            |max_sessions| CoreAnalysisSessionSelection::FirstCanonicalSessions {
                max_sessions: NonZeroUsize::new(max_sessions)
                    .expect("Python constructor validates a positive selection size"),
            },
        );
        let (scope, _) = SessionQuery {
            provider: self.provider,
            path_prefix: self.path_prefix,
            exclusions: self.exclusions,
            session_kinds: self.session_kinds,
            parent_session_id: self.parent_session_id,
            current_repo: None,
            dates: self.dates,
            limit: 0,
        }
        .into_filters()?;
        CoreAnalysisRequest::new(scope, selection).map_err(value_error)
    }
}

#[derive(Clone, Default)]
/// Shared provider, session, path, exclusion, and date scope for typed queries.
#[pyclass(module = "ai_session_search._native", frozen, from_py_object)]
struct QueryScope {
    provider: Option<Provider>,
    session_id: Option<String>,
    path_prefix: Option<String>,
    exclusions: QueryExclusions,
    dates: DateRange,
}

#[pymethods]
impl QueryScope {
    #[new]
    #[pyo3(signature = (*, provider=None, session_id=None, path_prefix=None, exclusions=None, dates=None))]
    fn new(
        provider: Option<String>,
        session_id: Option<String>,
        path_prefix: Option<String>,
        exclusions: Option<QueryExclusions>,
        dates: Option<DateRange>,
    ) -> PyResult<Self> {
        Ok(Self {
            provider: parse_provider(provider)?,
            session_id,
            path_prefix,
            exclusions: exclusions.unwrap_or_default(),
            dates: dates.unwrap_or_default(),
        })
    }

    #[getter]
    fn provider(&self) -> Option<String> {
        self.provider.map(|provider| provider.as_str().to_string())
    }

    #[getter]
    fn session_id(&self) -> Option<String> {
        self.session_id.clone()
    }

    #[getter]
    fn path_prefix(&self) -> Option<String> {
        self.path_prefix.clone()
    }

    #[getter]
    fn exclusions(&self) -> QueryExclusions {
        self.exclusions.clone()
    }

    #[getter]
    fn dates(&self) -> DateRange {
        self.dates.clone()
    }
}

struct ResolvedQueryScope {
    provider: Option<Provider>,
    session_id: Option<String>,
    path_prefix: Option<String>,
    exclude_path_prefixes: Vec<String>,
    exclude_session_ids: Vec<String>,
    bounds: ai_session_search::dates::Bounds,
}

impl QueryScope {
    fn resolve(self, app: &CoreSessionSearch) -> PyResult<ResolvedQueryScope> {
        let (since, until) = self.dates.resolve()?;
        let (exclude_path_prefixes, exclude_session_ids) = self.exclusions.into_filters();
        let session_id = self
            .session_id
            .map(|id| {
                app.catalog()
                    .resolve_session(&id)
                    .map(|session| session.id)
                    .map_err(runtime_error)
            })
            .transpose()?;
        Ok(ResolvedQueryScope {
            provider: self.provider,
            session_id,
            path_prefix: self
                .path_prefix
                .as_deref()
                .map(ai_session_search::util::normalize_path_prefix),
            exclude_path_prefixes,
            exclude_session_ids,
            bounds: (since, until),
        })
    }
}

/// Workspace, transcript, and session exclusions for message search.
#[derive(Clone, Default)]
#[pyclass(module = "ai_session_search._native", frozen, from_py_object)]
struct MessageExclusions {
    #[pyo3(get)]
    workspace_path_prefixes: Vec<String>,
    #[pyo3(get)]
    transcript_path_prefixes: Vec<String>,
    #[pyo3(get)]
    session_ids: Vec<String>,
}

#[pymethods]
impl MessageExclusions {
    #[new]
    #[pyo3(signature = (*, workspace_path_prefixes=None, transcript_path_prefixes=None, session_ids=None))]
    fn new(
        workspace_path_prefixes: Option<Vec<String>>,
        transcript_path_prefixes: Option<Vec<String>>,
        session_ids: Option<Vec<String>>,
    ) -> Self {
        Self {
            workspace_path_prefixes: workspace_path_prefixes.unwrap_or_default(),
            transcript_path_prefixes: transcript_path_prefixes.unwrap_or_default(),
            session_ids: session_ids.unwrap_or_default(),
        }
    }
}

/// Message-only scope with distinct workspace and transcript path domains.
#[derive(Clone, Default)]
#[pyclass(module = "ai_session_search._native", frozen, from_py_object)]
struct MessageScope {
    providers: Option<Vec<Provider>>,
    #[pyo3(get)]
    session_id: Option<String>,
    #[pyo3(get)]
    workspace_path_prefix: Option<String>,
    #[pyo3(get)]
    transcript_path_prefix: Option<String>,
    exclusions: MessageExclusions,
    dates: DateRange,
}

#[pymethods]
impl MessageScope {
    #[new]
    #[pyo3(signature = (*, providers=None, session_id=None, workspace_path_prefix=None, transcript_path_prefix=None, exclusions=None, dates=None))]
    fn new(
        providers: Option<Vec<String>>,
        session_id: Option<String>,
        workspace_path_prefix: Option<String>,
        transcript_path_prefix: Option<String>,
        exclusions: Option<MessageExclusions>,
        dates: Option<DateRange>,
    ) -> PyResult<Self> {
        Ok(Self {
            providers: parse_provider_set(providers)?,
            session_id,
            workspace_path_prefix,
            transcript_path_prefix,
            exclusions: exclusions.unwrap_or_default(),
            dates: dates.unwrap_or_default(),
        })
    }

    #[getter]
    fn providers(&self) -> Option<Vec<String>> {
        self.providers.as_ref().map(|providers| {
            providers
                .iter()
                .map(|provider| provider.as_str().to_string())
                .collect()
        })
    }

    #[getter]
    fn exclusions(&self) -> MessageExclusions {
        self.exclusions.clone()
    }

    #[getter]
    fn dates(&self) -> DateRange {
        self.dates.clone()
    }
}

/// Typed message filters; all fields compose and are applied before limit and offset.
///
/// The query searches only `field`: `content`, `tool_name`, or the canonical tool argument at
/// `argument_path`. `tool_name_contains` is an additional case-insensitive substring filter on
/// canonical `tool_name`, independent of `field`. When no configured operation/purpose default
/// applies, omitting `limit` returns all literal, regex, or no-text matches in Python. Fuzzy search
/// requires a positive limit. `all_results=True` makes the complete-corpus request explicit and
/// conflicts with `limit`.
#[derive(Clone)]
#[pyclass(module = "ai_session_search._native", frozen, from_py_object)]
struct MessageSearchRequest {
    scope: MessageScope,
    role: Option<Role>,
    kind: Option<MessageKind>,
    kinds: Option<Vec<MessageKind>>,
    field: SearchField,
    argument_path: Option<String>,
    #[pyo3(get)]
    seq_from: Option<i64>,
    #[pyo3(get)]
    seq_to: Option<i64>,
    tool_name_contains: Option<String>,
    #[pyo3(get)]
    include_compaction: bool,
    #[pyo3(get)]
    limit: Option<usize>,
    #[pyo3(get)]
    all_results: bool,
    #[pyo3(get)]
    offset: usize,
    match_window: Option<CoreMatchWindow>,
    #[pyo3(get)]
    context: Option<usize>,
    #[pyo3(get)]
    context_before: Option<usize>,
    #[pyo3(get)]
    context_after: Option<usize>,
    includes: Option<Vec<CoreMessageSearchInclude>>,
    #[pyo3(get)]
    lines_per_message: Option<i64>,
    detail: Option<CoreDetailLevel>,
    field_view: Option<CoreFieldViewBudget>,
    match_view: Option<CoreMatchViewBudget>,
    purpose: Option<String>,
    purpose_version: Option<std::num::NonZeroU32>,
    receipt_level: Option<CoreReceiptLevel>,
}

#[pymethods]
impl MessageSearchRequest {
    #[new]
    // Independent message filters stay flat and keyword-only; grouping them would restore the
    // one-use wrapper types this API intentionally removed.
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (*, scope=None, role=None, kind=None, kinds=None, field="content", argument_path=None, seq_from=None, seq_to=None, tool_name_contains=None, include_compaction=true, limit=None, all_results=false, offset=0, match_window=None, context=None, context_before=None, context_after=None, include=None, lines_per_message=None, detail=None, field_view=None, match_view=None, purpose=None, purpose_version=None, receipt_level=None))]
    fn new(
        scope: Option<MessageScope>,
        role: Option<&str>,
        kind: Option<&str>,
        kinds: Option<Vec<String>>,
        field: &str,
        argument_path: Option<String>,
        seq_from: Option<i64>,
        seq_to: Option<i64>,
        tool_name_contains: Option<String>,
        include_compaction: bool,
        limit: Option<i64>,
        all_results: bool,
        offset: i64,
        match_window: Option<&str>,
        context: Option<i64>,
        context_before: Option<i64>,
        context_after: Option<i64>,
        include: Option<Vec<String>>,
        lines_per_message: Option<i64>,
        detail: Option<&str>,
        field_view: Option<&Bound<'_, PyDict>>,
        match_view: Option<&Bound<'_, PyDict>>,
        purpose: Option<String>,
        purpose_version: Option<u32>,
        receipt_level: Option<&str>,
    ) -> PyResult<Self> {
        let nonnegative = |name: &'static str, value: Option<i64>| -> PyResult<Option<usize>> {
            value
                .map(|value| {
                    usize::try_from(value).map_err(|_| {
                        PyValueError::new_err(format!("{name} must be an integer 0 or greater"))
                    })
                })
                .transpose()
        };
        if kind.is_some() && kinds.is_some() {
            return Err(PyValueError::new_err(
                "kind and kinds cannot be used together; use kind for one class or kinds for several",
            ));
        }
        if all_results && limit.is_some() {
            return Err(PyValueError::new_err(
                "limit and all_results cannot be used together",
            ));
        }
        if purpose.is_none() && purpose_version.is_some() {
            return Err(PyValueError::new_err("purpose_version requires purpose"));
        }
        let field = field.parse().map_err(PyValueError::new_err)?;
        if argument_path.is_some() && field != SearchField::ToolArgument {
            return Err(PyValueError::new_err(
                "argument_path requires field='tool_argument'",
            ));
        }
        let limit = limit
            .map(|value| {
                usize::try_from(value).map_err(|_| {
                    PyValueError::new_err(format!(
                        "limit must be greater than zero; use all_results=True for every match; got {value}"
                    ))
                })
            })
            .transpose()?;
        if limit == Some(0) {
            return Err(PyValueError::new_err(
                "limit must be greater than zero; use all_results=True for every match",
            ));
        }
        let field_view = python_view_budget_value("field_view", field_view)?
            .map(|value| {
                ai_session_search::message_search::decode_field_view_budget(&value, usize::MAX)
                    .map_err(value_error)
            })
            .transpose()?;
        let match_view = python_view_budget_value("match_view", match_view)?
            .map(|value| {
                ai_session_search::message_search::decode_match_view_budget(&value, usize::MAX)
                    .map_err(value_error)
            })
            .transpose()?;
        Ok(Self {
            scope: scope.unwrap_or_default(),
            role: role
                .map(str::parse)
                .transpose()
                .map_err(PyValueError::new_err)?,
            kind: kind
                .map(str::parse)
                .transpose()
                .map_err(PyValueError::new_err)?,
            kinds: kinds
                .map(|kinds| {
                    kinds
                        .iter()
                        .map(|kind| kind.parse::<MessageKind>())
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()
                .map_err(PyValueError::new_err)?,
            field,
            argument_path,
            seq_from,
            seq_to,
            tool_name_contains,
            include_compaction,
            limit,
            all_results,
            offset: paging_argument(PagingArgument::Offset, offset)?,
            match_window: match_window
                .map(|value| match value {
                    "earliest" => Ok(CoreMatchWindow::Earliest),
                    "latest" => Ok(CoreMatchWindow::Latest),
                    _ => Err(PyValueError::new_err(
                        "match_window must be 'earliest' or 'latest'",
                    )),
                })
                .transpose()?,
            context: nonnegative("context", context)?,
            context_before: nonnegative("context_before", context_before)?,
            context_after: nonnegative("context_after", context_after)?,
            includes: parse_message_search_includes(include)?,
            lines_per_message,
            detail: detail
                .map(|value| match value {
                    "compact" => Ok(CoreDetailLevel::Compact),
                    "full" => Ok(CoreDetailLevel::Full),
                    _ => Err(PyValueError::new_err("detail must be 'compact' or 'full'")),
                })
                .transpose()?,
            field_view,
            match_view,
            purpose,
            purpose_version: purpose_version
                .map(|value| {
                    std::num::NonZeroU32::new(value).ok_or_else(|| {
                        PyValueError::new_err("purpose_version must be greater than zero")
                    })
                })
                .transpose()?,
            receipt_level: receipt_level
                .map(|value| match value {
                    "none" => Ok(CoreReceiptLevel::None),
                    "summary" => Ok(CoreReceiptLevel::Summary),
                    "full" => Ok(CoreReceiptLevel::Full),
                    _ => Err(PyValueError::new_err(
                        "receipt_level must be 'none', 'summary', or 'full'",
                    )),
                })
                .transpose()?,
        })
    }

    #[getter]
    fn scope(&self) -> MessageScope {
        self.scope.clone()
    }

    #[getter]
    fn role(&self) -> Option<&str> {
        self.role.map(Role::as_str)
    }

    #[getter]
    fn kind(&self) -> Option<&str> {
        self.kind.map(|kind| kind.as_str())
    }

    #[getter]
    fn kinds(&self) -> Option<Vec<&str>> {
        self.kinds
            .as_ref()
            .map(|kinds| kinds.iter().map(|kind| kind.as_str()).collect())
    }

    #[getter]
    fn field(&self) -> &str {
        self.field.as_str()
    }

    #[getter]
    fn argument_path(&self) -> Option<&str> {
        self.argument_path.as_deref()
    }

    #[getter]
    fn tool_name_contains(&self) -> Option<&str> {
        self.tool_name_contains.as_deref()
    }

    #[getter]
    fn match_window(&self) -> Option<&'static str> {
        self.match_window.map(|window| match window {
            CoreMatchWindow::Earliest => "earliest",
            CoreMatchWindow::Latest => "latest",
        })
    }

    #[getter]
    fn detail(&self) -> Option<&'static str> {
        self.detail.map(|detail| match detail {
            CoreDetailLevel::Compact => "compact",
            CoreDetailLevel::Full => "full",
        })
    }

    #[getter]
    fn field_view<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyDict>>> {
        self.field_view
            .map(|view| {
                let result = PyDict::new(py);
                match view {
                    CoreFieldViewBudget::NoCharLimit => result.set_item("kind", "no_char_limit")?,
                    CoreFieldViewBudget::MaxChars { max_chars } => {
                        result.set_item("kind", "max_chars")?;
                        result.set_item("max_chars", max_chars.get())?;
                    }
                }
                Ok(result)
            })
            .transpose()
    }

    #[getter]
    fn match_view<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyDict>>> {
        self.match_view
            .map(|view| {
                let result = PyDict::new(py);
                match view {
                    CoreMatchViewBudget::MinimalSpan => result.set_item("kind", "minimal_span")?,
                    CoreMatchViewBudget::MaxChars { max_chars } => {
                        result.set_item("kind", "max_chars")?;
                        result.set_item("max_chars", max_chars.get())?;
                    }
                }
                Ok(result)
            })
            .transpose()
    }

    #[getter]
    fn purpose(&self) -> Option<&str> {
        self.purpose.as_deref()
    }

    #[getter]
    fn purpose_version(&self) -> Option<u32> {
        self.purpose_version.map(std::num::NonZeroU32::get)
    }

    #[getter]
    fn receipt_level(&self) -> Option<&'static str> {
        self.receipt_level.map(|level| match level {
            CoreReceiptLevel::None => "none",
            CoreReceiptLevel::Summary => "summary",
            CoreReceiptLevel::Full => "full",
        })
    }

    #[getter]
    fn include(&self) -> Option<Vec<&'static str>> {
        self.includes.as_ref().map(|includes| {
            includes
                .iter()
                .copied()
                .map(message_search_include_name)
                .collect()
        })
    }
}

impl Default for MessageSearchRequest {
    fn default() -> Self {
        Self {
            scope: MessageScope::default(),
            role: None,
            kind: None,
            kinds: None,
            field: SearchField::Content,
            argument_path: None,
            seq_from: None,
            seq_to: None,
            tool_name_contains: None,
            include_compaction: true,
            limit: None,
            all_results: false,
            offset: 0,
            match_window: None,
            context: None,
            context_before: None,
            context_after: None,
            includes: None,
            lines_per_message: None,
            detail: None,
            field_view: None,
            match_view: None,
            purpose: None,
            purpose_version: None,
            receipt_level: None,
        }
    }
}

impl MessageSearchRequest {
    fn into_request(self, query: CoreMessageQuery) -> PyResult<CoreMessageSearchRequest> {
        let (since, until) = self.scope.dates.resolve()?;
        let target = match self.field {
            SearchField::Content => CoreMessageTarget::content(),
            SearchField::ToolName => CoreMessageTarget::tool_name(),
            SearchField::ToolArgument => {
                CoreMessageTarget::tool_argument(self.argument_path.unwrap_or_default())
                    .map_err(value_error)?
            }
        };
        let mut builder = CoreMessageSearchRequest::builder(query, target)
            .time(CoreRequestedTimeRange::new(since, until).map_err(value_error)?)
            .include_compaction(self.include_compaction)
            .extent(if self.all_results {
                CoreRequestedExtent::all_results_from(self.offset)
            } else {
                CoreRequestedExtent::page(self.limit, self.offset).map_err(value_error)?
            });
        if let Some(value) = self.role {
            builder = builder.role(value);
        }
        if let Some(value) = self.kind {
            builder = builder.kind(value);
        }
        if let Some(values) = self.kinds.clone() {
            builder = builder.kinds(values);
        }
        if let Some(values) = self.scope.providers {
            builder = builder.providers(values).map_err(value_error)?;
        }
        if let Some(value) = self.scope.session_id {
            builder = builder.session_id(value).map_err(value_error)?;
        }
        if let Some(value) = self.scope.workspace_path_prefix {
            builder = builder.workspace_path_prefix(value).map_err(value_error)?;
        }
        if let Some(value) = self.scope.transcript_path_prefix {
            builder = builder.transcript_path_prefix(value).map_err(value_error)?;
        }
        for value in self.scope.exclusions.workspace_path_prefixes {
            builder = builder
                .exclude_workspace_path_prefix(value)
                .map_err(value_error)?;
        }
        for value in self.scope.exclusions.transcript_path_prefixes {
            builder = builder
                .exclude_transcript_path_prefix(value)
                .map_err(value_error)?;
        }
        for value in self.scope.exclusions.session_ids {
            builder = builder.exclude_session_id(value).map_err(value_error)?;
        }
        if self.seq_from.is_some() || self.seq_to.is_some() {
            builder = builder
                .sequence(CoreSequenceRange::new(self.seq_from, self.seq_to).map_err(value_error)?);
        }
        if let Some(value) = self.tool_name_contains {
            builder = builder.tool_name_contains(value).map_err(value_error)?;
        }
        if let Some(value) = self.match_window {
            builder = builder.match_window(value);
        }
        if self.context.is_some() || self.context_before.is_some() || self.context_after.is_some() {
            let symmetric = self.context.unwrap_or(0);
            builder = builder.context(CoreContextWindow::new(
                self.context_before.unwrap_or(symmetric),
                self.context_after.unwrap_or(symmetric),
            ));
        }
        if let Some(values) = self.includes {
            builder = builder.includes(values);
        }
        if let Some(value) = self.lines_per_message {
            builder =
                builder.message_lines(CoreLineWindow::from_signed(value).map_err(value_error)?);
        }
        if let Some(value) = self.detail {
            builder = builder.detail(value);
        }
        if let Some(value) = self.field_view {
            builder = builder.field_view(value);
        }
        if let Some(value) = self.match_view {
            builder = builder.match_view(value);
        }
        if let Some(value) = self.purpose {
            builder = builder.purpose(
                CorePurposeSelection::new(value, self.purpose_version).map_err(value_error)?,
            );
        }
        if let Some(value) = self.receipt_level {
            builder = builder.receipt_level(value);
        }
        builder.build().map_err(value_error)
    }
}

#[derive(Clone)]
/// Session scope and maximum document count for aggregate analysis operations.
#[pyclass(module = "ai_session_search._native", frozen, from_py_object)]
struct AnalysisQuery {
    scope: QueryScope,
    #[pyo3(get)]
    limit: usize,
}

#[pymethods]
impl AnalysisQuery {
    #[new]
    #[pyo3(signature = (*, scope=None, limit=50))]
    fn new(scope: Option<QueryScope>, limit: i64) -> PyResult<Self> {
        Ok(Self {
            scope: scope.unwrap_or_default(),
            limit: paging_argument(PagingArgument::Limit, limit)?,
        })
    }

    #[getter]
    fn scope(&self) -> QueryScope {
        self.scope.clone()
    }
}

impl Default for AnalysisQuery {
    fn default() -> Self {
        Self {
            scope: QueryScope::default(),
            limit: 50,
        }
    }
}

impl AnalysisQuery {
    fn into_filters(self, app: &CoreSessionSearch) -> PyResult<MessageFilters> {
        let scope = self.scope.resolve(app)?;
        let (since, until) = scope.bounds;
        Ok(MessageFilters {
            providers: scope.provider.map(|provider| vec![provider]),
            session_id: scope.session_id,
            path_prefix: scope.path_prefix,
            exclude_path_prefixes: scope.exclude_path_prefixes,
            exclude_session_ids: scope.exclude_session_ids,
            since,
            until,
            limit: self.limit,
            ..Default::default()
        })
    }
}

/// Exactly one deterministic skill selected by standard name or an explicit package path.
#[derive(Clone)]
#[pyclass(
    name = "SkillSelector",
    module = "ai_session_search._native",
    frozen,
    from_py_object
)]
struct NativeSkillSelector {
    inner: ai_session_search::SkillSelector,
}

#[pymethods]
impl NativeSkillSelector {
    #[new]
    #[pyo3(signature = (*, name=None, path=None))]
    fn new(name: Option<String>, path: Option<PathBuf>) -> PyResult<Self> {
        let inner = match (name, path) {
            (Some(name), None) => {
                ai_session_search::SkillSelector::name(name).map_err(value_error)?
            }
            (None, Some(path)) if !path.as_os_str().is_empty() => {
                ai_session_search::SkillSelector::path(path)
            }
            (None, Some(_)) => {
                return Err(PyValueError::new_err(
                    "skill path is empty; pass a skill directory or exact SKILL.md path",
                ))
            }
            (None, None) => {
                return Err(PyValueError::new_err(
                    "SkillSelector requires exactly one of name or path",
                ))
            }
            (Some(_), Some(_)) => {
                return Err(PyValueError::new_err(
                    "SkillSelector accepts exactly one of name or path, not both",
                ))
            }
        };
        Ok(Self { inner })
    }

    #[getter]
    fn name(&self) -> Option<String> {
        match &self.inner {
            ai_session_search::SkillSelector::Name(selector) => {
                Some(selector.name.as_str().to_string())
            }
            ai_session_search::SkillSelector::Path(_) => None,
        }
    }

    #[getter]
    fn path(&self) -> Option<PathBuf> {
        match &self.inner {
            ai_session_search::SkillSelector::Name(_) => None,
            ai_session_search::SkillSelector::Path(selector) => Some(selector.path.clone()),
        }
    }
}

impl NativeSkillSelector {
    fn from_core(inner: ai_session_search::SkillSelector) -> Self {
        Self { inner }
    }
}

/// Typed arguments for the message-classification skill capability.
#[derive(Clone, Default)]
#[pyclass(
    name = "MessageClassificationQuery",
    module = "ai_session_search._native",
    frozen,
    from_py_object
)]
struct NativeMessageClassificationQuery {
    scope: QueryScope,
    session_kinds: Option<Vec<SessionKind>>,
    #[pyo3(get)]
    additional_skills: Vec<NativeSkillSelector>,
    limit: Option<usize>,
    #[pyo3(get)]
    offset: usize,
}

#[pymethods]
impl NativeMessageClassificationQuery {
    #[new]
    #[pyo3(signature = (*, scope=None, session_kinds=None, additional_skills=None, limit=None, offset=0))]
    fn new(
        scope: Option<QueryScope>,
        session_kinds: Option<Vec<String>>,
        additional_skills: Option<Vec<NativeSkillSelector>>,
        limit: Option<i64>,
        offset: i64,
    ) -> PyResult<Self> {
        Ok(Self {
            scope: scope.unwrap_or_default(),
            session_kinds: parse_session_kinds(session_kinds)?,
            additional_skills: additional_skills.unwrap_or_default(),
            limit: limit
                .map(|value| paging_argument(PagingArgument::Limit, value))
                .transpose()?,
            offset: paging_argument(PagingArgument::Offset, offset)?,
        })
    }

    #[getter]
    fn scope(&self) -> QueryScope {
        self.scope.clone()
    }

    /// Session classes to scan, or `None` for the operation's own default of user-started
    /// sessions only. `[]` deliberately matches nothing.
    #[getter]
    fn session_kinds(&self) -> Option<Vec<String>> {
        self.session_kinds
            .as_ref()
            .map(|kinds| kinds.iter().map(|kind| kind.as_str().to_string()).collect())
    }

    /// Max matches, or `None` to resolve the message-classification capability default at call
    /// time. `0` means every match.
    #[getter]
    fn limit(&self) -> Option<usize> {
        self.limit
    }
}

impl NativeMessageClassificationQuery {
    fn into_core(
        self,
        app: &CoreSessionSearch,
    ) -> PyResult<ai_session_search::MessageClassificationQuery> {
        let scope = self.scope.resolve(app)?;
        let (since, until) = scope.bounds;
        Ok(ai_session_search::MessageClassificationQuery {
            filters: MessageFilters {
                providers: scope.provider.map(|provider| vec![provider]),
                session_id: scope.session_id,
                path_prefix: scope.path_prefix,
                exclude_path_prefixes: scope.exclude_path_prefixes,
                exclude_session_ids: scope.exclude_session_ids,
                since,
                until,
                // Omitted resolves to the same config default the CLI uses, so the two surfaces
                // cannot drift apart: they read one value rather than each carrying a literal.
                limit: self
                    .limit
                    .unwrap_or(app.config().capabilities.message_classification.limit),
                offset: self.offset,
                session_kinds: self.session_kinds,
                ..Default::default()
            },
            additional_skills: self
                .additional_skills
                .into_iter()
                .map(|selector| selector.inner)
                .collect(),
        })
    }
}

/// One typed deterministic skill invocation.
#[derive(Clone)]
#[pyclass(
    name = "SkillRunQuery",
    module = "ai_session_search._native",
    frozen,
    from_py_object
)]
struct NativeSkillRunQuery {
    skill: NativeSkillSelector,
    definition: Option<ai_session_search::MessageClassificationDefinition>,
    input: NativeMessageClassificationQuery,
}

#[pymethods]
impl NativeSkillRunQuery {
    #[new]
    #[pyo3(signature = (*, skill, input, definition=None))]
    fn new(
        skill: NativeSkillSelector,
        input: NativeMessageClassificationQuery,
        definition: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Self> {
        Ok(Self {
            skill,
            definition: python_message_classification_definition(definition)?,
            input,
        })
    }

    #[getter]
    fn skill(&self) -> NativeSkillSelector {
        self.skill.clone()
    }

    #[getter]
    fn input(&self) -> NativeMessageClassificationQuery {
        self.input.clone()
    }

    #[getter]
    fn definition<'py>(&self, py: Python<'py>) -> PyResult<Option<Py<PyAny>>> {
        self.definition
            .as_ref()
            .map(|definition| Ok(json_compatible(py, definition)?.unbind()))
            .transpose()
    }
}

impl NativeSkillRunQuery {
    fn into_core(self, app: &CoreSessionSearch) -> PyResult<ai_session_search::SkillRunQuery> {
        Ok(ai_session_search::SkillRunQuery {
            skill: self.skill.inner,
            definition: self.definition,
            input: ai_session_search::SkillCapabilityInput::MessageClassification(
                self.input.into_core(app)?,
            ),
        })
    }
}

#[derive(Clone)]
/// Typed file-history filters shared by search, reconstruction, restore, and publication.
#[pyclass(module = "ai_session_search._native", frozen, from_py_object)]
struct FileQuery {
    scope: QueryScope,
    #[pyo3(get)]
    min_edits: Option<i64>,
    #[pyo3(get)]
    max_edits: Option<i64>,
    #[pyo3(get)]
    limit: usize,
    #[pyo3(get)]
    offset: usize,
}

#[pymethods]
impl FileQuery {
    #[new]
    #[pyo3(signature = (*, scope=None, min_edits=None, max_edits=None, limit=50, offset=0))]
    fn new(
        scope: Option<QueryScope>,
        min_edits: Option<i64>,
        max_edits: Option<i64>,
        limit: i64,
        offset: i64,
    ) -> PyResult<Self> {
        Ok(Self {
            scope: scope.unwrap_or_default(),
            min_edits,
            max_edits,
            limit: paging_argument(PagingArgument::Limit, limit)?,
            offset: paging_argument(PagingArgument::Offset, offset)?,
        })
    }

    #[getter]
    fn scope(&self) -> QueryScope {
        self.scope.clone()
    }
}

impl Default for FileQuery {
    fn default() -> Self {
        // Constructed directly rather than through `new`, whose paging validation is fallible;
        // these literals are non-negative by construction, so there is no error path to handle.
        Self {
            scope: QueryScope::default(),
            min_edits: None,
            max_edits: None,
            limit: 50,
            offset: 0,
        }
    }
}

impl FileQuery {
    fn into_query(
        self,
        pattern: Option<String>,
        app: &CoreSessionSearch,
    ) -> PyResult<CoreFileQuery> {
        let scope = self.scope.resolve(app)?;
        let (since, until) = scope.bounds;
        Ok(CoreFileQuery {
            pattern,
            provider: scope.provider,
            session_id: scope.session_id,
            path_prefix: scope.path_prefix,
            exclude_path_prefixes: scope.exclude_path_prefixes,
            exclude_session_ids: scope.exclude_session_ids,
            since,
            until,
            min_edits: self.min_edits,
            max_edits: self.max_edits,
            limit: self.limit,
            offset: self.offset,
        })
    }
}

/// One indexed message with canonical session, role, kind, tool, and content fields.
#[pyclass(name = "MessageHit", module = "ai_session_search._native", frozen)]
struct NativeMessageHit {
    #[pyo3(get)]
    session_id: String,
    #[pyo3(get)]
    provider: String,
    #[pyo3(get)]
    seq: i64,
    #[pyo3(get)]
    role: String,
    #[pyo3(get)]
    kind: String,
    #[pyo3(get)]
    timestamp: Option<String>,
    #[pyo3(get)]
    tool_name: Option<String>,
    #[pyo3(get)]
    tool_call_id: Option<String>,
    #[pyo3(get)]
    fuzzy_score: Option<u32>,
    #[pyo3(get)]
    content: String,
    refs: Vec<ai_session_search::refs::MessageRef>,
}

#[pymethods]
impl NativeMessageHit {
    #[getter]
    fn refs(&self, py: Python<'_>) -> PyResult<Vec<Py<NativeMessageRef>>> {
        self.refs
            .iter()
            .cloned()
            .map(|reference| Py::new(py, NativeMessageRef::from(reference)))
            .collect()
    }

    #[getter]
    fn ref_summary(&self) -> String {
        ai_session_search::refs::ref_summary(&self.refs)
    }
}

impl From<MessageHit> for NativeMessageHit {
    fn from(hit: MessageHit) -> Self {
        Self {
            session_id: hit.session_id,
            provider: hit.provider.as_str().to_string(),
            seq: hit.seq,
            role: hit.role.as_str().to_string(),
            kind: hit.kind.as_str().to_string(),
            timestamp: hit.ts.map(|value| value.to_rfc3339()),
            tool_name: hit.tool_name,
            tool_call_id: hit.tool_call_id,
            fuzzy_score: hit.fuzzy_score,
            content: hit.content,
            refs: Vec::new(),
        }
    }
}

/// Cap each hit's content to `lines_per_message` lines (positive=head, negative=tail,
/// 0=full content) before conversion, so bounded strings cross into Python instead of
/// full tool output. This caps each message independently; it never windows a whole
/// session transcript.
fn capped_native_hits(hits: Vec<MessageHit>, lines_per_message: i64) -> Vec<NativeMessageHit> {
    hits.into_iter()
        .map(|mut hit| {
            hit.content =
                ai_session_search::util::select_message_lines(&hit.content, lines_per_message);
            NativeMessageHit::from(hit)
        })
        .collect()
}

/// Canonical version-1 message-search document projected into Python-native dictionaries.
///
/// Each result is converted independently, keeping transient Rust-to-Python encoding memory
/// bounded by the largest result rather than constructing a second whole-response JSON tree.
#[pyclass(
    name = "MessageSearchResponse",
    module = "ai_session_search._native",
    frozen
)]
struct NativeMessageSearchResponse {
    #[pyo3(get)]
    response_schema_version: u32,
    #[pyo3(get)]
    coordinate_unit: &'static str,
    #[pyo3(get)]
    effective_request: Py<PyAny>,
    #[pyo3(get)]
    results: Vec<Py<PyAny>>,
    #[pyo3(get)]
    page: Py<PyAny>,
    #[pyo3(get)]
    included: Option<Py<PyAny>>,
    #[pyo3(get)]
    receipt: Option<Py<PyAny>>,
}

impl NativeMessageSearchResponse {
    fn from_response(
        py: Python<'_>,
        response: ai_session_search::message_search::MessageSearchResponse,
    ) -> PyResult<Self> {
        let loads = py.import("json")?.getattr("loads")?;
        let effective_request = json_compatible_with_loads(&loads, response.request())?;
        let results = (0..response.results().len())
            .map(|index| {
                json_compatible_with_loads(
                    &loads,
                    &response
                        .result_document(index)
                        .expect("index comes from canonical result length"),
                )
            })
            .collect::<PyResult<Vec<_>>>()?;
        let page = json_compatible_with_loads(&loads, &response.page_document())?;
        let included = response
            .has_included_data()
            .then(|| json_compatible_with_loads(&loads, response.included()))
            .transpose()?;
        let receipt = response
            .receipt_document()
            .map(|receipt| json_compatible_with_loads(&loads, &receipt))
            .transpose()?;
        Ok(Self {
            response_schema_version:
                ai_session_search::message_search::MESSAGE_SEARCH_RESPONSE_SCHEMA_VERSION,
            coordinate_unit: "unicode_scalar",
            effective_request,
            results,
            page,
            included,
            receipt,
        })
    }
}

/// Package, database, response-contract, surface, and configuration identity for one request.
#[pyclass(
    name = "MessageSearchRuntimeDiagnostics",
    module = "ai_session_search._native",
    frozen
)]
struct NativeMessageSearchRuntimeDiagnostics {
    #[pyo3(get)]
    package_version: &'static str,
    #[pyo3(get)]
    database_schema_version: i64,
    #[pyo3(get)]
    response_schema_version: u32,
    #[pyo3(get)]
    surface: &'static str,
    #[pyo3(get)]
    config_digest: String,
}

impl From<&CoreMessageSearchRuntimeDiagnostics> for NativeMessageSearchRuntimeDiagnostics {
    fn from(diagnostics: &CoreMessageSearchRuntimeDiagnostics) -> Self {
        let surface = match diagnostics.surface() {
            CoreSearchSurface::Rust => "rust",
            CoreSearchSurface::Cli => "cli",
            CoreSearchSurface::Mcp => "mcp",
            CoreSearchSurface::Python => "python",
        };
        Self {
            package_version: diagnostics.package_version(),
            database_schema_version: diagnostics.database_schema_version(),
            response_schema_version: diagnostics.response_schema_version(),
            surface,
            config_digest: diagnostics.config_digest().to_owned(),
        }
    }
}

/// One owned batch from an exhaustive message search.
///
/// `results` and `context_windows` are index-aligned. `included` is the canonical JSON-compatible
/// dictionary for session-scoped data first encountered in this batch; it does not repeat data
/// already emitted by an earlier batch.
#[pyclass(
    name = "MessageSearchBatch",
    module = "ai_session_search._native",
    frozen
)]
struct NativeMessageSearchBatch {
    #[pyo3(get)]
    results: Vec<Py<PyAny>>,
    #[pyo3(get)]
    included: Py<PyAny>,
}

impl NativeMessageSearchBatch {
    fn from_batch(
        py: Python<'_>,
        batch: &CoreMessageSearchBatch,
        request: &ai_session_search::message_search::ResolvedMessageSearchRequest,
    ) -> PyResult<Self> {
        let loads = py.import("json")?.getattr("loads")?;
        let results = (0..batch.results().len())
            .map(|index| {
                json_compatible_with_loads(
                    &loads,
                    &batch
                        .result_document(request, index)
                        .expect("index comes from canonical batch result length"),
                )
            })
            .collect::<PyResult<Vec<_>>>()?;
        let included = json_compatible_with_loads(&loads, batch.included())?;
        Ok(Self { results, included })
    }
}

/// Terminal page and receipt facts available only after a batch stream reaches natural exhaustion.
#[pyclass(
    name = "MessageSearchCompletion",
    module = "ai_session_search._native",
    frozen
)]
struct NativeMessageSearchCompletion {
    #[pyo3(get)]
    page: Py<PyAny>,
    #[pyo3(get)]
    receipt: Option<Py<PyAny>>,
}

impl NativeMessageSearchCompletion {
    fn from_completion(
        py: Python<'_>,
        completion: &CoreMessageSearchCompletion,
        request: &ai_session_search::message_search::ResolvedMessageSearchRequest,
    ) -> PyResult<Self> {
        let loads = py.import("json")?.getattr("loads")?;
        let page = json_compatible_with_loads(&loads, &completion.page_document())?;
        let receipt = completion
            .receipt_document(request)
            .map(|receipt| json_compatible_with_loads(&loads, &receipt))
            .transpose()?;
        Ok(Self { page, receipt })
    }
}

/// Advanced, context-managed exhaustive message-search batches.
///
/// Ordinary callers should use `SessionSearch.search_messages`, whose `results` attribute is an
/// ordinary materialized list. This owner is for large exhaustive literal, regex, or queryless
/// searches where bounded internal retention matters. Each `next()` releases the GIL while waiting
/// for one Rust-owned batch. `close()` is idempotent, interrupts unread SQLite work, and joins the
/// producer without draining remaining results; `with` calls it automatically on every exit path.
#[pyclass(name = "MessageSearchBatches", module = "ai_session_search._native")]
struct NativeMessageSearchBatches {
    inner: Mutex<CoreMessageSearchBatches>,
    request: ai_session_search::message_search::ResolvedMessageSearchRequest,
    #[pyo3(get)]
    coordinate_unit: &'static str,
    #[pyo3(get)]
    runtime_diagnostics: Option<Py<NativeMessageSearchRuntimeDiagnostics>>,
}

#[pymethods]
impl NativeMessageSearchBatches {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&self, py: Python<'_>) -> PyResult<Option<NativeMessageSearchBatch>> {
        let batch = py
            .detach(|| {
                let mut batches = self
                    .inner
                    .lock()
                    .map_err(|error| format!("message-search batch lock was poisoned: {error}"))?;
                batches.next_batch().map_err(|error| format!("{error:#}"))
            })
            .map_err(runtime_error)?;
        let Some(batch) = batch else {
            return Ok(None);
        };
        match NativeMessageSearchBatch::from_batch(py, &batch, &self.request) {
            Ok(batch) => Ok(Some(batch)),
            Err(projection_error) => {
                let cleanup = py.detach(|| {
                    let mut batches = self.inner.lock().map_err(|error| {
                        format!("message-search batch lock was poisoned: {error}")
                    })?;
                    batches.close().map_err(|error| format!("{error:#}"))
                });
                match cleanup {
                    Ok(()) => Err(projection_error),
                    Err(cleanup_error) => Err(PyRuntimeError::new_err(format!(
                        "message-search batch projection failed: {projection_error}; producer cleanup also failed: {cleanup_error}"
                    ))),
                }
            }
        }
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __exit__(
        &self,
        py: Python<'_>,
        _exception_type: &Bound<'_, PyAny>,
        _exception: &Bound<'_, PyAny>,
        _traceback: &Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        self.close(py)?;
        Ok(false)
    }

    /// Stop unread work and release the snapshot. Repeated calls are safe.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| {
            let mut batches = self
                .inner
                .lock()
                .map_err(|error| format!("message-search batch lock was poisoned: {error}"))?;
            batches.close().map_err(|error| format!("{error:#}"))
        })
        .map_err(runtime_error)
    }

    /// Terminal facts are available only after iteration naturally returns `StopIteration`.
    #[getter]
    fn completion(&self, py: Python<'_>) -> PyResult<NativeMessageSearchCompletion> {
        let batches = self.inner.lock().map_err(|error| {
            runtime_error(format!("message-search batch lock was poisoned: {error}"))
        })?;
        let completion = batches.completion().map_err(|error| {
            let rendered = format!("{error:#}");
            if rendered.contains("have unread results") {
                return PyRuntimeError::new_err(
                    "message-search batches have unread results; iterate until StopIteration or \
                     call next() for another batch, or call close() to stop without terminal metadata",
                );
            }
            runtime_error(rendered)
        })?;
        NativeMessageSearchCompletion::from_completion(py, completion, &self.request)
    }
}

/// Outcome of an opportunistic incremental index refresh.
#[pyclass(module = "ai_session_search._native", frozen)]
struct RefreshOutcome {
    #[pyo3(get)]
    status: &'static str,
    #[pyo3(get)]
    files_seen: Option<usize>,
    #[pyo3(get)]
    sessions_updated: Option<usize>,
    #[pyo3(get)]
    reason: Option<String>,
}

impl From<AutoReindexOutcome> for RefreshOutcome {
    fn from(outcome: AutoReindexOutcome) -> Self {
        match outcome {
            AutoReindexOutcome::Updated {
                files_seen,
                sessions_updated,
            } => Self {
                status: "updated",
                files_seen: Some(files_seen),
                sessions_updated: Some(sessions_updated),
                reason: None,
            },
            AutoReindexOutcome::SkippedBusy => Self {
                status: "skipped_busy",
                files_seen: None,
                sessions_updated: None,
                reason: None,
            },
            AutoReindexOutcome::SkippedFresh => Self {
                status: "skipped_fresh",
                files_seen: None,
                sessions_updated: None,
                reason: None,
            },
            AutoReindexOutcome::SkippedLockUnavailable { reason } => Self {
                status: "skipped_lock_unavailable",
                files_seen: None,
                sessions_updated: None,
                reason: Some(reason),
            },
        }
    }
}

/// Session-file and changed-session counts from an explicit reindex.
#[pyclass(name = "ReindexOutcome", module = "ai_session_search._native", frozen)]
struct NativeReindexOutcome {
    #[pyo3(get)]
    files_seen: usize,
    #[pyo3(get)]
    sessions_updated: usize,
    #[pyo3(get)]
    discovery_warnings: Vec<NativeProviderDiscoveryWarning>,
}

#[derive(Clone)]
/// Expected parser version and current/stale counts for one provider.
#[pyclass(
    name = "ProviderParserHealth",
    module = "ai_session_search._native",
    frozen,
    skip_from_py_object
)]
struct NativeProviderParserHealth {
    #[pyo3(get)]
    provider: String,
    #[pyo3(get)]
    expected_parse_version: String,
    #[pyo3(get)]
    indexed_sessions: i64,
    #[pyo3(get)]
    current_sessions: i64,
    #[pyo3(get)]
    stale_sessions: i64,
}

impl From<ProviderParserHealth> for NativeProviderParserHealth {
    fn from(health: ProviderParserHealth) -> Self {
        Self {
            provider: health.provider.as_str().to_string(),
            expected_parse_version: health.expected_parse_version,
            indexed_sessions: health.indexed_sessions,
            current_sessions: health.current_sessions,
            stale_sessions: health.stale_sessions,
        }
    }
}

#[derive(Clone)]
/// Aggregate schema and parser-version freshness across indexed sessions.
#[pyclass(
    name = "ParserHealth",
    module = "ai_session_search._native",
    frozen,
    skip_from_py_object
)]
struct NativeParserHealth {
    #[pyo3(get)]
    schema_version: i64,
    #[pyo3(get)]
    expected_schema_version: i64,
    #[pyo3(get)]
    schema_current: bool,
    #[pyo3(get)]
    indexed_sessions: i64,
    #[pyo3(get)]
    current_sessions: i64,
    #[pyo3(get)]
    stale_sessions: i64,
    #[pyo3(get)]
    parse_warnings: i64,
    #[pyo3(get)]
    providers: Vec<NativeProviderParserHealth>,
}

impl From<ParserHealth> for NativeParserHealth {
    fn from(health: ParserHealth) -> Self {
        Self {
            schema_version: health.schema_version,
            expected_schema_version: health.expected_schema_version,
            schema_current: health.schema_current,
            indexed_sessions: health.indexed_sessions,
            current_sessions: health.current_sessions,
            stale_sessions: health.stale_sessions,
            parse_warnings: health.parse_warnings,
            providers: health.providers.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Clone)]
/// Parser/schema freshness and applicable repair commands for the index.
#[pyclass(
    name = "IndexStatus",
    module = "ai_session_search._native",
    frozen,
    skip_from_py_object
)]
struct NativeIndexStatus {
    #[pyo3(get)]
    parser_health: NativeParserHealth,
    #[pyo3(get)]
    repairable_stale_sessions: i64,
    #[pyo3(get)]
    unavailable_stale_sessions: i64,
    #[pyo3(get)]
    repair_commands: Vec<String>,
    #[pyo3(get)]
    readiness: NativeIndexReadinessStatus,
}

#[derive(Clone)]
/// Snapshot usability and automatic refresh state reported independently.
#[pyclass(
    name = "IndexReadinessStatus",
    module = "ai_session_search._native",
    frozen,
    skip_from_py_object
)]
struct NativeIndexReadinessStatus {
    #[pyo3(get)]
    snapshot_availability: String,
    #[pyo3(get)]
    last_successful_refresh_at: Option<String>,
    #[pyo3(get)]
    refresh: NativeIndexRefreshStatus,
}

#[derive(Clone)]
/// Bounded durable progress and recovery for automatic index refresh.
#[pyclass(
    name = "IndexRefreshStatus",
    module = "ai_session_search._native",
    frozen,
    skip_from_py_object
)]
struct NativeIndexRefreshStatus {
    #[pyo3(get)]
    state: String,
    #[pyo3(get)]
    started_by: Option<String>,
    #[pyo3(get)]
    started_at: Option<String>,
    #[pyo3(get)]
    finished_at: Option<String>,
    #[pyo3(get)]
    files_discovered: Option<usize>,
    #[pyo3(get)]
    files_processed: Option<usize>,
    #[pyo3(get)]
    sessions_updated: Option<usize>,
    #[pyo3(get)]
    retry_after_ms: Option<u64>,
    #[pyo3(get)]
    message: Option<String>,
    #[pyo3(get)]
    next_command: Option<String>,
}

impl From<IndexRefreshStatus> for NativeIndexRefreshStatus {
    fn from(status: IndexRefreshStatus) -> Self {
        Self {
            state: status.state.as_str().to_string(),
            started_by: status
                .started_by
                .map(|trigger| trigger.as_str().to_string()),
            started_at: status.started_at.map(|value| value.to_rfc3339()),
            finished_at: status.finished_at.map(|value| value.to_rfc3339()),
            files_discovered: status.files_discovered,
            files_processed: status.files_processed,
            sessions_updated: status.sessions_updated,
            retry_after_ms: status.retry_after_ms,
            message: status.message,
            next_command: status.next_command,
        }
    }
}

impl From<IndexReadinessStatus> for NativeIndexReadinessStatus {
    fn from(status: IndexReadinessStatus) -> Self {
        Self {
            snapshot_availability: status.snapshot.availability.as_str().to_string(),
            last_successful_refresh_at: status
                .snapshot
                .last_successful_refresh_at
                .map(|value| value.to_rfc3339()),
            refresh: status.refresh.into(),
        }
    }
}

impl From<IndexStatus> for NativeIndexStatus {
    fn from(status: IndexStatus) -> Self {
        Self {
            parser_health: status.parser_health.into(),
            repairable_stale_sessions: status.repairable_stale_sessions,
            unavailable_stale_sessions: status.unavailable_stale_sessions,
            repair_commands: status.repair_commands,
            readiness: status.readiness.into(),
        }
    }
}

#[derive(Clone)]
/// Discovery, parser, index, CLI, and resume status for one provider.
#[pyclass(
    name = "ProviderHealth",
    module = "ai_session_search._native",
    frozen,
    skip_from_py_object
)]
struct NativeProviderHealth {
    #[pyo3(get)]
    provider: String,
    #[pyo3(get)]
    enabled: bool,
    #[pyo3(get)]
    cli_available: bool,
    #[pyo3(get)]
    roots: Vec<String>,
    #[pyo3(get)]
    discovered_files: usize,
    #[pyo3(get)]
    indexed_sessions: i64,
    #[pyo3(get)]
    expected_parse_version: String,
    #[pyo3(get)]
    current_sessions: i64,
    #[pyo3(get)]
    stale_sessions: i64,
    #[pyo3(get)]
    repairable_stale_sessions: i64,
    #[pyo3(get)]
    unavailable_stale_sessions: i64,
    #[pyo3(get)]
    resume_command: Option<String>,
}

impl From<ProviderHealth> for NativeProviderHealth {
    fn from(health: ProviderHealth) -> Self {
        Self {
            provider: health.provider.as_str().to_string(),
            enabled: health.enabled,
            cli_available: health.cli_available,
            roots: health.roots,
            discovered_files: health.discovered_files,
            indexed_sessions: health.indexed_sessions,
            expected_parse_version: health.expected_parse_version,
            current_sessions: health.current_sessions,
            stale_sessions: health.stale_sessions,
            repairable_stale_sessions: health.repairable_stale_sessions,
            unavailable_stale_sessions: health.unavailable_stale_sessions,
            resume_command: health.resume_command,
        }
    }
}

/// Database, parser, automatic-update, and provider health report.
#[pyclass(
    name = "DiagnosticStatus",
    module = "ai_session_search._native",
    frozen
)]
struct NativeDiagnosticStatus {
    #[pyo3(get)]
    db_path: String,
    #[pyo3(get)]
    index_status: NativeIndexStatus,
    #[pyo3(get)]
    discovery_warnings: Vec<NativeProviderDiscoveryWarning>,
    #[pyo3(get)]
    providers: Vec<NativeProviderHealth>,
}

#[derive(Clone)]
/// One non-fatal provider traversal or metadata-sidecar discovery failure.
#[pyclass(
    name = "ProviderDiscoveryWarning",
    module = "ai_session_search._native",
    frozen,
    skip_from_py_object
)]
struct NativeProviderDiscoveryWarning {
    #[pyo3(get)]
    provider: String,
    #[pyo3(get)]
    path: String,
    #[pyo3(get)]
    operation: String,
    #[pyo3(get)]
    message: String,
    #[pyo3(get)]
    readable_sources_preserved: bool,
    #[pyo3(get)]
    verification_command: String,
    #[pyo3(get)]
    guidance: String,
}

impl From<ai_session_search::source::ProviderDiscoveryWarning> for NativeProviderDiscoveryWarning {
    fn from(warning: ai_session_search::source::ProviderDiscoveryWarning) -> Self {
        Self {
            provider: warning.provider.as_str().to_string(),
            path: warning.path,
            operation: warning.operation,
            message: warning.message,
            readable_sources_preserved: warning.readable_sources_preserved,
            verification_command: warning.verification_command,
            guidance: warning.guidance,
        }
    }
}

/// Database byte counts before and after successful compaction.
#[pyclass(name = "CompactOutcome", module = "ai_session_search._native", frozen)]
struct NativeCompactOutcome {
    #[pyo3(get)]
    before_bytes: u64,
    #[pyo3(get)]
    after_bytes: u64,
    #[pyo3(get)]
    reclaimed_bytes: u64,
}

impl From<CompactOutcome> for NativeCompactOutcome {
    fn from(outcome: CompactOutcome) -> Self {
        Self {
            before_bytes: outcome.before_bytes,
            after_bytes: outcome.after_bytes,
            reclaimed_bytes: outcome.reclaimed_bytes(),
        }
    }
}

/// Rust-backed search, recovery, export, and analysis service.
///
/// Methods accepting `session_id` accept a canonical provider-qualified ID or a unique ID prefix.
/// Ambiguous prefixes fail with the matching canonical IDs instead of selecting one.
#[pyclass(module = "ai_session_search._native")]
struct SessionSearch {
    inner: Mutex<CoreSessionSearch>,
}

#[pymethods]
impl SessionSearch {
    #[new]
    #[pyo3(signature = (db_path=None, *, config_path=None, cache_dir=None, threads=None))]
    fn new(
        db_path: Option<PathBuf>,
        config_path: Option<PathBuf>,
        cache_dir: Option<PathBuf>,
        threads: Option<usize>,
    ) -> PyResult<Self> {
        for (name, path) in [
            ("db_path", db_path.as_ref()),
            ("config_path", config_path.as_ref()),
            ("cache_dir", cache_dir.as_ref()),
        ] {
            if path.is_some_and(|path| path.as_os_str().is_empty()) {
                return Err(PyValueError::new_err(format!(
                    "{name} must not be empty; omit it (pass None) to use the default \
                     {name}, or pass a non-empty path"
                )));
            }
        }
        if threads == Some(0) {
            return Err(PyValueError::new_err(
                "threads must be greater than zero; pass threads=None to use the platform \
                 default, or a positive integer",
            ));
        }
        let config = Config::resolve(ConfigOverrides {
            config_path,
            database_path: db_path,
            cache_dir,
            threads,
            index_refresh: None,
        })
        .map_err(runtime_error)?
        .config;
        let inner = CoreSessionSearch::open(config).map_err(runtime_error)?;
        Ok(Self {
            inner: Mutex::new(inner),
        })
    }

    #[getter]
    fn db_path(&self) -> PyResult<PathBuf> {
        let app = self.inner.lock().map_err(runtime_error)?;
        Ok(app.config().db_path())
    }

    /// Return the executable message-search parameter catalogue and this instance's configured
    /// Python defaults as a JSON-compatible dictionary.
    ///
    /// The configured request is resolved by the same Rust planner used by `search_messages`.
    /// This call reads no indexed sessions or messages.
    fn message_search_spec<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let specification = {
            let app = self.inner.lock().map_err(runtime_error)?;
            app.messages_for_surface(CoreSearchSurface::Python)
                .message_search_spec()
                .map_err(runtime_error)?
        };
        json_compatible(py, &specification)
    }

    #[pyo3(signature = (query="", request=None, *, query_mode="literal"))]
    /// Search messages through the shared typed planner and return results with aligned context,
    /// paging, resolved presentation, and optional planner receipts.
    ///
    /// Fuzzy mode scores every structurally eligible message with Nucleo sequence matching,
    /// orders results deterministically, and then applies the finite request offset and limit.
    /// It requires at least three characters and does not support all-results output. With no
    /// explicit, purpose, or operation limit, Python returns every literal, regex, or no-text
    /// match; `all_results=True` states that complete-corpus choice explicitly.
    fn search_messages(
        &self,
        py: Python<'_>,
        query: &str,
        request: Option<MessageSearchRequest>,
        query_mode: &str,
    ) -> PyResult<NativeMessageSearchResponse> {
        let (query, _has_content_query) = core_message_query(query.to_owned(), query_mode)?;
        let response = py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            let request = request.unwrap_or_default().into_request(query)?;
            app.messages_for_surface(ai_session_search::message_search::SearchSurface::Python)
                .search(request)
                .map_err(python_message_search_error)
        })?;
        NativeMessageSearchResponse::from_response(py, response)
    }

    #[pyo3(signature = (query="", request=None, *, query_mode="literal", batch_rows=256))]
    /// Open advanced, bounded-retention batches for one exhaustive message search.
    ///
    /// Prefer `search_messages` for ordinary use: it returns a response whose `results` is a normal
    /// Python list. Use this context-managed iterator only for an all-results literal, regex, or
    /// queryless request whose result bytes should cross into Python in bounded batches.
    ///
    /// `batch_rows` must be positive and controls handoff/enrichment frequency, not membership,
    /// ordering, context, or terminal receipt facts. Each `next()` and `close()` releases the GIL.
    /// A finite page or fuzzy query is rejected with guidance to use `search_messages`.
    fn search_message_batches(
        &self,
        py: Python<'_>,
        query: &str,
        request: Option<MessageSearchRequest>,
        query_mode: &str,
        batch_rows: i64,
    ) -> PyResult<NativeMessageSearchBatches> {
        let batch_rows = usize::try_from(batch_rows)
            .ok()
            .and_then(NonZeroUsize::new)
            .ok_or_else(|| {
                PyValueError::new_err(format!(
                    "batch_rows must be a positive integer; use 256 for the default balance or a smaller positive value to reduce active result memory; got {batch_rows}"
                ))
            })?;
        let (query, _has_content_query) = core_message_query(query.to_owned(), query_mode)?;
        if query_mode == "fuzzy" {
            return Err(PyValueError::new_err(
                "fuzzy search is not available from search_message_batches; use \
                 search_messages() with query_mode='fuzzy' and a positive limit; exhaustive batches \
                 support literal, regex, and queryless searches",
            ));
        }
        let batches = py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            let request = request.unwrap_or_default().into_request(query)?;
            app.message_search_batches_for_surface(
                ai_session_search::message_search::SearchSurface::Python,
                request,
                batch_rows,
            )
            .map_err(python_batch_open_error)
        })?;
        let request = batches.request().clone();
        let runtime_diagnostics = batches
            .runtime_diagnostics()
            .map(NativeMessageSearchRuntimeDiagnostics::from)
            .map(|diagnostics| Py::new(py, diagnostics))
            .transpose()?;
        Ok(NativeMessageSearchBatches {
            inner: Mutex::new(batches),
            request,
            coordinate_unit: "unicode_scalar",
            runtime_diagnostics,
        })
    }

    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (session_id, seq, *, context=0, context_before=None, context_after=None, lines_per_message=0))]
    /// Return the messages surrounding `seq` in one session.
    ///
    /// `context` is a symmetric radius (messages on each side); `context_before`/`context_after`
    /// override one side each, like grep's `-C` / `-B` / `-A`. All are non-negative and default
    /// to 0, so the default returns just the anchor message. `lines_per_message` is a
    /// presentation-only per-message line window: positive keeps the first N lines, negative the
    /// last N, 0 the complete content; it never removes messages from the context.
    fn message_context(
        &self,
        py: Python<'_>,
        session_id: String,
        seq: i64,
        context: i64,
        context_before: Option<i64>,
        context_after: Option<i64>,
        lines_per_message: i64,
    ) -> PyResult<Vec<NativeMessageHit>> {
        let before = context_before.unwrap_or(context);
        let after = context_after.unwrap_or(context);
        if seq < 0 || context < 0 || before < 0 || after < 0 {
            return Err(PyValueError::new_err(
                "seq, context, context_before, and context_after must be non-negative",
            ));
        }
        py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            let session = app
                .catalog()
                .resolve_session(&session_id)
                .map_err(runtime_error)?;
            app.messages()
                .context(&session.id, seq, before, after)
                .map(|hits| capped_native_hits(hits, lines_per_message))
                .map_err(runtime_error)
        })
    }

    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (session_id, *, order="oldest", role=None, limit=0, offset=0, seq_from=None, seq_to=None, lines_per_message=0))]
    /// Read one session's messages, selecting the oldest or newest `limit` by `order`, always
    /// returned in chronological (seq-ascending) order.
    ///
    /// `order` is "oldest" (the first `limit` by sequence, the default) or "newest" (the last
    /// `limit`); a negative count is not accepted — direction is `order`, not a sign. `limit=0`
    /// returns all. To read a long session in chunks, advance `seq_from` (the next chunk starts
    /// at the last seq + 1) rather than growing `limit`, which re-sends what you already read.
    /// `lines_per_message` is the presentation-only per-message line window.
    fn read_session_messages(
        &self,
        py: Python<'_>,
        session_id: String,
        order: &str,
        role: Option<&str>,
        limit: i64,
        offset: i64,
        seq_from: Option<i64>,
        seq_to: Option<i64>,
        lines_per_message: i64,
    ) -> PyResult<Vec<NativeMessageHit>> {
        let order = match order {
            "oldest" => ai_session_search::db::MessageOrder::OldestFirst,
            "newest" => ai_session_search::db::MessageOrder::NewestFirst,
            other => {
                return Err(PyValueError::new_err(format!(
                    "order must be 'oldest' or 'newest', got {other:?}"
                )))
            }
        };
        let role = role
            .map(str::parse)
            .transpose()
            .map_err(PyValueError::new_err)?;
        let limit = paging_argument(PagingArgument::Limit, limit)?;
        let offset = paging_argument(PagingArgument::Offset, offset)?;
        // Reject an inverted range up front as a ValueError, so a caller mistake reads the same
        // way as the other input errors here rather than surfacing later as a RuntimeError.
        if let (Some(from), Some(to)) = (seq_from, seq_to) {
            if from > to {
                return Err(PyValueError::new_err("seq_from must be <= seq_to"));
            }
        }
        py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            let session = app
                .catalog()
                .resolve_session(&session_id)
                .map_err(runtime_error)?;
            let filters = MessageFilters {
                role,
                session_id: Some(session.id),
                seq_from,
                seq_to,
                limit,
                offset,
                ..Default::default()
            };
            app.messages()
                .read_session(&filters, order)
                .map(|hits| capped_native_hits(hits, lines_per_message))
                .map_err(runtime_error)
        })
    }

    #[pyo3(signature = (session_id, *, preview_chars=None, summary_items=None, include_time_profile=false))]
    fn inspect_session(
        &self,
        py: Python<'_>,
        session_id: String,
        preview_chars: Option<usize>,
        summary_items: Option<i64>,
        include_time_profile: bool,
    ) -> PyResult<NativeSessionInspection> {
        if preview_chars == Some(0) {
            return Err(PyValueError::new_err(
                "preview_chars must be greater than zero",
            ));
        }
        let options = ai_session_search::inspect::InspectionOptions {
            preview_chars: preview_chars
                .unwrap_or(ai_session_search::inspect::DEFAULT_PREVIEW_CHARS),
            evidence_window: ai_session_search::inspect::EvidenceWindow::from_signed_items(
                summary_items
                    .unwrap_or(-(ai_session_search::inspect::DEFAULT_EVIDENCE_LIMIT as i64)),
            )
            .map_err(value_error)?,
            include_time_profile,
        };
        let inspection = py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            app.catalog()
                .inspect(&session_id, options)
                .map_err(runtime_error)
        })?;
        NativeSessionInspection::from_inspection(py, inspection)
    }

    #[pyo3(signature = (request=None))]
    fn list_sessions(
        &self,
        py: Python<'_>,
        request: Option<SessionQuery>,
    ) -> PyResult<Vec<NativeSessionRecord>> {
        let (filters, _) = request.unwrap_or_default().into_filters()?;
        py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            app.catalog()
                .list_sessions(&filters)
                .map(|sessions| {
                    sessions
                        .into_iter()
                        .map(NativeSessionRecord::from)
                        .collect()
                })
                .map_err(runtime_error)
        })
    }

    #[pyo3(signature = (query, request=None))]
    fn search_sessions(
        &self,
        py: Python<'_>,
        query: String,
        request: Option<SessionQuery>,
    ) -> PyResult<Vec<NativeSessionSearchHit>> {
        let (filters, current_repo) = request.unwrap_or_default().into_filters()?;
        let hits = py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            let current_repo =
                current_repo.or_else(|| ai_session_search::util::current_repo(app.config()));
            app.catalog()
                .search_sessions(
                    &query,
                    &filters,
                    current_repo.as_deref(),
                    &app.config().search.scoring,
                )
                .map_err(runtime_error)
        })?;
        hits.into_iter()
            .map(|hit| NativeSessionSearchHit::from_hit(py, hit))
            .collect()
    }

    #[pyo3(signature = (pattern=None, request=None))]
    fn search_files(
        &self,
        py: Python<'_>,
        pattern: Option<String>,
        request: Option<FileQuery>,
    ) -> PyResult<Vec<NativeFileEditSummary>> {
        py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            let query = request.unwrap_or_default().into_query(pattern, &app)?;
            app.files()
                .search(&query)
                .map(|files| files.into_iter().map(NativeFileEditSummary::from).collect())
                .map_err(runtime_error)
        })
    }

    #[pyo3(signature = (file, request=None))]
    fn file_history(
        &self,
        py: Python<'_>,
        file: String,
        request: Option<FileQuery>,
    ) -> PyResult<Vec<NativeFileVersion>> {
        py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            let query = request.unwrap_or_default().into_query(None, &app)?;
            app.files()
                .history(&file, &query)
                .map(|versions| versions.into_iter().map(NativeFileVersion::from).collect())
                .map_err(runtime_error)
        })
    }

    #[pyo3(signature = (pattern=None, request=None))]
    fn cross_reference_files(
        &self,
        py: Python<'_>,
        pattern: Option<String>,
        request: Option<FileQuery>,
    ) -> PyResult<Vec<NativeFileCrossRef>> {
        py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            let query = request.unwrap_or_default().into_query(pattern, &app)?;
            app.files()
                .cross_reference(&query)
                .map(|references| {
                    references
                        .into_iter()
                        .map(NativeFileCrossRef::from)
                        .collect()
                })
                .map_err(runtime_error)
        })
    }

    #[pyo3(signature = (file, *, version=None, request=None))]
    fn reconstruct_file(
        &self,
        py: Python<'_>,
        file: String,
        version: Option<usize>,
        request: Option<FileQuery>,
    ) -> PyResult<NativeReconstructedFile> {
        py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            let query = request.unwrap_or_default().into_query(None, &app)?;
            app.files()
                .reconstruct(&file, &query, version)
                .map(NativeReconstructedFile::from)
                .map_err(runtime_error)
        })
    }

    #[pyo3(signature = (file, *, request=None))]
    fn reconstruct_file_versions(
        &self,
        py: Python<'_>,
        file: String,
        request: Option<FileQuery>,
    ) -> PyResult<NativeReconstructedFileVersions> {
        py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            let query = request.unwrap_or_default().into_query(None, &app)?;
            app.files()
                .reconstruct_versions(&file, &query)
                .map(|inner| NativeReconstructedFileVersions { inner })
                .map_err(runtime_error)
        })
    }

    #[pyo3(signature = (file, destination, *, request=None))]
    fn publish_file_versions(
        &self,
        py: Python<'_>,
        file: String,
        destination: PathBuf,
        request: Option<FileQuery>,
    ) -> PyResult<NativeRecoveryPublicationReceipt> {
        py.detach(|| {
            let versions = {
                let app = self.inner.lock().map_err(runtime_error)?;
                let query = request.unwrap_or_default().into_query(None, &app)?;
                app.files()
                    .reconstruct_versions(&file, &query)
                    .map_err(runtime_error)?
            };
            ai_session_search::files::publish_reconstructed_versions(versions, &destination)
                .map(NativeRecoveryPublicationReceipt::from)
                .map_err(runtime_error)
        })
    }

    #[pyo3(signature = (session_id, format="markdown"))]
    fn export_session(
        &self,
        py: Python<'_>,
        session_id: String,
        format: &str,
    ) -> PyResult<NativeExportDocument> {
        let format = format
            .parse::<ai_session_search::export::ExportFormat>()
            .map_err(value_error)?;
        py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            app.exports()
                .render_full(&session_id, format)
                .map(NativeExportDocument::from)
                .map_err(runtime_error)
        })
    }

    #[pyo3(signature = (destination, request=None, *, format="markdown"))]
    fn export_sessions(
        &self,
        py: Python<'_>,
        destination: PathBuf,
        request: Option<SessionQuery>,
        format: &str,
    ) -> PyResult<NativeExportPublicationReceipt> {
        let format = format
            .parse::<ai_session_search::export::ExportFormat>()
            .map_err(value_error)?;
        let plan = ai_session_search::export::ExportPublicationPlan::new(destination, format)
            .map_err(value_error)?;
        let (filters, _) = request.unwrap_or_default().into_filters()?;
        py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            app.exports()
                .publish_bundle(&filters, &plan)
                .map(NativeExportPublicationReceipt::from)
                .map_err(runtime_error)
        })
    }

    fn source_inventory(&self, py: Python<'_>) -> PyResult<Vec<NativeProviderSourceStatus>> {
        py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            Ok(app
                .sources()
                .inventory()
                .into_iter()
                .map(NativeProviderSourceStatus::from)
                .collect())
        })
    }

    /// Return one bounded keyset page of analysis documents.
    ///
    /// `request.limit` must be a positive page size. `SessionQuery(limit=0)` remains the
    /// all-matches spelling for APIs that materialize complete results, but an unbounded analysis
    /// page cannot produce a continuation cursor and is rejected here before service work starts.
    #[pyo3(signature = (request=None, *, cursor=None))]
    fn analysis_documents(
        &self,
        py: Python<'_>,
        request: Option<SessionQuery>,
        cursor: Option<NativeAnalysisCursor>,
    ) -> PyResult<NativeAnalysisDocumentPage> {
        let request = request.unwrap_or_default();
        if NonZeroUsize::new(request.limit).is_none() {
            return Err(PyValueError::new_err(
                "analysis_documents request limit must be greater than zero; pass a positive page \
                 size and follow next_cursor until it is None",
            ));
        }
        let (filters, _) = request.into_filters()?;
        let page = py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            app.analysis()
                .documents(&filters, cursor.as_ref().map(|value| &value.inner))
                .map_err(runtime_error)
        })?;
        NativeAnalysisDocumentPage::from_page(py, page)
    }

    /// Analyze every eligible session by default, or the explicit selection and typed policy.
    #[pyo3(signature = (request=None, *, policy=None))]
    fn analyze(
        &self,
        py: Python<'_>,
        request: Option<NativeAnalysisRequest>,
        policy: Option<AnalysisPolicy>,
    ) -> PyResult<NativeReceiptedAnalysis> {
        let request = request.unwrap_or_default().into_core()?;
        let policy = policy.map(|policy| policy.inner).unwrap_or_else(|| {
            RustAnalysisPolicySpec::default()
                .compile()
                .expect("empty analysis policy is valid")
        });
        let result = py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            app.analysis().run(&request, &policy).map_err(runtime_error)
        })?;
        Ok(NativeReceiptedAnalysis {
            inner: Arc::new(result),
        })
    }

    fn run_skill(
        &self,
        py: Python<'_>,
        request: NativeSkillRunQuery,
    ) -> PyResult<NativeSkillRunReport> {
        let report = py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            let query = request.into_core(&app)?;
            app.analysis().run_skill(&query).map_err(runtime_error)
        })?;
        NativeSkillRunReport::from_report(py, report)
    }

    #[pyo3(signature = (request=None, command_patterns=None))]
    fn planning(
        &self,
        py: Python<'_>,
        request: Option<AnalysisQuery>,
        command_patterns: Option<Vec<String>>,
    ) -> PyResult<Vec<NativePlanningCount>> {
        py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            let filters = request.unwrap_or_default().into_filters(&app)?;
            app.analysis()
                .planning(&filters, command_patterns.as_deref().unwrap_or_default())
                .map(|counts| counts.into_iter().map(NativePlanningCount::from).collect())
                .map_err(runtime_error)
        })
    }

    #[pyo3(signature = (request=None))]
    fn role_statistics(
        &self,
        py: Python<'_>,
        request: Option<AnalysisQuery>,
    ) -> PyResult<Vec<NativeRoleStatistic>> {
        py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            let filters = request.unwrap_or_default().into_filters(&app)?;
            app.analysis()
                .role_statistics(&filters)
                .map(|rows| rows.into_iter().map(NativeRoleStatistic::from).collect())
                .map_err(runtime_error)
        })
    }

    fn refresh(&self, py: Python<'_>) -> PyResult<RefreshOutcome> {
        py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            app.index()
                .refresh()
                .map(RefreshOutcome::from)
                .map_err(runtime_error)
        })
    }

    fn index_status(&self, py: Python<'_>) -> PyResult<NativeIndexStatus> {
        py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            app.index()
                .status()
                .map(NativeIndexStatus::from)
                .map_err(runtime_error)
        })
    }

    #[pyo3(signature = (*, full=false))]
    fn reindex(&self, py: Python<'_>, full: bool) -> PyResult<NativeReindexOutcome> {
        py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            app.index()
                .reindex_report(full)
                .map(|outcome| NativeReindexOutcome {
                    files_seen: outcome.files_seen,
                    sessions_updated: outcome.sessions_updated,
                    discovery_warnings: outcome
                        .discovery_warnings
                        .into_iter()
                        .map(Into::into)
                        .collect(),
                })
                .map_err(runtime_error)
        })
    }

    fn diagnostics(&self, py: Python<'_>) -> PyResult<NativeDiagnosticStatus> {
        py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            app.maintenance()
                .diagnostics()
                .map(|status| NativeDiagnosticStatus {
                    db_path: status.db_path,
                    index_status: status.index_status.into(),
                    discovery_warnings: status
                        .discovery_warnings
                        .into_iter()
                        .map(Into::into)
                        .collect(),
                    providers: status.providers.into_iter().map(Into::into).collect(),
                })
                .map_err(runtime_error)
        })
    }

    fn compact(&self, py: Python<'_>) -> PyResult<NativeCompactOutcome> {
        py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            app.maintenance()
                .compact()
                .map(NativeCompactOutcome::from)
                .map_err(runtime_error)
        })
    }
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(serve_mcp, module)?)?;
    module.add_function(wrap_pyfunction!(_run_cli_command, module)?)?;
    module.add_class::<SessionSearch>()?;
    module.add_class::<NativeSessionRecord>()?;
    module.add_class::<NativeAnalysisCursor>()?;
    module.add_class::<NativeAnalysisDocument>()?;
    module.add_class::<NativeAnalysisDocumentPage>()?;
    module.add_class::<ClassificationRule>()?;
    module.add_class::<RelationshipRule>()?;
    module.add_class::<PhraseVocabulary>()?;
    module.add_class::<AnalysisPolicy>()?;
    module.add_class::<NativeClassificationMatch>()?;
    module.add_class::<NativeRelationshipHint>()?;
    module.add_class::<NativeAnalyzedSession>()?;
    module.add_class::<NativePhraseFrequency>()?;
    module.add_class::<NativeAnalysisResult>()?;
    module.add_class::<NativeAnalysisReceipt>()?;
    module.add_class::<NativeReceiptedAnalysis>()?;
    module.add_class::<NativeAnalysisArtifact>()?;
    module.add_class::<NativePublishedAnalysisArtifact>()?;
    module.add_class::<NativeAnalysisPublicationReceipt>()?;
    module.add_class::<NativeAnalysisPublicationPlan>()?;
    module.add_class::<NativeSessionGraphNode>()?;
    module.add_class::<NativeSessionGraphEdge>()?;
    module.add_class::<NativeSessionGraphGroup>()?;
    module.add_class::<NativeSessionGraph>()?;
    module.add_class::<NativeSessionSearchHit>()?;
    module.add_class::<NativeMessagePreview>()?;
    module.add_class::<NativeToolActivity>()?;
    module.add_class::<NativeMessageRef>()?;
    module.add_class::<NativeRefEvidence>()?;
    module.add_class::<NativeChangedFileEvidence>()?;
    module.add_class::<NativeSessionTimeProfile>()?;
    module.add_class::<NativeSessionInspection>()?;
    module.add_class::<NativeFileEditSummary>()?;
    module.add_class::<NativeFileVersion>()?;
    module.add_class::<NativeFileCrossRef>()?;
    module.add_class::<NativeReconstructedFile>()?;
    module.add_class::<NativeReconstructedFileVersions>()?;
    module.add_class::<NativeRecoveryPublicationReceipt>()?;
    module.add_class::<NativeExportDocument>()?;
    module.add_class::<NativeExportPublicationReceipt>()?;
    module.add_class::<NativeProviderSourceStatus>()?;
    module.add_class::<NativeMessageClassificationMatch>()?;
    module.add_class::<NativeCapabilityReceipt>()?;
    module.add_class::<NativeMessageClassificationReport>()?;
    module.add_class::<NativeSelectedSkillLocation>()?;
    module.add_class::<NativeCapabilityExecutionSource>()?;
    module.add_class::<NativeResolvedSkillReceipt>()?;
    module.add_class::<NativeMessageClassificationResult>()?;
    module.add_class::<NativeSkillRunReport>()?;
    module.add_class::<NativePlanningCount>()?;
    module.add_class::<NativeRoleStatistic>()?;
    module.add_class::<SessionQuery>()?;
    module.add_class::<NativeAnalysisRequest>()?;
    module.add_class::<QueryExclusions>()?;
    module.add_class::<DateRange>()?;
    module.add_class::<ResolvedDateRange>()?;
    module.add_class::<QueryScope>()?;
    module.add_class::<MessageExclusions>()?;
    module.add_class::<MessageScope>()?;
    module.add_class::<MessageSearchRequest>()?;
    module.add_class::<AnalysisQuery>()?;
    module.add_class::<NativeSkillSelector>()?;
    module.add_class::<NativeMessageClassificationQuery>()?;
    module.add_class::<NativeSkillRunQuery>()?;
    module.add_class::<FileQuery>()?;
    module.add_class::<NativeMessageHit>()?;
    module.add_class::<NativeMessageSearchResponse>()?;
    module.add_class::<NativeMessageSearchRuntimeDiagnostics>()?;
    module.add_class::<NativeMessageSearchBatch>()?;
    module.add_class::<NativeMessageSearchCompletion>()?;
    module.add_class::<NativeMessageSearchBatches>()?;
    module.add_class::<RefreshOutcome>()?;
    module.add_class::<NativeReindexOutcome>()?;
    module.add_class::<NativeProviderParserHealth>()?;
    module.add_class::<NativeParserHealth>()?;
    module.add_class::<NativeIndexStatus>()?;
    module.add_class::<NativeIndexReadinessStatus>()?;
    module.add_class::<NativeIndexRefreshStatus>()?;
    module.add_class::<NativeProviderHealth>()?;
    module.add_class::<NativeDiagnosticStatus>()?;
    module.add_class::<NativeProviderDiscoveryWarning>()?;
    module.add_class::<NativeCompactOutcome>()?;
    Ok(())
}
