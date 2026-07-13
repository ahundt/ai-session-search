from pathlib import Path
from typing import Literal

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

class NativeSessionSearchHit:
    session: NativeSessionRecord
    score: int
    match_source: str
    match_snippet: str

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

class MessageQuery:
    scope: QueryScope

    def __init__(
        self,
        *,
        scope: QueryScope | None = None,
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

class SessionSearch:
    def __init__(self, db_path: str | Path | None = None) -> None: ...
    @property
    def db_path(self) -> Path: ...
    def search_messages(
        self,
        query: str,
        request: MessageQuery | None = None,
    ) -> list[NativeMessageHit]: ...
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
