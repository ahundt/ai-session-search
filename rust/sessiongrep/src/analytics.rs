//! Phase 3 analytics: corrections detection, planning-command frequency, and stats.
//!
//! Correction categories are ported verbatim from aise (`engine.py:216-232`); order
//! matters (first match wins, so `other` is last). Config `analytics.correction_patterns`
//! (`"CATEGORY:REGEX"`, repeatable, same-category ORed) replaces the built-ins entirely.

use std::collections::HashMap;
use std::io::{self, Write};

use anyhow::{Result, anyhow};
use clap::Args;
use regex::Regex;
use serde::Serialize;

use crate::config::Config;
use crate::db::Db;
use crate::models::{CorrectionMatch, MessageFilters, PlanningCount};
use crate::render::{OutputFormat, Row, render};
use crate::util::{parse_date_bound, truncate_for_display};

const TABLE_CONTENT_CHARS: usize = 100;

/// Built-in correction categories (aise `engine.py:216-232`). First match wins.
fn default_correction_patterns() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        (
            "regression",
            vec![
                r"\byou deleted\b",
                r"\byou removed\b",
                r"\blost\b",
                r"\bregressed\b",
                r"\brollback\b",
                r"\brevert\b",
                r"\bbroke\b",
            ],
        ),
        (
            "skip_step",
            vec![
                r"\byou forgot\b",
                r"\byou missed\b",
                r"\byou skipped\b",
                r"\bdon't forget\b",
                r"\bmissing step\b",
                r"\byou didn't\b",
            ],
        ),
        (
            "misunderstanding",
            vec![
                r"\bwrong\b",
                r"\bincorrect\b",
                r"\bmistake\b",
                r"\bnono\b",
                r"\bno,\s",
                r"\bthat's not correct\b",
                r"\bactually\b",
                r"\bwait,?\s",
                r"\bwhat,",
            ],
        ),
        (
            "incomplete",
            vec![
                r"\balso need\b",
                r"\bmust also\b",
                r"\bnot done\b",
                r"\bnot finished\b",
                r"\bstill need\b",
                r"\bshould have\b",
                r"\bbut you\b",
            ],
        ),
        ("other", vec![r"\bstop\b"]),
    ]
}

/// Compile the active correction patterns: config override (`CATEGORY:REGEX`,
/// same-category ORed, first-seen order) when present, else the built-ins.
fn compile_patterns(config: &Config) -> Result<Vec<(String, Regex)>> {
    let custom = &config.analytics.correction_patterns;
    if custom.is_empty() {
        return default_correction_patterns()
            .into_iter()
            .map(|(category, kws)| {
                let re = Regex::new(&format!("(?i){}", kws.join("|")))
                    .map_err(|err| anyhow!("invalid built-in pattern for '{category}': {err}"))?;
                Ok((category.to_string(), re))
            })
            .collect();
    }

    let mut order: Vec<String> = Vec::new();
    let mut grouped: HashMap<String, Vec<String>> = HashMap::new();
    for spec in custom {
        let (category, rx) = spec
            .split_once(':')
            .ok_or_else(|| anyhow!("invalid correction pattern '{spec}': expected CATEGORY:REGEX"))?;
        if !grouped.contains_key(category) {
            order.push(category.to_string());
        }
        grouped.entry(category.to_string()).or_default().push(rx.to_string());
    }
    order
        .into_iter()
        .map(|category| {
            let joined = grouped[&category].join("|");
            let re = Regex::new(&format!("(?i){joined}"))
                .map_err(|err| anyhow!("invalid regex for category '{category}': {err}"))?;
            Ok((category, re))
        })
        .collect()
}

impl Row for CorrectionMatch {
    fn headers() -> &'static [&'static str] {
        &["session", "ts", "category", "pattern", "content"]
    }
    fn cells(&self) -> Vec<String> {
        vec![
            self.session_id.clone(),
            self.ts.map(|ts| ts.to_rfc3339()).unwrap_or_default(),
            self.category.clone(),
            self.matched_pattern.clone(),
            truncate_for_display(&self.content, TABLE_CONTENT_CHARS),
        ]
    }
}

