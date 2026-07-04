//! Analytics: corrections, data-driven repeat mining, planning-command frequency, and stats.
//!
//! Correction categories are narrowed to second-person/imperative forms for precision
//! (see `default_correction_patterns`);
//! order matters (first match wins, so `other` is last). Nothing is hard-coded as a fixed
//! list: `analytics.correction_patterns` replaces the correction built-ins, and
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
const DEFAULT_REPEAT_MIN_MATCHES: usize = 2;
const DEFAULT_REPEAT_PHRASE_MIN_WORDS: usize = 2;
const DEFAULT_REPEAT_PHRASE_MAX_WORDS: usize = 5;
const DEFAULT_REPEAT_MAX_GROUPS: usize = 50;
const USER_REQUEST_START: &str = "<USER_REQUEST>";
const USER_REQUEST_END: &str = "</USER_REQUEST>";

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
    #[arg(long, conflicts_with = "session_id")]
    pub session: Option<String>,
    /// Exact session id or unique prefix. Prefer this when chaining from search output.
    #[arg(long, conflicts_with = "session")]
    pub session_id: Option<String>,
    /// Restrict to one harness.
    #[arg(long, value_enum)]
    pub provider: Option<Provider>,
    /// Restrict to sessions whose cwd or repo root starts with this path prefix.
    #[arg(long)]
    pub path: Option<String>,
    #[command(flatten)]
    pub dates: DateRange,
    /// Max results. 0 = unlimited.
    #[arg(long, default_value_t = 0)]
    pub limit: usize,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct PlanningArgs {
    /// Restrict to one harness.
    #[arg(long, value_enum)]
    pub provider: Option<Provider>,
    /// Scope to one session id (substring match).
    #[arg(long, conflicts_with = "session_id")]
    pub session: Option<String>,
    /// Exact session id or unique prefix. Prefer this when chaining from search output.
    #[arg(long, conflicts_with = "session")]
    pub session_id: Option<String>,
    /// Restrict to sessions whose cwd or repo root starts with this path prefix.
    #[arg(long)]
    pub path: Option<String>,
    #[command(flatten)]
    pub dates: DateRange,
    /// Keep only slash-command tokens matching this case-insensitive regex.
    ///
    /// Regexes match the leading command token; repeat to OR several token regexes.
    #[arg(long = "commands", alias = "command")]
    pub command_patterns: Vec<String>,
    /// Max distinct commands. 0 = unlimited.
    #[arg(long, default_value_t = 0)]
    pub limit: usize,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct StatsArgs {
    /// Restrict to one harness.
    #[arg(long, value_enum)]
    pub provider: Option<Provider>,
    /// Scope to one session id (substring match).
    #[arg(long, conflicts_with = "session_id")]
    pub session: Option<String>,
    /// Exact session id or unique prefix. Prefer this when chaining from search output.
    #[arg(long, conflicts_with = "session")]
    pub session_id: Option<String>,
    /// Restrict to sessions whose cwd or repo root starts with this path prefix.
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
    session: &Option<String>,
    session_id: &Option<String>,
    provider: Option<Provider>,
    path: &Option<String>,
    dates: &DateRange,
    limit: usize,
) -> Result<MessageFilters> {
    let (since, until) = dates.resolve_now()?;
    let exact_session_id = session_id
        .as_deref()
        .map(|id| db.resolve_session_record(id).map(|session| session.id))
        .transpose()?;
    Ok(MessageFilters {
        provider,
        session_id: exact_session_id,
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
    let filters = filters_from(
        db,
        &args.session,
        &args.session_id,
        args.provider,
        &args.path,
        &args.dates,
        args.limit,
    )?;
    // Scan the user-message slice directly — `find_corrections` filters `role='user'` (a small,
    // selective subset), so the trigram prefilter would only add cost (see its doc comment).
    let hits = db.find_corrections(&patterns, &filters)?;
    emit(&hits, args.format)
}

fn compile_planning_regex(label: &str, pattern: &str) -> Result<Regex> {
    Regex::new(&format!("(?i){pattern}"))
        .map_err(|err| anyhow!("invalid {label} regex '{pattern}': {err}"))
}

/// Compile config and CLI slash-command filters. Regexes match the command token including its
/// leading slash; empty input counts every slash command.
fn compile_planning_filters(config: &Config, cli_patterns: &[String]) -> Result<Vec<Regex>> {
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
        &args.session,
        &args.session_id,
        args.provider,
        &args.path,
        &args.dates,
        args.limit,
    )?;
    let command_filters = compile_planning_filters(config, &args.command_patterns)?;
    let counts = db.planning_usage(&filters, &command_filters)?;
    emit(&counts, args.format)
}

pub fn run_stats(db: &Db, args: &StatsArgs) -> Result<()> {
    let filters = filters_from(
        db,
        &args.session,
        &args.session_id,
        args.provider,
        &args.path,
        &args.dates,
        0,
    )?;
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
    /// Output format.
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

#[derive(Debug, Clone, Serialize)]
struct RepeatGroupMember {
    session_id: String,
    seq: i64,
    ts: Option<String>,
    matched_text: String,
    preview: String,
    context_command: String,
}

impl RepeatGroupMember {
    fn from_hit(hit: &MessageHit, matched_text: String, context: i64) -> Self {
        Self {
            session_id: hit.session_id.clone(),
            seq: hit.seq,
            ts: hit.ts.map(|ts| ts.to_rfc3339()),
            matched_text,
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
    members: Vec<RepeatGroupMember>,
}

impl Row for RepeatGroup {
    fn headers() -> &'static [&'static str] {
        &["repeat", "matches", "sessions", "examples", "preview"]
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
    /// add --regex to interpret it as a Rust regex.
    pub query: Option<String>,
    /// Interpret QUERY as a Rust regex before repeat mining.
    #[arg(long)]
    pub regex: bool,
    /// Filter by role: user (non-command prompts), assistant, tool (calls/results),
    /// slash (human-entered commands), or compaction.
    #[arg(long = "type", value_enum)]
    pub role: Option<Role>,
    /// Restrict to one harness.
    #[arg(long, value_enum)]
    pub provider: Option<Provider>,
    /// Scope to one session id (substring match).
    #[arg(long, conflicts_with = "session_id")]
    pub session: Option<String>,
    /// Exact session id or unique prefix. Prefer this when chaining from search output.
    #[arg(long, conflicts_with = "session")]
    pub session_id: Option<String>,
    /// Restrict to sessions whose cwd or repo root starts with this path prefix.
    #[arg(long)]
    pub path: Option<String>,
    #[command(flatten)]
    pub dates: DateRange,
    /// Neighboring messages before/after each match in generated follow-up commands.
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(i64).range(0..))]
    pub context: i64,
    /// Max candidate messages to scan (0 = all).
    #[arg(long, default_value_t = 0)]
    pub limit: usize,
    /// Max repeat groups to output (0 = all).
    #[arg(long, default_value_t = DEFAULT_REPEAT_MAX_GROUPS)]
    pub max_groups: usize,
    /// Minimum messages a discovered phrase must appear in.
    #[arg(long, default_value_t = DEFAULT_REPEAT_MIN_MATCHES)]
    pub min_matches: usize,
    /// Minimum words in a discovered phrase.
    #[arg(long, default_value_t = DEFAULT_REPEAT_PHRASE_MIN_WORDS)]
    pub phrase_min_words: usize,
    /// Maximum words in a discovered phrase.
    #[arg(long, default_value_t = DEFAULT_REPEAT_PHRASE_MAX_WORDS)]
    pub phrase_max_words: usize,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

pub fn run_repeats(db: &Db, args: &RepeatsArgs) -> Result<()> {
    if args.regex && args.query.is_none() {
        bail!("--regex requires QUERY");
    }
    run_repeats_issues(db, args)
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
        provider: args.provider,
        session: args.session.clone(),
        session_id: exact_session_id,
        path_prefix: args.path.as_deref().map(crate::util::normalize_path_prefix),
        since,
        until,
        regex: args.regex.then(|| args.query.clone().unwrap_or_default()),
        limit: args.limit,
        ..Default::default()
    })
}

fn run_repeats_issues(db: &Db, args: &RepeatsArgs) -> Result<()> {
    if args.phrase_min_words == 0 {
        bail!("--phrase-min-words must be at least 1");
    }
    if args.min_matches == 0 {
        bail!("--min-matches must be at least 1");
    }
    if args.phrase_max_words < args.phrase_min_words {
        bail!("--phrase-max-words must be >= --phrase-min-words");
    }
    let filters = repeat_filters(db, args, Some(Role::User))?;
    let query = if args.regex {
        ""
    } else {
        args.query.as_deref().unwrap_or("")
    };
    let hits = db.search_messages(query, &filters)?;
    let rows = repeat_phrase_groups(
        &hits,
        args.context,
        args.min_matches,
        args.phrase_min_words,
        args.phrase_max_words,
    );
    emit(&limit_repeat_groups(rows, args.max_groups), args.format)
}

fn repeat_phrase_groups(
    hits: &[MessageHit],
    context: i64,
    min_matches: usize,
    min_words: usize,
    max_words: usize,
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

    remove_exact_subphrase_duplicates(&mut candidates);

    candidates
        .into_iter()
        .map(|(repeat, indices)| {
            let members: Vec<RepeatGroupMember> = indices
                .iter()
                .map(|&index| RepeatGroupMember::from_hit(&hits[index], repeat.clone(), context))
                .collect();
            let sessions = members
                .iter()
                .map(|m| m.session_id.as_str())
                .collect::<BTreeSet<_>>()
                .len();
            RepeatGroup {
                repeat,
                matches: members.len(),
                sessions,
                members,
            }
        })
        .collect()
}

fn remove_exact_subphrase_duplicates(candidates: &mut Vec<(String, BTreeSet<usize>)>) {
    let mut kept: Vec<(String, BTreeSet<usize>)> = Vec::with_capacity(candidates.len());
    for (phrase, indices) in candidates.drain(..) {
        let is_duplicate = kept.iter().any(|(kept_phrase, kept_indices)| {
            kept_indices == &indices && phrase_is_prefix_of(&phrase, kept_phrase)
        });
        if !is_duplicate {
            kept.push((phrase, indices));
        }
    }
    *candidates = kept;
}

fn phrase_is_prefix_of(needle: &str, haystack: &str) -> bool {
    if needle == haystack {
        return true;
    }
    let needle_words = needle.split_whitespace().collect::<Vec<_>>();
    let haystack_words = haystack.split_whitespace().collect::<Vec<_>>();
    needle_words.len() < haystack_words.len() && haystack_words.starts_with(&needle_words)
}

fn limit_repeat_groups(mut rows: Vec<RepeatGroup>, max_groups: usize) -> Vec<RepeatGroup> {
    if max_groups > 0 && rows.len() > max_groups {
        rows.truncate(max_groups);
    }
    rows
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

fn normalized_tokens(content: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in content.chars() {
        if ch.is_alphanumeric() {
            current.extend(ch.to_lowercase());
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
    format!("sessiongrep messages get {session_id} --seq {seq} --context {context}")
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
            fuzzy_score: None,
            content: content.to_string(),
        }
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

        let groups = repeat_phrase_groups(&hits, 3, 2, 2, 4);

        let magic_values = groups
            .iter()
            .find(|group| group.repeat == "magic values")
            .expect("repeated phrase is discovered from the data");
        assert!(groups.iter().all(|group| group.repeat != "avoid magic"));
        assert_eq!(magic_values.matches, 2);
        assert_eq!(magic_values.sessions, 1);
        assert_eq!(
            magic_values
                .members
                .iter()
                .map(|m| m.seq)
                .collect::<Vec<_>>(),
            vec![10, 20]
        );
        assert_eq!(
            magic_values.members[0].context_command,
            "sessiongrep messages get claude:test --seq 10 --context 3"
        );
    }

    #[test]
    fn repeat_phrase_mining_uses_user_request_body_not_harness_metadata() {
        let content = "<USER_REQUEST>\navoid magic values and keep settings configurable\n</USER_REQUEST><ADDITIONAL_METADATA>\nThe current local time is 2026-06-30T06:49:05Z.\n</ADDITIONAL_METADATA>";

        let phrases = phrases_in_message(content, 2, 4);

        assert!(phrases.contains("magic values"));
        assert!(phrases.contains("avoid magic values"));
        assert!(!phrases.contains("current local time"));
        assert!(!phrases.contains("additional metadata"));

        let member = RepeatGroupMember::from_hit(
            &hit(1, Role::User, content),
            "magic values".to_string(),
            0,
        );
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
    fn repeat_groups_respect_max_groups() {
        fn rows() -> Vec<RepeatGroup> {
            vec![
                RepeatGroup {
                    repeat: "first".to_string(),
                    matches: 3,
                    sessions: 2,
                    members: Vec::new(),
                },
                RepeatGroup {
                    repeat: "second".to_string(),
                    matches: 2,
                    sessions: 1,
                    members: Vec::new(),
                },
                RepeatGroup {
                    repeat: "third".to_string(),
                    matches: 2,
                    sessions: 1,
                    members: Vec::new(),
                },
            ]
        }

        assert_eq!(limit_repeat_groups(rows(), 2).len(), 2);
        assert_eq!(limit_repeat_groups(rows(), 0).len(), 3);
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
