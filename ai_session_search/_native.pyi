from pathlib import Path
from typing import Literal, Self, final

_ProviderId = Literal["claude", "claude-desktop", "codex", "cursor", "antigravity", "pi", "aistudio", "gemini-cli"]
_MessageRole = Literal["user", "assistant", "tool", "slash", "compaction"]
_MessageKind = Literal["conversation", "compaction", "tool_call", "tool_result", "harness_notice", "unknown"]
_SessionKind = Literal["user", "subagent"]
_SearchField = Literal["content", "tool_name", "tool_argument"]
_MessageQueryMode = Literal["literal", "regex", "fuzzy"]
_MatchWindow = Literal["earliest", "latest"]
_DetailLevel = Literal["compact", "full"]
_ReceiptLevel = Literal["none", "summary", "full"]
_MessageSearchInclude = Literal["normalized_session_metadata", "parsed_references", "raw_provider_metadata", "runtime_diagnostics"]

__all__ = [  # noqa: RUF022 - match the extension module's canonical export order
    "serve_mcp",
    "_run_cli_command",
    "SessionSearch",
    "SessionRecord",
    "AnalysisCursor",
    "AnalysisDocument",
    "AnalysisDocumentPage",
    "ClassificationRule",
    "RelationshipRule",
    "PhraseVocabulary",
    "AnalysisPolicy",
    "ClassificationMatch",
    "RelationshipHint",
    "AnalyzedSession",
    "PhraseFrequency",
    "AnalysisResult",
    "AnalysisArtifact",
    "PublishedAnalysisArtifact",
    "AnalysisPublicationReceipt",
    "AnalysisPublicationPlan",
    "SessionGraphNode",
    "SessionGraphEdge",
    "SessionGraphGroup",
    "SessionGraph",
    "SearchHit",
    "MessagePreview",
    "ToolActivity",
    "MessageRef",
    "RefEvidence",
    "ChangedFileEvidence",
    "SessionTimeProfile",
    "SessionInspection",
    "FileEditSummary",
    "FileVersion",
    "FileCrossRef",
    "ReconstructedFile",
    "ReconstructedFileVersions",
    "RecoveryPublicationReceipt",
    "ExportDocument",
    "ExportPublicationReceipt",
    "ProviderSourceStatus",
    "MessageClassificationMatch",
    "CapabilityReceipt",
    "MessageClassificationReport",
    "SelectedSkillLocation",
    "CapabilityExecutionSource",
    "ResolvedSkillReceipt",
    "MessageClassificationResult",
    "SkillRunReport",
    "PlanningCount",
    "RoleStat",
    "SessionQuery",
    "QueryExclusions",
    "DateRange",
    "ResolvedDateRange",
    "QueryScope",
    "MessageExclusions",
    "MessageScope",
    "MessageSearchRequest",
    "AnalysisQuery",
    "SkillSelector",
    "MessageClassificationQuery",
    "SkillRunQuery",
    "FileQuery",
    "ViewCharRange",
    "MessageMatchViewMarkers",
    "MessageMatchEvidence",
    "MessageLiteralMatch",
    "MessageContentExtent",
    "MessageHit",
    "ValueOrigin",
    "MessageSearchOrigins",
    "MessageSearchTarget",
    "MessageSearchExplain",
    "MessageSearchResponse",
    "MessageSearchRuntimeDiagnostics",
    "MessageSearchBatch",
    "MessageSearchCompletion",
    "MessageSearchBatches",
    "RefreshOutcome",
    "ReindexOutcome",
    "ProviderParserHealth",
    "ParserHealth",
    "IndexStatus",
    "IndexReadinessStatus",
    "IndexRefreshStatus",
    "ProviderHealth",
    "DiagnosticStatus",
    "CompactOutcome",
]

def serve_mcp() -> None: ...
def _run_cli_command(args: list[str]) -> int: ...

