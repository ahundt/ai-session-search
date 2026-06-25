//! `messages` command group: search and read per-message rows (Phase 2).
//!
//! Thin command glue over [`crate::db::Db::search_messages`] + [`crate::render`],
//! so `cli.rs` stays a dispatcher. `--limit 0` means unlimited (avoids the session
//! `--limit 25` trap). Date parsing here is RFC3339 / `YYYY-MM-DD` for now; Phase 4
//! upgrades `--since/--until` to full EDTF/duration parsing.

use std::io::{self, Write};

use anyhow::{Result, anyhow};
use clap::{Args, Subcommand};

use crate::db::Db;
use crate::models::{MessageFilters, MessageHit, Role};
use crate::render::{OutputFormat, Row, render};
use crate::util::{parse_datetime, truncate_for_display};

/// Max characters of content shown in tabular formats (json/jsonl keep full content).
const TABLE_CONTENT_CHARS: usize = 120;

impl Row for MessageHit {
    fn headers() -> &'static [&'static str] {
        &["session", "provider", "seq", "role", "ts", "content"]
    }

    fn cells(&self) -> Vec<String> {
        vec![
            self.session_id.clone(),
            self.provider.as_str().to_string(),
            self.seq.to_string(),
            self.role.as_str().to_string(),
            self.ts.map(|ts| ts.to_rfc3339()).unwrap_or_default(),
            truncate_for_display(&self.content, TABLE_CONTENT_CHARS),
        ]
    }
}

#[derive(Debug, Subcommand)]
pub enum MessagesCmd {
    /// Search messages by content / role / date across sessions.
    Search(MessageSearchArgs),
    /// Read all messages from one session (by id or prefix).
    Get(MessageGetArgs),
}

#[derive(Debug, Args)]
pub struct MessageSearchArgs {
    /// Case-insensitive literal substring to match in content. Omit to list all.
    pub query: Option<String>,
    /// Filter by role (user|assistant|tool|slash|compaction).
    #[arg(long = "type", value_enum)]
    pub role: Option<Role>,
    /// Match content with a Rust regex instead of a literal substring.
    #[arg(long)]
    pub regex: Option<String>,
    /// Scope to one session id (substring/prefix match).
    #[arg(long)]
    pub session: Option<String>,
    /// Lower time bound (RFC3339 or YYYY-MM-DD).
    #[arg(long)]
    pub since: Option<String>,
    /// Upper time bound (RFC3339 or YYYY-MM-DD).
    #[arg(long)]
    pub until: Option<String>,
    /// Exclude context-compaction messages.
    #[arg(long)]
    pub no_compaction: bool,
    /// Max results. 0 = unlimited.
    #[arg(long, default_value_t = 0)]
    pub limit: usize,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct MessageGetArgs {
    /// Session id or prefix.
    pub id: String,
    /// Filter by role.
    #[arg(long = "type", value_enum)]
    pub role: Option<Role>,
    /// Max results. 0 = unlimited.
    #[arg(long, default_value_t = 0)]
    pub limit: usize,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

/// Parse an optional `--since/--until` bound (RFC3339 or `YYYY-MM-DD`).
fn parse_bound(value: &Option<String>, flag: &str) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    match value.as_deref() {
        Some(raw) => Ok(Some(parse_datetime(raw).ok_or_else(|| {
            anyhow!("invalid {flag} value '{raw}': expected RFC3339 timestamp or YYYY-MM-DD")
        })?)),
        None => Ok(None),
    }
}

pub fn run(db: &Db, cmd: &MessagesCmd) -> Result<()> {
    match cmd {
        MessagesCmd::Search(args) => {
            let filters = MessageFilters {
                role: args.role,
                session: args.session.clone(),
                since: parse_bound(&args.since, "--since")?,
                until: parse_bound(&args.until, "--until")?,
                regex: args.regex.clone(),
                no_compaction: args.no_compaction,
                limit: args.limit,
            };
            let hits = db.search_messages(args.query.as_deref().unwrap_or(""), &filters)?;
            emit(&hits, args.format)
        }
        MessagesCmd::Get(args) => {
            let filters = MessageFilters {
                role: args.role,
                session: Some(args.id.clone()),
                limit: args.limit,
                ..Default::default()
            };
            let hits = db.search_messages("", &filters)?;
            emit(&hits, args.format)
        }
    }
}

fn emit(hits: &[MessageHit], format: OutputFormat) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    render(hits, format, &mut out)?;
    out.flush()?;
    Ok(())
}
