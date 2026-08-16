// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-FileCopyrightText: 2026 Nisarg Patel
// SPDX-FileCopyrightText: 2026 Thomas Funk
// SPDX-License-Identifier: Apache-2.0

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use chrono::{DateTime, Duration, Utc};
use regex::RegexBuilder;
use serde_json::{json, Value};

use crate::config::Config;
use crate::models::{
    FileEdit, Message, MessageAuthorship, MessageContentPart, MessageCorrelationAuthority,
    MessageCorrelationIdentity, MessageKind, MessageProvenance, MessageRecordRelation,
    ParsedSession, Provider, Role, SessionRecord,
};

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

/// Expand a leading `~`, FAILING when there is no home directory to expand it to.
///
/// The counterpart to [`expand_tilde`], which falls back to the literal input. That fallback is
/// right for a configured path that is only read -- a missing directory is simply not found -- and
/// wrong for a path that gets WRITTEN to: `~/skills` with no home would silently create a
/// directory literally named `~` in the working directory, leaving junk somewhere the caller never
/// named. Prefer this wherever the expansion decides where a file lands.
///
/// # Errors
///
/// Returns an error naming the environment variables to set when the path starts with `~` and no
/// home directory can be resolved.
pub fn expand_tilde_required(path: &Path) -> anyhow::Result<PathBuf> {
    let home = || {
        dirs::home_dir().ok_or_else(|| {
            anyhow::anyhow!(
                "cannot expand `~` in {}: no home directory was found. Set HOME (or USERPROFILE \
                 on Windows), or pass an absolute path",
                path.display()
            )
        })
    };
    if path == Path::new("~") {
        home()
    } else if let Ok(rest) = path.strip_prefix(Path::new("~")) {
        Ok(home()?.join(rest))
    } else {
        Ok(path.to_path_buf())
    }
}

pub fn normalize_path(path: &Path) -> String {
    let rendered = path.to_string_lossy();
    #[cfg(windows)]
    {
        if let Some(unc) = rendered.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{unc}");
        }
        if let Some(disk) = rendered.strip_prefix(r"\\?\") {
            return disk.to_string();
        }
    }
    rendered.into_owned()
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
    // Session databases are portable. Preserve an absolute path recorded on a different host
    // instead of interpreting it relative to this host's current drive or working directory.
    #[cfg(windows)]
    if path.starts_with('/') {
        return path.to_string();
    }
    #[cfg(not(windows))]
    if is_windows_absolute_path(path) {
        return path.to_string();
    }

    let expanded = expand_tilde(path);
    std::fs::canonicalize(&expanded)
        .or_else(|_| std::path::absolute(&expanded))
        .map(|p| normalize_path(&p))
        .unwrap_or_else(|_| normalize_path(&expanded))
}

#[cfg(not(windows))]
fn is_windows_absolute_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with(r"\\")
        || matches!(bytes, [drive, b':', b'\\' | b'/', ..] if drive.is_ascii_alphabetic())
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
///
/// The input is compacted lazily. A truncated result consumes only the raw-input prefix needed
/// to observe `max_len + 1` compacted characters; a result that fits must consume the whole input
/// to prove that no later non-whitespace character exists. Allocation is bounded by the compacted
/// output retained for that decision, rather than by `value.len()`.
pub fn truncate_for_display(value: &str, max_len: usize) -> String {
    truncate_for_display_with_extent(value, max_len).0
}

/// A reusable Unicode-caseless substring matcher for one already-lowercased needle.
///
/// Rust's [`str::to_lowercase`] can expand one scalar value into several (for example, `İ`), so
/// byte-wise or scalar-wise case folding would not preserve the existing search contract. The
/// streaming matcher feeds each scalar's full lowercase expansion through a KMP matcher instead;
/// it therefore has the same sequence semantics as `haystack.to_lowercase().contains(...)` while
/// retaining only the lowercased needle and its prefix table.
pub(crate) struct UnicodeLowerNeedle {
    pattern: Vec<char>,
    prefix: Vec<usize>,
}

impl UnicodeLowerNeedle {
    pub(crate) fn from_lowered(lowered_needle: &str) -> Self {
        let pattern = lowered_needle.chars().collect::<Vec<_>>();
        let mut prefix = vec![0_usize; pattern.len()];
        for index in 1..pattern.len() {
            let mut matched = prefix[index - 1];
            while matched > 0 && pattern[index] != pattern[matched] {
                matched = prefix[matched - 1];
            }
            if pattern[index] == pattern[matched] {
                matched += 1;
            }
            prefix[index] = matched;
        }
        Self { pattern, prefix }
    }

    pub(crate) fn contains(&self, haystack: &str) -> bool {
        if self.pattern.is_empty() {
            return true;
        }

        let mut matched = 0_usize;
        for lowered in haystack
            .chars()
            .flat_map(|character| character.to_lowercase())
        {
            while matched > 0 && lowered != self.pattern[matched] {
                matched = self.prefix[matched - 1];
            }
            if lowered == self.pattern[matched] {
                matched += 1;
                if matched == self.pattern.len() {
                    return true;
                }
            }
        }
        false
    }
}

/// The compact display string plus whether non-whitespace content was omitted at the end.
pub fn truncate_for_display_with_extent(value: &str, max_len: usize) -> (String, bool) {
    truncate_compacted_chars(value.chars(), max_len)
}

