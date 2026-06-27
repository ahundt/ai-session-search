//! Analytics: corrections detection, planning-command frequency, and stats.
//!
//! Correction categories are narrowed to second-person/imperative forms for precision
//! (see `default_correction_patterns`);
//! order matters (first match wins, so `other` is last). Nothing is hard-coded as a fixed
//! list: `analytics.correction_patterns` (`"CATEGORY:REGEX"`, repeatable, same-category
//! ORed) fully replaces the correction built-ins, and `analytics.planning_commands`
//! (regexes over the slash-command token) optionally restricts which commands `planning`
//! counts (empty = all). Both are plain TOML config (the repo's config mechanism); the
//! built-in defaults are the documented fallback, not a fixed policy.

use std::collections::HashMap;
use std::io::{self, Write};

use anyhow::{Result, anyhow};
use clap::Args;
use regex::Regex;
use serde::Serialize;

use crate::config::Config;
use crate::dates::DateRange;
use crate::db::Db;
use crate::models::{CorrectionMatch, MessageFilters, PlanningCount, Provider, Role};
use crate::render::{OutputFormat, Row, render};
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
    #[command(flatten)]
    pub dates: DateRange,
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

/// Build a [`MessageFilters`] from a session scope, a [`DateRange`], and a limit.
fn filters_from(session: &Option<String>, dates: &DateRange, limit: usize) -> Result<MessageFilters> {
    let (since, until) = dates.resolve_now()?;
    Ok(MessageFilters {
        session: session.clone(),
        since,
        until,
        limit,
        ..Default::default()
    })
}

/// The raw pattern fragments (BEFORE the `(?i)` wrapper) for the corrections trigram prefilter.
/// Mirrors `compile_patterns`' source selection so the prefilter is a superset of exactly the
/// regexes that classify. Lower-casing is left to the case-insensitive trigram index.
fn correction_pattern_sources(config: &Config) -> Vec<String> {
    let custom = &config.analytics.correction_patterns;
    if custom.is_empty() {
        default_correction_patterns()
            .into_iter()
            .flat_map(|(_, kws)| kws.into_iter().map(String::from))
            .collect()
    } else {
        custom
            .iter()
            .filter_map(|spec| spec.split_once(':').map(|(_, rx)| rx.to_string()))
            .collect()
    }
}

pub fn run_corrections(db: &Db, config: &Config, args: &CorrectionsArgs) -> Result<()> {
    let patterns = compile_patterns(config)?;
    let filters = filters_from(&args.session, &args.dates, args.limit)?;
    // Narrow the user-message scan to trigram candidates when every pattern fragment has an
    // indexable (>=3-char) literal; otherwise fall back to a full scan (fail-closed superset).
    let prefilter = crate::trigram::trigram_prefilter_all(correction_pattern_sources(config));
    let hits = db.find_corrections(&patterns, prefilter.as_deref(), &filters)?;
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
    let filters = filters_from(&args.session, &args.dates, args.limit)?;
    let command_filters = compile_planning_filters(config)?;
    let counts = db.planning_usage(&filters, &command_filters)?;
    emit(&counts, args.format)
}

pub fn run_stats(db: &Db, args: &StatsArgs) -> Result<()> {
    let filters = filters_from(&None, &args.dates, 0)?;
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
        vec![self.term.clone(), self.docs.to_string(), self.count.to_string()]
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

/// A near-duplicate message pair (rendered by `repeats`).
#[derive(Debug, Serialize)]
struct RepeatPair {
    similarity: f64,
    session_a: String,
    seq_a: i64,
    session_b: String,
    seq_b: i64,
    preview: String,
}

impl Row for RepeatPair {
    fn headers() -> &'static [&'static str] {
        &["similarity", "session_a", "seq_a", "session_b", "seq_b", "preview"]
    }
    fn cells(&self) -> Vec<String> {
        vec![
            format!("{:.3}", self.similarity),
            self.session_a.clone(),
            self.seq_a.to_string(),
            self.session_b.clone(),
            self.seq_b.to_string(),
            truncate_for_display(&self.preview, 80),
        ]
    }
}

#[derive(Debug, Args)]
pub struct RepeatsArgs {
    /// Filter by role (user|assistant|tool|slash|compaction).
    #[arg(long = "type", value_enum)]
    pub role: Option<Role>,
    /// Restrict to one harness.
    #[arg(long, value_enum)]
    pub provider: Option<Provider>,
    /// Scope to one session id (substring match).
    #[arg(long)]
    pub session: Option<String>,
    #[command(flatten)]
    pub dates: DateRange,
    /// Minimum word-3-gram Jaccard similarity to report a pair as a near-duplicate.
    #[arg(long, default_value_t = 0.8)]
    pub threshold: f64,
    /// Max messages to compare in scope (0 = all). Bounds the candidate set, not the results.
    #[arg(long, default_value_t = 0)]
    pub limit: usize,
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

pub fn run_repeats(db: &Db, args: &RepeatsArgs) -> Result<()> {
    let (since, until) = args.dates.resolve_now()?;
    let filters = MessageFilters {
        role: args.role,
        provider: args.provider,
        session: args.session.clone(),
        since,
        until,
        limit: args.limit,
        ..Default::default()
    };
    let hits = db.search_messages("", &filters)?;
    let contents: Vec<String> = hits.iter().map(|h| h.content.clone()).collect();
    let rows: Vec<RepeatPair> = crate::minhash::near_duplicate_pairs(&contents, args.threshold)
        .into_iter()
        .map(|(i, j, similarity)| RepeatPair {
            similarity,
            session_a: hits[i].session_id.clone(),
            seq_a: hits[i].seq,
            session_b: hits[j].session_id.clone(),
            seq_b: hits[j].seq,
            preview: hits[i].content.clone(),
        })
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
    fn categories_match_expected_keywords() {
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
        assert_eq!(categorize("please stop, that approach is off").as_deref(), Some("other"));
        // Benign workflow phrasings must NOT be flagged as corrections: a bare
        // \bstop\b matched all of these (test fixtures, checkpoint instructions).
        assert_eq!(categorize("Run this bash command once and stop: grep hi /tmp/x"), None);
        assert_eq!(categorize("at your next progress point commit and then stop"), None);
        assert_eq!(categorize("keep going dont stop for trivial questions"), None);
        assert_eq!(categorize("a clear way to start and stop all the tooling"), None);
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
            assert_eq!(categorize(text).as_deref(), Some(*want), "true positive: {text:?}");
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
            assert_eq!(categorize(text), None, "true negative must not match: {text:?}");
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
        config.analytics.correction_patterns = vec!["oops:nono".to_string(), "oops:whoops".to_string()];
        let compiled = compile_patterns(&config).unwrap();
        assert_eq!(compiled.len(), 1, "same category is ORed into one entry");
        assert_eq!(compiled[0].0, "oops");
        assert!(compiled[0].1.is_match("whoops that broke"));
        assert!(!compiled[0].1.is_match("you forgot"));
    }
}
