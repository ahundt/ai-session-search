from datetime import date, datetime
from os import PathLike
from pathlib import Path
from typing import Any, Literal, NotRequired, Self, TypedDict, final

from .types import FieldView, MatchView, MessageClassificationDefinition

_PathInput = str | PathLike[str]

_ProviderId = Literal["claude", "claude-desktop", "codex", "cursor", "antigravity", "pi", "prime-agent", "aistudio", "gemini-cli"]
_MessageRole = Literal["user", "assistant", "tool", "slash", "compaction"]
_MessageKind = Literal["conversation", "compaction", "tool_call", "tool_result", "harness_notice", "unknown"]
_SessionKind = Literal["user", "subagent"]
_SearchField = Literal["content", "tool_name", "tool_argument"]
_MessageQueryMode = Literal["literal", "regex", "fuzzy"]
_MatchWindow = Literal["earliest", "latest"]
_DetailLevel = Literal["compact", "full"]
_ReceiptLevel = Literal["none", "summary", "full"]
_MessageSearchInclude = Literal["normalized_session_metadata", "parsed_references", "raw_provider_metadata", "runtime_diagnostics"]

__all__ = [
    "AlreadyExistsError",
    "AmbiguousPrefixError",
    "AnalysisArtifact",
    "AnalysisCursor",
    "AnalysisDocument",
    "AnalysisDocumentPage",
    "AnalysisPolicy",
    "AnalysisPublicationPlan",
    "AnalysisPublicationReceipt",
    "AnalysisQuery",
    "AnalysisReceipt",
    "AnalysisRequest",
    "AnalysisResult",
    "AnalyzedSession",
    "CapabilityExecutionSource",
    "CapabilityReceipt",
    "ChangedFileEvidence",
    "ClassificationMatch",
    "ClassificationRule",
    "CompactOutcome",
    "DateRange",
    "DiagnosticStatus",
    "ExportDocument",
    "ExportPublicationReceipt",
    "FileCrossRef",
    "FileEditSummary",
    "FileQuery",
    "FileVersion",
    "IndexNotRefreshedError",
    "IndexReadinessStatus",
    "IndexRefreshStatus",
    "IndexStatus",
    "InvalidArgumentError",
    "MessageClassificationMatch",
    "MessageClassificationQuery",
    "MessageClassificationReport",
    "MessageClassificationResult",
    "MessageExclusions",
    "MessageHit",
    "MessagePreview",
    "MessageRef",
    "MessageScope",
    "MessageSearchBatch",
    "MessageSearchBatches",
    "MessageSearchCompletion",
    "MessageSearchRequest",
    "MessageSearchResponse",
    "MessageSearchRuntimeDiagnostics",
    "NotFoundError",
    "ParserHealth",
    "PhraseFrequency",
    "PhraseVocabulary",
    "PlanningCount",
    "ProviderDiscoveryWarning",
    "ProviderHealth",
    "ProviderParserHealth",
    "ProviderSourceStatus",
    "PublishedAnalysisArtifact",
    "PublishedFileVersion",
    "QueryExclusions",
    "QueryScope",
    "ReceiptedAnalysis",
    "ReconstructedFile",
    "ReconstructedFileVersions",
    "RecoveryPublicationReceipt",
    "RefEvidence",
    "RefreshOutcome",
    "ReindexOutcome",
    "RelationshipHint",
    "RelationshipRule",
    "ResolvedDateRange",
    "ResolvedSkillReceipt",
    "RoleStat",
    "SearchHit",
    "SelectedSkillLocation",
    "SessionGraph",
    "SessionGraphEdge",
    "SessionGraphGroup",
    "SessionGraphNode",
    "SessionInspection",
    "SessionQuery",
    "SessionRecord",
    "SessionSearch",
    "SessionTimeProfile",
    "SkillRunQuery",
    "SkillRunReport",
    "SkillSelector",
    "SqlSchemaResult",
    "ToolActivity",
    "UnknownConfigKey",
    "UnrecognizedRecordType",
    "_run_cli_command",
    "serve_mcp",
]

def serve_mcp() -> None: ...
def _run_cli_command(args: list[str]) -> int: ...

@final
class SessionRecord:
    """Canonical indexed session metadata and source provenance."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    id: str
    provider: str
    provider_session_id: str
    discovery_source: str
    parse_version: str
    title: str | None
    summary: str | None
    cwd: str | None
    repo_root: str | None
    created_at: str | None
    updated_at: str | None
    last_message_at: str | None
    preview_text: str
    raw_metadata_json: str | None
    source_path: str
    message_count: int | None
    parse_warning: str | None
    parent_session_id: str | None
    agent_label: str | None

@final
class AnalysisCursor:
    """Opaque keyset cursor for the next non-overlapping analysis document page."""

    def to_dict(self) -> str:
        """Return the serialized opaque cursor token."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    ...

@final
class AnalysisDocument:
    """One indexed session and its normalized user-message text for analysis."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    session: SessionRecord
    user_text: str
    first_user_text: str | None
    message_count: int
    user_message_count: int

@final
class AnalysisDocumentPage:
    """Bounded analysis document page with an optional continuation cursor."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    documents: list[AnalysisDocument]
    next_cursor: AnalysisCursor | None

@final
class ClassificationRule:
    """One weighted regex classification applied to a selected session text field."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    dimension: str
    label: str
    pattern: str
    target: Literal["title", "summary", "first_user_text", "user_text", "any"]
    weight: int

    def __new__(
        cls,
        dimension: str,
        label: str,
        pattern: str,
        *,
        target: Literal["title", "summary", "first_user_text", "user_text", "any"] = "user_text",
        weight: int = 0,
    ) -> Self: ...

@final
class RelationshipRule:
    """One regex rule identifying a branch, copy, or version relationship."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    id: str
    kind: Literal["branch", "copy", "version"]
    pattern: str

    def __new__(
        cls,
        id: str,
        kind: Literal["branch", "copy", "version"],
        pattern: str,
    ) -> Self: ...