@final
class SessionRecord:
    """Canonical indexed session metadata and source provenance."""

    id: str
    provider: str
    provider_session_id: str
    title: str | None
    summary: str | None
    cwd: str | None
    repo_root: str | None
    created_at: str | None
    updated_at: str | None
    last_message_at: str | None
    preview_text: str
    source_path: str
    message_count: int | None
    parse_warning: str | None
    parent_session_id: str | None
    agent_label: str | None

@final
class AnalysisCursor:
    """Opaque keyset cursor for the next non-overlapping analysis document page."""

    ...

@final
class AnalysisDocument:
    """One indexed session and its normalized user-message text for analysis."""

    session: SessionRecord
    user_text: str
    first_user_text: str | None
    message_count: int
    user_message_count: int

@final
class AnalysisDocumentPage:
    """Bounded analysis document page with an optional continuation cursor."""

    documents: list[AnalysisDocument]
    next_cursor: AnalysisCursor | None

@final
class ClassificationRule:
    """One weighted regex classification applied to a selected session text field."""

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

    dimension: str
    label: str
    target: Literal["title", "summary", "first_user_text", "user_text", "any"]
    weight: int

@final
class RelationshipHint:
    """Resolved, ambiguous, or unresolved relationship inferred for a session."""

    rule_id: str
    kind: Literal["branch", "copy", "version"]
    parent_title: str
    status: Literal["unresolved", "resolved", "ambiguous"]
    resolved_session_id: str | None
    candidate_session_ids: list[str]

@final
class AnalyzedSession:
    """One session with its analysis score, classifications, and relationship hints."""

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

    phrase: str
    words: int
    documents: int
    occurrences: int

@final
class AnalysisResult:
    """Typed classifications, relationships, vocabulary, and graph for analyzed sessions."""

    sessions: dict[str, AnalyzedSession]
    vocabulary: list[PhraseFrequency]
    graph: SessionGraph

@final
class AnalysisArtifact:
    """Rendered analysis artifact held in memory before publication."""

    name: str
    content: str
    sha256: str
    bytes: int

@final
class PublishedAnalysisArtifact:
    """Name, byte count, and SHA-256 digest of one published artifact."""

    name: str
    bytes: int
    sha256: str

@final
class AnalysisPublicationReceipt:
    """Receipt for an atomically published immutable analysis bundle."""

    destination: Path
    artifacts: list[PublishedAnalysisArtifact]

@final
class AnalysisPublicationPlan:
    """Immutable, no-replace publication plan for JSON and Markdown analysis artifacts."""
    def __new__(cls, destination: str | Path, formats: list[Literal["json", "markdown"]] | None = None) -> Self: ...
    @property
    def destination(self) -> Path: ...
    @property
    def formats(self) -> list[Literal["json", "markdown"]]: ...
    def render(self, result: AnalysisResult) -> list[AnalysisArtifact]: ...
    def publish(self, result: AnalysisResult) -> AnalysisPublicationReceipt: ...

@final
class SessionGraphNode:
    """Graph node containing one analyzed session identity and classifications."""

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

    source_session_id: str
    target_session_id: str
    kind: Literal["branch", "copy", "version"]
    rule_id: str

@final
class SessionGraphGroup:
    """Session IDs sharing one classification dimension and label."""

    dimension: Literal["working_directory", "repository"]
    key: str
    session_ids: list[str]

@final
class SessionGraph:
    """Deterministic nodes, resolved edges, and classification groups for analyzed sessions."""

    nodes: dict[str, SessionGraphNode]
    edges: list[SessionGraphEdge]
    groups: list[SessionGraphGroup]

@final
class SearchHit:
    """Ranked session search result with score and matched-field preview."""

    session: SessionRecord
    score: int
    match_source: str
    match_snippet: str

@final
class MessagePreview:
    """Bounded message preview with its exact expansion command."""

    seq: int
    timestamp: str | None
    chars: int
    preview: str
    expand_command: str