fn truncate_compacted_chars(chars: impl Iterator<Item = char>, max_len: usize) -> (String, bool) {
    fn push_char(
        compact: &mut String,
        character: char,
        compact_chars: &mut usize,
        keep_bytes: &mut usize,
        keep_chars: usize,
        max_len: usize,
    ) -> bool {
        *compact_chars += 1;
        if *compact_chars > max_len {
            compact.truncate(*keep_bytes);
            compact.push_str("...");
            return false;
        }

        compact.push(character);
        if *compact_chars == keep_chars {
            *keep_bytes = compact.len();
        }
        true
    }

    // A small cap should never reserve in proportion to a huge input. The string grows only when
    // the requested character cap itself requires more retained output.
    let mut compact = String::with_capacity(max_len.min(256));
    let keep_chars = max_len.saturating_sub(3);
    let mut keep_bytes = 0;
    let mut compact_chars = 0;
    let mut saw_non_whitespace = false;
    let mut pending_space = false;

    for character in chars {
        if character.is_whitespace() {
            pending_space = saw_non_whitespace;
            continue;
        }

        if pending_space {
            if !push_char(
                &mut compact,
                ' ',
                &mut compact_chars,
                &mut keep_bytes,
                keep_chars,
                max_len,
            ) {
                return (compact, true);
            }
            pending_space = false;
        }

        if !push_char(
            &mut compact,
            character,
            &mut compact_chars,
            &mut keep_bytes,
            keep_chars,
            max_len,
        ) {
            return (compact, true);
        }
        saw_non_whitespace = true;
    }

    (compact, false)
}

/// Cap one message's content to `lines_per_message` lines: positive=head, negative=tail,
/// 0=unchanged (unlimited).
///
/// This bounds each individual message independently, unlike [`select_transcript_lines`] which
/// windows one whole session transcript. Content that already fits is returned unchanged, so a
/// `0` default preserves exact current output everywhere.
pub fn select_message_lines(content: &str, lines_per_message: i64) -> String {
    if lines_per_message == 0 {
        return content.to_string();
    }
    select_transcript_lines(content, lines_per_message).0
}

/// Window one whole session transcript: positive=head, negative=tail, 0=entire transcript.
///
/// Returns the selected text plus a human-readable label describing how many lines were
/// returned. This applies to the complete session transcript, not to individual messages; use
/// [`select_message_lines`] to bound each message's content separately.
pub fn select_transcript_lines(transcript: &str, transcript_lines: i64) -> (String, String) {
    if transcript_lines == 0 {
        return (transcript.to_string(), "all".to_string());
    }
    if transcript_lines < 0 {
        let requested = transcript_lines.unsigned_abs() as usize;
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
                "last {} (truncated; 0 returns the entire transcript and may be very large)",
                selected.len()
            )
        } else {
            selected.len().to_string()
        };
        return (selected.into_iter().collect::<Vec<_>>().join("\n"), label);
    }

    let transcript_lines = transcript_lines as usize;
    let mut lines = transcript.lines();
    let selected: Vec<&str> = lines.by_ref().take(transcript_lines).collect();
    let truncated = lines.next().is_some();
    let label = if truncated {
        format!(
            "first {transcript_lines} (truncated; 0 returns the entire transcript and may be very large)"
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

/// Render an argument vector as one single-line POSIX-shell command without changing argument
/// boundaries.
///
/// The returned text is for presentation or explicit shell evaluation only. Process execution
/// should continue to pass the original argument vector to [`std::process::Command`]. This helper
/// does not produce PowerShell or `cmd.exe` syntax.
pub fn render_posix_shell_command(parts: &[String]) -> Result<String> {
    for (argument_index, part) in parts.iter().enumerate() {
        if let Some(control) = part
            .chars()
            .find(|character| matches!(*character, '\u{0000}'..='\u{001f}' | '\u{007f}'))
        {
            let detail = if control == '\0' {
                "NUL cannot be represented in a POSIX shell argument"
            } else {
                "control characters are rejected so rendered commands remain single-line and copy-pastable"
            };
            return Err(anyhow!(
                "cannot render POSIX shell command: argument {argument_index} contains unsupported control character U+{:04X}; {detail}",
                control as u32
            ));
        }
    }

    shlex::try_join(parts.iter().map(String::as_str))
        .map_err(|error| anyhow!("cannot render POSIX shell command: {error}"))
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
/// A user message that begins with a slash command token becomes
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
    let raw: Vec<LegacyRawMessage> = raw
        .into_iter()
        .map(|(role, content, ts)| (role, content, ts, None))
        .collect();
    to_messages_with_tools(raw)
}

/// Compact adapter input for provider formats that expose only role, text, timestamp, and an
/// optional tool name. [`to_messages_with_tools`] converts this shape immediately to
/// [`RawMessage`], so role/kind normalization remains provider-neutral and has one implementation.
pub type LegacyRawMessage = (String, String, Option<DateTime<Utc>>, Option<String>);

/// Provider evidence before role normalization and sequence assignment.
#[derive(Debug, Clone)]
pub struct RawMessage {
    role: String,
    content: String,
    ts: Option<DateTime<Utc>>,
    tool_name: Option<String>,
    kind: Option<MessageKind>,
    tool_call_id: Option<String>,
    provenance: MessageProvenance,
    native_event_identity: Option<(MessageCorrelationAuthority, String)>,
}

fn source_event_provenance() -> MessageProvenance {
    MessageProvenance {
        record_relation: MessageRecordRelation::Original,
        ..MessageProvenance::default()
    }
}

impl RawMessage {
    pub fn message(
        role: impl Into<String>,
        content: String,
        ts: Option<DateTime<Utc>>,
        tool_name: Option<String>,
    ) -> Self {
        Self {
            role: role.into(),
            content,
            ts,
            tool_name,
            kind: None,
            tool_call_id: None,
            provenance: source_event_provenance(),
            native_event_identity: None,
        }
    }

    /// A message the harness injected into the transcript rather than the user or model
    /// writing it: Stop-hook feedback, PreToolUse blocks, local-command caveats and stdout,
    /// task notifications. Stored with `role: "user"` because that is how the harness records
    /// it, and tagged `HarnessNotice` so every query and analytic can exclude it by default
    /// while it stays findable -- it is the only record of what a hook told an agent.
    pub fn harness_notice(content: String, ts: Option<DateTime<Utc>>) -> Self {
        Self {
            role: "user".to_string(),
            content,
            ts,
            tool_name: None,
            kind: Some(MessageKind::HarnessNotice),
            tool_call_id: None,
            provenance: source_event_provenance(),
            native_event_identity: None,
        }
    }

    pub fn tool_call(
        tool_name: &str,
        args: Value,
        tool_call_id: Option<&str>,
        ts: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            role: "tool".to_string(),
            content: tool_call_message_content(tool_name, args),
            ts,
            tool_name: Some(tool_name.to_string()),
            kind: Some(MessageKind::ToolCall),
            tool_call_id: tool_call_id.map(str::to_string),
            provenance: source_event_provenance(),
            native_event_identity: None,
        }
    }

    pub fn tool_result(
        tool_name: &str,
        content: String,
        tool_call_id: Option<&str>,
        ts: Option<DateTime<Utc>>,
    ) -> Self {
        Self::tool_result_with_name(Some(tool_name.to_string()), content, tool_call_id, ts)
    }

    pub fn tool_result_with_name(
        tool_name: Option<String>,
        content: String,
        tool_call_id: Option<&str>,
        ts: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            role: "tool".to_string(),
            content,
            ts,
            tool_name,
            kind: Some(MessageKind::ToolResult),
            tool_call_id: tool_call_id.map(str::to_string),
            provenance: source_event_provenance(),
            native_event_identity: None,
        }
    }

    /// Attach provider-verified semantic authorship before role normalization.
    pub fn with_authorship(mut self, authorship: MessageAuthorship) -> Self {
        self.provenance.authorship = authorship;
        self
    }

    /// Attach whether this record is the provider's original event or a known replay.
    pub fn with_record_relation(mut self, relation: MessageRecordRelation) -> Self {
        self.provenance.record_relation = relation;
        self
    }

    /// Attach a stable provider-native event identity. Callers must supply its native authority
    /// and collision scope; content and timestamps are never valid synthetic identities.
    pub fn with_correlation_identity(mut self, identity: MessageCorrelationIdentity) -> Self {
        self.provenance.correlation_identity = Some(identity);
        self
    }

    /// Attach a provider-native event ID before the parser knows the final canonical session ID.
    ///
    /// Normalize this message through [`to_messages_with_tools_in_scope`]. Tool-call IDs are not
    /// event identities: a call and its result are distinct messages that intentionally share the
    /// call ID and must never be collapsed as copies.
    pub fn with_native_event_identity(
        mut self,
        authority: MessageCorrelationAuthority,
        id: impl Into<String>,
    ) -> Self {
        self.native_event_identity = Some((authority, id.into()));
        self
    }

    /// Attach a complete structural partition for content with multiple semantic authors.
    ///
    /// [`MessageProvenance::validate`] enforces ordered, gap-free Unicode-scalar ranges before
    /// persistence. The builder sets the required aggregate `Mixed` authorship automatically.
    pub fn with_content_parts(mut self, parts: Vec<MessageContentPart>) -> Self {
        self.provenance.authorship = MessageAuthorship::Mixed;
        self.provenance.content_parts = parts;
        self
    }

    pub fn role(&self) -> &str {
        &self.role
    }

    pub fn content(&self) -> &str {
        &self.content
    }
}