@final
class PhraseVocabulary:
    """Bounded recurring-phrase extraction policy for analyzed user messages."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    widths: list[int]
    max_unique_phrases: int
    min_document_tokens: int
    excluded_tokens: list[str]
    exclude_numeric_tokens: bool
    prose_only: bool

    def __new__(
        cls,
        widths: list[int],
        max_unique_phrases: int,
        *,
        min_document_tokens: int = 0,
        excluded_tokens: list[str] | None = None,
        exclude_numeric_tokens: bool = True,
        prose_only: bool = False,
    ) -> Self: ...

@final
class AnalysisPolicy:
    """Validated classification, relationship, and optional phrase-analysis policy."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...
    def __new__(
        cls,
        *,
        classification_rules: list[ClassificationRule] | None = None,
        relationship_rules: list[RelationshipRule] | None = None,
        phrase_vocabulary: PhraseVocabulary | None = None,
        max_classification_chars: int | None = None,
    ) -> Self: ...

@final
class ClassificationMatch:
    """One classification label and weight matched in a session."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    dimension: str
    label: str
    target: Literal["title", "summary", "first_user_text", "user_text", "any"]
    weight: int

@final
class RelationshipHint:
    """Resolved, ambiguous, or unresolved relationship inferred for a session."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    rule_id: str
    kind: Literal["branch", "copy", "version"]
    parent_title: str
    status: Literal["unresolved", "resolved", "ambiguous"]
    resolved_session_id: str | None
    candidate_session_ids: list[str]

@final
class AnalyzedSession:
    """One session with its analysis score, classifications, and relationship hints."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    session: SessionRecord
    classifications: list[ClassificationMatch]
    score: int
    relationship_hints: list[RelationshipHint]
    has_user_text: bool
    message_count: int
    user_message_count: int

@final
class PhraseFrequency:
    """Recurring normalized phrase with document and occurrence counts."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    phrase: str
    words: int
    documents: int
    occurrences: int

@final
class AnalysisResult:
    """Typed classifications, relationships, vocabulary, and graph for analyzed sessions."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    sessions: dict[str, AnalyzedSession]
    vocabulary: list[PhraseFrequency]
    graph: SessionGraph

@final
class AnalysisReceipt:
    """Same-snapshot selection, count, and digest facts for one analysis result."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    selection_kind: Literal["all_eligible", "first_canonical_sessions"]
    max_selected_sessions: int | None
    database_schema_version: int
    selected_sessions: int
    messages_in_selected_sessions: int
    analyzed_user_messages: int
    has_more: bool
    last_selected_session_id: str | None
    max_selected_session_updated_at: str | None
    policy_digest: str
    corpus_digest: str
    result_digest: str

@final
class ReceiptedAnalysis:
    """Analysis data paired with the exact same-snapshot selection and digest receipt."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    result: AnalysisResult
    receipt: AnalysisReceipt

@final
class AnalysisArtifact:
    """Rendered analysis artifact held in memory before publication."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    name: str
    content: str
    sha256: str
    bytes: int

@final
class PublishedAnalysisArtifact:
    """Name, byte count, and SHA-256 digest of one published artifact."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    name: str
    bytes: int
    sha256: str

@final
class AnalysisPublicationReceipt:
    """Receipt for an atomically published immutable analysis bundle."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    destination: Path
    artifacts: list[PublishedAnalysisArtifact]

@final
class AnalysisPublicationPlan:
    """Immutable, no-replace publication plan for JSON and Markdown analysis artifacts.

    Every bundle includes format-independent ``analysis-receipt.v1.json`` and
    ``manifest.v1.json`` control artifacts.
    """

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...
    def __new__(cls, destination: _PathInput, formats: list[Literal["json", "markdown"]] | None = None) -> Self: ...
    @property
    def destination(self) -> Path: ...
    @property
    def formats(self) -> list[Literal["json", "markdown"]]: ...
    def render(self, analysis: ReceiptedAnalysis) -> list[AnalysisArtifact]:
        """Render the requested analysis artifacts in memory without writing files. Use publish to create the immutable bundle."""
    def publish(self, analysis: ReceiptedAnalysis) -> AnalysisPublicationReceipt:
        """Publish one immutable analysis bundle and return its receipt. The destination must not exist; an existing bundle is preserved and raises AlreadyExistsError."""

@final
class SessionGraphNode:
    """Graph node containing one analyzed session identity and classifications."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    session_id: str
    provider: str
    title: str | None
    cwd: str | None
    repo_root: str | None
    created_at: str | None
    updated_at: str | None
    score: int
    classifications: list[ClassificationMatch]

@final
class SessionGraphEdge:
    """One resolved directed relationship between two session IDs."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    source_session_id: str
    target_session_id: str
    kind: Literal["branch", "copy", "version"]
    rule_id: str

@final
class SessionGraphGroup:
    """Session IDs sharing one classification dimension and label."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    dimension: Literal["working_directory", "repository"]
    key: str
    session_ids: list[str]

@final
class SessionGraph:
    """Deterministic nodes, resolved edges, and classification groups for analyzed sessions."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    nodes: dict[str, SessionGraphNode]
    edges: list[SessionGraphEdge]
    groups: list[SessionGraphGroup]

@final
class SearchHit:
    """Ranked session search result with score and matched-field preview."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    session: SessionRecord
    score: int
    match_source: str
    match_snippet: str

@final
class MessagePreview:
    """Bounded message preview with its exact expansion command."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    seq: int
    timestamp: str | None
    chars: int
    preview: str
    expand_command: str

@final
class ToolActivity:
    """Bounded tool-call or tool-result evidence with an exact expansion command."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    seq: int
    timestamp: str | None
    tool_name: str | None
    kind: str
    chars: int
    preview: str
    expand_command: str

@final
class MessageRef:
    """One normalized URL-like reference extracted from a message."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    kind: str
    value: str
    normalized_value: str | None
    host: str | None
    source_tool: str | None
    source_field: str | None
    confidence: str
    span_start: int
    span_end: int

@final
class RefEvidence:
    """Message preview and normalized references used as session evidence."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    seq: int
    role: str
    tool_name: str | None
    ref_summary: str
    refs: list[MessageRef]
    preview: str
    expand_command: str

@final
class ChangedFileEvidence:
    """Aggregate edit count and expansion command for one changed file."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    file_path: str
    provider: str
    edits: int
    follow_up_command: str

@final
class SessionTimeProfile:
    """Observed timestamp span, gaps, and tool/message counts for one session."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    messages: int
    timestamped_messages: int
    undated_messages: int
    first_timestamp: str | None
    last_timestamp: str | None
    observed_span_seconds: int | None
    max_message_gap_seconds: int | None
    tool_calls: int
    tool_results: int

@final
class SessionInspection:
    """Compact purpose, activity, reference, file, and optional timing evidence for one session."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    session: SessionRecord
    user_intent: list[MessagePreview]
    tool_activity: list[ToolActivity]
    refs: list[RefEvidence]
    changed_files: list[ChangedFileEvidence]
    truncated_evidence: list[
        Literal[
            "user_intent",
            "tool_activity",
            "reference_messages",
            "references",
            "changed_files",
        ]
    ]
    time_profile: SessionTimeProfile | None
    next_commands: list[str]