@final
class ToolActivity:
    """Bounded tool-call or tool-result evidence with an exact expansion command."""

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

    file_path: str
    provider: str
    edits: int
    follow_up_command: str

@final
class SessionTimeProfile:
    """Observed timestamp span, gaps, and tool/message counts for one session."""

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

    file_path: str
    file_name: str
    edits: int
    sessions: int
    last_edited: str | None

@final
class FileVersion:
    """One causally ordered historical file version reconstructed from an edit."""

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

    file_path: str
    session_id: str
    provider: str
    edits: int

@final
class ReconstructedFile:
    """One reconstructed historical file with provenance and complete content."""

    session_id: str
    provider: str
    version: int
    file_path: str
    content: str
    def restore(self, *, output_dir: str | Path | None = None) -> Path: ...

@final
class ReconstructedFileVersions:
    """Single-pass iterator over causally reconstructable file versions."""
    def __iter__(self) -> ReconstructedFileVersions: ...
    def __next__(self) -> ReconstructedFile: ...

@final
class RecoveryPublicationReceipt:
    """Receipt for an atomically published directory of recovered file versions."""

    destination: Path
    files: list[Path]

@final
class ExportDocument:
    """Complete rendered session export in the requested format."""

    format: Literal["markdown", "text", "json"]
    content: str

@final
class ExportPublicationReceipt:
    """Receipt for an atomically published session export bundle."""

    destination: Path
    format: Literal["markdown", "text", "json"]
    sessions: int
    files: list[Path]

@final
class ProviderSourceStatus:
    """Enabled roots and discovered session-file count for one provider."""

    provider: str
    enabled: bool
    roots: list[str]
    discovered_files: int

@final
class MessageClassificationMatch:
    """One message classified by a named capability rule category."""

    session_id: str
    provider: str
    timestamp: str | None
    policy_name: str
    """Which compiled classification policy produced this match.

    The name only. Version and digest appear once per run on
    :attr:`MessageClassificationReport.policies` rather than repeated on every row.
    """
    category: str
    matched_text: str
    content: str

@final
class CapabilityReceipt:
    """Identity of one message-classification capability evaluated for a report."""

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

    policies: list[CapabilityReceipt]
    """Every evaluated capability, in evaluation order, including any that matched nothing.

    Carried so an empty :attr:`matches` list is unambiguous: "these rules ran and
    found nothing" and "no rules ran" are different answers.
    """
    matches: list[MessageClassificationMatch]
    """Matches newest first, after ``offset`` is skipped and ``limit`` taken."""

@final
class SelectedSkillLocation:
    """Where the selected skill package was resolved."""

    kind: Literal["embedded", "path"]
    canonical_skill_md: Path | None

@final
class CapabilityExecutionSource:
    """Where the deterministic capability bytes executed by a skill came from."""

    kind: Literal["embedded", "path"]
    canonical_capability_toml: Path | None

@final
class ResolvedSkillReceipt:
    """Provenance for the package and capability selected by one skill run."""

    name: str
    package_version: str | None
    selected_location: SelectedSkillLocation
    execution_source: CapabilityExecutionSource

@final
class MessageClassificationResult:
    """Typed message-classification output nested inside a skill-run report."""

    receipt: CapabilityReceipt
    """Primary selected skill's policy receipt.

    This equals the first entry in ``report.policies``. The report list also
    records every additional ``--skill`` policy in evaluation order.
    """
    report: MessageClassificationReport

@final
class SkillRunReport:
    """Result and provenance from one deterministic skill invocation."""

    requested_selector: SkillSelector
    resolved_skill: ResolvedSkillReceipt
    output: MessageClassificationResult

@final
class PlanningCount:
    """Slash-command usage count with distinct session and project counts."""

    command: str
    count: int
    unique_sessions: int
    unique_projects: int