impl From<LegacyRawMessage> for RawMessage {
    fn from((role, content, ts, tool_name): LegacyRawMessage) -> Self {
        Self {
            role,
            content,
            ts,
            tool_name,
            kind: None,
            tool_call_id: None,
            provenance: source_event_provenance(),
            native_event_identity: None,
        }
    }
}

fn infer_message_kind(role: Role, content: &str) -> MessageKind {
    match role {
        Role::Compaction => MessageKind::Compaction,
        Role::Tool => {
            let is_call = serde_json::from_str::<Value>(content)
                .ok()
                .is_some_and(|value| {
                    value.get("kind").and_then(Value::as_str) == Some("tool_call")
                });
            if is_call {
                MessageKind::ToolCall
            } else {
                MessageKind::ToolResult
            }
        }
        _ => MessageKind::Conversation,
    }
}

fn infer_message_authorship(role: Role, kind: MessageKind) -> MessageAuthorship {
    match kind {
        MessageKind::HarnessNotice => MessageAuthorship::Harness,
        MessageKind::ToolCall => MessageAuthorship::Agent,
        MessageKind::Compaction | MessageKind::ToolResult => MessageAuthorship::Generated,
        MessageKind::Conversation | MessageKind::Unknown => match role {
            Role::Assistant => MessageAuthorship::Agent,
            // User/slash authorship depends on whether the session is person-started or spawned.
            // The provider finalizer supplies that structured session evidence.
            Role::User | Role::Slash => MessageAuthorship::Unknown,
            Role::Tool | Role::Compaction => MessageAuthorship::Generated,
        },
    }
}

/// Provider evidence that can resolve an otherwise-unknown user-role conversation row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserRoleAuthorshipEvidence {
    HumanInputEvent,
    AgentDelegationEvent,
    Unverified,
}

/// Resolve user-role authorship from provider-supplied structured event evidence.
///
/// This is linear in message count and changes only still-unknown conversation/slash rows. It
/// therefore preserves explicit provider evidence while preventing a spawned agent's delegation
/// prompt (stored by harnesses as `role = user`) from becoming human-authored evidence.
pub fn apply_user_role_authorship(messages: &mut [Message], evidence: UserRoleAuthorshipEvidence) {
    let user_authorship = match evidence {
        UserRoleAuthorshipEvidence::HumanInputEvent => MessageAuthorship::Human,
        UserRoleAuthorshipEvidence::AgentDelegationEvent => MessageAuthorship::Agent,
        UserRoleAuthorshipEvidence::Unverified => return,
    };
    for message in messages {
        if message.provenance.authorship == MessageAuthorship::Unknown
            && matches!(message.role, Role::User | Role::Slash)
            && matches!(
                message.kind,
                MessageKind::Conversation | MessageKind::Unknown
            )
        {
            message.provenance.authorship = user_authorship;
        }
    }
}

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
pub fn to_messages_with_tools<T>(raw: Vec<T>) -> Vec<Message>
where
    T: Into<RawMessage>,
{
    normalize_raw_messages(raw, None)
}

