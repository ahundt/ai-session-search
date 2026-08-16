// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

//! CLI adapters for message-classification skills, data-driven repeat mining,
//! planning-command frequency, and stats.
//!
//! The executable correction categories live in the selected skill's `aise-capability.toml`; the
//! category helpers below remain only as compatibility test oracles. `analytics.planning_commands`
//! (regexes over the slash-command token) optionally restricts which commands `planning` counts
//! (empty = all). `vocab` and `repeats` use config-backed defaults for their public scan/output
//! controls.

#[cfg(test)]
use std::collections::HashMap;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};

use anyhow::{anyhow, bail, Result};
use clap::Args;
use regex::Regex;
use serde::Serialize;

use crate::config::{AnalyticsConfig, Config};
use crate::dates::DateRange;
use crate::db::Db;
use crate::message_search::{
    DetailLevel, FieldViewBudget, MatchViewBudget, DEFAULT_MATCH_EVIDENCE_MAX_CHARS,
};
use crate::messages::{CliFieldViewChars, CliMatchViewChars};
use crate::models::{
    MessageClassificationMatch, MessageFilters, MessageHit, MessageSearchMode, PlanningCount,
    Provider, Role,
};
use crate::render::{render, OutputFormat, Row};
use crate::util::truncate_for_display;

const TABLE_CONTENT_CHARS: usize = 100;
const USER_REQUEST_START: &str = "<USER_REQUEST>";
const USER_REQUEST_END: &str = "</USER_REQUEST>";

/// Historical built-in correction categories retained as a compatibility test oracle.
///
/// The executable source is the selected message-classification skill's `aise-capability.toml`.
/// These patterns remain narrowed to second-person / imperative / demonstrative forms so tests
/// can compare the current compiled capability with the pre-skills behavior.
pub(crate) fn default_correction_patterns() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        (
            "regression",
            vec![
                r"\byou (deleted|removed|reverted|lost|regressed|undid|rolled back|broke)\b",
                r"\b(that|this|it) (reverted|deleted|removed|undid|regressed)\b",
                r"\bbroke the (build|tests?|code|app)\b",
                r"\bregressed\b",
            ],
        ),
        (
            "skip_step",
            vec![
                r"\byou forgot\b",
                r"\byou missed\b",
                r"\byou skipped\b",
                r"\bdon'?t forget\b",
                r"\bmissing step\b",
                r"\byou didn'?t\b",
            ],
        ),
        (
            "misunderstanding",
            vec![
                r"\b(that is|that'?s|it is|it'?s) (actually )?(wrong|incorrect|not correct|not right|not what|a mistake)\b",
                r"\byou'?re wrong\b",
                r"\byou (misunderstood|got it wrong|misread)\b",
                r"\bnono\b",
                r"\bno,?\s+that'?s\b",
                r"\bno,?\s+i (meant|asked|said)\b",
                r"\bwait,?\s+(no|that'?s)\b",
                r"\bwrong approach\b",
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
                r"\byou should have\b",
                r"\bbut you\b",
            ],
        ),
        // Catch-all (last). A bare `\bstop\b` was ~98% false positives on real data:
        // it matched test fixtures ("run this command once and stop"), checkpoint
        // instructions ("commit and then stop"), and negations ("don't stop"). Restrict
        // to imperative-stop corrections: a leading "stop" (optionally softened by
        // ok/no/wait/please/just) or an explicit "stop <doing/that/it/...>" directive.
        (
            "other",
            vec![
                r"^\s*(?:ok,?\s+|no,?\s+|wait,?\s+|please\s+|just\s+)?stop\b",
                r"\bjust stop\b",
                r"\bplease stop\b",
                r"\bstop doing\b",
                r"\bstop that\b",
                r"\bstop it\b",
                r"\bstop changing\b",
                r"\bstop making\b",
                r"\bstop breaking\b",
            ],
        ),
    ]
}

/// The pre-skills correction compiler, kept as a TEST ORACLE only.
///
/// `corrections.rs` replaced this on the live path, so nothing outside tests calls it. It stays
/// because it is the reference the new resolution is checked against: S17 showed that compiling
/// one regex per pattern instead of one `(?i)` alternation per category silently changes both
/// case-sensitivity and which substring is reported, and the only way to keep asserting the new
/// path produces byte-identical regex sources is to keep the old path around to compare with.
/// Delete it only together with the tests that compare against it.
#[cfg(test)]
fn compile_category_patterns(
    custom: &[String],
    builtins: Vec<(&'static str, Vec<&'static str>)>,
    label: &str,
) -> Result<Vec<(String, Regex)>> {
    if custom.is_empty() {
        return builtins
            .into_iter()
            .map(|(category, patterns)| {
                let re = Regex::new(&format!("(?i){}", patterns.join("|"))).map_err(|err| {
                    anyhow!("invalid built-in {label} pattern for '{category}': {err}")
                })?;
                Ok((category.to_string(), re))
            })
            .collect();
    }
    let mut order: Vec<String> = Vec::new();
    let mut grouped: HashMap<String, Vec<String>> = HashMap::new();
    for spec in custom {
        let (category, rx) = spec
            .split_once(':')
            .ok_or_else(|| anyhow!("invalid {label} pattern '{spec}': expected CATEGORY:REGEX"))?;
        if !grouped.contains_key(category) {
            order.push(category.to_string());
        }
        grouped
            .entry(category.to_string())
            .or_default()
            .push(rx.to_string());
    }
    order
        .into_iter()
        .map(|category| {
            let joined = grouped[&category].join("|");
            let re = Regex::new(&format!("(?i){joined}"))
                .map_err(|err| anyhow!("invalid {label} regex for category '{category}': {err}"))?;
            Ok((category, re))
        })
        .collect()
}

/// Compile the active correction patterns the pre-skills way: config override
/// (`CATEGORY:REGEX`, same-category ORed, first-seen order) when present, else the built-ins.
///
/// Test oracle only — see [`compile_category_patterns`].
#[cfg(test)]
pub(crate) fn compile_patterns(_config: &Config) -> Result<Vec<(String, Regex)>> {
    compile_category_patterns(&[], default_correction_patterns(), "correction")
}

