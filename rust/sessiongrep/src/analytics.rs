//! Analytics: corrections, recurring issue mining, planning-command frequency, and stats.
//!
//! Correction categories are narrowed to second-person/imperative forms for precision
//! (see `default_correction_patterns`);
//! order matters (first match wins, so `other` is last). Nothing is hard-coded as a fixed
//! list: `analytics.correction_patterns` / `analytics.repeat_patterns`
//! (`"CATEGORY:REGEX"`, repeatable, same-category ORed) replace their built-ins, and
//! `analytics.planning_commands`
//! (regexes over the slash-command token) optionally restricts which commands `planning`
//! counts (empty = all). Both are plain TOML config (the repo's config mechanism); the
//! built-in defaults are the documented fallback, not a fixed policy.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, Write};

use anyhow::{anyhow, bail, Result};
use clap::Args;
use regex::Regex;
use serde::Serialize;

use crate::config::Config;
use crate::dates::DateRange;
use crate::db::Db;
use crate::models::{CorrectionMatch, MessageFilters, MessageHit, PlanningCount, Provider, Role};
use crate::render::{render, OutputFormat, Row};
use crate::util::truncate_for_display;

const TABLE_CONTENT_CHARS: usize = 100;

/// Built-in correction categories, NARROWED to second-person / imperative / demonstrative
/// forms for precision: bare single words (`lost`, `revert`, `rollback`, `broke`, `wrong`,
/// `actually`, `wait,`, `mistake`) fire on benign developer phrasing ("let's revert to the
/// design doc", "actually, use a HashMap", "this broke down into subtasks"). A correction
/// addresses the assistant, so the defaults key on `you …` / `that|this|it …` / explicit
/// corrective phrases. First match wins (`other` is last). Set
/// `analytics.correction_patterns` in config to fully replace these with any custom set;
/// see [`compile_patterns`].
fn default_correction_patterns() -> Vec<(&'static str, Vec<&'static str>)> {
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

/// Built-in recurring issue categories for `repeats`. These are workflow-improvement seeds, not
/// a fixed policy: `[analytics].repeat_patterns` can replace them with project-specific categories.
fn default_repeat_patterns() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        (
            "magic_values",
            vec![
                r"\bmagic (values?|numbers?|constants?)\b",
                r"\bhard-?coded (values?|numbers?|constants?|thresholds?|limits?|timeouts?)\b",
                r"\barbitrary (values?|numbers?|constants?|thresholds?|limits?|timeouts?)\b",
                r"\bavoid magic\b",
                r"\bconfig(?:urable| support)?\b.*\b(values?|numbers?|constants?|thresholds?|limits?|timeouts?)\b",
                r"\b(values?|numbers?|constants?|thresholds?|limits?|timeouts?)\b.*\bconfig(?:urable| support)?\b",
            ],
        ),
        (
            "reuse_dry",
            vec![
                r"\bDRY\b",
                r"\bcode reuse\b",
                r"\breuse (existing|shared|common|the) (code|helper|helpers|api|function|functions|path|paths)\b",
                r"\bsemantic duplicates?\b",
                r"\bduplicate code\b",
                r"\bconsolidat(e|ion)\b",
            ],
        ),
        (
            "verification",
            vec![
                r"\bTDD\b",
                r"\btest(s|ing)?\b.*\b(first|coverage|regression|verify|verification)\b",
                r"\bverify\b.*\b(actual|real|diff|code|behavior|output)\b",
                r"\bdogfood(ing)?\b",
                r"\bregression(s)?\b",
            ],
        ),
        (
            "api_clarity",
            vec![
                r"\beasy to use correctly\b",
                r"\bhard to use incorrectly\b",
                r"\b(clear|clean|streamlined|ambiguous|ambiguity)\b.*\b(api|cli|mcp|ux|interface|param|parameter)\b",
                r"\b(api|cli|mcp|ux|interface|param|parameter)\b.*\b(clear|clean|streamlined|ambiguous|ambiguity)\b",
            ],
        ),
        (
            "upstream_review",
            vec![
                r"\bupstream\b",
                r"\bmaintainer\b",
                r"\bPR\b",
                r"\bpull request\b",
                r"\bactual (code )?diff\b",
                r"\bcheck (the )?diff\b",
            ],
        ),
        (
            "git_safety",
            vec![
                r"\bstash\b",
                r"\bcommit\b.*\b(progress|clear|subject|body|message)\b",
                r"\bdon'?t push\b",
                r"\bbackup\b",
                r"\brecoverable\b",
                r"\bnot (lose|lost) (work|changes|history)\b",
            ],
        ),
        (
            "context_recovery",
            vec![
                r"\bsessiongrep\b",
                r"\bsession history\b",
                r"\bCLAUDE\.md\b",
                r"\bAGENTS\.md\b",
                r"\bMCP\b.*\b(context|history|search)\b",
                r"\b(context|history|search)\b.*\bMCP\b",
            ],
        ),
    ]
}

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

