from pathlib import Path
from typing import Literal, Self, final

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
    "CorrectionMatch",
    "PlanningCount",
    "RoleStat",
    "SessionQuery",
    "QueryExclusions",
    "DateRange",
    "ResolvedDateRange",
    "QueryScope",
    "MessageQuery",
    "AnalysisQuery",
    "FileQuery",
    "MessageHit",
    "RefreshOutcome",
    "ReindexOutcome",
    "ProviderParserHealth",
    "ParserHealth",
    "IndexStatus",
    "IndexUpdateStatus",
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
class CorrectionMatch:
    """One user correction classified by a named correction category."""
    session_id: str
    provider: str
    timestamp: str | None
    category: str
    matched_pattern: str
    content: str

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
    provider: str | None
    session_id: str | None
    path_prefix: str | None
    exclusions: QueryExclusions
    dates: DateRange

    def __new__(
        cls,
        *,
        provider: str | None = None,
        session_id: str | None = None,
        path_prefix: str | None = None,
        exclusions: QueryExclusions | None = None,
        dates: DateRange | None = None,
    ) -> Self: ...

@final
class SessionQuery:
    """Session list/search filters; limit=0 explicitly selects every match."""
    provider: str | None
    path_prefix: str | None
    exclusions: QueryExclusions
    current_repo: str | None
    dates: DateRange
    limit: int

    def __new__(
        cls,
        *,
        provider: str | None = None,
        path_prefix: str | None = None,
        exclusions: QueryExclusions | None = None,
        current_repo: str | None = None,
        dates: DateRange | None = None,
        limit: int = 50,
    ) -> Self: ...

@final
class MessageQuery:
    """Composable message filters applied before ``limit`` and ``offset``.

    The query searches only ``field``: ``content``, ``tool_name``, or ``tool_argument``. Tool
    arguments require an RFC 6901 ``argument_path``. ``tool`` is an additional case-insensitive
    substring filter on canonical ``tool_name``, independent of ``field``. Sequence bounds are
    inclusive and session-local.
    """

    scope: QueryScope
    role: str | None
    kind: str | None
    field: str
    argument_path: str | None
    seq_from: int | None
    seq_to: int | None
    tool: str | None
    no_compaction: bool
    limit: int
    offset: int

    def __new__(
        cls,
        *,
        scope: QueryScope | None = None,
        role: str | None = None,
        kind: str | None = None,
        field: str = "content",
        argument_path: str | None = None,
        seq_from: int | None = None,
        seq_to: int | None = None,
        tool: str | None = None,
        no_compaction: bool = False,
        limit: int = 50,
        offset: int = 0,
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
class FileQuery:
    """File-history filters shared by search, reconstruction, restore, and publication."""

    scope: QueryScope
    min_edits: int | None
    max_edits: int | None
    limit: int

    def __new__(
        cls,
        *,
        scope: QueryScope | None = None,
        min_edits: int | None = None,
        max_edits: int | None = None,
        limit: int = 50,
    ) -> Self: ...

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
    index_update: IndexUpdateStatus | None

@final
class IndexUpdateStatus:
    """Actionable state for an automatic background index update."""
    state: Literal["in_progress", "attention_required"]
    started_at: str
    message: str
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
        request: MessageQuery | None = None,
        *,
        match_mode: str = "exact",
        lines_per_message: int = 0,
    ) -> list[MessageHit]:
        """Search messages using ``exact``, ``regex``, or ``fuzzy`` matching.

        ``exact`` is the default case-insensitive literal substring match. ``regex`` uses Rust
        regex syntax; ``fuzzy`` uses nucleo matching. Regex and fuzzy modes require a non-empty
        query. ``lines_per_message`` changes displayed content, never selection or pagination.
        """
        ...
    def message_context(
        self,
        session_id: str,
        seq: int,
        *,
        before: int = 5,
        after: int = 5,
        lines_per_message: int = 0,
    ) -> list[MessageHit]:
        """Return context; ``lines_per_message`` never removes messages from that context."""
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
        """Analyze selected sessions with the bounded default or supplied typed policy."""
        ...
    def corrections(
        self,
        request: AnalysisQuery | None = None,
    ) -> list[CorrectionMatch]: ...
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
