from pathlib import Path
from typing import Literal, Self, final

__all__ = [  # noqa: RUF022 - match the extension module's canonical export order
    "serve_mcp",
    "_run_cli_command",
    "SessionSearch",
    "NativeSessionRecord",
    "NativeAnalysisCursor",
    "NativeAnalysisDocument",
    "NativeAnalysisDocumentPage",
    "ClassificationRule",
    "RelationshipRule",
    "PhraseVocabulary",
    "AnalysisPolicy",
    "NativeClassificationMatch",
    "NativeRelationshipHint",
    "NativeAnalyzedSession",
    "NativePhraseFrequency",
    "NativeAnalysisResult",
    "NativeAnalysisArtifact",
    "NativePublishedAnalysisArtifact",
    "NativeAnalysisPublicationReceipt",
    "AnalysisPublicationPlan",
    "NativeSessionGraphNode",
    "NativeSessionGraphEdge",
    "NativeSessionGraphGroup",
    "NativeSessionGraph",
    "NativeSessionSearchHit",
    "NativeMessagePreview",
    "NativeToolActivity",
    "NativeMessageRef",
    "NativeRefEvidence",
    "NativeChangedFileEvidence",
    "NativeSessionTimeProfile",
    "NativeSessionInspection",
    "NativeFileEditSummary",
    "NativeFileVersion",
    "NativeFileCrossRef",
    "NativeReconstructedFile",
    "NativeReconstructedFileVersions",
    "NativeRecoveryPublicationReceipt",
    "NativeExportDocument",
    "NativeExportPublicationReceipt",
    "NativeProviderSourceStatus",
    "NativeCorrectionMatch",
    "NativePlanningCount",
    "NativeRoleStatistic",
    "SessionQuery",
    "QueryExclusions",
    "DateRangeQuery",
    "ResolvedDateRange",
    "QueryScope",
    "MessageSearchTarget",
    "MessageSequenceRange",
    "MessageSelector",
    "MessageQuery",
    "AnalysisQuery",
    "FileQueryRequest",
    "NativeMessageHit",
    "RefreshOutcome",
    "NativeReindexOutcome",
    "NativeProviderParserHealth",
    "NativeParserHealth",
    "NativeIndexStatus",
    "NativeProviderHealth",
    "NativeDiagnosticStatus",
    "NativeCompactOutcome",
]

def serve_mcp() -> None: ...
def _run_cli_command(args: list[str]) -> int: ...

@final
class NativeSessionRecord:
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
class NativeAnalysisCursor: ...

@final
class NativeAnalysisDocument:
    session: NativeSessionRecord
    user_text: str
    first_user_text: str | None
    message_count: int
    user_message_count: int

@final
class NativeAnalysisDocumentPage:
    documents: list[NativeAnalysisDocument]
    next_cursor: NativeAnalysisCursor | None

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
class NativeClassificationMatch:
    dimension: str
    label: str
    target: Literal["title", "summary", "first_user_text", "user_text", "any"]
    weight: int

@final
class NativeRelationshipHint:
    rule_id: str
    kind: Literal["branch", "copy", "version"]
    parent_title: str
    status: Literal["unresolved", "resolved", "ambiguous"]
    resolved_session_id: str | None
    candidate_session_ids: list[str]

@final
class NativeAnalyzedSession:
    session: NativeSessionRecord
    classifications: list[NativeClassificationMatch]
    score: int
    relationship_hints: list[NativeRelationshipHint]
    has_user_text: bool
    message_count: int
    user_message_count: int

@final
class NativePhraseFrequency:
    phrase: str
    words: int
    documents: int
    occurrences: int

@final
class NativeAnalysisResult:
    sessions: dict[str, NativeAnalyzedSession]
    vocabulary: list[NativePhraseFrequency]
    graph: NativeSessionGraph

@final
class NativeAnalysisArtifact:
    name: str
    content: str
    sha256: str
    bytes: int

@final
class NativePublishedAnalysisArtifact:
    name: str
    bytes: int
    sha256: str

@final
class NativeAnalysisPublicationReceipt:
    destination: Path
    artifacts: list[NativePublishedAnalysisArtifact]

@final
class AnalysisPublicationPlan:
    def __new__(cls, destination: str | Path, formats: list[Literal["json", "markdown"]] | None = None) -> Self: ...
    @property
    def destination(self) -> Path: ...
    @property
    def formats(self) -> list[Literal["json", "markdown"]]: ...
    def render(self, result: NativeAnalysisResult) -> list[NativeAnalysisArtifact]: ...
    def publish(self, result: NativeAnalysisResult) -> NativeAnalysisPublicationReceipt: ...

@final
class NativeSessionGraphNode:
    session_id: str
    provider: str
    title: str | None
    cwd: str | None
    repo_root: str | None
    created_at: str | None
    updated_at: str | None
    score: int
    classifications: list[NativeClassificationMatch]