/// Compile the active correction patterns: config override (`CATEGORY:REGEX`,
/// same-category ORed, first-seen order) when present, else the built-ins.
fn compile_patterns(config: &Config) -> Result<Vec<(String, Regex)>> {
    compile_category_patterns(
        &config.analytics.correction_patterns,
        default_correction_patterns(),
        "correction",
    )
}

fn compile_repeat_patterns(config: &Config) -> Result<Vec<(String, Regex)>> {
    compile_category_patterns(
        &config.analytics.repeat_patterns,
        default_repeat_patterns(),
        "repeat",
    )
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
    /// Restrict to sessions whose cwd or repo root starts with this path prefix.
    #[arg(long)]
    pub path: Option<String>,
    #[command(flatten)]
    pub dates: DateRange,
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
    /// Restrict to sessions whose cwd or repo root starts with this path prefix.
    #[arg(long)]
    pub path: Option<String>,
    #[command(flatten)]
    pub dates: DateRange,
    /// Max distinct commands. 0 = unlimited.
    #[arg(long, default_value_t = 0)]
    pub limit: usize,
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct StatsArgs {
    /// Restrict to sessions whose cwd or repo root starts with this path prefix.
    #[arg(long)]
    pub path: Option<String>,
    #[command(flatten)]
    pub dates: DateRange,
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

/// Build a [`MessageFilters`] from a session scope, a path prefix, a [`DateRange`], and a
/// limit. `path` is normalized to an absolute prefix (`~`/relative resolved) by
/// [`crate::util::normalize_path_prefix`], matching the session- and message-search `--path`.
fn filters_from(
    session: &Option<String>,
    path: &Option<String>,
    dates: &DateRange,
    limit: usize,
) -> Result<MessageFilters> {
    let (since, until) = dates.resolve_now()?;
    Ok(MessageFilters {
        session: session.clone(),
        path_prefix: path.as_deref().map(crate::util::normalize_path_prefix),
        since,
        until,
        limit,
        ..Default::default()
    })
}

pub fn run_corrections(db: &Db, config: &Config, args: &CorrectionsArgs) -> Result<()> {
    let patterns = compile_patterns(config)?;
    let filters = filters_from(&args.session, &args.path, &args.dates, args.limit)?;
    // Scan the user-message slice directly — `find_corrections` filters `role='user'` (a small,
    // selective subset), so the trigram prefilter would only add cost (see its doc comment).
    let hits = db.find_corrections(&patterns, &filters)?;
    emit(&hits, args.format)
}

/// Compile the optional `analytics.planning_commands` config — regexes matched against the
/// slash-command token (e.g. `ar:plannew`). Empty (the default) counts every slash command.
fn compile_planning_filters(config: &Config) -> Result<Vec<Regex>> {
    config
        .analytics
        .planning_commands
        .iter()
        .map(|p| {
            Regex::new(&format!("(?i){p}"))
                .map_err(|err| anyhow!("invalid planning_commands regex '{p}': {err}"))
        })
        .collect()
}

pub fn run_planning(db: &Db, config: &Config, args: &PlanningArgs) -> Result<()> {
    let filters = filters_from(&args.session, &args.path, &args.dates, args.limit)?;
    let command_filters = compile_planning_filters(config)?;
    let counts = db.planning_usage(&filters, &command_filters)?;
    emit(&counts, args.format)
}

pub fn run_stats(db: &Db, args: &StatsArgs) -> Result<()> {
    let filters = filters_from(&None, &args.path, &args.dates, 0)?;
    let rows: Vec<RoleStat> = db
        .message_role_counts(&filters)?
        .into_iter()
        .map(|(role, count)| RoleStat { role, count })
        .collect();
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
    /// Read the substring (3-gram) index instead of word tokens (substring statistics).
    #[arg(long)]
    pub trigram: bool,
    /// Max terms (most frequent first). 0 = unlimited.
    #[arg(long, default_value_t = 50)]
    pub limit: usize,
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

pub fn run_vocab(db: &Db, args: &VocabArgs) -> Result<()> {
    let rows: Vec<VocabRow> = db
        .vocabulary(args.trigram, args.limit)?
        .into_iter()
        .map(|(term, docs, count)| VocabRow { term, docs, count })
        .collect();
    emit(&rows, args.format)
}

/// A near-duplicate message pair (rendered by `repeats --similarity`).
#[derive(Debug, Serialize)]
struct RepeatPair {
    similarity: f64,
    session_a: String,
    seq_a: i64,
    session_b: String,
    seq_b: i64,
    anchor_preview_a: String,
    anchor_preview_b: String,
    comparison_preview_a: String,
    comparison_preview_b: String,
    context_command_a: String,
    context_command_b: String,
}

impl Row for RepeatPair {
    fn headers() -> &'static [&'static str] {
        &[
            "similarity",
            "session_a",
            "seq_a",
            "session_b",
            "seq_b",
            "comparison_preview",
        ]
    }
    fn cells(&self) -> Vec<String> {
        vec![
            format!("{:.3}", self.similarity),
            self.session_a.clone(),
            self.seq_a.to_string(),
            self.session_b.clone(),
            self.seq_b.to_string(),
            truncate_for_display(&self.comparison_preview_a, 80),
        ]
    }
}

#[derive(Debug, Clone, Serialize)]
struct RepeatMember {
    session_id: String,
    seq: i64,
    role: String,
    anchor_preview: String,
    comparison_preview: String,
    context_command: String,
}

impl RepeatMember {
    fn from_hit(hit: &MessageHit, comparison_text: &str, context: i64) -> Self {
        Self {
            session_id: hit.session_id.clone(),
            seq: hit.seq,
            role: hit.role.as_str().to_string(),
            anchor_preview: truncate_for_display(&hit.content, TABLE_CONTENT_CHARS),
            comparison_preview: truncate_for_display(comparison_text, TABLE_CONTENT_CHARS),
            context_command: context_command(&hit.session_id, hit.seq, context),
        }
    }
}

#[derive(Debug, Serialize)]
struct RepeatSimilarityGroup {
    group: usize,
    size: usize,
    best_similarity: f64,
    members: Vec<RepeatMember>,
}

impl Row for RepeatSimilarityGroup {
    fn headers() -> &'static [&'static str] {
        &[
            "group",
            "size",
            "best_similarity",
            "members",
            "comparison_preview",
        ]
    }
    fn cells(&self) -> Vec<String> {
        let member_ids = self
            .members
            .iter()
            .map(|m| format!("{}:{}", m.session_id, m.seq))
            .collect::<Vec<_>>()
            .join(", ");
        let preview = self
            .members
            .first()
            .map(|m| m.comparison_preview.clone())
            .unwrap_or_default();
        vec![
            self.group.to_string(),
            self.size.to_string(),
            format!("{:.3}", self.best_similarity),
            truncate_for_display(&member_ids, TABLE_CONTENT_CHARS),
            preview,
        ]
    }
}

#[derive(Debug, Clone, Serialize)]
struct RepeatIssueMember {
    session_id: String,
    seq: i64,
    ts: Option<String>,
    matched_text: String,
    preview: String,
    context_command: String,
}

impl RepeatIssueMember {
    fn from_hit(hit: &MessageHit, matched_text: String, context: i64) -> Self {
        Self {
            session_id: hit.session_id.clone(),
            seq: hit.seq,
            ts: hit.ts.map(|ts| ts.to_rfc3339()),
            matched_text,
            preview: truncate_for_display(&hit.content, TABLE_CONTENT_CHARS),
            context_command: context_command(&hit.session_id, hit.seq, context),
        }
    }
}

#[derive(Debug, Serialize)]
struct RepeatIssueGroup {
    category: String,
    matches: usize,
    sessions: usize,
    members: Vec<RepeatIssueMember>,
}

impl Row for RepeatIssueGroup {
    fn headers() -> &'static [&'static str] {
        &["category", "matches", "sessions", "examples", "preview"]
    }
    fn cells(&self) -> Vec<String> {
        let examples = self
            .members
            .iter()
            .take(3)
            .map(|m| format!("{}:{}", m.session_id, m.seq))
            .collect::<Vec<_>>()
            .join(", ");
        let preview = self
            .members
            .first()
            .map(|m| m.preview.clone())
            .unwrap_or_default();
        vec![
            self.category.clone(),
            self.matches.to_string(),
            self.sessions.to_string(),
            truncate_for_display(&examples, TABLE_CONTENT_CHARS),
            preview,
        ]
    }
}

