from pathlib import Path
from typing import Literal

def serve_mcp() -> None: ...
def _run_cli_command(args: list[str]) -> int: ...

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

class NativeAnalysisCursor: ...

class NativeAnalysisDocument:
    session: NativeSessionRecord
    user_text: str
    first_user_text: str | None
    message_count: int
    user_message_count: int

class NativeAnalysisDocumentPage:
    documents: list[NativeAnalysisDocument]
    next_cursor: NativeAnalysisCursor | None

class ClassificationRule:
    dimension: str
    label: str
    pattern: str
    target: Literal["title", "summary", "first_user_text", "user_text", "any"]
    weight: int

    def __init__(
        self,
        dimension: str,
        label: str,
        pattern: str,
        *,
        target: Literal["title", "summary", "first_user_text", "user_text", "any"] = "user_text",
        weight: int = 0,
    ) -> None: ...

class RelationshipRule:
    id: str
    kind: Literal["branch", "copy", "version"]
    pattern: str

    def __init__(
        self,
        id: str,
        kind: Literal["branch", "copy", "version"],
        pattern: str,
    ) -> None: ...

class PhraseVocabulary:
    widths: list[int]
    max_unique_phrases: int
    min_document_tokens: int
    excluded_tokens: list[str]
    exclude_numeric_tokens: bool
    prose_only: bool

    def __init__(
        self,
        widths: list[int],
        max_unique_phrases: int,
        *,
        min_document_tokens: int = 0,
        excluded_tokens: list[str] | None = None,
        exclude_numeric_tokens: bool = True,
        prose_only: bool = False,
    ) -> None: ...

class AnalysisPolicy:
    def __init__(
        self,
        *,
        classification_rules: list[ClassificationRule] | None = None,
        relationship_rules: list[RelationshipRule] | None = None,
        phrase_vocabulary: PhraseVocabulary | None = None,
        max_classification_chars: int | None = None,
    ) -> None: ...

class NativeClassificationMatch:
    dimension: str
    label: str
    target: Literal["title", "summary", "first_user_text", "user_text", "any"]
    weight: int

class NativeRelationshipHint:
    rule_id: str
    kind: Literal["branch", "copy", "version"]
    parent_title: str
    status: Literal["unresolved", "resolved", "ambiguous"]
    resolved_session_id: str | None
    candidate_session_ids: list[str]

class NativeAnalyzedSession:
    session: NativeSessionRecord
    classifications: list[NativeClassificationMatch]
    score: int
    relationship_hints: list[NativeRelationshipHint]
    has_user_text: bool
    message_count: int
    user_message_count: int

class NativePhraseFrequency:
    phrase: str
    words: int
    documents: int
    occurrences: int

class NativeAnalysisResult:
    sessions: dict[str, NativeAnalyzedSession]
    vocabulary: list[NativePhraseFrequency]
    graph: NativeSessionGraph

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

class NativeSessionGraphEdge:
    source_session_id: str
    target_session_id: str
    kind: Literal["branch", "copy", "version"]
    rule_id: str

class NativeSessionGraphGroup:
    dimension: Literal["working_directory", "repository"]
    key: str
    session_ids: list[str]

class NativeSessionGraph:
    nodes: dict[str, NativeSessionGraphNode]
    edges: list[NativeSessionGraphEdge]
    groups: list[NativeSessionGraphGroup]

class NativeSessionSearchHit:
    session: NativeSessionRecord
    score: int
    match_source: str
    match_snippet: str

class NativeMessagePreview:
    seq: int
    timestamp: str | None
    chars: int
    preview: str
    expand_command: str

class NativeToolActivity:
    seq: int
    timestamp: str | None
    tool_name: str | None
    kind: str
    chars: int
    preview: str
    expand_command: str

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

class NativeRefEvidence:
    seq: int
    role: str
    tool_name: str | None
    ref_summary: str
    refs: list[NativeMessageRef]
    preview: str
    expand_command: str

class NativeChangedFileEvidence:
    file_path: str
    provider: str
    edits: int
    follow_up_command: str

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

class NativeSessionInspection:
    session: NativeSessionRecord
    user_intent: list[NativeMessagePreview]
    tool_activity: list[NativeToolActivity]
    refs: list[NativeRefEvidence]
    changed_files: list[NativeChangedFileEvidence]
    time_profile: NativeSessionTimeProfile | None
    next_commands: list[str]

class NativeFileEditSummary:
    file_path: str
    file_name: str
    edits: int
    sessions: int
    last_edited: str | None

class NativeFileVersion:
    session_id: str
    provider: str
    version: int
    tool: str
    timestamp: str | None
    lines: int
    file_path: str

class NativeFileCrossRef:
    file_path: str
    session_id: str
    provider: str
    edits: int