/// Normalize provider records while binding deferred native event IDs to one canonical session.
///
/// This stays `O(M)` time and `O(M)` output for `M` messages, with `O(1)` extra state beyond the
/// returned rows. Binding at finalization handles providers whose true session ID is discovered
/// after earlier records without synthesizing message identity from sequence, timestamps, or text.
pub fn to_messages_with_tools_in_scope<T>(raw: Vec<T>, scope: &str) -> Vec<Message>
where
    T: Into<RawMessage>,
{
    normalize_raw_messages(raw, Some(scope))
}

fn normalize_raw_messages<T>(raw: Vec<T>, correlation_scope: Option<&str>) -> Vec<Message>
where
    T: Into<RawMessage>,
{
    raw.into_iter()
        .enumerate()
        .map(|(i, item)| {
            let raw = item.into();
            let normalized = classify_role(&raw.role, &raw.content);
            let kind = raw
                .kind
                .unwrap_or_else(|| infer_message_kind(normalized, &raw.content));
            let mut provenance = raw.provenance;
            if provenance.authorship == MessageAuthorship::Unknown {
                provenance.authorship = infer_message_authorship(normalized, kind);
            }
            if let Some((authority, id)) = raw.native_event_identity {
                let scope = correlation_scope.expect(
                    "provider-native event identity requires to_messages_with_tools_in_scope",
                );
                provenance.correlation_identity = Some(MessageCorrelationIdentity {
                    authority,
                    scope: scope.to_string(),
                    id,
                });
            }
            Message {
                seq: i as i64,
                role: normalized,
                ts: raw.ts,
                tool_name: raw.tool_name,
                kind,
                tool_call_id: raw.tool_call_id,
                is_compaction: normalized == Role::Compaction,
                content: raw.content,
                provenance,
            }
        })
        .collect()
}

#[cfg(test)]
mod role_classification_tests {
    use super::*;
    use crate::models::{
        MessageCorrelationAuthority, MessageCorrelationIdentity, MessageKind, MessageRecordRelation,
    };

    #[test]
    fn typed_tool_events_preserve_kind_and_native_call_id() {
        let messages = to_messages_with_tools(vec![
            RawMessage::tool_call(
                "exec_command",
                json!({"cmd": "cargo test"}),
                Some("call-1"),
                None,
            ),
            RawMessage::tool_result("exec_command", "1 passed".to_string(), Some("call-1"), None),
        ]);

        assert_eq!(messages[0].kind, MessageKind::ToolCall);
        assert_eq!(messages[1].kind, MessageKind::ToolResult);
        assert_eq!(messages[0].tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(messages[1].tool_call_id.as_deref(), Some("call-1"));
    }

    #[test]
    fn legacy_raw_tuples_classify_call_and_result_without_provider_branches() {
        let messages = to_messages_with_tools(vec![
            (
                "tool".to_string(),
                tool_call_message_content("Bash", json!({"command": "pwd"})),
                None,
                Some("Bash".to_string()),
            ),
            (
                "tool".to_string(),
                "/repo".to_string(),
                None,
                Some("Bash".to_string()),
            ),
        ]);

        assert_eq!(messages[0].kind, MessageKind::ToolCall);
        assert_eq!(messages[1].kind, MessageKind::ToolResult);
        assert!(messages
            .iter()
            .all(|message| message.tool_call_id.is_none()));
    }

    #[test]
    fn raw_message_marks_source_events_original_and_preserves_provider_overrides() {
        let correlation_identity = MessageCorrelationIdentity {
            authority: MessageCorrelationAuthority::Anthropic,
            scope: "claude:session-1".to_string(),
            id: "event-1".to_string(),
        };
        let messages = to_messages_with_tools(vec![
            RawMessage::message("user", "direct request".to_string(), None, None)
                .with_authorship(MessageAuthorship::Human)
                .with_correlation_identity(correlation_identity.clone()),
            RawMessage::message("user", "known replay".to_string(), None, None)
                .with_record_relation(MessageRecordRelation::Mirror),
        ]);

        assert_eq!(messages[0].provenance.authorship, MessageAuthorship::Human);
        assert_eq!(
            messages[0].provenance.record_relation,
            MessageRecordRelation::Original
        );
        assert_eq!(
            messages[0].provenance.correlation_identity.as_ref(),
            Some(&correlation_identity)
        );
        assert_eq!(
            messages[1].provenance.record_relation,
            MessageRecordRelation::Mirror
        );
    }

    #[test]
    fn provider_native_event_identity_is_bound_to_the_final_session_scope() {
        let messages = to_messages_with_tools_in_scope(
            vec![
                RawMessage::message("user", "direct request".to_string(), None, None)
                    .with_native_event_identity(MessageCorrelationAuthority::Anthropic, "event-1"),
            ],
            "claude:session-1",
        );

        assert_eq!(
            messages[0].provenance.correlation_identity,
            Some(MessageCorrelationIdentity {
                authority: MessageCorrelationAuthority::Anthropic,
                scope: "claude:session-1".to_string(),
                id: "event-1".to_string(),
            })
        );
    }

    #[test]
    fn slash_commands_classified_but_paths_excluded() {
        assert_eq!(classify_role("user", "/cmd-a make a plan"), Role::Slash);
        assert_eq!(classify_role("user", "/cmd-b"), Role::Slash);
        assert_eq!(
            classify_role("user", "/review-url https://example.com"),
            Role::Slash
        );
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
            // No spawn concept on this path: subagent runs are either excluded from
            // discovery or unmarked by this provider. See models.rs SessionRecord.
            parent_session_id: None,
            agent_label: None,
        },
        transcript_text: String::new(),
        messages: Vec::new(),
        file_edits: Vec::new(),
    }
}

