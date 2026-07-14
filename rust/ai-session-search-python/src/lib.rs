use std::collections::BTreeMap;
use std::ffi::OsString;
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
use ai_session_search::models::{
    AnalysisCursor, AnalysisDocument, AnalysisDocumentPage, FileCrossRef, FileEditSummary,
    FileQuery, FileVersion, IndexStatus, MessageFilters, MessageHit, MessageKind,
    MessageSearchMode, ParserHealth, Provider, ProviderHealth, ProviderParserHealth, Role,
    SearchField, SearchFilters, SearchHit, SessionRecord,
};
use ai_session_search::service::{CompactOutcome, SessionSearch as CoreSessionSearch};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;

fn runtime_error(error: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(error.to_string())
}

#[pyfunction]
fn serve_mcp(py: Python<'_>) -> PyResult<()> {
    let sys = py.import("sys")?;
    let stdin = sys.getattr("stdin")?;
    let stdout = sys.getattr("stdout")?;
    let mut server = ai_session_search::mcp_server::McpServer::load().map_err(runtime_error)?;
    loop {
        let line = stdin.call_method0("readline")?.extract::<String>()?;
        if line.is_empty() {
            return Ok(());
        }
        server
            .handle_line(&line, |response| {
                stdout.call_method1("write", (format!("{response}\n"),))?;
                stdout.call_method0("flush")?;
                Ok::<(), PyErr>(())
            })
            .map_err(runtime_error)?;
    }
}

#[pyfunction]
fn _run_cli_command(py: Python<'_>, args: Vec<String>) -> PyResult<i32> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(OsString::from("aise"));
    argv.extend(args.into_iter().map(OsString::from));
    py.detach(move || ai_session_search::run_cli_from(argv).map_err(runtime_error))
}

fn parse_provider(value: Option<String>) -> PyResult<Option<Provider>> {
    value
        .map(|value| {
            value
                .parse()
                .map_err(|error| PyValueError::new_err(format!("invalid provider: {error}")))
        })
        .transpose()
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

#[pyclass(module = "ai_session_search._native", frozen)]
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
        }
    }
}

#[derive(Clone)]
#[pyclass(module = "ai_session_search._native", frozen, from_py_object)]
struct NativeAnalysisCursor {
    inner: AnalysisCursor,
}

#[pyclass(module = "ai_session_search._native", frozen)]
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

#[pyclass(module = "ai_session_search._native", frozen)]
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
        RustAnalysisPolicy::compile(vec![inner.clone()], Vec::new())
            .map_err(|error| PyValueError::new_err(error.to_string()))?;
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
#[pyclass(module = "ai_session_search._native", frozen, from_py_object)]
struct RelationshipRule {
    inner: RelationshipRuleSpec,
}

#[derive(Clone)]
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
        spec.compile()
            .map_err(|error| PyValueError::new_err(error.to_string()))?;
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
        let inner = spec
            .compile()
            .map_err(|error| PyValueError::new_err(error.to_string()))?;
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
        RustAnalysisPolicy::compile(Vec::new(), vec![inner.clone()])
            .map_err(|error| PyValueError::new_err(error.to_string()))?;
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

#[pyclass(module = "ai_session_search._native", frozen)]
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

#[pyclass(module = "ai_session_search._native", frozen)]
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

#[pyclass(module = "ai_session_search._native", frozen)]
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

#[pyclass(module = "ai_session_search._native", frozen)]
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

#[pyclass(module = "ai_session_search._native", frozen)]
struct NativeAnalysisResult {
    inner: Arc<AnalysisResult>,
    graph: OnceLock<SessionGraph>,
}

impl NativeAnalysisResult {
    fn from_result(_py: Python<'_>, value: AnalysisResult) -> PyResult<Self> {
        Ok(Self {
            inner: Arc::new(value),
            graph: OnceLock::new(),
        })
    }
}