@final
class RoleStat:
    """Exact indexed message count for one normalized role."""

    role: str
    count: int

@final
class DateRange:
    """Date bounds parsed by the same Rust grammar used by the CLI and MCP server."""

    since: str | None
    until: str | None
    when: str | None

    def __new__(
        cls,
        *,
        since: str | None = None,
        until: str | None = None,
        when: str | None = None,
    ) -> Self: ...
    def resolve_bounds(
        self,
        *,
        reference_time: str | None = None,
    ) -> ResolvedDateRange: ...

@final
class ResolvedDateRange:
    """Concrete inclusive UTC bounds produced by resolving a DateRange."""

    since: str | None
    until: str | None

@final
class QueryExclusions:
    """Reusable exclusions applied before result limits.

    ``path_prefixes`` exclude normalized filesystem path prefixes.
    ``session_ids`` exclude exact canonical session IDs.
    """

    path_prefixes: list[str]
    session_ids: list[str]

    def __new__(
        cls,
        *,
        path_prefixes: list[str] | None = None,
        session_ids: list[str] | None = None,
    ) -> Self: ...

@final
class QueryScope:
    """Shared provider, session, path, exclusion, and date scope for typed queries."""

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
        path_prefix: str | None = None,
        exclusions: QueryExclusions | None = None,
        dates: DateRange | None = None,
    ) -> Self: ...

@final
class SessionQuery:
    """Session list/search filters; limit=0 explicitly selects every match.

    ``current_repo`` overrides repository-aware ranking. When omitted, ``search_sessions`` honors
    configured ``prefer_current_repo`` behavior and derives the repository from the working
    directory.
    """

    provider: _ProviderId | None
    path_prefix: str | None
    exclusions: QueryExclusions
    session_kinds: list[_SessionKind] | None
    parent_session_id: str | None
    current_repo: str | None
    dates: DateRange
    limit: int

    def __new__(
        cls,
        *,
        provider: _ProviderId | None = None,
        path_prefix: str | None = None,
        exclusions: QueryExclusions | None = None,
        session_kinds: list[_SessionKind] | None = None,
        parent_session_id: str | None = None,
        current_repo: str | None = None,
        dates: DateRange | None = None,
        limit: int = 50,
    ) -> Self: ...

@final
class MessageExclusions:
    """Workspace, transcript, and session exclusions for message search."""

    workspace_path_prefixes: list[str]
    transcript_path_prefixes: list[str]
    session_ids: list[str]

    def __new__(
        cls,
        *,
        workspace_path_prefixes: list[str] | None = None,
        transcript_path_prefixes: list[str] | None = None,
        session_ids: list[str] | None = None,
    ) -> Self: ...

@final
class MessageScope:
    """Message-only scope with distinct workspace and transcript path domains."""

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
        workspace_path_prefix: str | None = None,
        transcript_path_prefix: str | None = None,
        exclusions: MessageExclusions | None = None,
        dates: DateRange | None = None,
    ) -> Self: ...

@final
class MessageSearchRequest:
    """Canonical message predicates, match window, presentation, extent, purpose, and receipt.

    The query searches only ``field``: ``content``, ``tool_name``, or ``tool_argument``. Tool
    arguments use an RFC 6901 ``argument_path``. ``tool_name_contains`` is an additional
    case-insensitive substring filter on canonical ``tool_name``, independent of ``field``.
    Sequence bounds are inclusive and session-local. When no configured operation/purpose
    default applies, omitting ``limit`` returns all literal, regex, or no-text Python matches;
    fuzzy search requires a positive limit. ``all_results=True`` states the complete-corpus
    request explicitly and conflicts with ``limit``. MCP separately uses a bounded default.
    """

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
    field_view: dict[str, str | int] | None
    match_view: dict[str, str | int] | None
    purpose: str | None
    purpose_version: int | None
    receipt_level: _ReceiptLevel | None

    def __new__(
        cls,
        *,
        scope: MessageScope | None = None,
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
    ) -> Self: ...

