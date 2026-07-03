use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use chrono::{DateTime, Duration, Utc};
use regex::RegexBuilder;
use serde_json::{json, Value};

use crate::config::Config;
use crate::models::{FileEdit, Message, ParsedSession, Provider, Role, SessionRecord};

/// Read a reader's lines like [`std::io::BufRead::lines`], but never fail on a line that is not
/// valid UTF-8: each invalid byte sequence is replaced with the Unicode replacement character
/// `U+FFFD` (via [`String::from_utf8_lossy`]) instead of returning an error.
///
/// Why this exists: session transcripts occasionally contain non-UTF-8 bytes — e.g. binary
/// captured in a tool's output. The strict [`std::io::BufRead::lines`] returns an error on the
/// first such byte, which (because parsing aborts) would discard the ENTIRE session. Substituting
/// the replacement character lets us still index the surrounding text; one stray byte becomes a
/// single `U+FFFD` and nothing else is lost. Genuine I/O errors are still surfaced via the `Err`.
///
/// Line semantics match [`std::io::BufRead::lines`]: split on `\n`, drop a trailing `\n` and an
/// immediately preceding `\r`, and emit no trailing empty line for input ending in `\n`. Yields one
/// line at a time, so peak memory stays O(longest line) — the streaming parse path is preserved.
pub fn lines_replacing_invalid_utf8<R: io::BufRead>(
    reader: R,
) -> impl Iterator<Item = io::Result<String>> {
    reader.split(b'\n').map(|line| {
        line.map(|mut bytes| {
            // `BufRead::split` keeps the `\r` of a `\r\n` ending; drop it to match `lines`.
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
            String::from_utf8_lossy(&bytes).into_owned()
        })
    })
}

/// Expand a leading `~` to the user's home directory, cross-platform. Std has no tilde
/// expansion (it's a shell-ism, not a filesystem operation), so we resolve it via
/// `dirs::home_dir()`. Handles a bare `~`, `~/rest`, and (on Windows) `~\rest`; `~user`
/// (other users' homes) is intentionally left unexpanded. Falls back to the literal input
/// when there is no home directory or no tilde prefix.
pub fn expand_tilde(input: &str) -> PathBuf {
    let home = || dirs::home_dir().unwrap_or_else(|| PathBuf::from(input));
    match input.strip_prefix('~') {
        Some("") => home(),
        Some(rest) if rest.starts_with('/') || (cfg!(windows) && rest.starts_with('\\')) => {
            home().join(&rest[1..])
        }
        _ => PathBuf::from(input),
    }
}

pub fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

/// Normalize a user-supplied `--path` prefix into an absolute path that matches the absolute
/// `cwd` / `repo_root` the indexer stores. A leading `~` expands to the home directory
/// ([`expand_tilde`]); the result is then resolved against the current working directory so
/// relative inputs (`.`, `src/foo`, `../bar`) work:
///   * [`std::fs::canonicalize`] is tried first — it resolves `.`/`..` and symlinks to the real
///     absolute path, matching the canonical cwd tools record via `getcwd` (so `--path .` from a
///     symlinked checkout resolves the same way the session's cwd was stored);
///   * if the path does not exist (filtering by a deleted or another machine's directory),
///     [`std::path::absolute`] makes it absolute lexically, without touching the filesystem.
///
/// Shared by the session-level (`build_filters`) and message-level (`messages …`) filters so
/// `--path` behaves identically everywhere (DRY).
pub fn normalize_path_prefix(path: &str) -> String {
    let expanded = expand_tilde(path);
    std::fs::canonicalize(&expanded)
        .or_else(|_| std::path::absolute(&expanded))
        .map(|p| normalize_path(&p))
        .unwrap_or_else(|_| normalize_path(&expanded))
}

/// Basename of a path string: the final component after the last `/` or `\`. Splits on
/// BOTH separators (not just the host OS's) so a Windows-style path captured on a Windows
/// machine but searched on a unix host — e.g. a cursor/antigravity session — still yields
/// the file name. Falls back to the whole string when there is no separator, so a
/// searchable name is always recorded. Shared by every provider's file-edit extraction.
pub fn file_basename(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}