@final
class NativeSessionGraphEdge:
    source_session_id: str
    target_session_id: str
    kind: Literal["branch", "copy", "version"]
    rule_id: str

@final
class NativeSessionGraphGroup:
    dimension: Literal["working_directory", "repository"]
    key: str
    session_ids: list[str]

@final
class NativeSessionGraph:
    nodes: dict[str, NativeSessionGraphNode]
    edges: list[NativeSessionGraphEdge]
    groups: list[NativeSessionGraphGroup]

@final
class NativeSessionSearchHit:
    session: NativeSessionRecord
    score: int
    match_source: str
    match_snippet: str

@final
class NativeMessagePreview:
    seq: int
    timestamp: str | None
    chars: int
    preview: str
    expand_command: str

@final
class NativeToolActivity:
    seq: int
    timestamp: str | None
    tool_name: str | None
    kind: str
    chars: int
    preview: str
    expand_command: str

@final
class NativeMessageRef:
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
class NativeRefEvidence:
    seq: int
    role: str
    tool_name: str | None
    ref_summary: str
    refs: list[NativeMessageRef]
    preview: str
    expand_command: str

@final
class NativeChangedFileEvidence:
    file_path: str
    provider: str
    edits: int
    follow_up_command: str

@final
class NativeSessionTimeProfile:
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
class NativeSessionInspection:
    session: NativeSessionRecord
    user_intent: list[NativeMessagePreview]
    tool_activity: list[NativeToolActivity]
    refs: list[NativeRefEvidence]
    changed_files: list[NativeChangedFileEvidence]
    time_profile: NativeSessionTimeProfile | None
    next_commands: list[str]

@final
class NativeFileEditSummary:
    file_path: str
    file_name: str
    edits: int
    sessions: int
    last_edited: str | None

@final
class NativeFileVersion:
    session_id: str
    provider: str
    version: int
    tool: str
    timestamp: str | None
    lines: int
    file_path: str

@final
class NativeFileCrossRef:
    file_path: str
    session_id: str
    provider: str
    edits: int

@final
class NativeReconstructedFile:
    session_id: str
    provider: str
    version: int
    file_path: str
    content: str
    def restore(self, *, output_dir: str | Path | None = None) -> Path: ...

@final
class NativeReconstructedFileVersions:
    def __iter__(self) -> NativeReconstructedFileVersions: ...
    def __next__(self) -> NativeReconstructedFile: ...

@final
class NativeRecoveryPublicationReceipt:
    destination: Path
    files: list[Path]

@final
class NativeExportDocument:
    format: Literal["markdown", "text", "json"]
    content: str

@final
class NativeExportPublicationReceipt:
    destination: Path
    format: Literal["markdown", "text", "json"]
    sessions: int
    files: list[Path]

@final
class NativeProviderSourceStatus:
    provider: str
    enabled: bool
    roots: list[str]
    discovered_files: int

@final
class NativeCorrectionMatch:
    session_id: str
    provider: str
    timestamp: str | None
    category: str
    matched_pattern: str
    content: str

@final
class NativePlanningCount:
    command: str
    count: int
    unique_sessions: int
    unique_projects: int

@final
class NativeRoleStatistic:
    role: str
    count: int

@final
class DateRangeQuery:
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
    dates: DateRangeQuery

    def __new__(
        cls,
        *,
        provider: str | None = None,
        session_id: str | None = None,
        path_prefix: str | None = None,
        exclusions: QueryExclusions | None = None,
        dates: DateRangeQuery | None = None,
    ) -> Self: ...

@final
class SessionQuery:
    provider: str | None
    path_prefix: str | None
    exclusions: QueryExclusions
    current_repo: str | None
    dates: DateRangeQuery
    limit: int

    def __new__(
        cls,
        *,
        provider: str | None = None,
        path_prefix: str | None = None,
        exclusions: QueryExclusions | None = None,
        current_repo: str | None = None,
        dates: DateRangeQuery | None = None,
        limit: int = 50,
    ) -> Self: ...

@final
class MessageSearchTarget:
    field: str
    argument_path: str | None

    def __new__(
        cls,
        *,
        field: str = "content",
        argument_path: str | None = None,
    ) -> Self: ...

@final
class MessageSequenceRange:
    seq_from: int | None
    seq_to: int | None

    def __new__(
        cls,
        *,
        seq_from: int | None = None,
        seq_to: int | None = None,
    ) -> Self: ...

@final
class MessageSelector:
    role: str | None
    kind: str | None
    target: MessageSearchTarget
    sequence: MessageSequenceRange
    tool: str | None
    no_compaction: bool

    def __new__(
        cls,
        *,
        role: str | None = None,
        kind: str | None = None,
        target: MessageSearchTarget | None = None,
        sequence: MessageSequenceRange | None = None,
        tool: str | None = None,
        no_compaction: bool = False,
    ) -> Self: ...