@final
class FileEditSummary:
    """Aggregate edit and session counts for one indexed file path."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    file_path: str
    file_name: str
    edits: int
    sessions: int
    last_edited: str | None

@final
class FileVersion:
    """One causally ordered historical file version reconstructed from an edit."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    session_id: str
    provider: str
    version: int
    tool: str
    timestamp: str | None
    lines: int
    file_path: str

@final
class FileCrossRef:
    """One session-to-file edit relationship from indexed tool activity."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    file_path: str
    session_id: str
    provider: str
    edits: int

@final
class ReconstructedFile:
    """One reconstructed historical file with provenance and complete content."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    session_id: str
    provider: str
    version: int
    file_path: str
    content: str
    def restore(self, *, output_dir: _PathInput | None = None) -> Path:
        """Write a collision-safe .recovered copy and return its path; preserve existing files.

        Omit output_dir to write beside the recorded original; otherwise use only its basename
        inside output_dir. Existing copies add _2, _3, and so on.
        """

@final
class ReconstructedFileVersions:
    """Single-pass iterator over causally reconstructable file versions."""
    def __iter__(self) -> ReconstructedFileVersions: ...
    def __next__(self) -> ReconstructedFile: ...

@final
class PublishedFileVersion:
    """One published file version, with its source session, version, path, and content digest."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    source_session_id: str
    version: int
    path: Path
    sha256: str

@final
class RecoveryPublicationReceipt:
    """Receipt for an atomically published directory of recovered file versions."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    destination: Path
    files: list[PublishedFileVersion]

@final
class ExportDocument:
    """Complete rendered session export in the requested format."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    format: Literal["markdown", "text", "json"]
    content: str

@final
class ExportPublicationReceipt:
    """Receipt for an atomically published session export bundle."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    destination: Path
    format: Literal["markdown", "text", "json"]
    sessions: int
    files: list[Path]

@final
class ProviderSourceStatus:
    """Enabled roots and discovered session-file count for one provider."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    provider: str
    enabled: bool
    roots: list[str]
    discovered_files: int
    warnings: list[ProviderDiscoveryWarning]

@final
class MessageClassificationMatch:
    """One message classified by a named capability rule category."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    session_id: str
    message_seq: int
    provider: str
    timestamp: str | None
    policy_name: str
    """Which compiled classification policy produced this match.

    The name only. Version and digest appear once per run on
    :attr:`MessageClassificationReport.policies` rather than repeated on every row.
    """
    category: str
    matched_text: str
    match_start_char: int
    match_end_char_exclusive: int
    content: str

@final
class CapabilityReceipt:
    """Identity of one message-classification capability evaluated for a report."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    name: str
    version: str
    sha256: str
    """Digest of the exact resolved policy bytes.

    A name and version alone are not reproducible: a policy file can be edited
    without a version bump, and two runs reporting the same version would then
    disagree with no way to tell which rules produced which.
    """

@final
class MessageClassificationReport:
    """Classified messages and the capabilities evaluated to produce them."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    policies: list[CapabilityReceipt]
    """Every evaluated capability, in evaluation order, including any that matched nothing.

    Carried so an empty :attr:`matches` list is unambiguous: "these rules ran and
    found nothing" and "no rules ran" are different answers.
    """
    matches: list[MessageClassificationMatch]
    """Matches newest first, after ``offset`` is skipped and ``limit`` taken."""
    unattributed_input_count: int
    """Messages in scope typed while the agent was working whose transcript does not record
    who sent them (Claude Code versions that wrote no sender for queued input).

    Counted whether or not they were read. A queued message also delivered as its own prompt
    is read once, as that prompt, and not counted here.
    """
    unattributed_input_included: bool
    """Whether :attr:`matches` read those messages. ``False`` unless the query set
    ``include_unattributed_input=True``."""

@final
class SelectedSkillLocation:
    """Where the selected skill package was resolved."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    kind: Literal["embedded", "path"]
    canonical_skill_md: Path | None

@final
class CapabilityExecutionSource:
    """Where the deterministic capability bytes executed by a skill came from."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    kind: Literal["embedded", "path", "inline"]
    canonical_capability_toml: Path | None

@final
class ResolvedSkillReceipt:
    """Provenance for the package and capability selected by one skill run."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    name: str
    package_version: str | None
    selected_location: SelectedSkillLocation
    execution_source: CapabilityExecutionSource

@final
class MessageClassificationResult:
    """Typed message-classification output nested inside a skill-run report."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    receipt: CapabilityReceipt
    """Primary selected skill's policy receipt.

    This equals the first entry in ``report.policies``. The report list also
    records every additional ``--skill`` policy in evaluation order.
    """
    report: MessageClassificationReport

@final
class SkillRunReport:
    """Result and provenance from one deterministic skill invocation."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    requested_selector: SkillSelector
    resolved_skill: ResolvedSkillReceipt
    output: MessageClassificationResult

@final
class PlanningCount:
    """Slash-command usage count with distinct session and project counts."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    command: str
    count: int
    unique_sessions: int
    unique_projects: int

@final
class RoleStat:
    """Exact indexed message count for one normalized role."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    role: str
    count: int

@final
class DateRange:
    """Date bounds parsed by the same Rust grammar used by the CLI and MCP server."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    since: str | None
    until: str | None
    when: str | None

    def __new__(
        cls,
        *,
        since: str | date | datetime | None = None,
        until: str | date | datetime | None = None,
        when: str | date | datetime | None = None,
    ) -> Self: ...
    def resolve_bounds(
        self,
        *,
        reference_time: str | None = None,
    ) -> ResolvedDateRange: ...