pub fn find_repo_root(cwd: &str) -> Option<String> {
    let mut current = PathBuf::from(cwd);
    loop {
        let git_path = current.join(".git");
        if git_path.exists() {
            // .git is a file in git worktrees (and submodules) — try to resolve to the real root
            if git_path.is_file() {
                if let Ok(content) = fs::read_to_string(&git_path) {
                    if let Some(gitdir) = content.trim().strip_prefix("gitdir: ") {
                        // Resolve relative gitdir against the directory containing the .git file
                        let gitdir_path = {
                            let p = PathBuf::from(gitdir);
                            if p.is_absolute() {
                                p
                            } else {
                                current.join(p)
                            }
                        };
                        // Worktree gitdir is <repo>/.git/worktrees/<name>; go up 3 levels.
                        // Validate the result actually contains a .git dir so submodule gitdirs
                        // (which land on .git itself) fall through to the regular path.
                        let resolved = gitdir_path
                            .parent()
                            .and_then(|p| p.parent())
                            .and_then(|p| p.parent())
                            .filter(|p| p.join(".git").exists());
                        if let Some(resolved) = resolved {
                            return Some(resolved.to_string_lossy().to_string());
                        }
                    }
                }
            }
            return Some(current.to_string_lossy().to_string());
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Whitespace-compact `value` and cap it to `max_len` **characters** (not bytes), so
/// multibyte/emoji content is budgeted by visible length and never split mid-codepoint.
/// When truncation is needed, 3 chars are reserved for the `...` ellipsis.
pub fn truncate_for_display(value: &str, max_len: usize) -> String {
    let compact = compact_whitespace(value);
    if compact.chars().count() <= max_len {
        compact
    } else {
        let keep = max_len.saturating_sub(3);
        let truncated: String = compact.chars().take(keep).collect();
        format!("{truncated}...")
    }
}

pub fn select_transcript_lines(transcript: &str, max_lines: i64) -> (String, String) {
    if max_lines == 0 {
        return (transcript.to_string(), "all".to_string());
    }
    if max_lines < 0 {
        let requested = max_lines.unsigned_abs() as usize;
        let mut selected = std::collections::VecDeque::new();
        let mut seen = 0usize;
        for line in transcript.lines() {
            seen += 1;
            if selected.len() == requested {
                selected.pop_front();
            }
            selected.push_back(line);
        }
        let label = if seen > selected.len() {
            format!(
                "last {} (truncated; max_lines=0 returns the entire transcript and may be very large)",
                selected.len()
            )
        } else {
            selected.len().to_string()
        };
        return (selected.into_iter().collect::<Vec<_>>().join("\n"), label);
    }

    let max_lines = max_lines as usize;
    let mut lines = transcript.lines();
    let selected: Vec<&str> = lines.by_ref().take(max_lines).collect();
    let truncated = lines.next().is_some();
    let label = if truncated {
        format!(
            "first {max_lines} (truncated; max_lines=0 returns the entire transcript and may be very large)"
        )
    } else {
        selected.len().to_string()
    };
    (selected.join("\n"), label)
}

pub fn compact_whitespace(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for (i, word) in value.split_whitespace().enumerate() {
        if i > 0 {
            result.push(' ');
        }
        result.push_str(word);
    }
    result
}

pub fn relative_age(value: Option<DateTime<Utc>>) -> String {
    let Some(value) = value else {
        return "-".to_string();
    };
    let delta = Utc::now().signed_duration_since(value);
    if delta < Duration::minutes(1) {
        "just now".to_string()
    } else if delta < Duration::hours(1) {
        format!("{}m ago", delta.num_minutes())
    } else if delta < Duration::days(1) {
        format!("{}h ago", delta.num_hours())
    } else if delta < Duration::days(30) {
        format!("{}d ago", delta.num_days())
    } else {
        value.format("%Y-%m-%d").to_string()
    }
}

pub fn prompt_confirm(prompt: &str) -> anyhow::Result<bool> {
    print!("{prompt} [y/N]: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(matches!(
        input.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

pub fn render_command(parts: &[String]) -> String {
    parts
        .iter()
        .map(|part| {
            if part.contains(' ') {
                format!("{part:?}")
            } else {
                part.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn parse_datetime(value: &str) -> Option<DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|| {
            chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .ok()
                .and_then(|date| date.and_hms_opt(0, 0, 0))
                .map(|naive| naive.and_utc())
        })
}

pub fn parse_unix_seconds(value: i64) -> Option<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp(value, 0)
}

pub fn preview_from_text(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        "(no preview available)".to_string()
    } else {
        truncate_for_display(trimmed, 140)
    }
}

pub fn extract_text(value: &Value) -> String {
    fn walk(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(text) => {
                if !text.trim().is_empty() {
                    out.push(text.trim().to_string());
                }
            }
            Value::Array(items) => {
                for item in items {
                    walk(item, out);
                }
            }
            Value::Object(map) => {
                if let Some(text) = map.get("text").and_then(Value::as_str) {
                    if !text.trim().is_empty() {
                        out.push(text.trim().to_string());
                    }
                }
                if let Some(content) = map.get("content") {
                    walk(content, out);
                }
                if let Some(message) = map.get("message") {
                    walk(message, out);
                }
                if let Some(input) = map.get("input") {
                    walk(input, out);
                }
                if let Some(output) = map.get("output") {
                    walk(output, out);
                }
            }
            _ => {}
        }
    }

    let mut parts = Vec::new();
    walk(value, &mut parts);
    parts.join("\n")
}

pub fn substantive_text(value: &str) -> bool {
    let normalized = value.trim();
    if normalized.is_empty() {
        return false;
    }

    if normalized.contains("<local-command-") || normalized.contains("<command-name>/") {
        return false;
    }

    let ignored = [
        "/exit",
        "/clear",
        "/compact",
        "resume cancelled",
        "i'll start by studying",
    ];

    !ignored
        .iter()
        .any(|needle| normalized.eq_ignore_ascii_case(needle))
}

pub fn snippet_from_match(value: &str, query: &str, max_len: usize) -> String {
    let compact = compact_whitespace(value);
    if compact.is_empty() {
        return "(no snippet available)".to_string();
    }

    let compact_lower = compact.to_ascii_lowercase();
    let query_lower = query.to_ascii_lowercase();
    if let Some(index) = compact_lower.find(&query_lower) {
        let half = max_len / 2;
        let mut start = index.saturating_sub(half);
        let mut end = (index + query.len() + half).min(compact.len());

        while start > 0 && !compact.is_char_boundary(start) {
            start -= 1;
        }
        while end < compact.len() && !compact.is_char_boundary(end) {
            end += 1;
        }

        let mut snippet = compact[start..end].to_string();
        if start > 0 {
            snippet = format!("...{snippet}");
        }
        if end < compact.len() {
            snippet.push_str("...");
        }
        return snippet;
    }

    for token in query_lower.split_whitespace() {
        if token.is_empty() {
            continue;
        }
        if let Some(index) = compact_lower.find(token) {
            let half = max_len / 2;
            let mut start = index.saturating_sub(half);
            let mut end = (index + token.len() + half).min(compact.len());
            while start > 0 && !compact.is_char_boundary(start) {
                start -= 1;
            }
            while end < compact.len() && !compact.is_char_boundary(end) {
                end += 1;
            }

            let mut snippet = compact[start..end].to_string();
            if start > 0 {
                snippet = format!("...{snippet}");
            }
            if end < compact.len() {
                snippet.push_str("...");
            }
            return snippet;
        }
    }

    truncate_for_display(&compact, max_len)
}

pub fn highlight_matches(value: &str, query: &str) -> String {
    let mut terms = Vec::new();
    let trimmed = query.trim();
    if !trimmed.is_empty() {
        terms.push(trimmed.to_string());
    }
    let stopwords = [
        "a", "an", "and", "are", "as", "at", "based", "be", "but", "by", "can", "check", "do",
        "double", "for", "from", "has", "have", "how", "i", "in", "into", "is", "it", "made",
        "not", "of", "on", "or", "please", "some", "that", "the", "this", "to", "update", "what",
        "with", "you", "your",
    ];
    for token in trimmed.split_whitespace() {
        if token.len() >= 3
            && !stopwords.contains(&token.to_ascii_lowercase().as_str())
            && !terms
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(token))
        {
            terms.push(token.to_string());
        }
    }
    if terms.is_empty() {
        return value.to_string();
    }

    terms.sort_by_key(|term| std::cmp::Reverse(term.len()));
    let pattern = terms
        .iter()
        .map(|term| regex::escape(term))
        .collect::<Vec<_>>()
        .join("|");

    let Ok(regex) = RegexBuilder::new(&pattern).case_insensitive(true).build() else {
        return value.to_string();
    };

    regex.replace_all(value, "[[$0]]").into_owned()
}

pub fn format_transcript_line(role: &str, timestamp: Option<DateTime<Utc>>, text: &str) -> String {
    let stamp = timestamp
        .map(|value| value.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "unknown-time".to_string());
    format!("[{stamp}] {role}\n{text}")
}

/// Classify a raw provider role string + message text into the normalized [`Role`].
/// A user message that begins with a slash command (e.g. "/ar:plannew") becomes
/// [`Role::Slash`]; this is uniform across providers so analytics can group on it.
pub fn classify_role(role: &str, text: &str) -> Role {
    match role.to_ascii_lowercase().as_str() {
        "assistant" | "model" => Role::Assistant,
        "tool" | "toolresult" | "tool_result" => Role::Tool,
        "compaction" => Role::Compaction,
        _ if is_slash_command(text) => Role::Slash,
        _ => Role::User,
    }
}

/// Extract the leading slash-command token (`/name`) when `text` begins with one,
/// terminated by whitespace or end-of-string. Matches a leading `/name` token without
/// lookahead, so file paths like `/Users/foo/bar` return None (the token
/// `Users` is followed by `/`, not whitespace). Reused by slash classification and
/// planning-command aggregation.
pub fn slash_command_token(text: &str) -> Option<String> {
    let trimmed = text.trim_start();
    let rest = trimmed.strip_prefix('/')?;
    // Command name must start with a word character.
    if !rest
        .chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
    {
        return None;
    }
    // Consume the command-name token (word chars plus ':' '.' '-').
    let end = rest
        .char_indices()
        .find(|(_, c)| !(c.is_alphanumeric() || matches!(c, '_' | ':' | '.' | '-')))
        .map(|(i, _)| i)
        .unwrap_or(rest.len());
    // The character following the token must be whitespace or end-of-string.
    if rest[end..].chars().next().is_none_or(|c| c.is_whitespace()) {
        Some(format!("/{}", &rest[..end]))
    } else {
        None
    }
}

fn is_slash_command(text: &str) -> bool {
    slash_command_token(text).is_some()
}

/// Convert a provider's accumulated `(role, text, ts)` tuples into persisted [`Message`]
/// rows, assigning sequence numbers and normalizing roles via [`classify_role`].
/// Use [`to_messages_with_tools`] when a provider can name the tool behind a
/// [`Role::Tool`] message.
pub fn to_messages(raw: Vec<(String, String, Option<DateTime<Utc>>)>) -> Vec<Message> {
    to_messages_with_tools(raw.into_iter().map(|(r, t, ts)| (r, t, ts, None)).collect())
}

/// A provider's accumulated raw message before normalization: `(role, text, ts, tool_name)`.
/// `tool_name` is the tool a [`Role::Tool`] message came from (e.g. `"Bash"`), or `None`.
pub type RawMessage = (String, String, Option<DateTime<Utc>>, Option<String>);

/// Compact, provider-neutral searchable content for a tool-call input row. Tool outputs remain
/// separate messages; this records what the agent attempted to call so commands, URLs, paths, and
/// MCP arguments are discoverable without reading raw JSONL files.
pub fn tool_call_message_content(tool_name: &str, args: Value) -> String {
    json!({
        "kind": "tool_call",
        "tool_name": tool_name,
        "args": args,
    })
    .to_string()
}

/// Like [`to_messages`], but each tuple also carries an optional `tool_name` — the tool a
/// [`Role::Tool`] message came from (e.g. `"Bash"`, `"ls"`, `"apply_patch"`). The name is
/// stored only as supplied; role classification still derives from the role/text so a
/// non-tool message with an incidental name is unaffected.
pub fn to_messages_with_tools(raw: Vec<RawMessage>) -> Vec<Message> {
    raw.into_iter()
        .enumerate()
        .map(|(i, (role, text, ts, tool_name))| {
            let normalized = classify_role(&role, &text);
            Message {
                seq: i as i64,
                role: normalized,
                ts,
                tool_name,
                is_compaction: normalized == Role::Compaction,
                content: text,
            }
        })
        .collect()
}

#[cfg(test)]
mod role_classification_tests {
    use super::*;

    #[test]
    fn slash_commands_classified_but_paths_excluded() {
        assert_eq!(
            classify_role("user", "/ar:plannew make a plan"),
            Role::Slash
        );
        assert_eq!(classify_role("user", "/help"), Role::Slash);
        assert_eq!(classify_role("user", "/ar:ok 'git push'"), Role::Slash);
        // File paths / tool output starting with '/' are NOT slash commands.
        assert_eq!(classify_role("user", "/Users/foo/bar/.zshrc"), Role::User);
        assert_eq!(classify_role("user", "/usr/local/bin exists"), Role::User);
        assert_eq!(classify_role("user", "hello world"), Role::User);
        // Role passthrough + normalization.
        assert_eq!(classify_role("assistant", "anything"), Role::Assistant);
        assert_eq!(classify_role("model", "x"), Role::Assistant);
    }

    #[test]
    fn to_messages_assigns_sequence_and_roles() {
        let raw = vec![
            ("user".to_string(), "hi".to_string(), None),
            ("assistant".to_string(), "yo".to_string(), None),
            ("user".to_string(), "/help".to_string(), None),
        ];
        let msgs = to_messages(raw);
        assert_eq!(msgs.len(), 3);
        assert_eq!((msgs[0].seq, msgs[2].seq), (0, 2));
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[1].role, Role::Assistant);
        assert_eq!(msgs[2].role, Role::Slash);
    }
}

pub fn minimal_record(provider: Provider, path: &Path, warning: String) -> ParsedSession {
    let provider_session_id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("unknown")
        .to_string();
    let parse_version = provider_parse_version(provider);
    ParsedSession {
        session: SessionRecord {
            id: format!("{provider}:{provider_session_id}"),
            provider,
            provider_session_id,
            title: None,
            summary: None,
            cwd: None,
            repo_root: None,
            created_at: None,
            updated_at: None,
            last_message_at: None,
            preview_text: "(parse failed)".to_string(),
            source_path: normalize_path(path),
            message_count: Some(0),
            parse_version: parse_version.to_string(),
            raw_metadata_json: None,
            parse_warning: Some(warning),
            discovery_source: "jsonl".to_string(),
        },
        transcript_text: String::new(),
        messages: Vec::new(),
        file_edits: Vec::new(),
    }
}

pub fn provider_parse_version(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "claude-v2",
        Provider::ClaudeDesktop => "claude-desktop-local-agent-v2",
        Provider::Codex => "codex-v2",
        Provider::Cursor => "cursor-v2",
        Provider::Antigravity => "antigravity-v1",
        Provider::Pi => "pi-v1",
    }
}

/// Convert a file mtime in nanoseconds-since-epoch to a UTC datetime.
pub fn datetime_from_mtime_ns(mtime_ns: i64) -> Option<DateTime<Utc>> {
    let secs = mtime_ns.div_euclid(1_000_000_000);
    let nanos = mtime_ns.rem_euclid(1_000_000_000) as u32;
    DateTime::from_timestamp(secs, nanos)
}

/// Guarantee a session has dates: fall back to the file mtime for any of
/// `created_at`/`updated_at`/`last_message_at` the parser left unset. The session file's
/// modification time is always available and is a faithful "last activity" timestamp, so
/// no session is ever left "undated" — date filters (`--since`/`--until`) and recency
/// sorting then always have a real value to compare. Parser-provided dates win.
pub fn backfill_session_dates(session: &mut SessionRecord, mtime_ns: i64) {
    let mtime = datetime_from_mtime_ns(mtime_ns);
    if session.updated_at.is_none() {
        session.updated_at = mtime;
    }
    if session.created_at.is_none() {
        session.created_at = mtime;
    }
    if session.last_message_at.is_none() {
        session.last_message_at = session.updated_at;
    }
}

fn fallback_event_time(session: &SessionRecord, mtime_ns: i64) -> Option<DateTime<Utc>> {
    session
        .last_message_at
        .or(session.updated_at)
        .or(session.created_at)
        .or_else(|| datetime_from_mtime_ns(mtime_ns))
}

/// Guarantee date-filterable per-message / file-edit rows when a provider lacks
/// per-event timestamps. Parser-provided event timestamps win; only missing values
/// fall back to the session/file timestamp. This is O(messages + file_edits), in
/// place, and runs once per parsed session or appended tail.
pub fn backfill_event_dates(
    session: &SessionRecord,
    messages: &mut [Message],
    file_edits: &mut [FileEdit],
    mtime_ns: i64,
) {
    let fallback = fallback_event_time(session, mtime_ns);
    for message in messages {
        if message.ts.is_none() {
            message.ts = fallback;
        }
    }
    for edit in file_edits {
        if edit.ts.is_none() {
            edit.ts = fallback;
        }
    }
}

/// Backfill both session-level dates and per-event dates for an indexed parse.
/// Use this before persisting a full parse so strict date filters can remain
/// precise without losing providers that only expose file/session timestamps.
pub fn backfill_parsed_dates(parsed: &mut ParsedSession, mtime_ns: i64) {
    backfill_session_dates(&mut parsed.session, mtime_ns);
    backfill_event_dates(
        &parsed.session,
        &mut parsed.messages,
        &mut parsed.file_edits,
        mtime_ns,
    );
}

pub fn current_repo(config: &Config) -> Option<String> {
    if !config.search.prefer_current_repo {
        return None;
    }
    std::env::current_dir()
        .ok()
        .and_then(|path| path.to_str().map(ToOwned::to_owned))
        .as_deref()
        .and_then(find_repo_root)
}

pub fn resume_plan(session: &SessionRecord) -> Result<(Vec<String>, Option<String>)> {
    let binary = match session.provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
        Provider::Pi => "pi",
        Provider::ClaudeDesktop | Provider::Cursor | Provider::Antigravity => {
            let id = &session.id;
            let provider = session.provider.as_str();
            return Err(anyhow!(
                "resuming is not supported for {provider} sessions — read this one here with \
                 `sessiongrep show {id}` or `sessiongrep export {id}`"
            ));
        }
    };
    if which(binary).is_none() {
        let id = &session.id;
        return Err(anyhow!(
            "`{binary}` is not on your PATH — resume launches the native CLI, so install `{binary}` \
             or add it to PATH. You can still read this session with `sessiongrep show {id}` or \
             `sessiongrep export {id}`"
        ));
    }
    let cwd = session
        .cwd
        .clone()
        .filter(|path| PathBuf::from(path).exists());
    let command = match session.provider {
        Provider::Claude => vec![
            "claude".to_string(),
            "--resume".to_string(),
            session.provider_session_id.clone(),
        ],
        Provider::Codex => vec![
            "codex".to_string(),
            "resume".to_string(),
            session.provider_session_id.clone(),
        ],
        Provider::Pi => vec![
            "pi".to_string(),
            "--session".to_string(),
            session.provider_session_id.clone(),
        ],
        Provider::ClaudeDesktop | Provider::Cursor | Provider::Antigravity => {
            unreachable!("resume is handled before command construction")
        }
    };
    Ok((command, cwd))
}