#[pymethods]
impl NativeAnalysisResult {
    #[getter]
    fn sessions(&self, py: Python<'_>) -> PyResult<BTreeMap<String, Py<NativeAnalyzedSession>>> {
        self.inner
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
        self.inner
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
            .get_or_init(|| self.inner.session_graph())
            .clone();
        Py::new(py, NativeSessionGraph::from_graph(py, graph)?)
    }
}

#[pyclass(module = "ai_session_search._native", frozen)]
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

#[pyclass(module = "ai_session_search._native", frozen)]
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

#[pyclass(module = "ai_session_search._native", frozen)]
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
            .map_err(|error| PyValueError::new_err(error.to_string()))
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
        result: PyRef<'_, NativeAnalysisResult>,
    ) -> PyResult<Vec<Py<NativeAnalysisArtifact>>> {
        let plan = self.inner.clone();
        let result = Arc::clone(&result.inner);
        let artifacts = py.detach(move || plan.render(&result).map_err(runtime_error))?;
        artifacts
            .into_iter()
            .map(|artifact| Py::new(py, NativeAnalysisArtifact::from(artifact)))
            .collect()
    }

    fn publish(
        &self,
        py: Python<'_>,
        result: PyRef<'_, NativeAnalysisResult>,
    ) -> PyResult<NativeAnalysisPublicationReceipt> {
        let plan = self.inner.clone();
        let result = Arc::clone(&result.inner);
        let receipt = py.detach(move || plan.publish(&result).map_err(runtime_error))?;
        NativeAnalysisPublicationReceipt::from_receipt(py, receipt)
    }
}

#[pyclass(module = "ai_session_search._native", frozen)]
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

#[pyclass(module = "ai_session_search._native", frozen)]
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

#[pyclass(module = "ai_session_search._native", frozen)]
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

#[pyclass(module = "ai_session_search._native", frozen)]
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

#[pyclass(module = "ai_session_search._native", frozen)]
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

#[pyclass(module = "ai_session_search._native", frozen)]
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

#[pyclass(module = "ai_session_search._native", frozen)]
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

#[pyclass(module = "ai_session_search._native", frozen)]
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

#[pyclass(module = "ai_session_search._native", frozen)]
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

#[pyclass(module = "ai_session_search._native", frozen)]
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

#[pyclass(module = "ai_session_search._native", frozen)]
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

#[pyclass(module = "ai_session_search._native", frozen)]
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
            time_profile: inspection
                .time_profile
                .map(|profile| Py::new(py, NativeSessionTimeProfile::from(profile)))
                .transpose()?,
            next_commands: inspection.next_commands,
        })
    }
}

#[pyclass(module = "ai_session_search._native", frozen)]
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

#[pyclass(module = "ai_session_search._native", frozen)]
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

#[pyclass(module = "ai_session_search._native", frozen)]
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
#[pyclass(module = "ai_session_search._native", frozen, from_py_object)]
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

#[pyclass(module = "ai_session_search._native")]
struct NativeReconstructedFileVersions {
    inner: ai_session_search::files::ReconstructedFileVersions,
}

#[pyclass(module = "ai_session_search._native", frozen)]
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

#[pyclass(module = "ai_session_search._native", frozen)]
struct NativeExportDocument {
    #[pyo3(get)]
    format: String,
    #[pyo3(get)]
    content: String,
}

#[pyclass(module = "ai_session_search._native", frozen)]
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

#[pyclass(module = "ai_session_search._native", frozen)]
struct NativeProviderSourceStatus {
    #[pyo3(get)]
    provider: String,
    #[pyo3(get)]
    enabled: bool,
    #[pyo3(get)]
    roots: Vec<String>,
    #[pyo3(get)]
    discovered_files: usize,
}

#[pyclass(module = "ai_session_search._native", frozen)]
struct NativeCorrectionMatch {
    #[pyo3(get)]
    session_id: String,
    #[pyo3(get)]
    provider: String,
    #[pyo3(get)]
    timestamp: Option<String>,
    #[pyo3(get)]
    category: String,
    #[pyo3(get)]
    matched_pattern: String,
    #[pyo3(get)]
    content: String,
}