class NativeReconstructedFile:
    session_id: str
    provider: str
    version: int
    file_path: str
    content: str
    def restore(self, *, output_dir: str | Path | None = None) -> Path: ...

class NativeExportDocument:
    format: Literal["markdown", "text", "json"]
    content: str

class NativeProviderSourceStatus:
    provider: str
    enabled: bool
    roots: list[str]
    discovered_files: int

class NativeCorrectionMatch:
    session_id: str
    provider: str
    timestamp: str | None
    category: str
    matched_pattern: str
    content: str

class NativePlanningCount:
    command: str
    count: int
    unique_sessions: int
    unique_projects: int

class NativeRoleStatistic:
    role: str
    count: int

class DateRangeQuery:
    since: str | None
    until: str | None
    when: str | None

    def __init__(
        self,
        *,
        since: str | None = None,
        until: str | None = None,
        when: str | None = None,
    ) -> None: ...

class QueryScope:
    provider: str | None
    session_id: str | None
    session: str | None
    path_prefix: str | None
    dates: DateRangeQuery

    def __init__(
        self,
        *,
        provider: str | None = None,
        session_id: str | None = None,
        session: str | None = None,
        path_prefix: str | None = None,
        dates: DateRangeQuery | None = None,
    ) -> None: ...

class SessionQuery:
    dates: DateRangeQuery

    def __init__(
        self,
        *,
        provider: str | None = None,
        path_prefix: str | None = None,
        current_repo: str | None = None,
        dates: DateRangeQuery | None = None,
        limit: int = 50,
    ) -> None: ...

class MessageSearchTarget:
    field: str
    argument_path: str | None

    def __init__(
        self,
        *,
        field: str = "content",
        argument_path: str | None = None,
    ) -> None: ...

class MessageSequenceRange:
    seq_from: int | None
    seq_to: int | None

    def __init__(
        self,
        *,
        seq_from: int | None = None,
        seq_to: int | None = None,
    ) -> None: ...

class MessageSelector:
    role: str | None
    kind: str | None
    target: MessageSearchTarget
    sequence: MessageSequenceRange
    tool: str | None
    no_compaction: bool

    def __init__(
        self,
        *,
        role: str | None = None,
        kind: str | None = None,
        target: MessageSearchTarget | None = None,
        sequence: MessageSequenceRange | None = None,
        tool: str | None = None,
        no_compaction: bool = False,
    ) -> None: ...

class MessageQuery:
    scope: QueryScope
    selector: MessageSelector

    def __init__(
        self,
        *,
        scope: QueryScope | None = None,
        selector: MessageSelector | None = None,
        limit: int = 50,
        offset: int = 0,
    ) -> None: ...

class AnalysisQuery:
    scope: QueryScope

    def __init__(
        self,
        *,
        scope: QueryScope | None = None,
        limit: int = 50,
    ) -> None: ...

class FileQueryRequest:
    scope: QueryScope

    def __init__(
        self,
        *,
        scope: QueryScope | None = None,
        min_edits: int | None = None,
        max_edits: int | None = None,
        limit: int = 50,
    ) -> None: ...

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

class RefreshOutcome:
    status: str
    files_seen: int | None
    sessions_updated: int | None
    reason: str | None

class NativeReindexOutcome:
    files_seen: int
    sessions_updated: int

class NativeProviderParserHealth:
    provider: str
    expected_parse_version: str
    indexed_sessions: int
    current_sessions: int
    stale_sessions: int

class NativeParserHealth:
    schema_version: int
    expected_schema_version: int
    schema_current: bool
    indexed_sessions: int
    current_sessions: int
    stale_sessions: int
    parse_warnings: int
    providers: list[NativeProviderParserHealth]

class NativeIndexStatus:
    parser_health: NativeParserHealth
    repairable_stale_sessions: int
    unavailable_stale_sessions: int
    repair_commands: list[str]

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

class NativeDiagnosticStatus:
    db_path: str
    index_status: NativeIndexStatus
    providers: list[NativeProviderHealth]

class NativeCompactOutcome:
    before_bytes: int
    after_bytes: int
    reclaimed_bytes: int

class SessionSearch:
    def __init__(self, db_path: str | Path | None = None) -> None: ...
    @property
    def db_path(self) -> Path: ...
    def search_messages(
        self,
        query: str,
        request: MessageQuery | None = None,
        *,
        mode: str = "exact",
    ) -> list[NativeMessageHit]: ...
    def message_context(
        self,
        session_id: str,
        seq: int,
        *,
        before: int = 5,
        after: int = 5,
    ) -> list[NativeMessageHit]: ...
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
    def export_session(
        self,
        session_id: str,
        format: Literal["markdown", "md", "text", "json"] = "markdown",
    ) -> NativeExportDocument: ...
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
        page_size: int = 50,
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