pub fn which(binary: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|path| path.join(binary))
            .find(|candidate| candidate.exists())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lines_replacing_invalid_utf8_recovers_bad_bytes_and_matches_lines_semantics() {
        use std::io::Cursor;
        let lines = |bytes: &[u8]| {
            lines_replacing_invalid_utf8(Cursor::new(bytes.to_vec()))
                .collect::<std::io::Result<Vec<_>>>()
                .unwrap()
        };
        // Split on `\n`, with no trailing empty line for input ending in `\n`.
        assert_eq!(lines(b"a\nb\n"), vec!["a", "b"]);
        // A final line without a trailing newline is still yielded.
        assert_eq!(lines(b"a\nb"), vec!["a", "b"]);
        // `\r\n` endings: the `\r` is stripped, matching `BufRead::lines`.
        assert_eq!(lines(b"a\r\nb\r\n"), vec!["a", "b"]);
        // A blank line is preserved as an empty string; empty input yields nothing.
        assert_eq!(lines(b"a\n\nb\n"), vec!["a", "", "b"]);
        assert!(lines(b"").is_empty());
        // Invalid UTF-8 (0xFF) becomes U+FFFD; the rest of the line is preserved, not dropped.
        assert_eq!(lines(&[b'h', b'i', 0xFF, b'!', b'\n']), vec!["hi\u{FFFD}!"]);
    }

    #[test]
    fn resume_plan_unsupported_provider_points_to_show_and_export() {
        // Cursor/antigravity can't be resumed; the error must offer a usable alternative
        // (read the session here) and reference the concrete session id.
        let parsed = minimal_record(
            Provider::Cursor,
            std::path::Path::new("/tmp/9f3b844f.jsonl"),
            "n/a".to_string(),
        );
        let err = resume_plan(&parsed.session).unwrap_err().to_string();
        assert!(err.contains("not supported"), "{err}");
        assert!(
            err.contains("sessiongrep show"),
            "offers an alternative: {err}"
        );
        assert!(
            err.contains(&parsed.session.id),
            "references the session id: {err}"
        );
    }

    #[test]
    fn extracts_nested_text() {
        let value = json!({
            "content": [
                {"text": "hello"},
                {"content": [{"text": "world"}]}
            ]
        });
        assert_eq!(extract_text(&value), "hello\nworld");
    }

    #[test]
    fn trims_preview() {
        let preview = preview_from_text("a ".repeat(100).as_str());
        assert!(preview.len() <= 140);
    }

    #[test]
    fn file_basename_splits_both_separators() {
        assert_eq!(file_basename("/Users/x/src/main.rs"), "main.rs");
        assert_eq!(file_basename("src/lib.rs"), "lib.rs");
        // No separator → the whole string (always something searchable).
        assert_eq!(file_basename("main.rs"), "main.rs");
        // Windows-style path captured on Windows, searched on a unix host.
        assert_eq!(file_basename(r"C:\Users\x\proj\main.rs"), "main.rs");
        assert_eq!(file_basename(r"proj\sub\a.txt"), "a.txt");
    }

    #[test]
    fn expand_tilde_handles_bare_and_prefixed_home() {
        let home = dirs::home_dir().expect("home dir");
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(
            expand_tilde("~/src/sessiongrep"),
            home.join("src/sessiongrep")
        );
        // `~user` is NOT expanded (we don't resolve other users' homes).
        assert_eq!(expand_tilde("~bob/x"), PathBuf::from("~bob/x"));
        // A non-tilde path is returned unchanged by expand_tilde.
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
    }

    #[test]
    fn normalize_path_prefix_resolves_relative_paths_via_cwd() {
        // `.` resolves to the canonical current directory, so `--path .` matches sessions
        // recorded in this directory — relative paths "just work".
        let cwd_canon = normalize_path(&std::fs::canonicalize(".").expect("cwd canonicalize"));
        let here = normalize_path_prefix(".");
        assert!(Path::new(&here).is_absolute(), "{here}");
        assert_eq!(here, cwd_canon, "`.` resolves to the canonical cwd");

        // `..` is resolved (not left literal), so it can prefix-match a stored absolute path.
        let parent = normalize_path_prefix("..");
        assert!(
            !parent.contains(".."),
            "`..` must be resolved away: {parent}"
        );
        assert!(
            cwd_canon.starts_with(&parent),
            "{cwd_canon} should be under {parent}"
        );

        // A non-existent relative path falls back to lexical absolute (still under the cwd).
        let sub = normalize_path_prefix("no_such_dir_xyz/child");
        assert!(Path::new(&sub).is_absolute(), "{sub}");
        assert!(
            sub.starts_with(&cwd_canon),
            "{sub} should be under {cwd_canon}"
        );

        // `~` expands to an absolute home path; a non-existent absolute path is left absolute
        // (the lexical fallback), so absolute filters keep working even for dirs not on disk.
        assert!(Path::new(&normalize_path_prefix("~")).is_absolute());
        assert_eq!(normalize_path_prefix("/Users/x/proj"), "/Users/x/proj");

        // An EXISTING absolute path round-trips to its canonical form, and a trailing slash or
        // `/.` component is normalized away so the prefix matches the stored dir exactly.
        let td = tempfile::tempdir().expect("tempdir");
        let canon = normalize_path(&std::fs::canonicalize(td.path()).unwrap());
        let abs = td.path().display().to_string();
        assert_eq!(normalize_path_prefix(&abs), canon);
        assert_eq!(normalize_path_prefix(&format!("{abs}/")), canon);
        assert_eq!(normalize_path_prefix(&format!("{abs}/.")), canon);
    }

    #[test]
    fn backfill_session_dates_fills_from_mtime_but_keeps_parsed_dates() {
        use crate::models::Provider;
        let mut rec = minimal_record(Provider::Claude, Path::new("/x/s.jsonl"), "w".into()).session;
        // minimal_record leaves all three dates unset → an "undated" session.
        assert!(rec.updated_at.is_none() && rec.created_at.is_none());
        let mtime_ns = 1_700_000_000_000_000_000; // 2023-11-14T22:13:20Z
        let mtime = datetime_from_mtime_ns(mtime_ns);
        assert!(mtime.is_some());
        backfill_session_dates(&mut rec, mtime_ns);
        assert_eq!(rec.updated_at, mtime, "no session left undated");
        assert_eq!(rec.created_at, mtime);
        assert_eq!(rec.last_message_at, mtime);
        // A parser-provided date is preserved (not overwritten by mtime).
        let parsed = datetime_from_mtime_ns(1).unwrap();
        rec.updated_at = Some(parsed);
        backfill_session_dates(&mut rec, mtime_ns);
        assert_eq!(rec.updated_at, Some(parsed));
    }

    #[test]
    fn backfill_parsed_dates_fills_missing_event_timestamps_only() {
        let mtime_ns = 1_800_000_000_123_000_000;
        let explicit = datetime_from_mtime_ns(1_700_000_000_000_000_000).unwrap();
        let mut parsed = minimal_record(Provider::Claude, Path::new("/tmp/s.jsonl"), String::new());
        parsed.session.updated_at = Some(explicit);
        parsed.messages = vec![
            Message {
                seq: 0,
                role: Role::User,
                ts: None,
                tool_name: None,
                is_compaction: false,
                content: "undated".into(),
            },
            Message {
                seq: 1,
                role: Role::Assistant,
                ts: Some(explicit),
                tool_name: None,
                is_compaction: false,
                content: "dated".into(),
            },
        ];
        parsed.file_edits = vec![FileEdit {
            seq: 0,
            ts: None,
            tool: "Write".into(),
            file_path: "/tmp/a.rs".into(),
            file_name: "a.rs".into(),
            new_content: Some("fn main() {}\n".into()),
            edits: Vec::new(),
        }];

        backfill_parsed_dates(&mut parsed, mtime_ns);

        let fallback = parsed.session.last_message_at.expect("fallback timestamp");
        assert_eq!(fallback, explicit);
        assert_eq!(parsed.messages[0].ts, Some(fallback));
        assert_eq!(
            parsed.messages[1].ts,
            Some(explicit),
            "existing event timestamps win"
        );
        assert_eq!(parsed.file_edits[0].ts, Some(fallback));
    }

    #[test]
    fn truncate_for_display_counts_chars_not_bytes() {
        // 60 emoji fit within a 120-CHAR budget even though they are 240 bytes.
        let emoji = "😀".repeat(60);
        assert_eq!(truncate_for_display(&emoji, 120), emoji);
        // Over budget: keep (limit-3) chars + ellipsis, on a char boundary.
        let many = "é".repeat(200);
        let out = truncate_for_display(&many, 10);
        assert_eq!(out, format!("{}...", "é".repeat(7)));
        assert_eq!(out.chars().count(), 10);
    }

    #[test]
    fn select_transcript_lines_supports_head_tail_and_all() {
        let transcript = "one\ntwo\nthree\nfour";
        let (head, head_label) = select_transcript_lines(transcript, 2);
        assert_eq!(head, "one\ntwo");
        assert_eq!(
            head_label,
            "first 2 (truncated; max_lines=0 returns the entire transcript and may be very large)"
        );

        let (tail, tail_label) = select_transcript_lines(transcript, -2);
        assert_eq!(tail, "three\nfour");
        assert_eq!(
            tail_label,
            "last 2 (truncated; max_lines=0 returns the entire transcript and may be very large)"
        );

        let (all, all_label) = select_transcript_lines(transcript, 0);
        assert_eq!(all, transcript);
        assert_eq!(all_label, "all");
    }

    #[test]
    fn select_transcript_lines_labels_untruncated_short_transcripts_by_count() {
        let transcript = "one\ntwo";
        assert_eq!(
            select_transcript_lines(transcript, 10),
            (transcript.to_string(), "2".to_string())
        );
        assert_eq!(
            select_transcript_lines(transcript, -10),
            (transcript.to_string(), "2".to_string())
        );
    }

    #[test]
    fn select_transcript_lines_tail_keeps_only_requested_suffix() {
        let transcript = (0..10_000)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (tail, label) = select_transcript_lines(&transcript, -3);
        assert_eq!(tail, "line 9997\nline 9998\nline 9999");
        assert_eq!(
            label,
            "last 3 (truncated; max_lines=0 returns the entire transcript and may be very large)"
        );
    }

    #[test]
    fn builds_match_snippet() {
        let text = "alpha beta gamma delta epsilon zeta eta theta";
        let snippet = snippet_from_match(text, "delta", 20);
        assert!(snippet.contains("delta"));
    }

    #[test]
    fn highlights_matches() {
        let value = highlight_matches("alpha beta gamma", "beta");
        assert!(value.contains("[[beta]]"));
    }

    #[test]
    fn find_repo_root_resolves_worktree_to_main_repo() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let main_repo = dir.path().join("myrepo");
        let worktree_gitdir = main_repo.join(".git").join("worktrees").join("wt");
        let wt_dir = dir.path().join("wt");
        fs::create_dir_all(&main_repo).unwrap();
        fs::create_dir_all(main_repo.join(".git")).unwrap();
        fs::create_dir_all(&worktree_gitdir).unwrap();
        fs::create_dir_all(&wt_dir).unwrap();
        fs::write(
            wt_dir.join(".git"),
            format!("gitdir: {}", worktree_gitdir.display()),
        )
        .unwrap();
        let root = find_repo_root(wt_dir.to_str().unwrap());
        assert_eq!(root.as_deref(), Some(main_repo.to_str().unwrap()));
    }

    #[test]
    fn find_repo_root_does_not_resolve_submodule_as_worktree() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let super_repo = dir.path().join("superrepo");
        let submodule_dir = super_repo.join("packages").join("foo");
        let submodule_gitdir = super_repo
            .join(".git")
            .join("modules")
            .join("packages")
            .join("foo");
        fs::create_dir_all(&super_repo).unwrap();
        fs::create_dir_all(super_repo.join(".git")).unwrap();
        fs::create_dir_all(&submodule_dir).unwrap();
        fs::create_dir_all(&submodule_gitdir).unwrap();
        // Submodule .git file points into <super>/.git/modules/...
        fs::write(
            submodule_dir.join(".git"),
            format!("gitdir: {}", submodule_gitdir.display()),
        )
        .unwrap();
        // Should resolve to the submodule's own directory (falls through worktree logic)
        // rather than some garbage path 3 levels above the modules entry
        let root = find_repo_root(submodule_dir.to_str().unwrap());
        assert_eq!(root.as_deref(), Some(submodule_dir.to_str().unwrap()));
    }
}
