//! Typed, destination-independent session export.

use std::fmt;
use std::str::FromStr;

use anyhow::{anyhow, Result};

use crate::models::SessionWithTranscript;

/// Supported session export representations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// Human-readable title followed by the transcript.
    Text,
    /// Markdown metadata, preview, and fenced transcript.
    Markdown,
    /// Pretty-printed structured session JSON.
    Json,
}

impl ExportFormat {
    /// Stable lowercase name used by command and language adapters.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Markdown => "markdown",
            Self::Json => "json",
        }
    }
}

impl fmt::Display for ExportFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ExportFormat {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "text" => Ok(Self::Text),
            "markdown" | "md" => Ok(Self::Markdown),
            "json" => Ok(Self::Json),
            other => Err(anyhow!("unsupported export format: {other}")),
        }
    }
}

/// Fully rendered export whose destination remains under caller control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportDocument {
    format: ExportFormat,
    content: String,
}

impl ExportDocument {
    /// Representation used to produce this document.
    pub const fn format(&self) -> ExportFormat {
        self.format
    }

    /// Borrow the complete rendered document.
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Consume the document and return its rendered bytes as UTF-8 text.
    pub fn into_content(self) -> String {
        self.content
    }
}

/// Render one resolved session, including its entire transcript, without performing I/O.
///
/// The complete document is allocated in memory. Callers should expose this operation as an
/// explicit full export rather than using it for bounded previews.
///
/// # Errors
///
/// Returns an error if structured JSON serialization fails.
pub fn render_full(
    session: &SessionWithTranscript,
    format: ExportFormat,
) -> Result<ExportDocument> {
    let title = session
        .session
        .title
        .as_deref()
        .unwrap_or(&session.session.id);
    let content = match format {
        ExportFormat::Text => format!("{title}\n\n{}\n", session.transcript_text),
        ExportFormat::Markdown => {
            let fence = markdown_fence(&session.transcript_text);
            format!(
                "# {title}\n\n- Provider: {}\n- Session ID: {}\n- CWD: {}\n- Updated At: {}\n\n## Preview\n\n{}\n\n## Transcript\n\n{fence}\n{}\n{fence}\n",
                session.session.provider,
                session.session.provider_session_id,
                session.session.cwd.as_deref().unwrap_or("-"),
                session
                    .session
                    .updated_at
                    .map(|value| value.to_rfc3339())
                    .unwrap_or_else(|| "-".to_string()),
                session.session.preview_text,
                session.transcript_text
            )
        }
        // Preserve the legacy CLI's Value intermediate so pretty-JSON key ordering stays stable.
        ExportFormat::Json => serde_json::to_string_pretty(&serde_json::to_value(session)?)?,
    };
    Ok(ExportDocument { format, content })
}

fn markdown_fence(transcript: &str) -> String {
    let mut longest = 0usize;
    let mut current = 0usize;
    for character in transcript.chars() {
        if character == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    "`".repeat(longest.saturating_add(1).max(3))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Provider, SessionRecord};

    fn fixture() -> SessionWithTranscript {
        SessionWithTranscript {
            session: SessionRecord {
                id: "claude:abc".into(),
                provider: Provider::Claude,
                provider_session_id: "abc".into(),
                source_path: "/sessions/abc.jsonl".into(),
                discovery_source: "configured".into(),
                cwd: Some("/repo".into()),
                repo_root: Some("/repo".into()),
                title: Some("Example".into()),
                summary: None,
                preview_text: "preview".into(),
                created_at: None,
                updated_at: None,
                last_message_at: None,
                message_count: Some(2),
                parse_version: "test".into(),
                raw_metadata_json: None,
                parse_warning: None,
            },
            transcript_text: "[user] hello\n\n[assistant] hi".into(),
        }
    }

    #[test]
    fn formats_parse_without_adapter_specific_alias_logic() {
        assert_eq!("text".parse::<ExportFormat>().unwrap(), ExportFormat::Text);
        assert_eq!(
            "markdown".parse::<ExportFormat>().unwrap(),
            ExportFormat::Markdown
        );
        assert_eq!(
            "md".parse::<ExportFormat>().unwrap(),
            ExportFormat::Markdown
        );
        assert_eq!("json".parse::<ExportFormat>().unwrap(), ExportFormat::Json);
        assert_eq!(
            "html".parse::<ExportFormat>().unwrap_err().to_string(),
            "unsupported export format: html"
        );
    }

    #[test]
    fn text_and_markdown_preserve_cli_export_bytes() {
        let session = fixture();
        assert_eq!(
            render_full(&session, ExportFormat::Text).unwrap().content(),
            "Example\n\n[user] hello\n\n[assistant] hi\n"
        );
        assert_eq!(
            render_full(&session, ExportFormat::Markdown)
                .unwrap()
                .content(),
            "# Example\n\n- Provider: claude\n- Session ID: abc\n- CWD: /repo\n- Updated At: -\n\n## Preview\n\npreview\n\n## Transcript\n\n```\n[user] hello\n\n[assistant] hi\n```\n"
        );
    }

    #[test]
    fn json_is_the_existing_pretty_structured_session_shape() {
        let session = fixture();
        let document = render_full(&session, ExportFormat::Json).unwrap();
        let legacy = serde_json::to_string_pretty(&serde_json::json!(session)).unwrap();
        assert_eq!(document.content(), legacy);
        let value: serde_json::Value = serde_json::from_str(document.content()).unwrap();
        assert_eq!(value["id"], "claude:abc");
        assert_eq!(value["transcript_text"], session.transcript_text);
        assert_eq!(document.format(), ExportFormat::Json);
    }

    #[test]
    fn markdown_uses_a_longer_fence_than_embedded_code_blocks() {
        let mut session = fixture();
        session.transcript_text = "example:\n```rust\nfn main() {}\n```".into();

        let document = render_full(&session, ExportFormat::Markdown).unwrap();

        assert!(document.content().contains("\n````\nexample:"));
        assert!(document.content().ends_with("```\n````\n"));
    }
}