impl From<ai_session_search::models::CorrectionMatch> for NativeCorrectionMatch {
    fn from(hit: ai_session_search::models::CorrectionMatch) -> Self {
        Self {
            session_id: hit.session_id,
            provider: hit.provider.as_str().to_string(),
            timestamp: hit.ts.map(|value| value.to_rfc3339()),
            category: hit.category,
            matched_pattern: hit.matched_pattern,
            content: hit.content,
        }
    }
}

#[pyclass(module = "ai_session_search._native", frozen)]
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

#[pyclass(module = "ai_session_search._native", frozen)]
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
        }
    }
}

#[derive(Clone, Default)]
#[pyclass(module = "ai_session_search._native", frozen, from_py_object)]
struct DateRangeQuery {
    dates: ai_session_search::dates::DateRange,
}

#[derive(Clone)]
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
impl DateRangeQuery {
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
        .map_err(|error| PyValueError::new_err(error.to_string()))?;
        Ok(bounds.into())
    }
}

impl DateRangeQuery {
    fn resolve(&self) -> PyResult<ai_session_search::dates::Bounds> {
        self.dates
            .resolve_now()
            .map_err(|error| PyValueError::new_err(error.to_string()))
    }
}

#[derive(Clone, Default)]
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
#[pyclass(module = "ai_session_search._native", frozen, from_py_object)]
struct SessionQuery {
    provider: Option<Provider>,
    path_prefix: Option<String>,
    exclusions: QueryExclusions,
    #[pyo3(get)]
    current_repo: Option<String>,
    dates: DateRangeQuery,
    #[pyo3(get)]
    limit: usize,
}

#[pymethods]
impl SessionQuery {
    #[new]
    #[pyo3(signature = (*, provider=None, path_prefix=None, exclusions=None, current_repo=None, dates=None, limit=50))]
    fn new(
        provider: Option<String>,
        path_prefix: Option<String>,
        exclusions: Option<QueryExclusions>,
        current_repo: Option<String>,
        dates: Option<DateRangeQuery>,
        limit: usize,
    ) -> PyResult<Self> {
        Ok(Self {
            provider: parse_provider(provider)?,
            path_prefix,
            exclusions: exclusions.unwrap_or_default(),
            current_repo,
            dates: dates.unwrap_or_default(),
            limit,
        })
    }