impl Row for MessageClassificationMatch {
    fn headers() -> &'static [&'static str] {
        // `policy` is a column rather than a header line because `--skill` is repeatable: with two
        // policies selected, "which rules called this a correction" differs per row, and a
        // caller comparing a candidate policy against the built-in one needs it beside each match.
        &[
            "session", "seq", "ts", "policy", "category", "match", "content",
        ]
    }
    fn cells(&self) -> Vec<String> {
        vec![
            self.session_id.clone(),
            self.message_seq.to_string(),
            self.ts.map(|ts| ts.to_rfc3339()).unwrap_or_default(),
            self.policy_name.clone(),
            self.category.clone(),
            self.matched_text.clone(),
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

#[derive(Debug, Clone, Args)]
pub struct CorrectionsArgs {
    /// JSON object containing direct `categories` for this run. It replaces the primary selected
    /// skill's adjacent aise-capability.toml rules while preserving that skill's identity and version.
    #[arg(long, value_name = "JSON")]
    pub definition_json: Option<String>,
    /// Exact session id or unique prefix. Use this when chaining from search output.
    /// Omit to include every session.
    #[arg(long)]
    pub session_id: Option<String>,
    /// Restrict to one indexed session source. Omit to include all nine.
    #[arg(long, value_enum)]
    pub provider: Option<Provider>,
    /// Restrict to sessions whose cwd or repo root is this directory or a descendant of it
    /// (component boundary).
    /// Omit to search every allowed root.
    #[arg(long)]
    pub path: Option<String>,
    #[command(flatten)]
    pub dates: DateRange,
    /// Max matches. Omit to use `[capabilities.message_classification].limit`. 0 = every match.
    #[arg(long)]
    pub limit: Option<usize>,
    /// Matches to skip before `--limit` applies, newest first. 0 starts at the newest match.
    #[arg(long, default_value_t = 0)]
    pub offset: usize,
    /// Session classes to scan. Omit for user-started sessions only, because a correction is
    /// something a person told the agent: in a spawned run the `user` rows are the calling
    /// agent's delegation prompt. Pass `user subagent` for both, as `search` and `list` return
    /// by default.
    #[arg(long = "session-kinds", value_enum, num_args = 1..)]
    pub session_kinds: Vec<crate::models::SessionKind>,
    /// Additional message-classification skills to evaluate, in argument order after the primary
    /// selected skill. Each value is a catalog name, skill directory, or exact SKILL.md path.
    #[arg(long = "skill", value_name = "NAME_OR_PATH")]
    pub additional_skills: Vec<std::ffi::OsString>,
    /// Presentation preset for JSON/JSONL results: compact bounds the selected field and match
    /// view; full keeps the complete field and only the exact match span. Omit to preserve the
    /// complete typed report. The bounded form keeps the full message recoverable by message_ref.
    #[arg(
        long,
        value_enum,
        conflicts_with_all = ["field_view_chars", "match_view_chars"]
    )]
    pub detail: Option<DetailLevel>,
    /// Bound the selected classified message field in JSON/JSONL output to this many Unicode
    /// scalar characters, or use no-char-limit. Omit for the complete field.
    #[arg(long)]
    pub field_view_chars: Option<CliFieldViewChars>,
    /// Bound the match-centered view in JSON/JSONL output to minimal or this many Unicode scalar
    /// characters. Omit for the standard 220-character match view.
    #[arg(long)]
    pub match_view_chars: Option<CliMatchViewChars>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct PlanningArgs {
    /// Restrict to one indexed session source. Omit to include all nine.
    #[arg(long, value_enum)]
    pub provider: Option<Provider>,
    /// Exact session id or unique prefix. Use this when chaining from search output. Omit to include every session.
    #[arg(long)]
    pub session_id: Option<String>,
    /// Restrict to sessions whose cwd or repo root is this directory or a descendant of it
    /// (component boundary).
    /// Omit to search every allowed root.
    #[arg(long)]
    pub path: Option<String>,
    #[command(flatten)]
    pub dates: DateRange,
    /// Keep only slash-command tokens matching this case-insensitive regex.
    ///
    /// Regexes match the leading command token; repeat to OR several token regexes. Omit to count
    /// every slash command.
    #[arg(long = "commands", alias = "command")]
    pub command_patterns: Vec<String>,
    /// Max distinct commands. Omit to use `[analytics].planning_limit`. 0 = every command.
    #[arg(long)]
    pub limit: Option<usize>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct StatsArgs {
    /// Restrict to one indexed session source. Omit to include all nine.
    #[arg(long, value_enum)]
    pub provider: Option<Provider>,
    /// Exact session id or unique prefix. Use this when chaining from search output. Omit to include every session.
    #[arg(long)]
    pub session_id: Option<String>,
    /// Restrict to sessions whose cwd or repo root is this directory or a descendant of it
    /// (component boundary).
    /// Omit to search every allowed root.
    #[arg(long)]
    pub path: Option<String>,
    #[command(flatten)]
    pub dates: DateRange,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

/// Build a [`MessageFilters`] from a session scope, a path prefix, a [`DateRange`], and a
/// limit. `path` is normalized to an absolute prefix (`~`/relative resolved) by
/// [`crate::util::normalize_path_prefix`], matching the session- and message-search `--path`.
fn filters_from(
    db: &Db,
    session_id: &Option<String>,
    provider: Option<Provider>,
    path: &Option<String>,
    dates: &DateRange,
    limit: usize,
    offset: usize,
) -> Result<MessageFilters> {
    let (since, until) = dates.resolve_now()?;
    let exact_session_id = session_id
        .as_deref()
        .map(|id| db.resolve_session_record(id).map(|session| session.id))
        .transpose()?;
    Ok(MessageFilters {
        providers: provider.map(|provider| vec![provider]),
        session_id: exact_session_id,
        path_prefix: path.as_deref().map(crate::util::normalize_path_prefix),
        since,
        until,
        limit,
        offset,
        ..Default::default()
    })
}

pub(crate) fn message_classification_filters(
    db: &Db,
    config: &Config,
    args: &CorrectionsArgs,
) -> Result<MessageFilters> {
    let mut filters = filters_from(
        db,
        &args.session_id,
        args.provider,
        &args.path,
        &args.dates,
        args.limit
            .unwrap_or(config.capabilities.message_classification.limit),
        args.offset,
    )?;
    // Left as `None` when the caller named no class, so `find_corrections` applies its own
    // user-only default. Setting it here instead would put the "what a correction IS" decision in
    // the CLI adapter, where the Python and MCP surfaces could not inherit it.
    if !args.session_kinds.is_empty() {
        filters.session_kinds = Some(args.session_kinds.clone());
    }
    Ok(filters)
}

pub(crate) fn render_skill_run_report(
    report: &crate::skill_run::SkillRunReport,
    config: &Config,
    args: &CorrectionsArgs,
) -> Result<()> {
    match args.format {
        OutputFormat::Json => {
            if let Some((field_budget, match_budget)) = skill_presentation_budgets(config, args) {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&bounded_skill_run_value(
                        report,
                        field_budget,
                        match_budget
                    )?)?
                );
            } else {
                println!("{}", serde_json::to_string_pretty(report)?);
            }
            Ok(())
        }
        OutputFormat::Jsonl => {
            if let Some((field_budget, match_budget)) = skill_presentation_budgets(config, args) {
                println!(
                    "{}",
                    serde_json::to_string(&bounded_skill_run_value(
                        report,
                        field_budget,
                        match_budget,
                    )?)?
                );
            } else {
                println!("{}", serde_json::to_string(report)?);
            }
            Ok(())
        }
        OutputFormat::Table | OutputFormat::Csv | OutputFormat::Plain => {
            let crate::skill_run::SkillCapabilityOutput::MessageClassification(result) =
                &report.output;
            render_correction_report(&result.report, args.format)
        }
    }
}

fn skill_presentation_budgets(
    config: &Config,
    args: &CorrectionsArgs,
) -> Option<(FieldViewBudget, MatchViewBudget)> {
    if args.detail.is_none() && args.field_view_chars.is_none() && args.match_view_chars.is_none() {
        return None;
    }
    let field_budget = match args.detail {
        Some(DetailLevel::Compact) => FieldViewBudget::MaxChars {
            max_chars: std::num::NonZeroUsize::new(config.cli.evidence_preview_chars)
                .expect("validated CLI evidence preview chars are positive"),
        },
        Some(DetailLevel::Full) => FieldViewBudget::NoCharLimit,
        None => match args.field_view_chars {
            Some(CliFieldViewChars::NoCharLimit) | None => FieldViewBudget::NoCharLimit,
            Some(CliFieldViewChars::MaxChars(max_chars)) => FieldViewBudget::MaxChars { max_chars },
        },
    };
    let match_budget = match args.detail {
        Some(DetailLevel::Full) => MatchViewBudget::MinimalSpan,
        Some(DetailLevel::Compact) => MatchViewBudget::MaxChars {
            max_chars: config
                .search
                .message_search
                .match_evidence_max_chars
                .unwrap_or_else(|| {
                    std::num::NonZeroUsize::new(DEFAULT_MATCH_EVIDENCE_MAX_CHARS)
                        .expect("typed match evidence default is positive")
                }),
        },
        None => match args.match_view_chars {
            Some(CliMatchViewChars::Minimal) => MatchViewBudget::MinimalSpan,
            Some(CliMatchViewChars::MaxChars(max_chars)) => MatchViewBudget::MaxChars { max_chars },
            None => MatchViewBudget::MaxChars {
                max_chars: config
                    .search
                    .message_search
                    .match_evidence_max_chars
                    .unwrap_or_else(|| {
                        std::num::NonZeroUsize::new(DEFAULT_MATCH_EVIDENCE_MAX_CHARS)
                            .expect("typed match evidence default is positive")
                    }),
            },
        },
    };
    Some((field_budget, match_budget))
}

fn bounded_skill_run_value(
    report: &crate::skill_run::SkillRunReport,
    field_budget: FieldViewBudget,
    match_budget: MatchViewBudget,
) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(report)?;
    let crate::skill_run::SkillCapabilityOutput::MessageClassification(output) = &report.output;
    let matches = output
        .report
        .matches
        .iter()
        .map(|matched| {
            crate::message_search::classification_presentation_document(
                matched,
                field_budget,
                match_budget,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    value["output"]["result"]["report"]["matches"] = serde_json::Value::Array(matches);
    Ok(value)
}

fn render_correction_report(
    report: &crate::corrections::MessageClassificationReport,
    format: OutputFormat,
) -> Result<()> {
    // JSON and JSONL return earlier from `render_skill_run_report` with the complete typed
    // SkillRunReport. Human/table formats intentionally render only match rows.
    let stdout = io::stdout();
    let mut out = stdout.lock();
    crate::render::render_record(report, &report.matches, format, &mut out)?;
    out.flush()?;
    Ok(())
}

fn compile_planning_regex(label: &str, pattern: &str) -> Result<Regex> {
    Regex::new(&format!("(?i){pattern}"))
        .map_err(|err| anyhow!("invalid {label} regex '{pattern}': {err}"))
}

/// Compile config and CLI slash-command filters. Regexes match the command token including its
/// leading slash; empty input counts every slash command.
pub(crate) fn compile_planning_filters(
    config: &Config,
    cli_patterns: &[String],
) -> Result<Vec<Regex>> {
    let mut filters =
        Vec::with_capacity(config.analytics.planning_commands.len() + cli_patterns.len());
    for pattern in &config.analytics.planning_commands {
        filters.push(compile_planning_regex("planning_commands", pattern)?);
    }
    for pattern in cli_patterns {
        filters.push(compile_planning_regex("--commands", pattern)?);
    }
    Ok(filters)
}

pub fn run_planning(db: &Db, config: &Config, args: &PlanningArgs) -> Result<()> {
    let filters = filters_from(
        db,
        &args.session_id,
        args.provider,
        &args.path,
        &args.dates,
        args.limit.unwrap_or(config.analytics.planning_limit),
        0,
    )?;
    let counts = crate::service::AnalysisService::new(config, db)
        .planning(&filters, &args.command_patterns)?;
    emit(&counts, args.format)
}

pub fn run_stats(db: &Db, config: &Config, args: &StatsArgs) -> Result<()> {
    let filters = filters_from(
        db,
        &args.session_id,
        args.provider,
        &args.path,
        &args.dates,
        0,
        0,
    )?;
    let rows = crate::service::AnalysisService::new(config, db).role_statistics(&filters)?;
    emit(&rows, args.format)
}

/// One vocabulary term and its frequency (rendered by `vocab`).
#[derive(Debug, Serialize)]
struct VocabRow {
    term: String,
    /// Documents (messages) containing the term.
    docs: i64,
    /// Total occurrences across all messages.
    count: i64,
}

impl Row for VocabRow {
    fn headers() -> &'static [&'static str] {
        &["term", "docs", "count"]
    }
    fn cells(&self) -> Vec<String> {
        vec![
            self.term.clone(),
            self.docs.to_string(),
            self.count.to_string(),
        ]
    }
}

#[derive(Debug, Args)]
pub struct VocabArgs {
    /// Report only terms starting with this text, which is how you ask about one term instead of
    /// reading all of them. Matching is case-insensitive. Omit it, or pass an empty string, for
    /// every term.
    #[arg(long)]
    pub prefix: Option<String>,
    /// Read the substring (3-gram) index instead of word tokens: terms are 3 characters, spaces
    /// and punctuation included, and both count columns report messages, never occurrences.
    #[arg(long)]
    pub trigram: bool,
    /// Max terms (most frequent first). Omit to use `[analytics].vocab_limit`. 0 = every term.
    #[arg(long)]
    pub limit: Option<usize>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

pub fn run_vocab(db: &Db, config: &AnalyticsConfig, args: &VocabArgs) -> Result<()> {
    let limit = args.limit.unwrap_or(config.vocab_limit);
    let rows: Vec<VocabRow> = db
        .vocabulary(args.trigram, limit, args.prefix.as_deref())?
        .into_iter()
        .map(|(term, docs, count)| VocabRow { term, docs, count })
        .collect();
    emit(&rows, args.format)
}

#[derive(Debug, Clone, Serialize)]
struct RepeatGroupExample {
    session_id: String,
    seq: i64,
    ts: Option<String>,
    preview: String,
    context_command: String,
}

impl RepeatGroupExample {
    fn from_hit(hit: &MessageHit, context: i64) -> Self {
        Self {
            session_id: hit.session_id.clone(),
            seq: hit.seq,
            ts: hit.ts.map(|ts| ts.to_rfc3339()),
            preview: truncate_for_display(repeat_mining_text(&hit.content), TABLE_CONTENT_CHARS),
            context_command: context_command(&hit.session_id, hit.seq, context),
        }
    }
}

#[derive(Debug, Serialize)]
struct RepeatGroup {
    repeat: String,
    matches: usize,
    sessions: usize,
    examples: Vec<RepeatGroupExample>,
}

impl Row for RepeatGroup {
    fn headers() -> &'static [&'static str] {
        &["repeat", "matches", "sessions", "examples", "preview"]
    }
    fn cells(&self) -> Vec<String> {
        let examples = self
            .examples
            .iter()
            .take(3)
            .map(|m| format!("{}:{}", m.session_id, m.seq))
            .collect::<Vec<_>>()
            .join(", ");
        let preview = self
            .examples
            .first()
            .map(|m| m.preview.clone())
            .unwrap_or_default();
        vec![
            self.repeat.clone(),
            self.matches.to_string(),
            self.sessions.to_string(),
            truncate_for_display(&examples, TABLE_CONTENT_CHARS),
            preview,
        ]
    }
}

#[derive(Debug, Args)]
pub struct RepeatsArgs {
    /// Optional text to narrow candidate messages before repeat mining. Exact literal by default;
    /// add --regex to interpret it as a Rust regex. A query starting with `-` is parsed as a flag
    /// here; pass it after `--`, with every other flag before the `--`, e.g. `--regex -- ^/\S+`.
    pub query: Option<String>,
    /// Interpret QUERY as a Rust regex before repeat mining.
    #[arg(long)]
    pub regex: bool,
    /// Filter by role: user (source-attributable human text only), assistant, tool
    /// (calls/results), slash (human-entered commands), or compaction. Omit to mine human text.
    #[arg(long = "role", value_enum)]
    pub role: Option<Role>,
    /// Restrict to one indexed session source. Omit to include all nine.
    #[arg(long, value_enum)]
    pub provider: Option<Provider>,
    /// Exact session id or unique prefix. Use this when chaining from search output. Omit to include every session.
    #[arg(long)]
    pub session_id: Option<String>,
    /// Restrict to sessions whose cwd or repo root is this directory or a descendant of it
    /// (component boundary).
    /// Omit to search every allowed root.
    #[arg(long)]
    pub path: Option<String>,
    #[command(flatten)]
    pub dates: DateRange,
    /// Neighboring messages before/after each match in generated follow-up commands.
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(i64).range(0..))]
    pub context: i64,
    /// Max candidate messages to scan. 0 = every candidate message.
    #[arg(long, default_value_t = 0)]
    pub limit: usize,
    /// Max repeat groups to output. Omit to use `[analytics].repeat_max_groups`. 0 = every group.
    #[arg(long)]
    pub max_groups: Option<usize>,
    /// Representative messages per group. Omit to use
    /// `[analytics].repeat_max_examples_per_group`. 0 = every matching message. Aggregate
    /// `matches` and `sessions` counts always cover the full group.
    #[arg(long)]
    pub max_examples_per_group: Option<usize>,
    /// Minimum messages a discovered phrase must appear in. Omit to use
    /// `[analytics].repeat_min_matches`.
    #[arg(long)]
    pub min_matches: Option<usize>,
    /// Minimum words in a discovered phrase. Omit to use `[analytics].repeat_phrase_min_words`.
    #[arg(long)]
    pub phrase_min_words: Option<usize>,
    /// Maximum words in a discovered phrase. Omit to use `[analytics].repeat_phrase_max_words`.
    #[arg(long)]
    pub phrase_max_words: Option<usize>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

pub fn run_repeats(db: &Db, config: &AnalyticsConfig, args: &RepeatsArgs) -> Result<()> {
    if args.regex && args.query.is_none() {
        bail!("--regex requires QUERY");
    }
    run_repeats_issues(db, config, args)
}

fn repeat_filters(
    db: &Db,
    args: &RepeatsArgs,
    default_role: Option<Role>,
) -> Result<MessageFilters> {
    let (since, until) = args.dates.resolve_now()?;
    let exact_session_id = args
        .session_id
        .as_deref()
        .map(|id| db.resolve_session_record(id).map(|session| session.id))
        .transpose()?;
    Ok(MessageFilters {
        role: args.role.or(default_role),
        providers: args.provider.map(|provider| vec![provider]),
        session_id: exact_session_id,
        path_prefix: args.path.as_deref().map(crate::util::normalize_path_prefix),
        since,
        until,
        match_mode: if args.regex {
            MessageSearchMode::Regex
        } else {
            MessageSearchMode::Literal
        },
        limit: args.limit,
        ..Default::default()
    })
}

/// The text the repeat scan filters messages by, in both literal and regex mode. `--regex` used
/// to travel inside the filters (`MessageFilters::regex`) while the query text was passed as
/// `""`; when the two modes were unified under `match_mode` (2026-07-14) the blank stayed, so
/// `aise repeats --regex PATTERN` mined every message and the pattern was never applied.
fn repeat_query(args: &RepeatsArgs) -> &str {
    args.query.as_deref().unwrap_or("")
}

fn run_repeats_issues(db: &Db, config: &AnalyticsConfig, args: &RepeatsArgs) -> Result<()> {
    let max_groups = args.max_groups.unwrap_or(config.repeat_max_groups);
    let max_examples_per_group = args
        .max_examples_per_group
        .unwrap_or(config.repeat_max_examples_per_group);
    let min_matches = args.min_matches.unwrap_or(config.repeat_min_matches);
    let phrase_min_words = args
        .phrase_min_words
        .unwrap_or(config.repeat_phrase_min_words);
    let phrase_max_words = args
        .phrase_max_words
        .unwrap_or(config.repeat_phrase_max_words);

    if phrase_min_words == 0 {
        bail!("--phrase-min-words must be at least 1");
    }
    if min_matches == 0 {
        bail!("--min-matches must be at least 1");
    }
    if phrase_max_words < phrase_min_words {
        bail!("--phrase-max-words must be >= --phrase-min-words");
    }
    let filters = repeat_filters(db, args, Some(Role::User))?;
    let query = repeat_query(args);
    // Default/user repeat mining is evidence about what a person wrote, not every provider row
    // stored with `role=user`. Explicit assistant/tool/slash/compaction analysis retains its
    // role-based contract because those roles do not claim human authorship.
    let hits = if args.role.is_none() || args.role == Some(Role::User) {
        db.search_attributable_human_messages(query, &filters)?
    } else {
        db.search_messages(query, &filters)?
    };
    let rows = repeat_phrase_groups(
        &hits,
        args.context,
        min_matches,
        phrase_min_words,
        phrase_max_words,
        max_groups,
        max_examples_per_group,
    );
    emit(&rows, args.format)
}

fn repeat_phrase_groups(
    hits: &[MessageHit],
    context: i64,
    min_matches: usize,
    min_words: usize,
    max_words: usize,
    max_groups: usize,
    max_examples_per_group: usize,
) -> Vec<RepeatGroup> {
    if hits.is_empty() {
        return Vec::new();
    }

    let mut phrase_hits: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    for (index, hit) in hits.iter().enumerate() {
        for phrase in phrases_in_message(&hit.content, min_words, max_words) {
            phrase_hits.entry(phrase).or_default().insert(index);
        }
    }

    let mut candidates: Vec<(String, BTreeSet<usize>)> = phrase_hits
        .into_iter()
        .filter(|(_, indices)| indices.len() >= min_matches)
        .collect();
    candidates.sort_by(|(phrase_a, hits_a), (phrase_b, hits_b)| {
        hits_b
            .len()
            .cmp(&hits_a.len())
            .then_with(|| phrase_word_count(phrase_b).cmp(&phrase_word_count(phrase_a)))
            .then_with(|| phrase_a.cmp(phrase_b))
    });

    remove_equal_support_contained_phrases(&mut candidates);
    if max_groups > 0 && candidates.len() > max_groups {
        candidates.truncate(max_groups);
    }

    candidates
        .into_iter()
        .map(|(repeat, indices)| {
            let matches = indices.len();
            let sessions = indices
                .iter()
                .map(|&index| hits[index].session_id.as_str())
                .collect::<BTreeSet<_>>()
                .len();
            let example_count = if max_examples_per_group == 0 {
                matches
            } else {
                max_examples_per_group.min(matches)
            };
            let examples = indices
                .iter()
                .take(example_count)
                .map(|&index| RepeatGroupExample::from_hit(&hits[index], context))
                .collect();
            RepeatGroup {
                repeat,
                matches,
                sessions,
                examples,
            }
        })
        .collect()
}

fn remove_equal_support_contained_phrases(candidates: &mut Vec<(String, BTreeSet<usize>)>) {
    let mut kept: Vec<(String, BTreeSet<usize>)> = Vec::with_capacity(candidates.len());
    for (phrase, indices) in candidates.drain(..) {
        let is_duplicate = kept.iter().any(|(kept_phrase, kept_indices)| {
            kept_indices == &indices && phrase_is_contiguous_subphrase_of(&phrase, kept_phrase)
        });
        if !is_duplicate {
            kept.push((phrase, indices));
        }
    }
    *candidates = kept;
}

fn phrase_is_contiguous_subphrase_of(needle: &str, haystack: &str) -> bool {
    if needle == haystack {
        return true;
    }
    let needle_words = needle.split_whitespace().collect::<Vec<_>>();
    let haystack_words = haystack.split_whitespace().collect::<Vec<_>>();
    needle_words.len() < haystack_words.len()
        && haystack_words
            .windows(needle_words.len())
            .any(|window| window == needle_words)
}

fn phrases_in_message(content: &str, min_words: usize, max_words: usize) -> BTreeSet<String> {
    let tokens = normalized_tokens(repeat_mining_text(content));
    let mut phrases = BTreeSet::new();
    if tokens.len() < min_words {
        return phrases;
    }
    let max_words = max_words.max(min_words).min(tokens.len());
    for width in min_words..=max_words {
        for window in tokens.windows(width) {
            if informative_phrase(window) {
                phrases.insert(window.join(" "));
            }
        }
    }
    phrases
}

fn repeat_mining_text(content: &str) -> &str {
    extract_user_request_body(content).unwrap_or(content)
}

fn extract_user_request_body(content: &str) -> Option<&str> {
    let start = content.find(USER_REQUEST_START)? + USER_REQUEST_START.len();
    let end = content[start..].find(USER_REQUEST_END)? + start;
    Some(content[start..end].trim())
}

/// Split `content` into caseless alphanumeric word tokens.
///
/// Tokens taken from stored text are compared against tokens a user typed, so both sides fold case
/// the way search does ([`crate::util::fold_caseless_char`]) rather than merely lowercasing: Greek
/// writes one letter as `Σ`, `σ`, or `ς` by position, and lowercasing keeps the last two apart.
pub(crate) fn normalized_tokens(content: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in content.chars() {
        if ch.is_alphanumeric() {
            current.extend(crate::util::fold_caseless_char(ch));
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn informative_phrase(tokens: &[String]) -> bool {
    if tokens
        .iter()
        .any(|token| token.chars().any(|ch| ch.is_ascii_digit()))
    {
        return false;
    }
    tokens
        .first()
        .is_some_and(|token| !is_repeat_stopword(token))
        && tokens
            .last()
            .is_some_and(|token| !is_repeat_stopword(token))
        && tokens.iter().any(|token| {
            token.len() >= 4
                && !is_repeat_stopword(token)
                && !token.chars().all(|ch| ch.is_ascii_digit())
        })
}

fn is_repeat_stopword(token: &str) -> bool {
    matches!(
        token,
        "a" | "an"
            | "additional"
            | "and"
            | "are"
            | "as"
            | "at"
            | "be"
            | "but"
            | "by"
            | "can"
            | "does"
            | "do"
            | "for"
            | "from"
            | "has"
            | "have"
            | "i"
            | "if"
            | "in"
            | "is"
            | "it"
            | "local"
            | "metadata"
            | "not"
            | "of"
            | "on"
            | "or"
            | "rather"
            | "request"
            | "s"
            | "so"
            | "that"
            | "than"
            | "the"
            | "there"
            | "this"
            | "time"
            | "to"
            | "user"
            | "was"
            | "with"
            | "you"
            | "your"
    )
}

fn phrase_word_count(phrase: &str) -> usize {
    phrase.split_whitespace().count()
}

fn context_command(session_id: &str, seq: i64, context: i64) -> String {
    format!("aise messages get {session_id} --seq {seq} --context {context}")
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

    /// One Greek word is one token however its sigma is written.
    ///
    /// Repeat mining and phrase analysis compare a token a user typed against tokens taken from
    /// stored text, so the two sides have to agree on what the same word is — the same requirement
    /// search has, and the same rule answers it ([`crate::util::fold_caseless_char`]). Lowercasing
    /// alone leaves `ς` and `σ` apart, which counted one repeated word as two and let an excluded
    /// phrase token pass through unexcluded.
    #[test]
    fn a_greek_word_normalizes_to_one_token_whichever_sigma_it_is_written_with() {
        let expected = vec!["οδοσσ".to_string()];
        for spelling in ["ΟΔΟΣΣ", "οδοσς", "οδοσσ", "Οδοσς"] {
            assert_eq!(
                normalized_tokens(spelling),
                expected,
                "{spelling:?} is the same token"
            );
        }
    }

    fn categorize(text: &str) -> Option<String> {
        patterns()
            .iter()
            .find_map(|(cat, re)| re.is_match(text).then(|| cat.clone()))
    }

    fn hit(seq: i64, role: Role, content: &str) -> MessageHit {
        MessageHit {
            session_id: "claude:test".to_string(),
            provider: Provider::Claude,
            seq,
            role,
            kind: crate::models::MessageKind::Conversation,
            ts: None,
            tool_name: None,
            tool_call_id: None,
            fuzzy_score: None,
            content: content.to_string(),
        }
    }

    #[test]
    fn phrase_is_contiguous_subphrase_of_requires_contiguity() {
        assert!(phrase_is_contiguous_subphrase_of("a b", "a b")); // equal phrases
        assert!(phrase_is_contiguous_subphrase_of("a b", "a b c")); // contiguous prefix
        assert!(phrase_is_contiguous_subphrase_of("b c", "a b c")); // contiguous suffix
        assert!(phrase_is_contiguous_subphrase_of("b", "a b c")); // single word inside
        assert!(!phrase_is_contiguous_subphrase_of("a c", "a b c")); // gap: not contiguous
        assert!(!phrase_is_contiguous_subphrase_of("a b c", "a b")); // needle longer than haystack
        assert!(!phrase_is_contiguous_subphrase_of("x", "a b c")); // absent entirely
    }

    #[test]
    fn informative_phrase_rejects_stopword_ends_and_numeric_noise() {
        let toks = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        // A content token of four or more characters at both ends is informative.
        assert!(informative_phrase(&toks("deploy service")));
        // A stopword at the first or last position disqualifies the phrase.
        assert!(!informative_phrase(&toks("and service")));
        assert!(!informative_phrase(&toks("deploy and")));
        // Any digit-bearing token marks the whole phrase as numeric noise.
        assert!(!informative_phrase(&toks("deploy service2")));
        // No token reaches the four-character content threshold.
        assert!(!informative_phrase(&toks("run ci")));
        // The empty phrase is never informative.
        assert!(!informative_phrase(&[]));
    }

    #[test]
    fn remove_equal_support_contained_phrases_is_support_and_order_sensitive() {
        let set = |xs: &[usize]| xs.iter().copied().collect::<BTreeSet<usize>>();
        // Same support and the container comes first: the contained subphrase drops.
        let mut c = vec![
            ("deploy service".to_string(), set(&[1, 2])),
            ("deploy".to_string(), set(&[1, 2])),
        ];
        remove_equal_support_contained_phrases(&mut c);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].0, "deploy service");
        // Different support: both survive even though one contains the other.
        let mut c = vec![
            ("deploy service".to_string(), set(&[1, 2])),
            ("deploy".to_string(), set(&[5])),
        ];
        remove_equal_support_contained_phrases(&mut c);
        assert_eq!(c.len(), 2);
        // Order matters: a container appearing after its subphrase is not itself a
        // subphrase of the kept shorter phrase, so both are kept.
        let mut c = vec![
            ("deploy".to_string(), set(&[1, 2])),
            ("deploy service".to_string(), set(&[1, 2])),
        ];
        remove_equal_support_contained_phrases(&mut c);
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn compile_planning_regex_is_case_insensitive_and_reports_invalid_patterns() {
        // The compiler prepends (?i), so matching ignores case.
        let re = compile_planning_regex("planning", "/goal").unwrap();
        assert!(re.is_match("/GOAL"));
        // An invalid pattern fails with the label and offending pattern named.
        let err = compile_planning_regex("planning", "[unclosed")
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid planning regex '[unclosed'"), "{err}");
    }

    #[test]
    fn repeats_mine_only_original_attributable_human_content() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.execute_batch_for_test(
            "insert into sessions (
                     id, provider, provider_session_id, preview_text,
                     source_path, parse_version, discovery_source
                 ) values ('claude:repeat-authorship', 'claude', 'repeat-authorship', '',
                           '/repeat-authorship', 'v1', 'test');
                 insert into messages (
                     id, session_id, provider, seq, role, kind, content, authorship, record_relation
                 ) values
                   (101, 'claude:repeat-authorship', 'claude', 0, 'user', 'conversation',
                    'durable human phrase', 'human', 'original'),
                   (102, 'claude:repeat-authorship', 'claude', 1, 'user', 'conversation',
                    'durable human phrase', 'human', 'original'),
                   (103, 'claude:repeat-authorship', 'claude', 2, 'user', 'conversation',
                    'durable agent phrase', 'agent', 'original'),
                   (104, 'claude:repeat-authorship', 'claude', 3, 'user', 'harness_notice',
                    'durable harness phrase', 'harness', 'original'),
                   (105, 'claude:repeat-authorship', 'claude', 4, 'user', 'conversation',
                    'durable mirror phrase', 'human', 'mirror'),
                   (106, 'claude:repeat-authorship', 'claude', 5, 'user', 'conversation',
                    'durable unknown phrase', 'unknown', 'unknown'),
                   (107, 'claude:repeat-authorship', 'claude', 6, 'user', 'conversation',
                    'mixed human phrase generated repeated phrase', 'mixed', 'original'),
                   (108, 'claude:repeat-authorship', 'claude', 7, 'user', 'conversation',
                    'mixed human phrase generated repeated phrase', 'mixed', 'original');
                 insert into message_content_parts (
                     message_id, ordinal, start_char, end_char, authorship, origin
                 ) values
                   (107, 0, 0, 18, 'human', 'direct_input'),
                   (107, 1, 18, 43, 'generated', 'tool_payload'),
                   (108, 0, 0, 18, 'human', 'direct_input'),
                   (108, 1, 18, 43, 'generated', 'tool_payload');",
        )
        .unwrap();
        let args = RepeatsArgs {
            query: None,
            regex: false,
            role: None,
            provider: None,
            session_id: None,
            path: None,
            dates: DateRange::default(),
            context: 0,
            limit: 0,
            max_groups: Some(0),
            max_examples_per_group: Some(0),
            min_matches: Some(2),
            phrase_min_words: Some(2),
            phrase_max_words: Some(4),
            format: OutputFormat::Json,
        };
        let filters = repeat_filters(&db, &args, Some(Role::User)).unwrap();
        let hits = db.search_attributable_human_messages("", &filters).unwrap();
        assert_eq!(
            hits.iter().map(|hit| hit.seq).collect::<Vec<_>>(),
            [0, 1, 6, 7]
        );
        assert_eq!(hits[2].content, "mixed human phrase");
        assert_eq!(hits[3].content, "mixed human phrase");
        let groups = repeat_phrase_groups(&hits, 0, 2, 2, 4, 0, 0);
        assert!(groups
            .iter()
            .any(|group| group.repeat == "durable human phrase"));
        assert!(groups
            .iter()
            .any(|group| group.repeat == "mixed human phrase"));
        assert!(!groups.iter().any(|group| group.repeat.contains("agent")));
        assert!(!groups.iter().any(|group| group.repeat.contains("harness")));
        assert!(!groups.iter().any(|group| group.repeat.contains("mirror")));
        assert!(!groups.iter().any(|group| group.repeat.contains("unknown")));
        assert!(!groups
            .iter()
            .any(|group| group.repeat.contains("generated")));
    }

    /// `--regex PATTERN` must narrow the mined messages by PATTERN. The pattern was passed as
    /// `""` after the match modes were unified, so every message was mined and the documented
    /// `--regex -- ^/\S+` example could not work.
    #[test]
    fn repeats_apply_the_regex_query_to_the_mined_messages() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.execute_batch_for_test(
            "insert into sessions (
                     id, provider, provider_session_id, preview_text,
                     source_path, parse_version, discovery_source
                 ) values ('claude:repeat-regex', 'claude', 'repeat-regex', '',
                           '/repeat-regex', 'v1', 'test');
                 insert into messages (
                     id, session_id, provider, seq, role, kind, content, authorship, record_relation
                 ) values
                   (201, 'claude:repeat-regex', 'claude', 0, 'user', 'conversation',
                    '/plan the login fix', 'human', 'original'),
                   (202, 'claude:repeat-regex', 'claude', 1, 'user', 'conversation',
                    '/plan the login fix', 'human', 'original'),
                   (203, 'claude:repeat-regex', 'claude', 2, 'user', 'conversation',
                    'please rerun the login fix', 'human', 'original');",
        )
        .unwrap();
        let regex_args = |pattern: &str| RepeatsArgs {
            query: Some(pattern.to_string()),
            regex: true,
            role: None,
            provider: None,
            session_id: None,
            path: None,
            dates: DateRange::default(),
            context: 0,
            limit: 0,
            max_groups: Some(0),
            max_examples_per_group: Some(0),
            min_matches: Some(2),
            phrase_min_words: Some(2),
            phrase_max_words: Some(4),
            format: OutputFormat::Json,
        };
        let slash = regex_args(r"^/\S+");
        assert_eq!(repeat_query(&slash), r"^/\S+");
        let filters = repeat_filters(&db, &slash, Some(Role::User)).unwrap();
        assert_eq!(filters.match_mode, MessageSearchMode::Regex);
        let hits = db
            .search_attributable_human_messages(repeat_query(&slash), &filters)
            .unwrap();
        assert_eq!(
            hits.iter().map(|hit| hit.seq).collect::<Vec<_>>(),
            [0, 1],
            "only the slash-command messages match ^/\\S+"
        );

        let none = regex_args("zzz_no_match");
        let filters = repeat_filters(&db, &none, Some(Role::User)).unwrap();
        assert!(db
            .search_attributable_human_messages(repeat_query(&none), &filters)
            .unwrap()
            .is_empty());

        let invalid = regex_args("[unclosed");
        let filters = repeat_filters(&db, &invalid, Some(Role::User)).unwrap();
        let error = db
            .search_attributable_human_messages(repeat_query(&invalid), &filters)
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid regex"), "{error}");
    }

    #[test]
    fn repeat_phrase_groups_find_repeated_phrases_without_builtins() {
        let hits = vec![
            hit(
                10,
                Role::User,
                "remember to avoid magic values and make the timeout configurable",
            ),
            hit(
                20,
                Role::User,
                "please avoid magic values and keep the limit configurable",
            ),
            hit(
                30,
                Role::User,
                "please reuse the existing helper instead of duplicate code",
            ),
        ];

        let groups = repeat_phrase_groups(&hits, 3, 2, 2, 4, 0, 0);

        let avoid_magic_values = groups
            .iter()
            .find(|group| group.repeat == "avoid magic values")
            .expect("maximal repeated phrase is discovered from the data");
        assert!(groups.iter().all(|group| group.repeat != "avoid magic"));
        assert!(groups.iter().all(|group| group.repeat != "magic values"));
        assert_eq!(avoid_magic_values.matches, 2);
        assert_eq!(avoid_magic_values.sessions, 1);
        assert_eq!(
            avoid_magic_values
                .examples
                .iter()
                .map(|m| m.seq)
                .collect::<Vec<_>>(),
            vec![10, 20]
        );
        assert_eq!(
            avoid_magic_values.examples[0].context_command,
            "aise messages get claude:test --seq 10 --context 3"
        );
    }

    #[test]
    fn repeat_phrase_groups_keep_only_maximal_phrase_for_equal_support() {
        let hits = vec![
            hit(10, Role::User, "avoid magic values everywhere"),
            hit(20, Role::User, "avoid magic values everywhere"),
        ];

        let groups = repeat_phrase_groups(&hits, 0, 2, 2, 4, 0, 0);

        assert_eq!(
            groups.len(),
            1,
            "contained fragments add no distinct evidence"
        );
        assert_eq!(groups[0].repeat, "avoid magic values everywhere");
        assert_eq!(groups[0].matches, 2);
    }

    #[test]
    fn repeat_phrase_mining_uses_user_request_body_not_harness_metadata() {
        let content = "<USER_REQUEST>\navoid magic values and keep settings configurable\n</USER_REQUEST><ADDITIONAL_METADATA>\nThe current local time is 2026-06-30T06:49:05Z.\n</ADDITIONAL_METADATA>";

        let phrases = phrases_in_message(content, 2, 4);

        assert!(phrases.contains("magic values"));
        assert!(phrases.contains("avoid magic values"));
        assert!(!phrases.contains("current local time"));
        assert!(!phrases.contains("additional metadata"));

        let member = RepeatGroupExample::from_hit(&hit(1, Role::User, content), 0);
        assert!(member.preview.starts_with("avoid magic values"));
        assert!(!member.preview.contains("USER_REQUEST"));
    }

    #[test]
    fn repeat_phrase_mining_skips_numeric_noise() {
        let phrases = phrases_in_message("local time is 2026 06 30 and version v4", 2, 4);

        assert!(!phrases.iter().any(|phrase| phrase.contains("2026")));
        assert!(!phrases.iter().any(|phrase| phrase.contains("v4")));
    }

    #[test]
    fn repeat_group_examples_are_bounded_without_losing_aggregate_counts() {
        let hits = vec![
            hit(10, Role::User, "avoid magic values everywhere"),
            hit(20, Role::User, "avoid magic values everywhere"),
            hit(30, Role::User, "avoid magic values everywhere"),
        ];

        let groups = repeat_phrase_groups(&hits, 0, 2, 2, 4, 1, 2);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].matches, 3);
        assert_eq!(groups[0].sessions, 1);
        assert_eq!(groups[0].examples.len(), 2);
        assert_eq!(
            groups[0]
                .examples
                .iter()
                .map(|example| example.seq)
                .collect::<Vec<_>>(),
            vec![10, 20]
        );
        let serialized = serde_json::to_value(&groups).unwrap();
        assert!(serialized[0].get("examples").is_some());
        assert!(serialized[0]["examples"][0].get("matched_text").is_none());

        let all_examples = repeat_phrase_groups(&hits, 0, 2, 2, 4, 1, 0);
        assert_eq!(all_examples[0].examples.len(), 3);
    }

    #[test]
    fn repeat_group_limit_applies_before_example_materialization() {
        let hits = vec![
            hit(10, Role::User, "alpha bravo first"),
            hit(20, Role::User, "alpha bravo second"),
            hit(30, Role::User, "charlie delta third"),
            hit(40, Role::User, "charlie delta fourth"),
        ];

        let groups = repeat_phrase_groups(&hits, 0, 2, 2, 2, 1, 1);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].examples.len(), 1);
    }

    // The value is `Regex::find(..).as_str()` — the matched substring, not the rule that matched.
    // The old `pattern` header and `matched_pattern` field named the rule INPUT while carrying the
    // OUTPUT, so a reader comparing the column against their config would never find it. Assert
    // the absence of the misleading spelling too: a presence-only test lets it return alongside.
    #[test]
    fn correction_column_names_the_matched_text_not_the_rule() {
        let headers = MessageClassificationMatch::headers();
        assert!(
            headers.contains(&"match"),
            "the column must name the matched text: {headers:?}"
        );
        assert!(
            !headers.contains(&"pattern"),
            "`pattern` names the rule input and must not return: {headers:?}"
        );
    }

    #[test]
    fn categories_match_expected_keywords() {
        assert_eq!(
            categorize("you forgot to run the tests").as_deref(),
            Some("skip_step")
        );
        assert_eq!(
            categorize("that is actually wrong").as_deref(),
            Some("misunderstanding")
        );
        assert_eq!(
            categorize("you must also add a test").as_deref(),
            Some("incomplete")
        );
    }

    #[test]
    fn first_match_wins_and_other_is_last() {
        // "stop" alone -> other (catch-all, last).
        assert_eq!(categorize("stop").as_deref(), Some("other"));
        // A message with a specific signal is categorized before falling to other.
        assert_eq!(
            categorize("you removed the function").as_deref(),
            Some("regression")
        );
        // No correction signal -> no category.
        assert_eq!(categorize("looks great, thanks"), None);
    }

    #[test]
    fn stop_matches_imperative_corrections_not_workflow_phrasing() {
        // Genuine imperative-stop corrections are kept.
        assert_eq!(categorize("stop").as_deref(), Some("other"));
        assert_eq!(
            categorize("stop falsely marking goals as complete").as_deref(),
            Some("other")
        );
        assert_eq!(
            categorize("stop incrementing the version so frequently").as_deref(),
            Some("other")
        );
        assert_eq!(
            categorize("please stop, that approach is off").as_deref(),
            Some("other")
        );
        // Benign workflow phrasings must NOT be flagged as corrections: a bare
        // \bstop\b matched all of these (test fixtures, checkpoint instructions).
        assert_eq!(
            categorize("Run this bash command once and stop: grep hi /tmp/x"),
            None
        );
        assert_eq!(
            categorize("at your next progress point commit and then stop"),
            None
        );
        assert_eq!(
            categorize("keep going dont stop for trivial questions"),
            None
        );
        assert_eq!(
            categorize("a clear way to start and stop all the tooling"),
            None
        );
    }

    #[test]
    fn default_patterns_are_precise_on_labeled_corpus() {
        // True positives: real corrections (user correcting the assistant) must be flagged.
        let positives: &[(&str, &str)] = &[
            ("you deleted my helper function", "regression"),
            ("you broke the build", "regression"),
            ("that reverted my changes", "regression"),
            ("you forgot to update the test", "skip_step"),
            ("you missed the edge case", "skip_step"),
            ("don't forget the migration", "skip_step"),
            ("that's wrong, the API returns a list", "misunderstanding"),
            ("no, that's not what I asked", "misunderstanding"),
            ("you're wrong about the types", "misunderstanding"),
            ("you also need to handle the error case", "incomplete"),
            ("that's not finished, the tests still fail", "incomplete"),
            ("stop changing the config", "other"),
            ("please stop", "other"),
        ];
        for (text, want) in positives {
            assert_eq!(
                categorize(text).as_deref(),
                Some(*want),
                "true positive: {text:?}"
            );
        }
        // True negatives: benign developer phrasing must NOT be flagged as a correction.
        let negatives: &[&str] = &[
            "let's revert to the design doc approach",
            "the rollback procedure is documented in the README",
            "this broke down into three subtasks",
            "I lost track of which branch we're on",
            "actually, let's use a HashMap here",
            "wait, let me check the logs first",
            "what could go wrong here?",
            "no thanks, that's all for now",
            "we should have access to the API",
            "run the command once and stop",
            "the incorrect assumption was already fixed",
        ];
        for text in negatives {
            assert_eq!(
                categorize(text),
                None,
                "true negative must not match: {text:?}"
            );
        }
    }

    #[test]
    fn planning_commands_config_compiles_to_filters() {
        let mut config = Config::default();
        // Default: no planning_commands → empty filter (count every slash command).
        assert!(compile_planning_filters(&config, &[]).unwrap().is_empty());
        // Configured: each entry compiles to a case-insensitive regex filter.
        config.analytics.planning_commands = vec!["^/cmd-a".to_string(), "review".to_string()];
        let filters = compile_planning_filters(&config, &["^/cmd-b$".to_string()]).unwrap();
        assert_eq!(filters.len(), 3);
        assert!(filters[0].is_match("/cmd-a"));
        assert!(filters[0].is_match("/cmd-a-review"));
        assert!(filters[1].is_match("/REVIEW"));
        assert!(filters[2].is_match("/cmd-b"));
        assert!(!filters[2].is_match("/cmd-b-extra"));
    }
}