pub fn provider_parse_version(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "claude-v4",
        Provider::ClaudeDesktop => "claude-desktop-local-agent-v4",
        Provider::Codex => "codex-v5",
        Provider::Cursor => "cursor-v4",
        Provider::Antigravity => "antigravity-v3",
        Provider::Pi => "pi-v3",
        Provider::PrimeAgent => "prime-agent-v1",
        Provider::AiStudio => "aistudio-v2",
        Provider::GeminiCli => "gemini-cli-v3",
    }
}

/// Convert a file mtime in nanoseconds-since-epoch to a UTC datetime.
pub fn datetime_from_mtime_ns(mtime_ns: i64) -> Option<DateTime<Utc>> {
    let secs = mtime_ns.div_euclid(1_000_000_000);
    let nanos = mtime_ns.rem_euclid(1_000_000_000) as u32;
    DateTime::from_timestamp(secs, nanos)
}

/// Guarantee a session has dates: fall back to the observed file mtime for any of
/// `created_at`/`updated_at`/`last_message_at` the parser left unset. This keeps session-span
/// filters and recency sorting defined for providers without native timestamps; it does not prove
/// continuous activity or reorder two parser-provided endpoints. Parser-provided dates win.
/// Runs in `O(1)` time and memory and performs no I/O because the caller supplies `mtime_ns`.
/// Fill the session's known span from what the parser found, then the file's mtime, and leave it
/// ordered. `O(1)`, no I/O.
///
/// The span is `[created_at, updated_at]`. A missing end takes the file mtime; a missing start
/// takes the end, a point span, because the only fact known is that the session was active then.
/// It must never take the mtime: a Codex tail slice past the 1 MiB overlap carries no
/// `session_meta`, a Pi or Prime tail of tool results advances only `updated_at`, and the mtime
/// follows the last write, so filling the start from it produced `created_at > updated_at` and
/// the reversed span aborted every refresh (and every read command that refreshes first) for as
/// long as the file kept growing. Two contradictory native endpoints are stored ordered with the
/// reversal named in `parse_warning`, where `aise doctor` and `--warnings-only` surface it, rather
/// than stopping the reindex for every later source.
pub fn backfill_session_dates(session: &mut SessionRecord, mtime_ns: i64) {
    let mtime = datetime_from_mtime_ns(mtime_ns);
    if session.updated_at.is_none() {
        session.updated_at = mtime;
    }
    if session.created_at.is_none() {
        session.created_at = session.updated_at;
    }
    if session.last_message_at.is_none() {
        session.last_message_at = session.updated_at;
    }
    if let (Some(created_at), Some(updated_at)) = (session.created_at, session.updated_at) {
        if created_at > updated_at {
            let reversal = format!(
                "session '{}' has created_at {} after updated_at {}; the span was stored in order",
                session.id,
                created_at.to_rfc3339(),
                updated_at.to_rfc3339()
            );
            session.created_at = Some(updated_at);
            session.updated_at = Some(created_at);
            session.parse_warning = Some(match session.parse_warning.take() {
                Some(existing) => format!("{existing}; {reversal}"),
                None => reversal,
            });
        }
    }
}

