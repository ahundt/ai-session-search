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
class AnalysisCursor: ...

@final
class AnalysisDocument:
    session: SessionRecord
    user_text: str
    first_user_text: str | None
    message_count: int
    user_message_count: int

@final
class AnalysisDocumentPage:
    documents: list[AnalysisDocument]
    next_cursor: AnalysisCursor | None

@final
class ClassificationRule:
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
    dimension: str
    label: str
    target: Literal["title", "summary", "first_user_text", "user_text", "any"]
    weight: int

@final
class RelationshipHint:
    rule_id: str
    kind: Literal["branch", "copy", "version"]
    parent_title: str
    status: Literal["unresolved", "resolved", "ambiguous"]
    resolved_session_id: str | None
    candidate_session_ids: list[str]

@final
class AnalyzedSession:
    session: SessionRecord
    classifications: list[ClassificationMatch]
    score: int
    relationship_hints: list[RelationshipHint]
    has_user_text: bool
    message_count: int
    user_message_count: int

@final
class PhraseFrequency:
    phrase: str
    words: int
    documents: int
    occurrences: int

@final
class AnalysisResult:
    sessions: dict[str, AnalyzedSession]
    vocabulary: list[PhraseFrequency]
    graph: SessionGraph

@final
class AnalysisArtifact:
    name: str
    content: str
    sha256: str
    bytes: int

@final
class PublishedAnalysisArtifact:
    name: str
    bytes: int
    sha256: str

@final
class AnalysisPublicationReceipt:
    destination: Path
    artifacts: list[PublishedAnalysisArtifact]

@final
class AnalysisPublicationPlan:
    def __new__(cls, destination: str | Path, formats: list[Literal["json", "markdown"]] | None = None) -> Self: ...
    @property
    def destination(self) -> Path: ...
    @property
    def formats(self) -> list[Literal["json", "markdown"]]: ...
    def render(self, result: AnalysisResult) -> list[AnalysisArtifact]: ...
    def publish(self, result: AnalysisResult) -> AnalysisPublicationReceipt: ...

@final
class SessionGraphNode:
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
    source_session_id: str
    target_session_id: str
    kind: Literal["branch", "copy", "version"]
    rule_id: str

@final
class SessionGraphGroup:
    dimension: Literal["working_directory", "repository"]
    key: str
    session_ids: list[str]

@final
class SessionGraph:
    nodes: dict[str, SessionGraphNode]
    edges: list[SessionGraphEdge]
    groups: list[SessionGraphGroup]

@final
class SearchHit:
    session: SessionRecord
    score: int
    match_source: str
    match_snippet: str

@final
class MessagePreview:
    seq: int
    timestamp: str | None
    chars: int
    preview: str
    expand_command: str

@final
class ToolActivity:
    seq: int
    timestamp: str | None
    tool_name: str | None
    kind: str
    chars: int
    preview: str
    expand_command: str

@final
class MessageRef:
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
    seq: int
    role: str
    tool_name: str | None
    ref_summary: str
    refs: list[MessageRef]
    preview: str
    expand_command: str

@final
class ChangedFileEvidence:
    file_path: str
    provider: str
    edits: int
    follow_up_command: str

@final
class SessionTimeProfile:
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
    file_path: str
    file_name: str
    edits: int
    sessions: int
    last_edited: str | None

@final
class FileVersion:
    session_id: str
    provider: str
    version: int
    tool: str
    timestamp: str | None
    lines: int
    file_path: str

@final
class FileCrossRef:
    file_path: str
    session_id: str
    provider: str
    edits: int

@final
class ReconstructedFile:
    session_id: str
    provider: str
    version: int
    file_path: str
    content: str
    def restore(self, *, output_dir: str | Path | None = None) -> Path: ...

@final
class ReconstructedFileVersions:
    def __iter__(self) -> ReconstructedFileVersions: ...
    def __next__(self) -> ReconstructedFile: ...

@final
class RecoveryPublicationReceipt:
    destination: Path
    files: list[Path]

@final
class ExportDocument:
    format: Literal["markdown", "text", "json"]
    content: str

@final
class ExportPublicationReceipt:
    destination: Path
    format: Literal["markdown", "text", "json"]
    sessions: int
    files: list[Path]

@final
class ProviderSourceStatus:
    provider: str
    enabled: bool
    roots: list[str]
    discovered_files: int

@final
class CorrectionMatch:
    session_id: str
    provider: str
    timestamp: str | None
    category: str
    matched_pattern: str
    content: str

@final
class PlanningCount:
    command: str
    count: int
    unique_sessions: int
    unique_projects: int

@final
class RoleStat:
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
    status: str
    files_seen: int | None
    sessions_updated: int | None
    reason: str | None

@final
class ReindexOutcome:
    files_seen: int
    sessions_updated: int

@final
class ProviderParserHealth:
    provider: str
    expected_parse_version: str
    indexed_sessions: int
    current_sessions: int
    stale_sessions: int

@final
class ParserHealth:
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
    parser_health: ParserHealth
    repairable_stale_sessions: int
    unavailable_stale_sessions: int
    repair_commands: list[str]
    index_update: IndexUpdateStatus | None

@final
class IndexUpdateStatus:
    state: Literal["in_progress", "attention_required"]
    started_at: str
    message: str
    next_command: str | None

@final
class ProviderHealth:
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
    db_path: str
    index_status: IndexStatus
    providers: list[ProviderHealth]

@final
class CompactOutcome:
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
    def analyze_sessions(
        self,
        request: SessionQuery | None = None,
        *,
        policy: AnalysisPolicy | None = None,
    ) -> AnalysisResult:
        """Analyze selected sessions with the bounded default or supplied typed policy."""
        ...
    def find_corrections(
        self,
        request: AnalysisQuery | None = None,
    ) -> list[CorrectionMatch]: ...
    def planning_usage(
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