@final
class ResolvedDateRange:
    """Concrete inclusive UTC bounds produced by resolving a DateRange."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    since: str | None
    until: str | None

@final
class QueryExclusions:
    """Reusable exclusions applied before result limits.

    ``path_prefixes`` exclude normalized filesystem path prefixes.
    ``session_ids`` exclude exact canonical session IDs.
    """

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    path_prefixes: list[str]
    session_ids: list[str]

    def __new__(
        cls,
        *,
        path_prefixes: list[_PathInput] | None = None,
        session_ids: list[str] | None = None,
    ) -> Self: ...

@final
class QueryScope:
    """Shared provider, session, path, exclusion, and date scope for typed queries."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    provider: _ProviderId | None
    session_id: str | None
    path_prefix: str | None
    exclusions: QueryExclusions
    dates: DateRange

    def __new__(
        cls,
        *,
        provider: _ProviderId | None = None,
        session_id: str | None = None,
        path_prefix: _PathInput | None = None,
        exclusions: QueryExclusions | None = None,
        dates: DateRange | None = None,
    ) -> Self: ...

@final
class SessionQuery:
    """Session list/search filters; None uses search.default_limit, 0 selects every match.

    ``dates`` intersects an inclusive query period with each known indexed session span from
    ``created_at`` through ``updated_at``. The span can contain gaps and does not assert continuous
    process activity. ``current_repo`` overrides repository-aware ranking. When omitted,
    ``search_sessions`` honors configured ``prefer_current_repo`` behavior and derives the
    repository from the working directory.
    """

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    provider: _ProviderId | None
    path_prefix: str | None
    exclusions: QueryExclusions
    session_kinds: list[_SessionKind] | None
    parent_session_id: str | None
    warnings_only: bool
    current_repo: str | None
    dates: DateRange
    limit: int | None
    offset: int

    def __new__(
        cls,
        *,
        provider: _ProviderId | None = None,
        path_prefix: _PathInput | None = None,
        exclusions: QueryExclusions | None = None,
        session_kinds: list[_SessionKind] | None = None,
        parent_session_id: str | None = None,
        warnings_only: bool = False,
        current_repo: _PathInput | None = None,
        dates: DateRange | None = None,
        limit: int | None = None,
        offset: int = 0,
        scope: QueryScope | None = None,
    ) -> Self: ...

@final
class SqlSchemaResult:
    """Schema objects or columns with semantic notes from SqlService."""

    columns: list[str]
    rows: list[dict[str, Any]]
    truncated: bool
    truncated_cell_char_limit: int | None

    def to_dict(self) -> dict[str, Any]: ...
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

@final
class AnalysisRequest:
    """Scope and explicit population strategy for longitudinal session analysis.

    Omit ``first_canonical_sessions`` to analyze every eligible session. A positive value selects
    the first N eligible sessions in canonical session-ID order; it is not a recency sample,
    representative sample, or message limit.
    """

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    provider: _ProviderId | None
    path_prefix: str | None
    exclusions: QueryExclusions
    session_kinds: list[_SessionKind] | None
    parent_session_id: str | None
    warnings_only: bool
    dates: DateRange
    first_canonical_sessions: int | None

    def __new__(
        cls,
        *,
        provider: _ProviderId | None = None,
        path_prefix: _PathInput | None = None,
        exclusions: QueryExclusions | None = None,
        session_kinds: list[_SessionKind] | None = None,
        parent_session_id: str | None = None,
        warnings_only: bool = False,
        dates: DateRange | None = None,
        first_canonical_sessions: int | None = None,
    ) -> Self: ...

@final
class MessageExclusions:
    """Workspace, transcript, and session exclusions for message search."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    workspace_path_prefixes: list[str]
    transcript_path_prefixes: list[str]
    session_ids: list[str]

    def __new__(
        cls,
        *,
        workspace_path_prefixes: list[_PathInput] | None = None,
        transcript_path_prefixes: list[_PathInput] | None = None,
        session_ids: list[str] | None = None,
    ) -> Self: ...

@final
class MessageScope:
    """Message-only scope with distinct workspace and transcript path domains."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    providers: list[_ProviderId] | None
    session_id: str | None
    workspace_path_prefix: str | None
    transcript_path_prefix: str | None
    exclusions: MessageExclusions
    dates: DateRange

    def __new__(
        cls,
        *,
        providers: list[_ProviderId] | None = None,
        session_id: str | None = None,
        workspace_path_prefix: _PathInput | None = None,
        transcript_path_prefix: _PathInput | None = None,
        exclusions: MessageExclusions | None = None,
        dates: DateRange | None = None,
    ) -> Self: ...

@final
class MessageSearchRequest:
    """Canonical message predicates, match window, presentation, extent, purpose, and receipt.

    ``role="user"`` searches recorded user input across source roles, including queued
    submissions and linked answers. Known parent-agent prompts sent to spawned sessions are excluded
    from this selector. Omit ``role`` to search complete content. Sender/delivery uncertainty
    remains in provenance; views/context may include generated text. Coordinates refer to the
    selected field.

    The query searches only ``field``: ``content``, ``tool_name``, or ``tool_argument``. Tool
    arguments use an RFC 6901 ``argument_path``. ``tool_name_contains`` is an additional
    case-insensitive substring filter on canonical ``tool_name``, independent of ``field``.
    Sequence bounds are inclusive and session-local. When no configured operation/purpose
    default applies, omitting ``limit`` returns all literal, regex, or no-text Python matches;
    fuzzy search requires a positive limit. ``all_results=True`` states the complete-corpus
    request explicitly and conflicts with ``limit``. MCP separately uses a bounded default.
    """

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    query_mode: _MessageQueryMode | None
    scope: MessageScope
    role: _MessageRole | None
    kind: _MessageKind | None
    kinds: list[_MessageKind] | None
    field: _SearchField
    argument_path: str | None
    seq_from: int | None
    seq_to: int | None
    tool_name_contains: str | None
    include_compaction: bool
    limit: int | None
    all_results: bool
    offset: int
    match_window: _MatchWindow | None
    context: int | None
    context_before: int | None
    context_after: int | None
    include: list[_MessageSearchInclude] | None
    lines_per_message: int | None
    detail: _DetailLevel | None
    field_view: FieldView | None
    match_view: MatchView | None
    purpose: str | None
    purpose_version: int | None
    receipt_level: _ReceiptLevel | None

    def __new__(
        cls,
        *,
        scope: MessageScope | QueryScope | None = None,
        role: _MessageRole | None = None,
        kind: _MessageKind | None = None,
        kinds: list[_MessageKind] | None = None,
        field: _SearchField = "content",
        argument_path: str | None = None,
        seq_from: int | None = None,
        seq_to: int | None = None,
        tool_name_contains: str | None = None,
        include_compaction: bool = True,
        limit: int | None = None,
        all_results: bool = False,
        offset: int = 0,
        match_window: _MatchWindow | None = None,
        context: int | None = None,
        context_before: int | None = None,
        context_after: int | None = None,
        include: list[_MessageSearchInclude] | None = None,
        lines_per_message: int | None = None,
        detail: _DetailLevel | None = None,
        field_view: dict[str, str | int] | None = None,
        match_view: dict[str, str | int] | None = None,
        purpose: str | None = None,
        purpose_version: int | None = None,
        receipt_level: _ReceiptLevel | None = None,
        query_mode: _MessageQueryMode | None = None,
        provider: _ProviderId | None = None,
        session_id: str | None = None,
        path_prefix: _PathInput | None = None,
        dates: DateRange | None = None,
    ) -> Self: ...