@final
class AnalysisQuery:
    """Session scope and maximum document count for aggregate analysis operations."""

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

    name: str | None
    path: Path | None

    def __new__(
        cls,
        *,
        name: str | None = None,
        path: str | Path | None = None,
    ) -> Self: ...

@final
class MessageClassificationQuery:
    """Typed arguments for the message-classification skill capability."""

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

    def __new__(
        cls,
        *,
        scope: QueryScope | None = None,
        session_kinds: list[str] | None = None,
        additional_skills: list[SkillSelector] | None = None,
        limit: int | None = None,
        offset: int = 0,
    ) -> Self: ...

@final
class SkillRunQuery:
    """One typed deterministic skill invocation."""

    skill: SkillSelector
    input: MessageClassificationQuery

    def __new__(
        cls,
        *,
        skill: SkillSelector,
        input: MessageClassificationQuery,
    ) -> Self: ...

@final
class FileQuery:
    """File filters with deterministic ``limit``/``offset`` pages; zero limit means unlimited."""

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
        limit: int = 50,
        offset: int = 0,
    ) -> Self: ...

@final
class ViewCharRange:
    view_start_char: int
    view_end_char_exclusive: int

@final
class MessageMatchViewMarkers:
    kind: Literal["characters", "boundary"]
    ranges: list[ViewCharRange]
    matched_chars_total: int | None
    matched_chars_shown: int | None
    view_at_char: int | None

@final
class MessageMatchEvidence:
    view_text: str
    field_start_char: int
    field_total_chars: int
    markers: MessageMatchViewMarkers

@final
class MessageLiteralMatch:
    """Complete selected-field occurrence for literal message search."""

    text: str
    field_start_char: int
    field_end_char_exclusive: int

@final
class MessageContentExtent:
    """Machine-readable disclosure of message-content selection and omission."""

    complete: bool
    omitted_start: bool
    omitted_end: bool
    returned_chars: int
    returned_lines: int
    original_chars: int | None
    original_lines: int | None

@final
class MessageHit:
    """One indexed message with canonical session, role, kind, tool, and content fields."""

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
    match_evidence: MessageMatchEvidence | None
    literal_match: MessageLiteralMatch | None
    content_extent: MessageContentExtent | None
    refs: list[MessageRef]
    ref_summary: str

@final
class ValueOrigin:
    """Resolved source of one message-search parameter value."""

    source: Literal[
        "explicit",
        "detail-preset",
        "purpose",
        "surface-config",
        "operation-config",
        "typed-default",
        "derived",
    ]
    purpose: str | None
    purpose_version: int | None
    surface: Literal["rust", "cli", "mcp", "python"] | None
    detail: Literal["compact", "full"] | None

@final
class MessageSearchOrigins:
    """Resolved origins for configurable message-search output parameters."""

    result_extent: ValueOrigin
    context_messages_before: ValueOrigin
    context_messages_after: ValueOrigin
    includes: ValueOrigin
    detail: ValueOrigin
    lines_per_message: ValueOrigin
    field_view: ValueOrigin
    match_view: ValueOrigin
    receipt_level: ValueOrigin
    result_order: ValueOrigin

@final
class MessageSearchExplain:
    """SQLite planner diagnostics included when a receipt was requested."""

    corpus: int
    prefilter: str | None
    candidates: int | None
    prefilter_skipped: str | None
    summary: str

@final
class MessageSearchTarget:
    field: _SearchField
    argument_path: str | None