@final
class MessageQuery:
    scope: QueryScope
    selector: MessageSelector
    limit: int
    offset: int

    def __new__(
        cls,
        *,
        scope: QueryScope | None = None,
        selector: MessageSelector | None = None,
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
class FileQueryRequest:
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
class NativeMessageHit:
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
class NativeReindexOutcome:
    files_seen: int
    sessions_updated: int

@final
class NativeProviderParserHealth:
    provider: str
    expected_parse_version: str
    indexed_sessions: int
    current_sessions: int
    stale_sessions: int

@final
class NativeParserHealth:
    schema_version: int
    expected_schema_version: int
    schema_current: bool
    indexed_sessions: int
    current_sessions: int
    stale_sessions: int
    parse_warnings: int
    providers: list[NativeProviderParserHealth]

@final
class NativeIndexStatus:
    parser_health: NativeParserHealth
    repairable_stale_sessions: int
    unavailable_stale_sessions: int
    repair_commands: list[str]

@final
class NativeProviderHealth:
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
    resume_supported: bool
    resume_command: str | None

@final
class NativeDiagnosticStatus:
    db_path: str
    index_status: NativeIndexStatus
    providers: list[NativeProviderHealth]

@final
class NativeCompactOutcome:
    before_bytes: int
    after_bytes: int
    reclaimed_bytes: int

@final
class SessionSearch:
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
        mode: str = "exact",
        lines_per_message: int = 0,
    ) -> list[NativeMessageHit]:
        """Search messages; ``lines_per_message`` changes content display, never result selection."""
        ...
    def message_context(
        self,
        session_id: str,
        seq: int,
        *,
        before: int = 5,
        after: int = 5,
        lines_per_message: int = 0,
    ) -> list[NativeMessageHit]:
        """Return context; ``lines_per_message`` never removes messages from that context."""
        ...
    def inspect_session(
        self,
        session_id: str,
        *,
        preview_chars: int | None = None,
        include_time_profile: bool = False,
    ) -> NativeSessionInspection: ...
    def list_sessions(
        self,
        request: SessionQuery | None = None,
    ) -> list[NativeSessionRecord]: ...
    def search_sessions(
        self,
        query: str,
        request: SessionQuery | None = None,
    ) -> list[NativeSessionSearchHit]: ...
    def search_files(
        self,
        pattern: str | None = None,
        request: FileQueryRequest | None = None,
    ) -> list[NativeFileEditSummary]: ...
    def file_history(
        self,
        file: str,
        request: FileQueryRequest | None = None,
    ) -> list[NativeFileVersion]: ...
    def cross_reference_files(
        self,
        pattern: str | None = None,
        request: FileQueryRequest | None = None,
    ) -> list[NativeFileCrossRef]: ...
    def reconstruct_file(
        self,
        file: str,
        *,
        version: int | None = None,
        request: FileQueryRequest | None = None,
    ) -> NativeReconstructedFile: ...
    def reconstruct_file_versions(
        self,
        file: str,
        *,
        request: FileQueryRequest | None = None,
    ) -> NativeReconstructedFileVersions: ...
    def publish_file_versions(
        self,
        file: str,
        destination: str | Path,
        *,
        request: FileQueryRequest | None = None,
    ) -> NativeRecoveryPublicationReceipt: ...
    def export_session(
        self,
        session_id: str,
        format: Literal["markdown", "md", "text", "json"] = "markdown",
    ) -> NativeExportDocument: ...
    def export_sessions(
        self,
        destination: str | Path,
        request: SessionQuery | None = None,
        *,
        format: Literal["markdown", "md", "text", "json"] = "markdown",
    ) -> NativeExportPublicationReceipt: ...
    def source_inventory(self) -> list[NativeProviderSourceStatus]: ...
    def analysis_documents(
        self,
        request: SessionQuery | None = None,
        *,
        cursor: NativeAnalysisCursor | None = None,
    ) -> NativeAnalysisDocumentPage: ...
    def analyze_sessions(
        self,
        request: SessionQuery | None = None,
        *,
        policy: AnalysisPolicy | None = None,
    ) -> NativeAnalysisResult: ...
    def find_corrections(
        self,
        request: AnalysisQuery | None = None,
    ) -> list[NativeCorrectionMatch]: ...
    def planning_usage(
        self,
        request: AnalysisQuery | None = None,
        command_patterns: list[str] | None = None,
    ) -> list[NativePlanningCount]: ...
    def role_statistics(
        self,
        request: AnalysisQuery | None = None,
    ) -> list[NativeRoleStatistic]: ...
    def refresh(self) -> RefreshOutcome: ...
    def index_status(self) -> NativeIndexStatus: ...
    def reindex(self, *, full: bool = False) -> NativeReindexOutcome: ...
    def diagnostics(self) -> NativeDiagnosticStatus: ...
    def compact(self) -> NativeCompactOutcome: ...