    #[getter]
    fn provider(&self) -> Option<String> {
        self.provider.map(|provider| provider.as_str().to_string())
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
    fn dates(&self) -> DateRangeQuery {
        self.dates.clone()
    }
}

impl Default for SessionQuery {
    fn default() -> Self {
        Self {
            provider: None,
            path_prefix: None,
            exclusions: QueryExclusions::default(),
            current_repo: None,
            dates: DateRangeQuery::default(),
            limit: 50,
        }
    }
}

impl SessionQuery {
    fn into_filters(self) -> PyResult<(SearchFilters, Option<String>)> {
        let (since, until) = self.dates.resolve()?;
        let (exclude_path_prefixes, exclude_session_ids) = self.exclusions.into_filters();
        Ok((
            SearchFilters {
                provider: self.provider,
                path_prefix: self
                    .path_prefix
                    .as_deref()
                    .map(ai_session_search::util::normalize_path_prefix),
                exclude_path_prefixes,
                exclude_session_ids,
                since,
                until,
                limit: self.limit,
                warnings_only: false,
            },
            self.current_repo,
        ))
    }
}

#[derive(Clone, Default)]
#[pyclass(module = "ai_session_search._native", frozen, from_py_object)]
struct QueryScope {
    provider: Option<Provider>,
    session_id: Option<String>,
    session: Option<String>,
    path_prefix: Option<String>,
    exclusions: QueryExclusions,
    dates: DateRangeQuery,
}

#[pymethods]
impl QueryScope {
    #[new]
    #[pyo3(signature = (*, provider=None, session_id=None, session=None, path_prefix=None, exclusions=None, dates=None))]
    fn new(
        provider: Option<String>,
        session_id: Option<String>,
        session: Option<String>,
        path_prefix: Option<String>,
        exclusions: Option<QueryExclusions>,
        dates: Option<DateRangeQuery>,
    ) -> PyResult<Self> {
        if session_id.is_some() && session.is_some() {
            return Err(PyValueError::new_err(
                "session_id and session are mutually exclusive",
            ));
        }
        Ok(Self {
            provider: parse_provider(provider)?,
            session_id,
            session,
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
    fn session(&self) -> Option<String> {
        self.session.clone()
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
    fn dates(&self) -> DateRangeQuery {
        self.dates.clone()
    }
}

struct ResolvedQueryScope {
    provider: Option<Provider>,
    session_id: Option<String>,
    session: Option<String>,
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
            session: self.session,
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

#[derive(Clone)]
#[pyclass(module = "ai_session_search._native", frozen, from_py_object)]
struct MessageQuery {
    scope: QueryScope,
    selector: MessageSelector,
    #[pyo3(get)]
    limit: usize,
    #[pyo3(get)]
    offset: usize,
}

#[pymethods]
impl MessageQuery {
    #[new]
    #[pyo3(signature = (*, scope=None, selector=None, limit=50, offset=0))]
    fn new(
        scope: Option<QueryScope>,
        selector: Option<MessageSelector>,
        limit: usize,
        offset: usize,
    ) -> Self {
        Self {
            scope: scope.unwrap_or_default(),
            selector: selector.unwrap_or_default(),
            limit,
            offset,
        }
    }

    #[getter]
    fn scope(&self) -> QueryScope {
        self.scope.clone()
    }

    #[getter]
    fn selector(&self) -> MessageSelector {
        self.selector.clone()
    }
}

impl Default for MessageQuery {
    fn default() -> Self {
        Self {
            scope: QueryScope::default(),
            selector: MessageSelector::default(),
            limit: 50,
            offset: 0,
        }
    }
}

impl MessageQuery {
    fn into_filters(self, app: &CoreSessionSearch) -> PyResult<MessageFilters> {
        let scope = self.scope.resolve(app)?;
        let (since, until) = scope.bounds;
        Ok(MessageFilters {
            role: self.selector.role,
            kind: self.selector.kind,
            field: Some(self.selector.target.field),
            argument_path: self.selector.target.argument_path,
            provider: scope.provider,
            session_id: scope.session_id,
            session: scope.session,
            path_prefix: scope.path_prefix,
            exclude_path_prefixes: scope.exclude_path_prefixes,
            exclude_session_ids: scope.exclude_session_ids,
            since,
            until,
            seq_from: self.selector.sequence.seq_from,
            seq_to: self.selector.sequence.seq_to,
            tool: self.selector.tool,
            no_compaction: self.selector.no_compaction,
            limit: self.limit,
            offset: self.offset,
            ..Default::default()
        })
    }
}

#[derive(Clone)]
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
    fn new(scope: Option<QueryScope>, limit: usize) -> Self {
        Self {
            scope: scope.unwrap_or_default(),
            limit,
        }
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
        MessageQuery {
            scope: self.scope,
            selector: MessageSelector::default(),
            limit: self.limit,
            offset: 0,
        }
        .into_filters(app)
    }
}

#[derive(Clone, Default)]
#[pyclass(module = "ai_session_search._native", frozen, from_py_object)]
struct MessageSelector {
    role: Option<Role>,
    kind: Option<MessageKind>,
    target: MessageSearchTarget,
    sequence: MessageSequenceRange,
    tool: Option<String>,
    #[pyo3(get)]
    no_compaction: bool,
}

#[pymethods]
impl MessageSelector {
    #[new]
    #[pyo3(signature = (*, role=None, kind=None, target=None, sequence=None, tool=None, no_compaction=false))]
    fn new(
        role: Option<&str>,
        kind: Option<&str>,
        target: Option<MessageSearchTarget>,
        sequence: Option<MessageSequenceRange>,
        tool: Option<String>,
        no_compaction: bool,
    ) -> PyResult<Self> {
        Ok(Self {
            role: role
                .map(str::parse)
                .transpose()
                .map_err(PyValueError::new_err)?,
            kind: kind
                .map(str::parse)
                .transpose()
                .map_err(PyValueError::new_err)?,
            target: target.unwrap_or_default(),
            sequence: sequence.unwrap_or_default(),
            tool,
            no_compaction,
        })
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
    fn target(&self) -> MessageSearchTarget {
        self.target.clone()
    }

    #[getter]
    fn sequence(&self) -> MessageSequenceRange {
        self.sequence.clone()
    }

    #[getter]
    fn tool(&self) -> Option<&str> {
        self.tool.as_deref()
    }
}

#[derive(Clone)]
#[pyclass(module = "ai_session_search._native", frozen, from_py_object)]
struct MessageSearchTarget {
    field: SearchField,
    argument_path: Option<String>,
}

#[pymethods]
impl MessageSearchTarget {
    #[new]
    #[pyo3(signature = (*, field="content", argument_path=None))]
    fn new(field: &str, argument_path: Option<String>) -> PyResult<Self> {
        Ok(Self {
            field: field.parse().map_err(PyValueError::new_err)?,
            argument_path,
        })
    }

    #[getter]
    fn field(&self) -> &str {
        self.field.as_str()
    }

    #[getter]
    fn argument_path(&self) -> Option<&str> {
        self.argument_path.as_deref()
    }
}

impl Default for MessageSearchTarget {
    fn default() -> Self {
        Self {
            field: SearchField::Content,
            argument_path: None,
        }
    }
}

#[derive(Clone, Default)]
#[pyclass(module = "ai_session_search._native", frozen, from_py_object)]
struct MessageSequenceRange {
    #[pyo3(get)]
    seq_from: Option<i64>,
    #[pyo3(get)]
    seq_to: Option<i64>,
}

#[pymethods]
impl MessageSequenceRange {
    #[new]
    #[pyo3(signature = (*, seq_from=None, seq_to=None))]
    fn new(seq_from: Option<i64>, seq_to: Option<i64>) -> Self {
        Self { seq_from, seq_to }
    }
}

#[derive(Clone)]
#[pyclass(module = "ai_session_search._native", frozen, from_py_object)]
struct FileQueryRequest {
    scope: QueryScope,
    #[pyo3(get)]
    min_edits: Option<i64>,
    #[pyo3(get)]
    max_edits: Option<i64>,
    #[pyo3(get)]
    limit: usize,
}

#[pymethods]
impl FileQueryRequest {
    #[new]
    #[pyo3(signature = (*, scope=None, min_edits=None, max_edits=None, limit=50))]
    fn new(
        scope: Option<QueryScope>,
        min_edits: Option<i64>,
        max_edits: Option<i64>,
        limit: usize,
    ) -> Self {
        Self {
            scope: scope.unwrap_or_default(),
            min_edits,
            max_edits,
            limit,
        }
    }

    #[getter]
    fn scope(&self) -> QueryScope {
        self.scope.clone()
    }
}

impl Default for FileQueryRequest {
    fn default() -> Self {
        Self::new(None, None, None, 50)
    }
}

impl FileQueryRequest {
    fn into_query(self, pattern: Option<String>, app: &CoreSessionSearch) -> PyResult<FileQuery> {
        let scope = self.scope.resolve(app)?;
        let (since, until) = scope.bounds;
        Ok(FileQuery {
            pattern,
            provider: scope.provider,
            session_id: scope.session_id,
            session: scope.session,
            path_prefix: scope.path_prefix,
            exclude_path_prefixes: scope.exclude_path_prefixes,
            exclude_session_ids: scope.exclude_session_ids,
            since,
            until,
            min_edits: self.min_edits,
            max_edits: self.max_edits,
            limit: self.limit,
        })
    }
}

#[pyclass(module = "ai_session_search._native", frozen)]
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

#[pyclass(module = "ai_session_search._native", frozen)]
struct NativeReindexOutcome {
    #[pyo3(get)]
    files_seen: usize,
    #[pyo3(get)]
    sessions_updated: usize,
}

#[derive(Clone)]
#[pyclass(module = "ai_session_search._native", frozen, skip_from_py_object)]
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
#[pyclass(module = "ai_session_search._native", frozen, skip_from_py_object)]
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
#[pyclass(module = "ai_session_search._native", frozen, skip_from_py_object)]
struct NativeIndexStatus {
    #[pyo3(get)]
    parser_health: NativeParserHealth,
    #[pyo3(get)]
    repairable_stale_sessions: i64,
    #[pyo3(get)]
    unavailable_stale_sessions: i64,
    #[pyo3(get)]
    repair_commands: Vec<String>,
}

impl From<IndexStatus> for NativeIndexStatus {
    fn from(status: IndexStatus) -> Self {
        Self {
            parser_health: status.parser_health.into(),
            repairable_stale_sessions: status.repairable_stale_sessions,
            unavailable_stale_sessions: status.unavailable_stale_sessions,
            repair_commands: status.repair_commands,
        }
    }
}

#[derive(Clone)]
#[pyclass(module = "ai_session_search._native", frozen, skip_from_py_object)]
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
    resume_supported: bool,
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
            resume_supported: health.resume_supported,
            resume_command: health.resume_command,
        }
    }
}

#[pyclass(module = "ai_session_search._native", frozen)]
struct NativeDiagnosticStatus {
    #[pyo3(get)]
    db_path: String,
    #[pyo3(get)]
    index_status: NativeIndexStatus,
    #[pyo3(get)]
    providers: Vec<NativeProviderHealth>,
}

#[pyclass(module = "ai_session_search._native", frozen)]
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
                return Err(PyValueError::new_err(format!("{name} must not be empty")));
            }
        }
        if threads == Some(0) {
            return Err(PyValueError::new_err("threads must be greater than zero"));
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

    #[pyo3(signature = (query, request=None, *, mode="exact", lines_per_message=0))]
    /// Search messages with an optional presentation-only line window per result.
    ///
    /// Positive keeps the first N lines of every returned message, negative keeps the last N, and
    /// zero keeps complete content. Matches, ranking, result count, and pagination are unchanged.
    fn search_messages(
        &self,
        py: Python<'_>,
        query: String,
        request: Option<MessageQuery>,
        mode: &str,
        lines_per_message: i64,
    ) -> PyResult<Vec<NativeMessageHit>> {
        let mode: MessageSearchMode = mode.parse().map_err(PyValueError::new_err)?;
        py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            let mut filters = request.unwrap_or_default().into_filters(&app)?;
            let exact_query = match mode {
                MessageSearchMode::Exact => query.as_str(),
                MessageSearchMode::Regex => {
                    filters.regex = Some(query.clone());
                    ""
                }
                MessageSearchMode::Fuzzy => {
                    filters.fuzzy_query = Some(query.clone());
                    ""
                }
            };
            app.messages()
                .search(exact_query, &filters)
                .map(|hits| capped_native_hits(hits, lines_per_message))
                .map_err(runtime_error)
        })
    }

    #[pyo3(signature = (session_id, seq, *, before=5, after=5, lines_per_message=0))]
    /// Return message context with an optional presentation-only line window per message.
    ///
    /// Positive keeps the first N lines, negative keeps the last N, and zero keeps complete
    /// content. The window never removes messages from the requested context.
    fn message_context(
        &self,
        py: Python<'_>,
        session_id: String,
        seq: i64,
        before: i64,
        after: i64,
        lines_per_message: i64,
    ) -> PyResult<Vec<NativeMessageHit>> {
        if seq < 0 || before < 0 || after < 0 {
            return Err(PyValueError::new_err(
                "seq, before, and after must be non-negative",
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

    #[pyo3(signature = (session_id, *, preview_chars=None, include_time_profile=false))]
    fn inspect_session(
        &self,
        py: Python<'_>,
        session_id: String,
        preview_chars: Option<usize>,
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
        request: Option<FileQueryRequest>,
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
        request: Option<FileQueryRequest>,
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
        request: Option<FileQueryRequest>,
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
        request: Option<FileQueryRequest>,
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
        request: Option<FileQueryRequest>,
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
        request: Option<FileQueryRequest>,
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
            .map_err(|error| PyValueError::new_err(error.to_string()))?;
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
            .map_err(|error| PyValueError::new_err(error.to_string()))?;
        let plan = ai_session_search::export::ExportPublicationPlan::new(destination, format)
            .map_err(|error| PyValueError::new_err(error.to_string()))?;
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

    #[pyo3(signature = (request=None, *, cursor=None))]
    fn analysis_documents(
        &self,
        py: Python<'_>,
        request: Option<SessionQuery>,
        cursor: Option<NativeAnalysisCursor>,
    ) -> PyResult<NativeAnalysisDocumentPage> {
        let (filters, _) = request.unwrap_or_default().into_filters()?;
        let page = py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            app.analysis()
                .documents(&filters, cursor.as_ref().map(|value| &value.inner))
                .map_err(runtime_error)
        })?;
        NativeAnalysisDocumentPage::from_page(py, page)
    }

    #[pyo3(signature = (request=None, *, policy=None))]
    fn analyze_sessions(
        &self,
        py: Python<'_>,
        request: Option<SessionQuery>,
        policy: Option<AnalysisPolicy>,
    ) -> PyResult<NativeAnalysisResult> {
        let (filters, _) = request.unwrap_or_default().into_filters()?;
        let policy = policy.map(|policy| policy.inner).unwrap_or_else(|| {
            RustAnalysisPolicySpec::default()
                .compile()
                .expect("empty analysis policy is valid")
        });
        let result = py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            app.analysis().run(&filters, &policy).map_err(runtime_error)
        })?;
        NativeAnalysisResult::from_result(py, result)
    }

    #[pyo3(signature = (request=None))]
    fn find_corrections(
        &self,
        py: Python<'_>,
        request: Option<AnalysisQuery>,
    ) -> PyResult<Vec<NativeCorrectionMatch>> {
        py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            let filters = request.unwrap_or_default().into_filters(&app)?;
            app.analysis()
                .corrections(&filters)
                .map(|hits| hits.into_iter().map(NativeCorrectionMatch::from).collect())
                .map_err(runtime_error)
        })
    }

    #[pyo3(signature = (request=None, command_patterns=None))]
    fn planning_usage(
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
                .reindex(full)
                .map(|(files_seen, sessions_updated)| NativeReindexOutcome {
                    files_seen,
                    sessions_updated,
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
    module.add_class::<NativeCorrectionMatch>()?;
    module.add_class::<NativePlanningCount>()?;
    module.add_class::<NativeRoleStatistic>()?;
    module.add_class::<SessionQuery>()?;
    module.add_class::<QueryExclusions>()?;
    module.add_class::<DateRangeQuery>()?;
    module.add_class::<ResolvedDateRange>()?;
    module.add_class::<QueryScope>()?;
    module.add_class::<MessageSearchTarget>()?;
    module.add_class::<MessageSequenceRange>()?;
    module.add_class::<MessageSelector>()?;
    module.add_class::<MessageQuery>()?;
    module.add_class::<AnalysisQuery>()?;
    module.add_class::<FileQueryRequest>()?;
    module.add_class::<NativeMessageHit>()?;
    module.add_class::<RefreshOutcome>()?;
    module.add_class::<NativeReindexOutcome>()?;
    module.add_class::<NativeProviderParserHealth>()?;
    module.add_class::<NativeParserHealth>()?;
    module.add_class::<NativeIndexStatus>()?;
    module.add_class::<NativeProviderHealth>()?;
    module.add_class::<NativeDiagnosticStatus>()?;
    module.add_class::<NativeCompactOutcome>()?;
    Ok(())
}