#[derive(Debug, Args)]
pub struct RepeatsArgs {
    /// Optional literal query to narrow candidate messages before repeat mining.
    pub query: Option<String>,
    /// Optional Rust regex to narrow candidate messages before repeat mining.
    #[arg(long)]
    pub regex: Option<String>,
    /// Filter by role (user|assistant|tool|slash|compaction).
    #[arg(long = "type", value_enum)]
    pub role: Option<Role>,
    /// Restrict to one harness.
    #[arg(long, value_enum)]
    pub provider: Option<Provider>,
    /// Scope to one session id (substring match).
    #[arg(long)]
    pub session: Option<String>,
    /// Restrict to sessions whose cwd or repo root starts with this path prefix.
    #[arg(long)]
    pub path: Option<String>,
    #[command(flatten)]
    pub dates: DateRange,
    /// Use MinHash near-duplicate comparison instead of the default recurring-issue categories.
    #[arg(long)]
    pub similarity: bool,
    /// Minimum word-3-gram Jaccard similarity for --similarity results.
    #[arg(long, default_value_t = 0.8)]
    pub threshold: f64,
    /// Neighboring messages before/after each match. Issue mode uses this in context commands;
    /// --similarity also includes those turns in the comparison text.
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(i64).range(0..))]
    pub context: i64,
    /// Group connected similar pairs into repeated-pattern clusters. Issue mode is always grouped.
    #[arg(long)]
    pub groups: bool,
    /// Max candidate messages to scan (0 = all).
    #[arg(long, default_value_t = 0)]
    pub limit: usize,
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