@final
class AnalysisQuery:
    """Scope and maximum returned aggregate rows for planning and role_statistics; 50 by default, 0 for all."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    scope: QueryScope
    limit: int

    def __new__(
        cls,
        *,
        scope: QueryScope | None = None,
        limit: int = 50,
    ) -> Self: ...

@final
class SkillSelector:
    """Exactly one deterministic skill selected by standard name or package path."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    name: str | None
    path: Path | None

    def __new__(
        cls,
        *,
        name: str | None = None,
        path: _PathInput | None = None,
    ) -> Self: ...

@final
class MessageClassificationQuery:
    """Typed arguments for the message-classification skill capability."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    scope: QueryScope
    session_kinds: list[str] | None
    """Session classes to scan.

    ``None`` uses this operation's own default of user-started sessions only,
    because this classifier targets person-authored feedback; in a spawned run
    the ``user`` rows contain the calling agent's delegation prompt. ``["user",
    "subagent"]`` scans both. ``[]`` deliberately matches nothing.
    """
    additional_skills: list[SkillSelector]
    """Additional same-capability packages evaluated after the primary skill."""
    limit: int | None
    """Max matches, or ``None`` to use ``[capabilities.message_classification].limit``.

    ``0`` means every match. ``None`` resolves at call time rather than here,
    because the value belongs to the :class:`SessionSearch` configuration.
    """
    offset: int
    """Matches to skip before ``limit`` applies, newest first. ``0`` starts at the newest."""
    include_unattributed_input: bool
    """Also classify messages typed while the agent was working whose transcript does not
    record who sent them.

    ``False`` by default because a program may have sent them; the report counts them in
    :attr:`MessageClassificationReport.unattributed_input_count` either way.
    """

    def __new__(
        cls,
        *,
        scope: QueryScope | None = None,
        session_kinds: list[str] | None = None,
        additional_skills: list[SkillSelector] | None = None,
        limit: int | None = None,
        offset: int = 0,
        include_unattributed_input: bool = False,
    ) -> Self: ...

@final
class SkillRunQuery:
    """One typed deterministic skill invocation."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    skill: SkillSelector
    definition: MessageClassificationDefinition | None
    input: MessageClassificationQuery

    def __new__(
        cls,
        *,
        skill: SkillSelector,
        input: MessageClassificationQuery,
        definition: MessageClassificationDefinition | None = None,
    ) -> Self: ...

@final
class FileQuery:
    """File filters with deterministic ``limit``/``offset`` pages; zero limit means unlimited."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    scope: QueryScope
    min_edits: int | None
    max_edits: int | None
    limit: int
    offset: int

    def __new__(
        cls,
        *,
        scope: QueryScope | None = None,
        min_edits: int | None = None,
        max_edits: int | None = None,
        limit: int = 0,
        offset: int = 0,
        provider: _ProviderId | None = None,
        session_id: str | None = None,
        path_prefix: _PathInput | None = None,
        exclusions: QueryExclusions | None = None,
        dates: DateRange | None = None,
    ) -> Self: ...

@final
class MessageHit:
    """One indexed message with canonical session, role, kind, tool, and content fields.

    Provenance origin and delivery are None when source evidence is absent.
    """

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    provenance: dict[str, str | None] | None
    session_id: str
    provider: str
    seq: int
    role: str
    kind: str
    timestamp: str | None
    tool_name: str | None
    tool_call_id: str | None
    fuzzy_score: int | None
    content: str
    refs: list[MessageRef]
    ref_summary: str

class _MessageSearchTarget(TypedDict):
    field: _SearchField
    argument_path: NotRequired[str]

class _MessageSearchEffectiveRequest(TypedDict):
    """Resolved selection and presentation choices applied to this response."""

    query: NotRequired[str]
    query_mode: NotRequired[_MessageQueryMode]
    target: _MessageSearchTarget
    provider_scope: dict[str, object]
    extent: dict[str, object]
    match_window: NotRequired[_MatchWindow]
    context: dict[str, int]
    presentation: dict[str, object]
    include: list[_MessageSearchInclude]
    receipt_level: _ReceiptLevel

class _MessageSearchMessageRef(TypedDict):
    session_id: str
    message_seq: int

class _MessageSearchMessageMetadata(TypedDict):
    provenance: NotRequired[dict[str, str | None]]
    provider: _ProviderId
    role: str
    kind: _MessageKind

class _MessageSearchViewMarker(TypedDict):
    view_start_char: int
    view_end_char_exclusive: int

class _MessageSearchFieldView(TypedDict):
    text: str
    field_start_char: int
    field_end_char_exclusive: int
    markers: NotRequired[list[_MessageSearchViewMarker]]
    additional_field_text: Literal["none", "before", "after", "before_and_after"]
    field_total_chars: int | None

class _MessageSearchPresentation(TypedDict):
    field_view: _MessageSearchFieldView
    match_view: NotRequired[_MessageSearchFieldView]

class _MessageSearchLiteralOccurrence(TypedDict):
    text: str
    field_start_char: int
    field_end_char_exclusive: int

class _MessageSearchMatch(TypedDict):
    field: _SearchField
    argument_path: NotRequired[str]
    fuzzy_score: NotRequired[int]
    literal_occurrence: NotRequired[_MessageSearchLiteralOccurrence]

class _MessageSearchContextMessage(TypedDict):
    message_ref: _MessageSearchMessageRef
    message_metadata: _MessageSearchMessageMetadata
    timestamp: str | None
    tool_name: str | None
    tool_call_id: str | None
    presentation: _MessageSearchPresentation

class _MessageSearchContext(TypedDict):
    messages_before: list[_MessageSearchContextMessage]
    messages_after: list[_MessageSearchContextMessage]

class _MessageSearchResultIncluded(TypedDict):
    parsed_references: list[dict[str, object]]

class _MessageSearchResult(TypedDict):
    timestamp: str | None
    message_ref: _MessageSearchMessageRef
    message_metadata: _MessageSearchMessageMetadata
    match: NotRequired[_MessageSearchMatch]
    presentation: _MessageSearchPresentation
    included: NotRequired[_MessageSearchResultIncluded]
    context: NotRequired[_MessageSearchContext]

class _MessageSearchPage(TypedDict):
    returned: int
    limit: int | None
    offset: int
    has_more: bool
    next_offset: int | None
    earlier_results: Literal["none", "present", "unknown"]
    result_set_extent: Literal["all", "partial", "unknown"]
    ordering: Literal["session-sequence", "fuzzy-relevance"]
    consistency: Literal["per-call"]

class _MessageSearchIncluded(TypedDict, total=False):
    normalized_session_metadata: dict[str, dict[str, object]]
    raw_provider_metadata: dict[str, object]
    runtime_diagnostics: dict[str, object]

class _MessageSearchReceipt(TypedDict, total=False):
    search_explanation: dict[str, object]
    parameter_origins: dict[str, dict[str, object]]
    ordered_digest: str

@final
class MessageSearchResponse:
    """Canonical version-1 response; ``results`` is the ordinary materialized Python list."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    response_schema_version: int
    coordinate_unit: Literal["unicode_scalar"]
    effective_request: _MessageSearchEffectiveRequest
    results: list[_MessageSearchResult]
    page: _MessageSearchPage
    included: _MessageSearchIncluded | None
    receipt: _MessageSearchReceipt | None