impl Row for PlanningCount {
    fn headers() -> &'static [&'static str] {
        &["command", "count", "sessions", "projects"]
    }
    fn cells(&self) -> Vec<String> {
        vec![
            self.command.clone(),
            self.count.to_string(),
            self.unique_sessions.to_string(),
            self.unique_projects.to_string(),
        ]
    }
}

/// One role's message count, for `stats`.
#[derive(Debug, Clone, Serialize)]
pub struct RoleStat {
    pub role: String,
    pub count: i64,
}
impl Row for RoleStat {
    fn headers() -> &'static [&'static str] {
        &["role", "count"]
    }
    fn cells(&self) -> Vec<String> {
        vec![self.role.clone(), self.count.to_string()]
    }
}

#[derive(Debug, Args)]
pub struct CorrectionsArgs {
    /// Scope to one session id (substring match).
    #[arg(long)]
    pub session: Option<String>,
    #[arg(long)]
    pub since: Option<String>,
    #[arg(long)]
    pub until: Option<String>,
    /// Max results. 0 = unlimited.
    #[arg(long, default_value_t = 0)]
    pub limit: usize,
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct PlanningArgs {
    #[arg(long)]
    pub session: Option<String>,
    #[arg(long)]
    pub since: Option<String>,
    #[arg(long)]
    pub until: Option<String>,
    /// Max distinct commands. 0 = unlimited.
    #[arg(long, default_value_t = 0)]
    pub limit: usize,
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct StatsArgs {
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

fn filters_from(
    session: &Option<String>,
    since: &Option<String>,
    until: &Option<String>,
    limit: usize,
) -> Result<MessageFilters> {
    Ok(MessageFilters {
        session: session.clone(),
        since: parse_date_bound(since.as_deref(), "--since")?,
        until: parse_date_bound(until.as_deref(), "--until")?,
        limit,
        ..Default::default()
    })
}

pub fn run_corrections(db: &Db, config: &Config, args: &CorrectionsArgs) -> Result<()> {
    let patterns = compile_patterns(config)?;
    let filters = filters_from(&args.session, &args.since, &args.until, args.limit)?;
    let hits = db.find_corrections(&patterns, &filters)?;
    emit(&hits, args.format)
}

pub fn run_planning(db: &Db, args: &PlanningArgs) -> Result<()> {
    let filters = filters_from(&args.session, &args.since, &args.until, args.limit)?;
    let counts = db.planning_usage(&filters)?;
    emit(&counts, args.format)
}

pub fn run_stats(db: &Db, args: &StatsArgs) -> Result<()> {
    let rows: Vec<RoleStat> = db
        .message_role_counts()?
        .into_iter()
        .map(|(role, count)| RoleStat { role, count })
        .collect();
    emit(&rows, args.format)
}

fn emit<T: Serialize + Row>(rows: &[T], format: OutputFormat) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    render(rows, format, &mut out)?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patterns() -> Vec<(String, Regex)> {
        compile_patterns(&Config::default()).unwrap()
    }

    fn categorize(text: &str) -> Option<String> {
        patterns()
            .iter()
            .find_map(|(cat, re)| re.is_match(text).then(|| cat.clone()))
    }

    #[test]
    fn categories_match_aise_keywords() {
        assert_eq!(categorize("you forgot to run the tests").as_deref(), Some("skip_step"));
        assert_eq!(categorize("that is actually wrong").as_deref(), Some("misunderstanding"));
        assert_eq!(categorize("you must also add a test").as_deref(), Some("incomplete"));
    }

    #[test]
    fn first_match_wins_and_other_is_last() {
        // "stop" alone -> other (catch-all, last).
        assert_eq!(categorize("stop").as_deref(), Some("other"));
        // A message with a specific signal is categorized before falling to other.
        assert_eq!(categorize("you removed the function").as_deref(), Some("regression"));
        // No correction signal -> no category.
        assert_eq!(categorize("looks great, thanks"), None);
    }

    #[test]
    fn config_override_replaces_builtins() {
        let mut config = Config::default();
        config.analytics.correction_patterns = vec!["oops:nono".to_string(), "oops:whoops".to_string()];
        let compiled = compile_patterns(&config).unwrap();
        assert_eq!(compiled.len(), 1, "same category is ORed into one entry");
        assert_eq!(compiled[0].0, "oops");
        assert!(compiled[0].1.is_match("whoops that broke"));
        assert!(!compiled[0].1.is_match("you forgot"));
    }
}