pub fn run_repeats(db: &Db, config: &Config, args: &RepeatsArgs) -> Result<()> {
    if args.query.is_some() && args.regex.is_some() {
        bail!("pass either QUERY or --regex, not both");
    }
    if args.similarity {
        run_repeats_similarity(db, args)
    } else {
        run_repeats_issues(db, config, args)
    }
}

fn repeat_filters(args: &RepeatsArgs, default_role: Option<Role>) -> Result<MessageFilters> {
    let (since, until) = args.dates.resolve_now()?;
    Ok(MessageFilters {
        role: args.role.or(default_role),
        provider: args.provider,
        session: args.session.clone(),
        path_prefix: args.path.as_deref().map(crate::util::normalize_path_prefix),
        since,
        until,
        regex: args.regex.clone(),
        limit: args.limit,
        ..Default::default()
    })
}

fn run_repeats_issues(db: &Db, config: &Config, args: &RepeatsArgs) -> Result<()> {
    let patterns = compile_repeat_patterns(config)?;
    let filters = repeat_filters(args, Some(Role::User))?;
    let hits = db.search_messages(args.query.as_deref().unwrap_or(""), &filters)?;
    let rows = repeat_issue_groups(&hits, &patterns, args.context);
    emit(&rows, args.format)
}

fn run_repeats_similarity(db: &Db, args: &RepeatsArgs) -> Result<()> {
    let filters = repeat_filters(args, None)?;
    let hits = db.search_messages(args.query.as_deref().unwrap_or(""), &filters)?;
    let contents = comparison_texts(db, &hits, args.context)?;
    let pairs = crate::minhash::near_duplicate_pairs(&contents, args.threshold);
    if args.groups {
        let rows = repeat_similarity_groups(&hits, &contents, &pairs, args.context);
        return emit(&rows, args.format);
    }
    let rows: Vec<RepeatPair> = pairs
        .into_iter()
        .map(|(i, j, similarity)| RepeatPair {
            similarity,
            session_a: hits[i].session_id.clone(),
            seq_a: hits[i].seq,
            session_b: hits[j].session_id.clone(),
            seq_b: hits[j].seq,
            anchor_preview_a: truncate_for_display(&hits[i].content, TABLE_CONTENT_CHARS),
            anchor_preview_b: truncate_for_display(&hits[j].content, TABLE_CONTENT_CHARS),
            comparison_preview_a: truncate_for_display(&contents[i], TABLE_CONTENT_CHARS),
            comparison_preview_b: truncate_for_display(&contents[j], TABLE_CONTENT_CHARS),
            context_command_a: context_command(&hits[i].session_id, hits[i].seq, args.context),
            context_command_b: context_command(&hits[j].session_id, hits[j].seq, args.context),
        })
        .collect();
    emit(&rows, args.format)
}