/// Reject a malformed known session span after provider parsing and fallback.
///
/// This is `O(1)` time/memory with no I/O. Keeping validation after fallback lets providers with
/// one or no native endpoint use the canonical mtime point span, while two contradictory native
/// endpoints fail before persistence instead of being silently reordered at query time.
pub fn validate_session_date_order(session: &SessionRecord) -> anyhow::Result<()> {
    match (session.created_at, session.updated_at) {
        (Some(created_at), Some(updated_at)) if created_at <= updated_at => Ok(()),
        (Some(created_at), Some(updated_at)) => anyhow::bail!(
            "session '{}' has created_at {} after updated_at {}; provider timestamp normalization must produce an ordered known span",
            session.id,
            created_at.to_rfc3339(),
            updated_at.to_rfc3339()
        ),
        _ => anyhow::bail!(
            "session '{}' has no complete date span after mtime fallback; verify the source file timestamp and provider parser",
            session.id
        ),
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

/// Printed beside a resume command wherever one is shown for a person to run, so the boundary is
/// visible before the confirmation prompt rather than after a session reopens under unexpected
/// behavior. One authority for the sentence; see [`resume_plan`] for why the command omits flags.
pub const RESUME_COMMAND_POLICY_NOTE: &str =
    "Provider defaults only: add your own permission or approval flags if your workflow uses them.";

/// Build the provider CLI invocation that reopens `session`, plus the directory to run it from.
///
/// The command carries the provider's own resume verb and this session's id and nothing else. It
/// deliberately does NOT reproduce the flags a particular user habitually passes — a cold-recovery
/// audit found Codex invocations that always carry `-c approval_policy=on-request` and Claude
/// invocations that always carry `--dangerously-skip-permissions`, so running this command
/// verbatim resumes the right conversation under different permission behavior than that user
/// expects. Resuming is the provider's contract; a local permission policy is not something this
/// index observes, and guessing one from transcript history would silently widen what an agent may
/// do. Surfaces that print this command say so, so a caller adds its own flags deliberately.
pub fn resume_plan(session: &SessionRecord) -> Result<(Vec<String>, Option<String>)> {
    let binary = match session.provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
        Provider::Pi => "pi",
        Provider::PrimeAgent => "prime-agent",
        Provider::ClaudeDesktop
        | Provider::Cursor
        | Provider::Antigravity
        | Provider::AiStudio
        | Provider::GeminiCli => {
            let id = &session.id;
            let provider = session.provider.as_str();
            return Err(anyhow!(
                "resuming is not supported for {provider} sessions — read this one here with \
                 `aise show {id}` or `aise export {id}`"
            ));
        }
    };
    if which(binary).is_none() {
        let id = &session.id;
        return Err(anyhow!(
            "`{binary}` is not on your PATH — resume launches the native CLI, so install `{binary}` \
             or add it to PATH. You can still read this session with `aise show {id}` or \
             `aise export {id}`"
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
        Provider::PrimeAgent => vec![
            "prime-agent".to_string(),
            "--resume".to_string(),
            session.provider_session_id.clone(),
        ],
        Provider::ClaudeDesktop
        | Provider::Cursor
        | Provider::Antigravity
        | Provider::AiStudio
        | Provider::GeminiCli => {
            unreachable!("resume is handled before command construction")
        }
    };
    Ok((command, cwd))
}

pub fn which(binary: &str) -> Option<PathBuf> {
    executable_candidates(binary).into_iter().next()
}

pub(crate) fn executable_candidates(binary: &str) -> Vec<PathBuf> {
    let Some(paths) = env::var_os("PATH") else {
        return Vec::new();
    };
    executable_candidates_from(
        binary,
        &paths,
        env::var_os("PATHEXT").as_deref(),
        cfg!(windows),
    )
}

fn executable_candidates_from(
    binary: &str,
    paths: &OsStr,
    path_ext: Option<&OsStr>,
    windows: bool,
) -> Vec<PathBuf> {
    let names = executable_names_for(binary, windows, path_ext);
    let mut candidates = Vec::new();
    for candidate in env::split_paths(paths)
        .flat_map(|directory| names.iter().map(move |name| directory.join(name)))
        .filter(|candidate| is_executable_file(candidate))
    {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    candidates
}

pub(crate) fn executable_names_for(
    binary: &str,
    windows: bool,
    path_ext: Option<&OsStr>,
) -> Vec<OsString> {
    let mut names = vec![OsString::from(binary)];
    if windows && Path::new(binary).extension().is_none() {
        let extensions = path_ext
            .and_then(OsStr::to_str)
            .filter(|value| !value.is_empty())
            .unwrap_or(".COM;.EXE;.BAT;.CMD");
        names.extend(
            extensions
                .split(';')
                .filter(|extension| !extension.is_empty())
                .map(|extension| {
                    let extension = if extension.starts_with('.') {
                        extension.to_string()
                    } else {
                        format!(".{extension}")
                    };
                    OsString::from(format!("{binary}{extension}"))
                }),
        );
    }
    names
}

pub(crate) fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// Serializes tests that mutate the process `PATH` so they never race the same env var
    /// across `cargo test`'s parallel test threads. Every test in this crate that touches the
    /// real `PATH` goes through [`with_stub_binary_on_path`], so this mutex is the only
    /// coordination required.
    static PATH_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Prepends a directory containing one fake, always-findable `name` executable to `PATH`,
    /// runs `f`, then restores the original `PATH` even if `f` panics. Lets a test exercise the
    /// real [`resume_plan`]/[`which`] resolution without depending on `claude`, `codex`, `pi`,
    /// or `prime-agent` actually being installed on the host or CI runner.
    pub(crate) fn with_stub_binary_on_path<T>(name: &str, f: impl FnOnce() -> T) -> T {
        let _guard = PATH_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stub_dir = tempfile::tempdir().unwrap();
        let stub = stub_dir.path().join(name);
        std::fs::write(&stub, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let original_path = std::env::var_os("PATH");
        let mut search_dirs = vec![stub_dir.path().to_path_buf()];
        if let Some(existing) = &original_path {
            search_dirs.extend(std::env::split_paths(existing));
        }
        let new_path = std::env::join_paths(search_dirs).unwrap();
        // SAFETY: serialized by PATH_MUTEX above, and no other test in this crate reads or
        // writes the real PATH env var, so no concurrent access races this mutation.
        unsafe {
            std::env::set_var("PATH", &new_path);
        }

        struct RestorePath(Option<std::ffi::OsString>);
        impl Drop for RestorePath {
            fn drop(&mut self) {
                // SAFETY: see above; runs on unwind too, so a panic in `f` never leaves PATH
                // mutated for later tests.
                unsafe {
                    match self.0.take() {
                        Some(value) => std::env::set_var("PATH", value),
                        None => std::env::remove_var("PATH"),
                    }
                }
            }
        }
        let _restore = RestorePath(original_path);

        f()
    }

    #[test]
    fn unicode_lower_contains_matches_eager_lowercase_for_unicode_cases() {
        for (haystack, needle) in [
            ("The CAFÉ is open", "café"),
            ("Straße and STRASSE", "strasse"),
            ("İstanbul", "i\u{307}"),
            ("prefix", ""),
            ("emoji 😀 suffix", "😀"),
            ("short", "longer"),
        ] {
            assert_eq!(
                UnicodeLowerNeedle::from_lowered(&needle.to_lowercase()).contains(haystack),
                haystack.to_lowercase().contains(&needle.to_lowercase()),
                "haystack={haystack:?}, needle={needle:?}"
            );
        }
    }

    #[test]
    fn windows_executable_names_follow_pathext_without_magic_extensions() {
        let names = executable_names_for("aise", true, Some(OsStr::new(".EXE;.CMD;.CUSTOM")));
        assert_eq!(
            names,
            ["aise", "aise.EXE", "aise.CMD", "aise.CUSTOM"].map(OsString::from)
        );
        assert_eq!(
            executable_names_for("aise.exe", true, None),
            vec![OsString::from("aise.exe")]
        );
        assert_eq!(
            executable_names_for("aise", false, None),
            vec![OsString::from("aise")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn executable_candidates_preserve_path_order_deduplicate_and_require_execute_bits() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let first_binary = first.join("aise");
        let second_binary = second.join("aise");
        fs::write(&first_binary, "not executable").unwrap();
        fs::write(&second_binary, "executable").unwrap();
        fs::set_permissions(&first_binary, fs::Permissions::from_mode(0o644)).unwrap();
        fs::set_permissions(&second_binary, fs::Permissions::from_mode(0o755)).unwrap();
        let paths = env::join_paths([&first, &second, &second]).unwrap();

        assert_eq!(
            executable_candidates_from("aise", &paths, None, false),
            vec![second_binary.clone()]
        );

        fs::set_permissions(&first_binary, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            executable_candidates_from("aise", &paths, None, false),
            vec![first_binary, second_binary]
        );
    }

    #[test]
    fn posix_shell_command_preserves_safe_adversarial_arguments() {
        let arguments = vec![
            "aise".to_string(),
            String::new(),
            "space value".to_string(),
            "single'and\"double-quotes".to_string(),
            "$HOME".to_string(),
            "; rm -rf never".to_string(),
            "*.rs?[x]".to_string(),
            "雪-🚀".to_string(),
        ];

        let rendered = render_posix_shell_command(&arguments).unwrap();

        assert_eq!(shlex::split(&rendered), Some(arguments));
    }

    #[test]
    fn posix_shell_command_rejects_controls_with_argument_context() {
        for (control, code_point) in [
            ('\t', "U+0009"),
            ('\n', "U+000A"),
            ('\r', "U+000D"),
            ('\u{007f}', "U+007F"),
        ] {
            let error = render_posix_shell_command(&[
                "aise".to_string(),
                format!("unsafe{control}argument"),
            ])
            .unwrap_err()
            .to_string();
            assert!(error.contains("argument 1"), "{error}");
            assert!(error.contains(code_point), "{error}");
            assert!(error.contains("single-line"), "{error}");
        }
    }

    #[test]
    fn posix_shell_command_rejects_nul_with_actionable_error() {
        let error = render_posix_shell_command(&["aise".to_string(), "bad\0arg".to_string()])
            .unwrap_err()
            .to_string();

        assert!(error.contains("argument 1"), "{error}");
        assert!(error.contains("U+0000"), "{error}");
        assert!(error.contains("NUL cannot be represented"), "{error}");
    }

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
    fn prime_agent_resume_plan_uses_native_session_id() {
        let parsed = minimal_record(
            Provider::PrimeAgent,
            std::path::Path::new("/tmp/019fea39-38c2-710e-8100-3624dfc0ac07.jsonl"),
            String::new(),
        );
        let session_id = parsed.session.provider_session_id.clone();
        // Stubbed rather than assumed: `resume_plan` refuses when the native CLI is absent, so
        // asserting the success path against the live PATH only passes on a machine that happens
        // to have `prime-agent` installed.
        let plan = with_stub_binary_on_path("prime-agent", || resume_plan(&parsed.session));
        assert_eq!(
            plan.unwrap(),
            (
                vec![
                    "prime-agent".to_string(),
                    "--resume".to_string(),
                    session_id,
                ],
                None,
            )
        );
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
        assert!(err.contains("aise show"), "offers an alternative: {err}");
        assert!(
            err.contains(&parsed.session.id),
            "references the session ID: {err}"
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
        assert_eq!(expand_tilde("~/src/aise"), home.join("src/aise"));
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

        let td = tempfile::tempdir().expect("tempdir");
        let missing_absolute = normalize_path(&td.path().join("missing"));
        assert_eq!(normalize_path_prefix(&missing_absolute), missing_absolute);

        #[cfg(windows)]
        assert_eq!(normalize_path_prefix("/Users/x/proj"), "/Users/x/proj");
        #[cfg(not(windows))]
        assert_eq!(
            normalize_path_prefix(r"C:\Users\x\proj"),
            r"C:\Users\x\proj"
        );

        // An EXISTING absolute path round-trips to its canonical form, and a trailing slash or
        // `/.` component is normalized away so the prefix matches the stored dir exactly.
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
        let parsed = datetime_from_mtime_ns(mtime_ns + 1_000_000_000).unwrap();
        rec.updated_at = Some(parsed);
        backfill_session_dates(&mut rec, mtime_ns);
        assert_eq!(rec.updated_at, Some(parsed));
        assert_eq!(rec.created_at, mtime, "the earlier start stays");
    }

    /// A parser that reports the last activity but not the start (a Codex tail slice past the
    /// 1 MiB overlap has no `session_meta`; a Pi/Prime tail of tool results advances only
    /// `updated_at`) must get a point span at that end. Filling the start from the file's mtime
    /// put it after the native end, and the reversed span aborted the whole reindex.
    #[test]
    fn backfill_session_dates_uses_the_known_end_when_only_the_end_is_native() {
        let mtime_ns = 1_700_000_000_000_000_000;
        let native_end = datetime_from_mtime_ns(mtime_ns - 3_600_000_000_000).unwrap();
        let mut record =
            minimal_record(Provider::Codex, Path::new("/tmp/tail.jsonl"), String::new());
        record.session.updated_at = Some(native_end);
        backfill_session_dates(&mut record.session, mtime_ns);
        assert_eq!(record.session.created_at, Some(native_end));
        assert_eq!(record.session.updated_at, Some(native_end));
        assert_eq!(record.session.last_message_at, Some(native_end));
        assert!(
            record
                .session
                .parse_warning
                .as_deref()
                .unwrap_or("")
                .is_empty(),
            "a point span is not a reorder"
        );
        validate_session_date_order(&record.session).unwrap();
    }

    /// Two contradictory native endpoints are a provider bug, not a reason to stop indexing every
    /// later source: the span is stored ordered and the row says so.
    #[test]
    fn backfill_session_dates_orders_a_reversed_native_span_and_records_a_warning() {
        let mtime_ns = 1_700_000_000_000_000_000;
        let earlier = datetime_from_mtime_ns(mtime_ns - 2_000_000_000).unwrap();
        let later = datetime_from_mtime_ns(mtime_ns - 1_000_000_000).unwrap();
        let mut record = minimal_record(
            Provider::Claude,
            Path::new("/tmp/reversed.jsonl"),
            String::new(),
        );
        record.session.created_at = Some(later);
        record.session.updated_at = Some(earlier);
        backfill_session_dates(&mut record.session, mtime_ns);
        assert_eq!(record.session.created_at, Some(earlier));
        assert_eq!(record.session.updated_at, Some(later));
        let warning = record
            .session
            .parse_warning
            .clone()
            .expect("the reorder is recorded");
        assert!(warning.contains("created_at"), "{warning}");
        assert!(warning.contains("after updated_at"), "{warning}");
        validate_session_date_order(&record.session).unwrap();

        // An existing warning is kept, not replaced.
        let mut twice = minimal_record(
            Provider::Claude,
            Path::new("/tmp/reversed.jsonl"),
            "earlier".into(),
        );
        twice.session.created_at = Some(later);
        twice.session.updated_at = Some(earlier);
        backfill_session_dates(&mut twice.session, mtime_ns);
        let warning = twice.session.parse_warning.clone().unwrap();
        assert!(
            warning.contains("earlier") && warning.contains("created_at"),
            "{warning}"
        );
    }

    #[test]
    fn session_date_order_validation_accepts_fallback_and_rejects_reversal() {
        let mtime_ns = 1_700_000_000_000_000_000;
        let mut fallback = minimal_record(
            Provider::Claude,
            Path::new("/tmp/fallback.jsonl"),
            String::new(),
        );
        backfill_session_dates(&mut fallback.session, mtime_ns);
        validate_session_date_order(&fallback.session).unwrap();

        fallback.session.created_at = datetime_from_mtime_ns(mtime_ns + 2_000_000_000);
        let error = validate_session_date_order(&fallback.session)
            .expect_err("reversed native endpoints must fail before persistence")
            .to_string();
        assert!(error.contains("created_at"), "{error}");
        assert!(error.contains("after updated_at"), "{error}");
        assert!(
            error.contains("provider timestamp normalization"),
            "{error}"
        );
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
                kind: MessageKind::Conversation,
                tool_call_id: None,
                is_compaction: false,
                content: "undated".into(),
                provenance: MessageProvenance::default(),
            },
            Message {
                seq: 1,
                role: Role::Assistant,
                ts: Some(explicit),
                tool_name: None,
                kind: MessageKind::Conversation,
                tool_call_id: None,
                is_compaction: false,
                content: "dated".into(),
                provenance: MessageProvenance::default(),
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
    fn truncate_for_display_stops_reading_a_long_line_once_truncation_is_certain() {
        struct PanicAfterPrefix {
            prefix: std::str::Chars<'static>,
        }

        impl Iterator for PanicAfterPrefix {
            type Item = char;

            fn next(&mut self) -> Option<Self::Item> {
                if let Some(character) = self.prefix.next() {
                    return Some(character);
                }
                panic!("requested long-line input after the character cap was exceeded");
            }
        }

        assert_eq!(
            truncate_compacted_chars(
                PanicAfterPrefix {
                    // Six compacted characters are enough to prove a five-character cap is
                    // exceeded. Asking for a seventh character would panic.
                    prefix: "abcdef".chars(),
                },
                5,
            ),
            ("ab...".to_string(), true)
        );
    }

    #[test]
    fn truncate_for_display_preserves_compact_whitespace_and_small_cap_semantics() {
        assert_eq!(
            truncate_for_display("\n alpha\t beta \r\n gamma ", 100),
            "alpha beta gamma"
        );
        assert_eq!(
            truncate_for_display("\n alpha\t beta \r\n gamma ", 10),
            "alpha b..."
        );
        assert_eq!(truncate_for_display("abcdef", 2), "...");
        assert_eq!(truncate_for_display("   \n\t", 2), "");
    }

    #[test]
    fn truncate_for_display_matches_the_eager_reference_semantics() {
        fn eager_reference(value: &str, max_len: usize) -> String {
            let compact = compact_whitespace(value);
            if compact.chars().count() <= max_len {
                compact
            } else {
                let keep = max_len.saturating_sub(3);
                format!("{}...", compact.chars().take(keep).collect::<String>())
            }
        }

        let cases = [
            "",
            "   \n\t",
            "alpha",
            " alpha\t beta \r\n gamma ",
            "é 😀\u{2003}雪",
            "a\u{00a0}\u{2009}b",
            "word-without-whitespace",
        ];
        for value in cases {
            for max_len in 0..=20 {
                assert_eq!(
                    truncate_for_display(value, max_len),
                    eager_reference(value, max_len),
                    "value={value:?}, max_len={max_len}"
                );
            }
        }
    }

    #[test]
    fn select_transcript_lines_supports_head_tail_and_all() {
        let transcript = "one\ntwo\nthree\nfour";
        let (head, head_label) = select_transcript_lines(transcript, 2);
        assert_eq!(head, "one\ntwo");
        assert_eq!(
            head_label,
            "first 2 (truncated; 0 returns the entire transcript and may be very large)"
        );

        let (tail, tail_label) = select_transcript_lines(transcript, -2);
        assert_eq!(tail, "three\nfour");
        assert_eq!(
            tail_label,
            "last 2 (truncated; 0 returns the entire transcript and may be very large)"
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
    fn select_message_lines_zero_returns_content_unchanged() {
        let content = "first\nsecond\nthird";
        assert_eq!(select_message_lines(content, 0), content);
        assert_eq!(select_message_lines("", 0), "");
    }

    #[test]
    fn select_message_lines_caps_head_and_tail_per_message() {
        let content = "first\nsecond\nthird\nfourth";
        assert_eq!(select_message_lines(content, 2), "first\nsecond");
        assert_eq!(select_message_lines(content, -2), "third\nfourth");
        assert_eq!(select_message_lines(content, 10), content);
        assert_eq!(select_message_lines(content, -10), content);
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
            "last 3 (truncated; 0 returns the entire transcript and may be very large)"
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