@final
class MessageSearchResponse:
    """Message hits with aligned context, paging, presentation, and optional receipts."""

    response_schema_version: int
    query: str | None
    query_mode: _MessageQueryMode
    match_target: MessageSearchTarget | None
    hits: list[MessageHit]
    context_windows: list[list[MessageHit]]
    limit: int | None
    offset: int
    next_offset: int | None
    returned: int
    has_more: bool
    ordering: Literal["session-sequence", "fuzzy-relevance"]
    context_before: int
    context_after: int
    include_refs: bool
    lines_per_message: int
    match_evidence_max_chars: int
    search_explanation: MessageSearchExplain | None
    origins: MessageSearchOrigins | None
    ordered_digest: str | None
    included: dict[str, object]

@final
class MessageSearchRuntimeDiagnostics:
    """Package, database, response-contract, surface, and configuration identity for one request."""

    package_version: str
    database_schema_version: int
    response_schema_version: int
    surface: Literal["rust", "cli", "mcp", "python"]
    config_digest: str

@final
class MessageSearchBatch:
    """One owned result batch with index-aligned context and newly encountered included data."""

    results: list[MessageHit]
    context_windows: list[list[MessageHit]]
    included: dict[str, object]

@final
class MessageSearchCompletion:
    """Terminal page and receipt facts available after natural batch-stream exhaustion."""

    returned: int
    next_offset: int | None
    ordering: Literal["session-sequence", "fuzzy-relevance"]
    earlier_results: Literal["none", "present", "unknown"]
    result_set_extent: Literal["all", "partial", "unknown"]
    search_explanation: MessageSearchExplain | None
    origins: MessageSearchOrigins | None
    ordered_digest: str | None

@final
class MessageSearchBatches:
    """Advanced context-managed exhaustive batches; prefer ``search_messages`` for a normal list."""

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

    status: str
    files_seen: int | None
    sessions_updated: int | None
    reason: str | None

@final
class ReindexOutcome:
    """Session-file and changed-session counts from an explicit reindex."""

    files_seen: int
    sessions_updated: int

@final
class ProviderParserHealth:
    """Expected parser version and current/stale counts for one provider."""

    provider: str
    expected_parse_version: str
    indexed_sessions: int
    current_sessions: int
    stale_sessions: int

@final
class ParserHealth:
    """Aggregate schema and parser-version freshness across indexed sessions."""

    schema_version: int
    expected_schema_version: int
    schema_current: bool
    indexed_sessions: int
    current_sessions: int
    stale_sessions: int
    parse_warnings: int
    providers: list[ProviderParserHealth]

@final
class IndexStatus:
    """Parser/schema freshness and applicable repair commands for the index."""

    parser_health: ParserHealth
    repairable_stale_sessions: int
    unavailable_stale_sessions: int
    repair_commands: list[str]
    readiness: IndexReadinessStatus

@final
class IndexReadinessStatus:
    """Snapshot usability and automatic refresh state reported independently."""

    snapshot_availability: Literal["unavailable", "usable"]
    last_successful_refresh_at: str | None
    refresh: IndexRefreshStatus

@final
class IndexRefreshStatus:
    """Bounded durable progress and recovery for automatic index refresh."""

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

@final
class DiagnosticStatus:
    """Database, parser, automatic-update, and provider health report."""

    db_path: str
    index_status: IndexStatus
    providers: list[ProviderHealth]

@final
class CompactOutcome:
    """Database byte counts before and after successful compaction."""

    before_bytes: int
    after_bytes: int
    reclaimed_bytes: int