@final
class MessageSearchRuntimeDiagnostics:
    """Package, database, response-contract, surface, and configuration identity for one request."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    package_version: str
    database_schema_version: int
    response_schema_version: int
    surface: Literal["rust", "cli", "mcp", "python"]
    config_digest: str

@final
class MessageSearchBatch:
    """One owned canonical result batch with newly encountered included data."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    results: list[_MessageSearchResult]
    included: _MessageSearchIncluded

@final
class MessageSearchCompletion:
    """Canonical terminal page and optional receipt after natural batch-stream exhaustion."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    page: _MessageSearchPage
    receipt: _MessageSearchReceipt | None

@final
class MessageSearchBatches:
    """Advanced context-managed exhaustive batches; prefer ``search_messages`` for a normal list."""

    coordinate_unit: Literal["unicode_scalar"]
    runtime_diagnostics: MessageSearchRuntimeDiagnostics | None
    def __iter__(self) -> Self: ...
    def __next__(self) -> MessageSearchBatch: ...
    def __enter__(self) -> Self: ...
    def __exit__(self, exception_type: object, exception: object, traceback: object) -> Literal[False]: ...
    @property
    def completion(self) -> MessageSearchCompletion: ...
    def close(self) -> None:
        """Interrupt unread work and release the SQLite snapshot. Repeated calls are safe."""
        ...

@final
class RefreshOutcome:
    """Outcome of an opportunistic incremental index refresh."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    status: str
    files_seen: int | None
    sessions_updated: int | None
    reason: str | None

@final
class ReindexOutcome:
    """Session-file and changed-session counts from an explicit reindex."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    files_seen: int
    sessions_updated: int
    discovery_warnings: list[ProviderDiscoveryWarning]

@final
class ProviderParserHealth:
    """Expected parser version and current/stale counts for one provider."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    provider: str
    expected_parse_version: str
    indexed_sessions: int
    current_sessions: int
    stale_sessions: int

@final
class ParserHealth:
    """Aggregate schema and parser-version freshness across indexed sessions."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    schema_version: int
    expected_schema_version: int
    schema_current: bool
    indexed_sessions: int
    current_sessions: int
    stale_sessions: int
    parse_warnings: int
    providers: list[ProviderParserHealth]
    unrecognized_record_types: list[UnrecognizedRecordType]

@final
class UnrecognizedRecordType:
    """One record type a provider's parser did not recognize, across the sessions containing it.

    ``records`` were not indexed; ``sessions`` counts indexed sessions holding at least one.
    """

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    provider: str
    record_type: str
    records: int
    sessions: int

@final
class IndexStatus:
    """Parser/schema freshness and applicable repair commands for the index."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    parser_health: ParserHealth
    unindexed_files: int
    repairable_stale_sessions: int
    unavailable_stale_sessions: int
    repair_commands: list[str]
    readiness: IndexReadinessStatus

@final
class IndexReadinessStatus:
    """Snapshot usability and automatic refresh state reported independently."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    snapshot_availability: Literal["unavailable", "usable"]
    last_successful_refresh_at: str | None
    refresh: IndexRefreshStatus

@final
class IndexRefreshStatus:
    """Bounded durable progress and recovery for automatic index refresh."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    state: Literal["not_started", "indexing", "fresh", "postponed", "failed_with_recovery"]
    started_by: Literal["integration_install", "command_line", "mcp"] | None
    started_at: str | None
    finished_at: str | None
    files_discovered: int | None
    files_processed: int | None
    sessions_updated: int | None
    retry_after_ms: int | None
    message: str | None
    next_command: str | None

@final
class ProviderHealth:
    """Discovery, parser, index, CLI, and resume status for one provider."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    provider: str
    enabled: bool
    cli_available: bool
    roots: list[str]
    discovered_files: int
    indexed_sessions: int
    expected_parse_version: str
    current_sessions: int
    stale_sessions: int
    repairable_stale_sessions: int
    unavailable_stale_sessions: int
    resume_command: str | None
    unindexed_files: int

@final
class UnknownConfigKey:
    """An ignored configuration key and its optional suggested spelling."""

    path: str
    suggestion: str | None

    def to_dict(self) -> dict[str, Any]: ...
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

@final
class DiagnosticStatus:
    """Database, parser, automatic-update, and provider health report."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    db_path: str
    index_status: IndexStatus
    unknown_config_keys: list[UnknownConfigKey]
    discovery_warnings: list[ProviderDiscoveryWarning]
    providers: list[ProviderHealth]