fn repeat_issue_groups(
    hits: &[MessageHit],
    patterns: &[(String, Regex)],
    context: i64,
) -> Vec<RepeatIssueGroup> {
    let mut grouped: BTreeMap<String, Vec<RepeatIssueMember>> = BTreeMap::new();
    for hit in hits {
        let Some((category, matched_text)) = patterns.iter().find_map(|(category, re)| {
            re.find(&hit.content)
                .map(|m| (category.clone(), m.as_str().to_string()))
        }) else {
            continue;
        };
        grouped
            .entry(category)
            .or_default()
            .push(RepeatIssueMember::from_hit(hit, matched_text, context));
    }

    let mut rows: Vec<RepeatIssueGroup> = grouped
        .into_iter()
        .map(|(category, members)| {
            let sessions = members
                .iter()
                .map(|m| m.session_id.as_str())
                .collect::<BTreeSet<_>>()
                .len();
            RepeatIssueGroup {
                category,
                matches: members.len(),
                sessions,
                members,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        b.matches
            .cmp(&a.matches)
            .then_with(|| b.sessions.cmp(&a.sessions))
            .then_with(|| a.category.cmp(&b.category))
    });
    rows
}

fn context_command(session_id: &str, seq: i64, context: i64) -> String {
    format!("sessiongrep messages get {session_id} --seq {seq} --context {context}")
}

fn comparison_texts(db: &Db, hits: &[MessageHit], context: i64) -> Result<Vec<String>> {
    if context == 0 {
        return Ok(hits.iter().map(|h| h.content.clone()).collect());
    }
    hits.iter()
        .map(|hit| {
            let rows = db.message_context(&hit.session_id, hit.seq, context, context)?;
            Ok(sequence_text(&rows))
        })
        .collect()
}

fn sequence_text(rows: &[MessageHit]) -> String {
    rows.iter()
        .map(|hit| format!("{}: {}", hit.role.as_str(), hit.content))
        .collect::<Vec<_>>()
        .join("\n")
}

fn repeat_similarity_groups(
    hits: &[MessageHit],
    contents: &[String],
    pairs: &[(usize, usize, f64)],
    context: i64,
) -> Vec<RepeatSimilarityGroup> {
    let mut parent: Vec<usize> = (0..hits.len()).collect();
    for &(a, b, _) in pairs {
        union(&mut parent, a, b);
    }

    let mut best: HashMap<usize, f64> = HashMap::new();
    let mut grouped: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for &(a, b, similarity) in pairs {
        let root = find(&mut parent, a);
        grouped.entry(root).or_default().extend([a, b]);
        best.entry(root)
            .and_modify(|value| *value = value.max(similarity))
            .or_insert(similarity);
    }

    let mut rows = Vec::new();
    for (root, mut indices) in grouped {
        indices.sort_unstable();
        indices.dedup();
        if indices.len() < 2 {
            continue;
        }
        let members = indices
            .iter()
            .map(|&index| RepeatMember::from_hit(&hits[index], &contents[index], context))
            .collect();
        rows.push(RepeatSimilarityGroup {
            group: root,
            size: indices.len(),
            best_similarity: best.get(&root).copied().unwrap_or(0.0),
            members,
        });
    }
    rows.sort_by(|a, b| {
        b.size
            .cmp(&a.size)
            .then_with(|| {
                b.best_similarity
                    .partial_cmp(&a.best_similarity)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.group.cmp(&b.group))
    });
    for (ordinal, row) in rows.iter_mut().enumerate() {
        row.group = ordinal + 1;
    }
    rows
}

fn find(parent: &mut [usize], x: usize) -> usize {
    if parent[x] != x {
        parent[x] = find(parent, parent[x]);
    }
    parent[x]
}

fn union(parent: &mut [usize], a: usize, b: usize) {
    let ra = find(parent, a);
    let rb = find(parent, b);
    if ra != rb {
        parent[rb] = ra;
    }
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

    fn hit(seq: i64, role: Role, content: &str) -> MessageHit {
        MessageHit {
            session_id: "claude:test".to_string(),
            provider: Provider::Claude,
            seq,
            role,
            ts: None,
            tool_name: None,
            content: content.to_string(),
        }
    }

    #[test]
    fn sequence_text_preserves_roles_and_order() {
        let rows = vec![
            hit(1, Role::Assistant, "I changed the wrong file."),
            hit(2, Role::User, "you changed the wrong file"),
            hit(3, Role::Assistant, "I'll fix that."),
        ];

        assert_eq!(
            sequence_text(&rows),
            "assistant: I changed the wrong file.\nuser: you changed the wrong file\nassistant: I'll fix that."
        );
    }

    #[test]
    fn repeat_similarity_groups_are_transitive_and_sorted_by_size() {
        let hits = vec![
            hit(0, Role::User, "alpha"),
            hit(1, Role::User, "alpha again"),
            hit(2, Role::User, "alpha later"),
            hit(3, Role::User, "beta"),
            hit(4, Role::User, "beta again"),
        ];
        let contents: Vec<String> = hits.iter().map(|h| h.content.clone()).collect();
        let pairs = vec![(0, 1, 0.95), (1, 2, 0.90), (3, 4, 0.99)];

        let groups = repeat_similarity_groups(&hits, &contents, &pairs, 2);

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].size, 3);
        assert_eq!(groups[0].best_similarity, 0.95);
        assert_eq!(
            groups[0].members.iter().map(|m| m.seq).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(
            groups[0].members[0].context_command,
            "sessiongrep messages get claude:test --seq 0 --context 2"
        );
        assert_eq!(groups[1].size, 2);
    }

    #[test]
    fn repeat_issue_groups_find_magic_values_across_different_wording() {
        let patterns = compile_repeat_patterns(&Config::default()).unwrap();
        let hits = vec![
            hit(
                10,
                Role::User,
                "remember to avoid magic values and make the timeout configurable",
            ),
            hit(
                20,
                Role::User,
                "that arbitrary threshold should live in config, not inline code",
            ),
            hit(
                30,
                Role::User,
                "please reuse the existing helper instead of duplicate code",
            ),
        ];

        let groups = repeat_issue_groups(&hits, &patterns, 3);

        let magic = groups
            .iter()
            .find(|group| group.category == "magic_values")
            .expect("magic-value variants are grouped together");
        assert_eq!(magic.matches, 2);
        assert_eq!(magic.sessions, 1);
        assert_eq!(
            magic.members.iter().map(|m| m.seq).collect::<Vec<_>>(),
            vec![10, 20]
        );
        assert_eq!(
            magic.members[0].context_command,
            "sessiongrep messages get claude:test --seq 10 --context 3"
        );
    }

    #[test]
    fn repeat_patterns_config_replaces_builtins() {
        let mut config = Config::default();
        config.analytics.repeat_patterns =
            vec!["custom_issue:bespoke recurring problem".to_string()];

        let patterns = compile_repeat_patterns(&config).unwrap();
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].0, "custom_issue");
        assert!(patterns[0]
            .1
            .is_match("this is a bespoke recurring problem"));
        assert!(!patterns[0].1.is_match("magic values"));
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
        assert!(compile_planning_filters(&config).unwrap().is_empty());
        // Configured: each entry compiles to a case-insensitive regex filter.
        config.analytics.planning_commands = vec!["ar:plan".to_string(), "review".to_string()];
        let filters = compile_planning_filters(&config).unwrap();
        assert_eq!(filters.len(), 2);
        assert!(filters[0].is_match("ar:plannew"));
        assert!(filters[1].is_match("REVIEW"));
    }

    #[test]
    fn config_override_replaces_builtins() {
        let mut config = Config::default();
        config.analytics.correction_patterns =
            vec!["oops:nono".to_string(), "oops:whoops".to_string()];
        let compiled = compile_patterns(&config).unwrap();
        assert_eq!(compiled.len(), 1, "same category is ORed into one entry");
        assert_eq!(compiled[0].0, "oops");
        assert!(compiled[0].1.is_match("whoops that broke"));
        assert!(!compiled[0].1.is_match("you forgot"));
    }
}