@final
class SessionSearch:
    """Rust-backed search, recovery, export, and analysis service.

    Methods accepting ``session_id`` accept a canonical provider-qualified ID or a unique ID
    prefix. Ambiguous prefixes fail with the matching canonical IDs instead of selecting one.
    """
    def __new__(
        cls,
        db_path: str | Path | None = None,
        *,
        config_path: str | Path | None = None,
        cache_dir: str | Path | None = None,
        threads: int | None = None,
    ) -> Self: ...
    @property
    def db_path(self) -> Path: ...
    def search_messages(
        self,
        query: str,
        request: MessageSearchRequest | None = None,
        *,
        query_mode: _MessageQueryMode = "literal",
    ) -> MessageSearchResponse:
        """Search through the shared planner and return results, context, paging, and receipts.

        ``literal`` is the default case-insensitive substring match. ``regex`` uses Rust regex
        syntax. ``fuzzy`` scores every structurally eligible row with Nucleo sequence matching,
        then applies a deterministic finite offset and limit. It requires at least three query
        characters and does not support all-results output. With no explicit, purpose, or
        operation limit, Python returns every literal, regex, or no-text match;
        ``all_results=True`` states that complete-corpus choice explicitly.
        """
        ...
    def search_message_batches(
        self,
        query: str,
        request: MessageSearchRequest | None = None,
        *,
        query_mode: _MessageQueryMode = "literal",
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
    ) -> list[MessageHit]:
        """Read one session's messages, selecting the oldest or newest ``limit`` by ``order``
        (``"oldest"``/``"newest"``), always returned chronologically. To page a long session,
        advance ``seq_from`` (next chunk = last seq + 1) rather than growing ``limit``."""
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
    ) -> list[SessionRecord]: ...
    def search_sessions(
        self,
        query: str,
        request: SessionQuery | None = None,
    ) -> list[SearchHit]: ...
    def search_files(
        self,
        pattern: str | None = None,
        request: FileQuery | None = None,
    ) -> list[FileEditSummary]: ...
    def file_history(
        self,
        file: str,
        request: FileQuery | None = None,
    ) -> list[FileVersion]: ...
    def cross_reference_files(
        self,
        pattern: str | None = None,
        request: FileQuery | None = None,
    ) -> list[FileCrossRef]: ...
    def reconstruct_file(
        self,
        file: str,
        *,
        version: int | None = None,
        request: FileQuery | None = None,
    ) -> ReconstructedFile: ...
    def reconstruct_file_versions(
        self,
        file: str,
        *,
        request: FileQuery | None = None,
    ) -> ReconstructedFileVersions: ...
    def publish_file_versions(
        self,
        file: str,
        destination: str | Path,
        *,
        request: FileQuery | None = None,
    ) -> RecoveryPublicationReceipt: ...
    def export_session(
        self,
        session_id: str,
        format: Literal["markdown", "md", "text", "json"] = "markdown",
    ) -> ExportDocument: ...
    def export_sessions(
        self,
        destination: str | Path,
        request: SessionQuery | None = None,
        *,
        format: Literal["markdown", "md", "text", "json"] = "markdown",
    ) -> ExportPublicationReceipt: ...
    def source_inventory(self) -> list[ProviderSourceStatus]: ...
    def analysis_documents(
        self,
        request: SessionQuery | None = None,
        *,
        cursor: AnalysisCursor | None = None,
    ) -> AnalysisDocumentPage: ...
    def analyze(
        self,
        request: SessionQuery | None = None,
        *,
        policy: AnalysisPolicy | None = None,
    ) -> AnalysisResult:
        """Analyze every eligible session by default, or the explicit selection and typed policy."""
        ...
    def run_skill(
        self,
        request: SkillRunQuery,
    ) -> SkillRunReport:
        """Execute one deterministic skill capability.

        Selected ``capability.toml`` documents share a 1 MiB aggregate parsing
        safety ceiling. Exceeding it raises an actionable error with byte counts;
        rules and results are never truncated to fit.
        """
        ...
    def planning(
        self,
        request: AnalysisQuery | None = None,
        command_patterns: list[str] | None = None,
    ) -> list[PlanningCount]: ...
    def role_statistics(
        self,
        request: AnalysisQuery | None = None,
    ) -> list[RoleStat]: ...
    def refresh(self) -> RefreshOutcome: ...
    def index_status(self) -> IndexStatus: ...
    def reindex(self, *, full: bool = False) -> ReindexOutcome: ...
    def diagnostics(self) -> DiagnosticStatus: ...
    def compact(self) -> CompactOutcome: ...
