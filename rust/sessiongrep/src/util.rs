use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use chrono::{DateTime, Duration, Utc};
use regex::RegexBuilder;
use serde_json::Value;

use crate::config::Config;
use crate::models::{ParsedSession, Provider, SessionRecord};

pub fn expand_tilde(input: &str) -> PathBuf {
    if let Some(rest) = input.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(input)
}

pub fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

pub fn find_repo_root(cwd: &str) -> Option<String> {
    let mut current = PathBuf::from(cwd);
    loop {
        if current.join(".git").exists() {
            return Some(current.to_string_lossy().to_string());
        }
        if !current.pop() {
            return None;
        }
    }
}

pub fn truncate_for_display(value: &str, max_len: usize) -> String {
    let compact = compact_whitespace(value);
    if compact.len() <= max_len {
        compact
    } else {
        let truncate_at = max_len.saturating_sub(3);
        let end = compact
            .char_indices()
            .map(|(i, _)| i)
            .take_while(|&i| i <= truncate_at)
            .last()
            .unwrap_or(0);
        format!("{}...", &compact[..end])
    }
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
        "not", "of", "on", "or", "please", "some", "that", "the", "this", "to", "update",
        "what", "with", "you", "your",
    ];
    for token in trimmed.split_whitespace() {
        if token.len() >= 3
            && !stopwords.contains(&token.to_ascii_lowercase().as_str())
            && !terms.iter().any(|existing| existing.eq_ignore_ascii_case(token))
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

pub fn minimal_record(
    provider: Provider,
    path: &Path,
    warning: String,
) -> ParsedSession {
    let provider_session_id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("unknown")
        .to_string();
    let parse_version = match provider {
        Provider::Claude => "claude-v1",
        Provider::Codex => "codex-v1",
        Provider::Cursor => "cursor-v1",
        Provider::Antigravity => "antigravity-v1",
    };
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
    }
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
        Provider::Cursor | Provider::Antigravity => {
            return Err(anyhow!(
                "resuming sessions is not supported for provider '{}'",
                session.provider
            ));
        }
    };
    if which(binary).is_none() {
        return Err(anyhow!("required binary '{binary}' is not on PATH"));
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
        Provider::Cursor | Provider::Antigravity => unreachable!("resume is handled before command construction"),
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
}