@final
class ProviderDiscoveryWarning:
    """One non-fatal provider traversal or metadata-sidecar discovery failure."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    provider: str
    path: str
    operation: str
    message: str
    readable_sources_preserved: bool
    verification_command: str
    guidance: str

@final
class CompactOutcome:
    """Database byte counts before and after successful compaction."""

    def to_dict(self) -> dict[str, Any]:
        """Return a detached JSON-ready dictionary using structured-output field names."""
    def __repr__(self) -> str: ...
    def __eq__(self, other: object, /) -> bool: ...

    before_bytes: int
    after_bytes: int
    reclaimed_bytes: int

@final
class SessionSearch:
    """Rust-backed search, recovery, export, and analysis service.

    Opening loads configuration and the database, not transcripts. Call refresh() or
    prepare_for_read() before a first read; index_refresh="existing-only" requires an existing
    index. Session IDs accept canonical IDs or unique prefixes. Use close() or a with block
    to release the connection; independent iterators own their own resources.
    """
    def close(self) -> None:
        """Release the connection and owned workers; repeated calls are safe."""
    def __enter__(self) -> Self: ...
    def __exit__(self, exception_type: object, exception: object, traceback: object) -> bool: ...
    def prepare_for_read(self) -> RefreshOutcome | None:
        """Apply the configured refresh policy before reading; existing-only never refreshes."""
    def classify_messages(self, *, skill: str = "corrections", request: MessageClassificationQuery | None = None, include_unattributed_input: bool | None = None, definition: MessageClassificationDefinition | None = None) -> list[MessageClassificationMatch]:
        """Return classification match rows directly; run_skill also returns policy receipts."""
    def vocabulary(self, *, prefix: str | None = None, trigram: bool = False, limit: int | None = None) -> list[dict[str, Any]]:
        """Report index-wide terms and counts; refused under allowed-roots."""
    def repeats(self, query: str | None = None, *, scope: QueryScope | None = None, role: str | None = None, query_mode: str = "literal", limit: int = 0, include_unattributed_input: bool = False, max_groups: int | None = None, max_examples_per_group: int | None = None, min_matches: int | None = None, phrase_min_words: int | None = None, phrase_max_words: int | None = None, context: int = 0) -> dict[str, Any]:
        """Mine recurring phrases with examples and unattributed-input counts."""
    def query_sql(self, sql: str, *, limit: int | None = None, offset: int = 0, timeout_ms: int | None = None, max_cell_chars: int = 0) -> dict[str, Any]:
        """Run one read-only SQL statement with configured access restrictions and bounds."""
    def resume_command(self, session_id: str) -> dict[str, Any]:
        """Return native resume argv and cwd, or the reason the session cannot be resumed."""
    def transcript(self, session_id: str, *, lines: int = 0) -> dict[str, Any]:
        """Read first N (positive), last N (negative), or all (0) transcript lines."""

    def __new__(
        cls,
        db_path: _PathInput | None = None,
        *,
        config_path: _PathInput | None = None,
        cache_dir: _PathInput | None = None,
        threads: int | None = None,
        index_refresh: Literal["auto", "before-query", "existing-only"] | None = None,
    ) -> Self: ...
    @property
    def db_path(self) -> Path: ...
    def message_search_spec(self) -> dict[str, object]:
        """Return the parameter catalogue and this instance's configured Python defaults.

        Values are JSON-compatible. Defaults come from the same Rust planner used by
        :meth:`search_messages`; this call does not search indexed session data.
        """
        ...
    def search_messages(
        self,
        query: str = "",
        request: MessageSearchRequest | None = None,
        *,
        query_mode: _MessageQueryMode | None = None,
    ) -> MessageSearchResponse:
        """Search through the shared planner and return results, context, paging, and receipts.

        Fuzzy mode matches query letters in order, allowing gaps; extra query letters can prevent
        a match. It scores the eligible corpus before retaining the requested finite page.

        ``literal`` is the default case-insensitive substring match. ``regex`` uses Rust regex
        syntax. ``fuzzy`` scores every structurally eligible row by matching letters in order,
        then applies a deterministic finite offset and limit. It requires at least three query
        characters and does not support all-results output. With no explicit, purpose, or
        operation limit, Python returns every literal, regex, or no-text match;
        ``all_results=True`` states that complete-corpus choice explicitly.
        """
        ...
    def search_message_batches(
        self,
        query: str = "",
        request: MessageSearchRequest | None = None,
        *,
        query_mode: _MessageQueryMode | None = None,
        batch_rows: int = 256,
    ) -> MessageSearchBatches:
        """Open advanced exhaustive batches.

        Prefer ``search_messages`` for ordinary use. ``batch_rows`` must be positive and changes
        handoff frequency and active memory, never membership or ordering. Use this object with
        ``with`` so early exits release the snapshot immediately.
        """
        ...
    def message_context(
        self,
        session_id: str,
        seq: int,
        *,
        context: int = 0,
        context_before: int | None = None,
        context_after: int | None = None,
        lines_per_message: int = 0,
    ) -> list[MessageHit]:
        """Return the messages around ``seq``. ``context`` is a symmetric radius;
        ``context_before``/``context_after`` override each side (grep ``-C``/``-B``/``-A``);
        ``lines_per_message`` never removes messages from that context."""
        ...
    def read_session_messages(
        self,
        session_id: str,
        *,
        order: str = "oldest",
        role: str | None = None,
        limit: int = 0,
        offset: int = 0,
        seq_from: int | None = None,
        seq_to: int | None = None,
        lines_per_message: int = 0,
        kinds: list[_MessageKind] | None = None,
        dates: DateRange | None = None,
    ) -> list[MessageHit]:
        """Read one session's messages, selecting the oldest or newest ``limit`` by ``order``
        (``"oldest"``/``"newest"``), always returned chronologically. To page a long session,
        advance ``seq_from`` (next chunk = last seq + 1) rather than growing ``limit``."""
        ...
    def schema(self, *, table: str | None = None, include_internal: bool = False) -> SqlSchemaResult:
        """Return schema objects or columns with semantic notes; allowed under every access scope."""
        ...
    def inspect_session(
        self,
        session_id: str,
        *,
        preview_chars: int | None = None,
        summary_items: int | None = None,
        include_time_profile: bool = False,
    ) -> SessionInspection:
        """Return compact evidence; positive items select first, negative last, and zero all."""
        ...
    def list_sessions(
        self,
        request: SessionQuery | None = None,
    ) -> list[SessionRecord]:
        """List matching sessions in database order.

        Work follows the Rust catalog query: worst-case session scan/sort plus output proportional
        to returned row and text bytes. ``request.limit=0`` can return the complete eligible set.
        """
        ...
    def search_sessions(
        self,
        query: str,
        request: SessionQuery | None = None,
    ) -> list[SearchHit]:
        """Rank matching sessions after metadata and session-span filtering.

        Empty queries are deprecated; use list_sessions(request) instead.

        A finite limit bounds retained top-K results, not eligible transcript bytes scored;
        ``request.limit=0`` can retain and return every match.
        """
        ...
    def search_files(
        self,
        pattern: _PathInput | None = None,
        request: FileQuery | None = None,
    ) -> list[FileEditSummary]:
        """List edited files matching a filename or glob pattern, with edit/session counts and last edit time. Omit the pattern for every file. FileQuery supplies scope and paging; its default limit=0 returns all matches."""
    def file_history(
        self,
        file: _PathInput,
        request: FileQuery | None = None,
    ) -> list[FileVersion]:
        """Return recorded versions of a file in stable session/version order. FileQuery narrows the source sessions and dates; limit=0 returns all versions, and offset skips versions."""
    def cross_reference_files(
        self,
        pattern: _PathInput | None = None,
        request: FileQuery | None = None,
    ) -> list[FileCrossRef]:
        """Count edits per file and session for a filename or glob pattern. FileQuery applies scope and paging; an omitted pattern selects every file."""
    def reconstruct_file(
        self,
        file: _PathInput,
        *,
        version: int | None = None,
        request: FileQuery | None = None,
    ) -> ReconstructedFile:
        """Replay recorded writes and edits and return complete file content in memory. version is 1-based; None selects the latest version. If the file matches several sessions, pass FileQuery(session_id=...) to select one. This call writes no file; use the result restore(output_dir=...) to publish a copy."""
    def reconstruct_file_versions(
        self,
        file: _PathInput,
        *,
        request: FileQuery | None = None,
    ) -> ReconstructedFileVersions:
        """Iterate reconstructable historical versions of one file without writing them. If several sessions match, select one with FileQuery(session_id=...). The iterator owns its selected rows and does not hold the parent service lock while consumed."""
    def publish_file_versions(
        self,
        file: _PathInput,
        destination: _PathInput,
        *,
        request: FileQuery | None = None,
    ) -> RecoveryPublicationReceipt:
        """Write reconstructable file versions to a new immutable destination directory and return paths and hashes. The destination must not exist; an existing directory is preserved and raises AlreadyExistsError. FileQuery selects the source session and history."""
    def export_session(
        self,
        session_id: str,
        format: Literal["markdown", "md", "text", "json"] = "markdown",
    ) -> ExportDocument:
        """Render one complete session as markdown, json, or text and return an ExportDocument in memory. session_id accepts a canonical ID or unique prefix. This call writes no file."""
    def export_sessions(
        self,
        destination: _PathInput,
        request: SessionQuery | None = None,
        *,
        format: Literal["markdown", "md", "text", "json"] = "markdown",
    ) -> ExportPublicationReceipt:
        """Publish selected sessions to a new destination as json or text and return a receipt. The destination must not exist. SessionQuery supplies filters; its historical export default is 50 sessions, and an explicit limit=0 selects all."""
    def source_inventory(self) -> list[ProviderSourceStatus]:
        """Return configured provider roots, discovered file counts and non-fatal traversal warnings. Disabled providers are reported without scanning their transcript roots."""
    def analysis_documents(
        self,
        request: SessionQuery | None = None,
        *,
        cursor: AnalysisCursor | None = None,
    ) -> AnalysisDocumentPage:
        """Return one bounded keyset page of analysis documents.

        ``request.limit`` must be a positive page size. ``SessionQuery(limit=0)`` remains the
        all-matches spelling for APIs that materialize complete results, but is rejected here.
        Follow ``next_cursor`` until it is ``None`` to traverse every eligible document.
        """
        ...
    def analyze(
        self,
        request: AnalysisRequest | None = None,
        *,
        policy: AnalysisPolicy | None = None,
    ) -> ReceiptedAnalysis:
        """Analyze every eligible session by default, or the explicit selection and typed policy."""
        ...
    def run_skill(
        self,
        request: SkillRunQuery,
    ) -> SkillRunReport:
        """Execute one deterministic skill capability.

        Selected ``aise-capability.toml`` documents share a 1 MiB aggregate parsing
        safety ceiling. Exceeding it raises an actionable error with byte counts;
        rules and results are never truncated to fit.
        """
        ...
    def planning(
        self,
        request: AnalysisQuery | None = None,
        command_patterns: list[str] | None = None,
    ) -> list[PlanningCount]:
        """Count recorded slash commands with occurrence, distinct-session and distinct-project totals. AnalysisQuery supplies scope and a returned-row limit (50 by default, 0 for all). command_patterns adds Rust regex filters to the configured planning_commands patterns."""
    def role_statistics(
        self,
        request: AnalysisQuery | None = None,
    ) -> list[RoleStat]:
        """Count indexed messages by their normalized storage role, in role order. AnalysisQuery supplies scope and a returned-row limit (50 by default, 0 for all)."""
    def refresh(self) -> RefreshOutcome:
        """Attempt an opportunistic incremental index refresh and return status, counts and a skip reason. A recent refresh or a busy writer can skip work. Use reindex() for a synchronous pass that waits for the writer lock."""
    def index_status(self) -> IndexStatus:
        """Return snapshot readiness, parser/schema freshness, indexed/current/stale session counts and applicable repair commands. doctor() also reports provider discovery and resume-tool availability."""
    def reindex(self, *, full: bool = False) -> ReindexOutcome:
        """Reindex enabled sources synchronously and return file/session counts and discovery warnings. full=False reads new or changed transcripts; full=True reparses every discoverable transcript. This call waits for the writer lock."""
    def doctor(self) -> DiagnosticStatus:
        """Return full index health, provider discovery and resume-tool availability, as aise doctor and MCP get_index_status do."""
        ...
    def diagnostics(self) -> DiagnosticStatus:
        """Deprecated: use doctor() for full index health and provider discovery."""
    def compact(self) -> CompactOutcome:
        """Merge full-text segments, VACUUM the index and truncate its WAL. Return exact before_bytes, after_bytes and reclaimed_bytes. Requires a writable compatible index."""

class NotFoundError(RuntimeError):
    """The requested session, file, or existing index was not found."""

class AmbiguousPrefixError(RuntimeError):
    """The session prefix identifies more than one session."""

class AlreadyExistsError(RuntimeError):
    """The publication destination already exists and was preserved."""

class InvalidArgumentError(RuntimeError, ValueError, OverflowError):
    """An argument or combination of arguments is invalid."""

class IndexNotRefreshedError(RuntimeError):
    """Refresh or prepare_for_read before querying this index."""
