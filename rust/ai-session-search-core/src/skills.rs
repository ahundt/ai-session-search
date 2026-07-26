//! `aise skills` — see every discovered skill, explain one, and validate a directory.
//!
//! `aise` must **see** every skill and **write** only what it owns. These three verbs are the
//! read-only half of that rule: they report aise-managed and user-authored skills alike, and they
//! never modify a directory. Ownership is reported, never assumed.
//!
//! Policy parsing, compilation, and discovery live in [`crate::corrections`], which stays pure.
//! This module is the command surface over it: it reads `SKILL.md`, decides what to call each
//! skill's state, and renders rows.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::config::Config;
use crate::corrections::{
    discover_skills_in, CorrectionPolicy, CorrectionPolicySource, DiscoveredSkill,
};
use crate::render::{render, OutputFormat, Row};
use crate::util::truncate_for_display;

/// Longest `description` the Agent Skills specification accepts.
const MAX_DESCRIPTION_CHARS: usize = 1024;
/// Longest `name` the Agent Skills specification accepts.
const MAX_NAME_CHARS: usize = 64;
/// Compiled category regexes are one alternation over every pattern, so they get long. Table
/// output truncates; `--format json` always carries the full source.
const TABLE_REGEX_CHARS: usize = 72;

#[derive(Debug, Subcommand)]
pub enum SkillsCmd {
    /// List every discovered skill — aise-managed and user-authored alike — with ownership,
    /// policy version, and whether its correction policy loads.
    #[command(
        after_help = "Skills come from `[skills].search_paths` plus the built-in \
                            `ai-session-search` policy. Diagnose one with `aise skills validate \
                            <path>`."
    )]
    List(SkillsListArgs),
    /// Explain one skill: where it resolved from, its policy identity, and the categories it
    /// evaluates, in order.
    Show(SkillsShowArgs),
    /// Check one skill directory's frontmatter and correction policy, naming the fix for each
    /// problem rather than only refusing.
    Validate(SkillsValidateArgs),
    /// Scaffold a new skill directory you own, seeded with the current default categories.
    #[command(
        after_help = "The scaffold is YOURS: it carries no managed marker, so \
                            `aise integrations install` and `aise skills update` will never \
                            rewrite it. Edit corrections/policy.toml, then select it with \
                            `aise corrections --skill <name>`."
    )]
    Create(SkillsCreateArgs),
    /// Bring aise-owned installed skills up to this build's content. User-authored skills are
    /// only diagnosed, never rewritten.
    #[command(
        after_help = "Updates the skill directories `aise integrations install` writes, \
                            NOT every directory on [skills].search_paths. A skill you wrote is \
                            reported and left alone: aise has no upstream copy of it to install."
    )]
    Update(SkillsUpdateArgs),
    /// Rewrite one owned skill's managed files from the copy embedded in this executable.
    #[command(
        after_help = "The repair path for a damaged install. Unlike `skills update`, this \
                            OVERWRITES managed files whose bytes changed since install, so run \
                            it with --dry-run first. Files you added under the skill directory \
                            are never touched."
    )]
    Restore(SkillsRestoreArgs),
}

#[derive(Debug, Args)]
pub struct SkillsUpdateArgs {
    /// Extra skill directory to update. Repeat for several; omit to update every detected client
    /// root that already holds an aise-owned skill.
    #[arg(long = "skill-root", value_name = "DIR")]
    pub skill_roots: Vec<PathBuf>,
    /// Report what would be rewritten without writing anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct SkillsRestoreArgs {
    /// Skill to restore. Only the built-in `ai-session-search` skill has an embedded copy to
    /// restore from; aise has no upstream for a skill you wrote.
    pub name: String,
    /// The exact installed directory to repair. Required: a repair that overwrites files must
    /// name what it will overwrite rather than searching for candidates.
    #[arg(long = "skill-root", value_name = "DIR")]
    pub skill_root: PathBuf,
    /// Report what would be rewritten without writing anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct SkillsListArgs {
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct SkillsShowArgs {
    /// Skill to explain, as `aise skills list` spells it. `ai-session-search` is always
    /// resolvable, because it is compiled into this executable.
    pub name: String,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct SkillsValidateArgs {
    /// Skill directory to check — the one holding `SKILL.md`. Need not be on a search path, so a
    /// skill can be checked before it is installed anywhere.
    pub path: PathBuf,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct SkillsCreateArgs {
    /// Name of the new skill. Becomes the directory name and the `SKILL.md` `name`, which the
    /// specification requires to be equal: 1-64 characters of lowercase letters, digits, and
    /// single interior hyphens.
    pub name: String,
    /// Parent directory to create `<name>/` under. Omit to use `[skills].authoring_root`.
    ///
    /// The parent, not the skill directory itself: `--output-dir ~/.claude/skills` with
    /// `NAME = my-rules` creates `~/.claude/skills/my-rules/`. Naming the parent is what lets the
    /// command refuse an existing destination instead of merging into one.
    #[arg(long)]
    pub output_dir: Option<PathBuf>,
    /// Print what would be created without creating anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

/// Who owns a skill directory, which decides whether `aise` may ever write to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkillOwnership {
    /// `SKILL.md` carries the managed marker: `aise` wrote it and may update it.
    Aise,
    /// No marker: somebody else wrote it. `aise` reports it and never rewrites it.
    User,
    /// `SKILL.md` could not be read as UTF-8 text, so ownership cannot be established. Reported
    /// rather than assumed, because guessing `User` hides a damaged install and guessing `Aise`
    /// would authorize overwriting a file nobody could read.
    Unknown,
}

impl SkillOwnership {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Aise => "aise",
            Self::User => "user",
            Self::Unknown => "unknown",
        }
    }
}

/// Whether a discovered skill can currently supply correction rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkillPolicyStatus {
    /// `corrections/policy.toml` is present and compiles.
    Ok,
    /// No `corrections/policy.toml`. A valid skill; it just defines no correction categories.
    NoPolicy,
    /// A policy file is present but does not load. `problem` says why.
    Invalid,
}

impl SkillPolicyStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NoPolicy => "no-policy",
            Self::Invalid => "invalid",
        }
    }
}

/// One skill as `aise skills list` reports it.
#[derive(Debug, Clone, Serialize)]
pub struct SkillSummary {
    pub name: String,
    pub ownership: SkillOwnership,
    pub policy_status: SkillPolicyStatus,
    /// The policy's own version, not the `aise` version. Absent when there is no loadable policy.
    pub policy_version: Option<String>,
    /// Digest of the exact policy bytes. Absent when there is no loadable policy.
    pub policy_sha256: Option<String>,
    pub category_count: Option<usize>,
    pub path: String,
    /// Why `policy_status` is not `ok`. Table output shows only the status token, so this is where
    /// `--format json` keeps the detail a caller needs to act.
    pub problem: Option<String>,
}

impl Row for SkillSummary {
    fn headers() -> &'static [&'static str] {
        &[
            "skill",
            "owner",
            "policy",
            "version",
            "digest",
            "categories",
            "path",
        ]
    }
    fn cells(&self) -> Vec<String> {
        vec![
            self.name.clone(),
            self.ownership.as_str().to_string(),
            self.policy_status.as_str().to_string(),
            self.policy_version.clone().unwrap_or_else(|| "-".into()),
            self.policy_sha256
                .as_deref()
                // A 12-hex prefix distinguishes edits at a glance; the full digest is in JSON.
                .map_or_else(|| "-".to_string(), |digest| digest[..12].to_string()),
            self.category_count
                .map_or_else(|| "-".to_string(), |count| count.to_string()),
            self.path.clone(),
        ]
    }
}

/// One category, at the position it is evaluated.
#[derive(Debug, Clone, Serialize)]
pub struct SkillCategoryRow {
    /// 1-based evaluation position. First match wins, so this is behavior, not presentation: a
    /// message matching two categories is reported as the lower-numbered one.
    pub order: usize,
    pub category: String,
    /// The regex actually evaluated — one case-insensitive alternation over the category's
    /// patterns. Shown rather than the source pattern list, because this is what runs.
    pub regex: String,
}

impl Row for SkillCategoryRow {
    fn headers() -> &'static [&'static str] {
        &["order", "category", "regex"]
    }
    fn cells(&self) -> Vec<String> {
        vec![
            self.order.to_string(),
            self.category.clone(),
            truncate_for_display(&self.regex, TABLE_REGEX_CHARS),
        ]
    }
}

/// One skill as `aise skills show` explains it.
#[derive(Debug, Clone, Serialize)]
pub struct SkillDetail {
    pub name: String,
    pub path: String,
    pub ownership: SkillOwnership,
    pub policy_status: SkillPolicyStatus,
    pub policy_version: Option<String>,
    pub policy_sha256: Option<String>,
    pub policy_source: Option<CorrectionPolicySource>,
    /// Categories in evaluation order. Empty when no policy loaded.
    pub categories: Vec<SkillCategoryRow>,
    pub problem: Option<String>,
}

/// One problem found by `aise skills validate`, with the fix.
#[derive(Debug, Clone, Serialize)]
pub struct SkillDiagnostic {
    /// Which file the problem is in, relative to the skill root.
    pub file: String,
    /// What is wrong, naming the offending value.
    pub problem: String,
    /// What to change. Never omitted: a diagnostic that only refuses makes the caller guess.
    pub fix: String,
}

impl Row for SkillDiagnostic {
    fn headers() -> &'static [&'static str] {
        &["file", "problem", "fix"]
    }
    fn cells(&self) -> Vec<String> {
        // Compacted, never truncated. A TOML parse error is several lines long, and `plain` is
        // tab-separated while `csv` is line-oriented, so an embedded newline would split one
        // diagnostic into two malformed records. Truncating instead would drop the fix, which is
        // the only part a caller can act on.
        vec![
            self.file.clone(),
            crate::util::compact_whitespace(&self.problem),
            crate::util::compact_whitespace(&self.fix),
        ]
    }
}

/// The full result of validating one directory.
#[derive(Debug, Clone, Serialize)]
pub struct SkillValidation {
    pub path: String,
    pub name: Option<String>,
    pub ownership: SkillOwnership,
    /// True when `diagnostics` is empty. Named rather than left implicit so a JSON consumer does
    /// not have to know that an empty list means success.
    pub valid: bool,
    pub diagnostics: Vec<SkillDiagnostic>,
}

/// Frontmatter fields this build reads. Absent means "not present in the document".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
    /// `metadata.version`. The Agent Skills specification puts version under `metadata` rather
    /// than at the top level, which is why this is nested and not a sibling of `name`.
    metadata_version: Option<String>,
}

/// Read the leading `---` fenced block of a `SKILL.md`.
///
/// DELIBERATELY NOT A YAML PARSER. It reads exactly the shapes the specification uses for the
/// fields this build needs — top-level `key: value` scalars and one nested level under
/// `metadata:` — because adding a YAML dependency to read three strings would pull a parser, its
/// error surface, and its version lifecycle into the crate for no gain. Anything richer (block
/// scalars, flow mappings, anchors) simply does not match, and the field is reported absent
/// rather than guessed at, which surfaces as a named diagnostic instead of a silent wrong answer.
///
/// Returns `None` when the document has no leading frontmatter fence at all.
fn parse_frontmatter(text: &str) -> Option<SkillFrontmatter> {
    let body = text.strip_prefix("---\n").or_else(|| {
        // Tolerate a UTF-8 BOM and CRLF, which a Windows-authored skill will have.
        text.strip_prefix("\u{feff}---\n")
            .or_else(|| text.strip_prefix("---\r\n"))
    })?;
    let end = body.find("\n---")?;
    let mut found = SkillFrontmatter::default();
    let mut in_metadata = false;
    for raw in body[..end].lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if !indented {
            in_metadata = false;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = unquote(value.trim());
        if indented {
            if in_metadata && key == "version" && !value.is_empty() {
                found.metadata_version = Some(value.to_string());
            }
            continue;
        }
        match key {
            "name" if !value.is_empty() => found.name = Some(value.to_string()),
            "description" if !value.is_empty() => found.description = Some(value.to_string()),
            "metadata" => in_metadata = true,
            _ => {}
        }
    }
    Some(found)
}

/// Strip one layer of matching single or double quotes, as a YAML scalar may carry.
fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

/// Check a skill name against the Agent Skills specification.
///
/// Returns the reason it is invalid, phrased as what is wrong with *this* value.
fn skill_name_problem(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("the name is empty".to_string());
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Some(format!(
            "the name is {} characters; the limit is {MAX_NAME_CHARS}",
            name.chars().count()
        ));
    }
    if let Some(bad) = name
        .chars()
        .find(|ch| !ch.is_ascii_lowercase() && !ch.is_ascii_digit() && *ch != '-')
    {
        return Some(format!(
            "the name contains {bad:?}; only lowercase letters, digits, and hyphens are allowed"
        ));
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Some("the name starts or ends with a hyphen".to_string());
    }
    if name.contains("--") {
        return Some("the name contains a doubled hyphen".to_string());
    }
    None
}

/// Read `SKILL.md` and decide who owns the directory.
fn ownership_of(root: &Path) -> SkillOwnership {
    match std::fs::read_to_string(root.join("SKILL.md")) {
        Ok(text) if text.contains(crate::integrations::SKILL_MANAGED_MARKER) => {
            SkillOwnership::Aise
        }
        Ok(_) => SkillOwnership::User,
        Err(_) => SkillOwnership::Unknown,
    }
}

/// Load a discovered skill's policy, keeping the failure rather than propagating it.
///
/// `skills list` must show every skill it can see even when one is broken: propagating the first
/// error would hide every other row, and a listing that disappears because one file is malformed
/// looks like the tool is broken rather than the file.
fn load_policy(skill: &DiscoveredSkill) -> Result<Option<CorrectionPolicy>, String> {
    let Some(path) = &skill.policy_path else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(path).map_err(|err| format!("failed to read: {err}"))?;
    CorrectionPolicy::parse_toml(
        &text,
        CorrectionPolicySource::File {
            path: path.to_path_buf(),
        },
    )
    .map(Some)
    .map_err(|err| format!("{err:#}"))
}

fn summarize(skill: &DiscoveredSkill) -> SkillSummary {
    let ownership = ownership_of(&skill.root);
    let path = skill.root.display().to_string();
    match load_policy(skill) {
        Ok(Some(policy)) => {
            let identity = policy.identity();
            SkillSummary {
                name: skill.name.clone(),
                ownership,
                policy_status: SkillPolicyStatus::Ok,
                policy_version: Some(identity.version.clone()),
                policy_sha256: Some(identity.sha256.clone()),
                category_count: Some(policy.category_count()),
                path,
                problem: None,
            }
        }
        Ok(None) => SkillSummary {
            name: skill.name.clone(),
            ownership,
            policy_status: SkillPolicyStatus::NoPolicy,
            policy_version: None,
            policy_sha256: None,
            category_count: None,
            path,
            problem: None,
        },
        Err(problem) => SkillSummary {
            name: skill.name.clone(),
            ownership,
            policy_status: SkillPolicyStatus::Invalid,
            policy_version: None,
            policy_sha256: None,
            category_count: None,
            path,
            problem: Some(problem),
        },
    }
}

fn category_rows(policy: &CorrectionPolicy) -> Vec<SkillCategoryRow> {
    policy
        .rules()
        .iter()
        .enumerate()
        .map(|(index, (category, regex))| SkillCategoryRow {
            order: index + 1,
            category: category.clone(),
            regex: regex.as_str().to_string(),
        })
        .collect()
}

/// Every skill `aise` can see: the embedded policy first, then the search paths.
///
/// The embedded row is synthesized rather than discovered, because it has no directory. Listing it
/// matters: it is what `corrections` uses by default, so a listing that omitted it would answer
/// "which rules run?" with everything except the answer.
fn summaries(config: &Config) -> Result<Vec<SkillSummary>> {
    let embedded = crate::corrections::embedded_policy()?;
    let embedded_identity = embedded.identity();
    let mut rows = vec![SkillSummary {
        name: embedded_identity.name.clone(),
        ownership: SkillOwnership::Aise,
        policy_status: SkillPolicyStatus::Ok,
        policy_version: Some(embedded_identity.version.clone()),
        policy_sha256: Some(embedded_identity.sha256.clone()),
        category_count: Some(embedded.category_count()),
        path: "(built in)".to_string(),
        problem: None,
    }];
    for configured in &config.skills.search_paths {
        let root = crate::util::expand_tilde(configured);
        for skill in discover_skills_in(&root)? {
            // The reserved name cannot be shadowed, so a directory claiming it is reported at its
            // real path and marked, rather than silently omitted or silently winning.
            if skill.name == crate::corrections::EMBEDDED_POLICY_NAME {
                let mut row = summarize(&skill);
                row.policy_status = SkillPolicyStatus::Invalid;
                row.problem = Some(format!(
                    "'{}' is reserved for the built-in policy and cannot be selected from disk; \
                     rename this directory and its SKILL.md name to use it",
                    crate::corrections::EMBEDDED_POLICY_NAME
                ));
                rows.push(row);
                continue;
            }
            rows.push(summarize(&skill));
        }
    }
    Ok(rows)
}

fn detail(config: &Config, name: &str) -> Result<SkillDetail> {
    if name == crate::corrections::EMBEDDED_POLICY_NAME {
        let policy = crate::corrections::embedded_policy()?;
        let identity = policy.identity();
        return Ok(SkillDetail {
            name: identity.name.clone(),
            path: "(built in)".to_string(),
            ownership: SkillOwnership::Aise,
            policy_status: SkillPolicyStatus::Ok,
            policy_version: Some(identity.version.clone()),
            policy_sha256: Some(identity.sha256.clone()),
            policy_source: Some(identity.source.clone()),
            categories: category_rows(&policy),
            problem: None,
        });
    }

    let discovered = crate::corrections::discover_skills(&config.skills.search_paths)?;
    let skill = discovered
        .iter()
        .find(|skill| skill.name == name)
        .with_context(|| {
            format!(
                "unknown skill '{name}'; run `aise skills list` to see discovered skills, or add \
                 its parent directory to [skills].search_paths"
            )
        })?;
    let summary = summarize(skill);
    let categories = match load_policy(skill) {
        Ok(Some(policy)) => category_rows(&policy),
        _ => Vec::new(),
    };
    let policy_source = skill
        .policy_path
        .as_ref()
        .map(|path| CorrectionPolicySource::File {
            path: path.to_path_buf(),
        });
    Ok(SkillDetail {
        name: summary.name,
        path: summary.path,
        ownership: summary.ownership,
        policy_status: summary.policy_status,
        policy_version: summary.policy_version,
        policy_sha256: summary.policy_sha256,
        policy_source,
        categories,
        problem: summary.problem,
    })
}

/// Check one directory, collecting every problem rather than stopping at the first.
///
/// Reporting all of them at once is the difference between one fix-and-rerun cycle and five.
fn validate(path: &Path) -> Result<SkillValidation> {
    let mut diagnostics = Vec::new();
    let display = path.display().to_string();

    if !path.is_dir() {
        return Ok(SkillValidation {
            path: display,
            name: None,
            ownership: SkillOwnership::Unknown,
            valid: false,
            diagnostics: vec![SkillDiagnostic {
                file: ".".to_string(),
                problem: "not a directory".to_string(),
                fix: "pass the skill directory that holds SKILL.md, not a file inside it"
                    .to_string(),
            }],
        });
    }

    let dir_name = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string);

    let skill_md = path.join("SKILL.md");
    let frontmatter = match std::fs::read_to_string(&skill_md) {
        Ok(text) => match parse_frontmatter(&text) {
            Some(found) => Some(found),
            None => {
                diagnostics.push(SkillDiagnostic {
                    file: "SKILL.md".to_string(),
                    problem: "no frontmatter block".to_string(),
                    fix: "start the file with a `---` line, the `name:` and `description:` \
                          fields, then a closing `---` line"
                        .to_string(),
                });
                None
            }
        },
        Err(err) => {
            diagnostics.push(SkillDiagnostic {
                file: "SKILL.md".to_string(),
                problem: format!("cannot be read as UTF-8 text: {err}"),
                fix: "every skill directory needs a readable UTF-8 SKILL.md; create or repair it"
                    .to_string(),
            });
            None
        }
    };

    if let Some(frontmatter) = &frontmatter {
        match &frontmatter.name {
            None => diagnostics.push(SkillDiagnostic {
                file: "SKILL.md".to_string(),
                problem: "frontmatter has no `name`".to_string(),
                fix: dir_name.as_ref().map_or_else(
                    || "add `name:` matching the directory name".to_string(),
                    |dir| format!("add `name: {dir}`, matching the directory name"),
                ),
            }),
            Some(name) => {
                if let Some(problem) = skill_name_problem(name) {
                    diagnostics.push(SkillDiagnostic {
                        file: "SKILL.md".to_string(),
                        problem: format!("`name: {name}` is not a valid skill name: {problem}"),
                        fix: "use 1-64 characters of lowercase letters, digits, and single \
                              interior hyphens"
                            .to_string(),
                    });
                }
                if let Some(dir) = &dir_name {
                    if dir != name {
                        diagnostics.push(SkillDiagnostic {
                            file: "SKILL.md".to_string(),
                            problem: format!(
                                "`name: {name}` does not match the directory name `{dir}`"
                            ),
                            fix: format!(
                                "the specification requires them to be equal: set `name: {dir}`, \
                                 or rename the directory to `{name}`"
                            ),
                        });
                    }
                }
            }
        }
        match &frontmatter.description {
            None => diagnostics.push(SkillDiagnostic {
                file: "SKILL.md".to_string(),
                problem: "frontmatter has no `description`".to_string(),
                fix: "add `description:` saying what the skill does AND when to use it; that \
                      text is all a host sees when deciding whether to load the skill"
                    .to_string(),
            }),
            Some(description) if description.chars().count() > MAX_DESCRIPTION_CHARS => {
                diagnostics.push(SkillDiagnostic {
                    file: "SKILL.md".to_string(),
                    problem: format!(
                        "`description` is {} characters; the limit is {MAX_DESCRIPTION_CHARS}",
                        description.chars().count()
                    ),
                    fix: "shorten it; move detail into the body or a references/ file".to_string(),
                });
            }
            Some(_) => {}
        }
    }

    let policy_path = path.join("corrections").join("policy.toml");
    if policy_path.is_file() {
        match std::fs::read_to_string(&policy_path) {
            Ok(text) => match CorrectionPolicy::parse_toml(
                &text,
                CorrectionPolicySource::File {
                    path: policy_path.clone(),
                },
            ) {
                Ok(policy) => {
                    let identity = policy.identity();
                    if let (Some(frontmatter), name) = (&frontmatter, &identity.name) {
                        if let Some(declared) = &frontmatter.name {
                            if declared != name {
                                diagnostics.push(SkillDiagnostic {
                                    file: "corrections/policy.toml".to_string(),
                                    problem: format!(
                                        "policy `name = \"{name}\"` does not match the SKILL.md \
                                         name `{declared}`"
                                    ),
                                    fix: format!(
                                        "a skill is selected by one name on every surface: set \
                                         `name = \"{declared}\"`"
                                    ),
                                });
                            }
                        }
                        if let Some(version) = &frontmatter.metadata_version {
                            if version != &identity.version {
                                diagnostics.push(SkillDiagnostic {
                                    file: "corrections/policy.toml".to_string(),
                                    problem: format!(
                                        "policy `version = \"{}\"` does not match SKILL.md \
                                         `metadata.version: {version}`",
                                        identity.version
                                    ),
                                    fix: "keep the two in step, or a reported version will not \
                                          identify the rules that ran"
                                        .to_string(),
                                });
                            }
                        }
                    }
                }
                Err(err) => diagnostics.push(SkillDiagnostic {
                    file: "corrections/policy.toml".to_string(),
                    problem: format!("{err:#}"),
                    fix: "correct the field named above; `aise skills show <name>` prints a \
                          working policy for comparison"
                        .to_string(),
                }),
            },
            Err(err) => diagnostics.push(SkillDiagnostic {
                file: "corrections/policy.toml".to_string(),
                problem: format!("cannot be read as UTF-8 text: {err}"),
                fix: "a correction policy must be a readable UTF-8 TOML file".to_string(),
            }),
        }
    }

    Ok(SkillValidation {
        path: display,
        name: frontmatter.and_then(|found| found.name).or(dir_name),
        ownership: ownership_of(path),
        valid: diagnostics.is_empty(),
        diagnostics,
    })
}

/// Render a whole record as JSON, or its row-shaped projection otherwise.
///
/// Same split `crate::messages::emit_inspection` uses: structured formats carry the complete
/// record, tabular formats carry the part that fits columns. `preamble` and `empty_note` are
/// human text, so they print for `table` ONLY: `plain` is tab-separated rows and `csv` is RFC
/// 4180, and prefixing either with prose would corrupt a parse.
///
/// `empty_note` exists because an empty table is ambiguous — headers with no rows read as "the
/// command failed" just as easily as "there is nothing to report", and for `skills validate` the
/// difference is between a valid skill and a broken tool.
fn emit_record<R, T>(
    record: &R,
    rows: &[T],
    format: OutputFormat,
    preamble: &[(&str, String)],
    empty_note: &str,
) -> Result<()>
where
    R: Serialize,
    T: Serialize + Row,
{
    let stdout = io::stdout();
    let mut out = stdout.lock();
    match format {
        OutputFormat::Json | OutputFormat::Jsonl | OutputFormat::Csv | OutputFormat::Plain => {
            crate::render::render_record(record, rows, format, &mut out)?;
        }
        OutputFormat::Table => {
            let width = preamble
                .iter()
                .map(|(label, _)| label.chars().count())
                .max()
                .unwrap_or(0);
            for (label, value) in preamble {
                writeln!(out, "{label:<width$}  {value}")?;
            }
            if rows.is_empty() {
                writeln!(out, "{empty_note}")?;
            } else {
                if !preamble.is_empty() {
                    writeln!(out)?;
                }
                render(rows, format, &mut out)?;
            }
        }
    }
    out.flush()?;
    Ok(())
}

/// One file a scaffold will create, relative to the new skill root.
#[derive(Debug, Clone, Serialize)]
pub struct ScaffoldedFile {
    pub relative_path: String,
    pub bytes: usize,
}

impl Row for ScaffoldedFile {
    fn headers() -> &'static [&'static str] {
        &["file", "bytes"]
    }
    fn cells(&self) -> Vec<String> {
        vec![self.relative_path.clone(), self.bytes.to_string()]
    }
}

/// What `aise skills create` did, or would do under `--dry-run`.
#[derive(Debug, Clone, Serialize)]
pub struct SkillScaffoldReceipt {
    pub name: String,
    pub root: String,
    /// False under `--dry-run`: nothing was written. Named rather than inferred from the flag,
    /// so a JSON consumer does not have to reconstruct the invocation to know what happened.
    pub created: bool,
    pub files: Vec<ScaffoldedFile>,
}

/// Create one user-owned skill directory, or refuse without touching anything.
///
/// Same three phases as [`crate::export::ExportPublicationPlan`] and
/// [`crate::analysis_publication::AnalysisPublicationPlan`]: preflight refuses an existing
/// destination, [`crate::durable_fs::StagedDirectory`] stages a sibling, publish renames it into
/// place. Reused rather than reimplemented so a half-written skill directory cannot exist: either
/// the whole tree appears or nothing does.
#[derive(Debug)]
pub struct SkillScaffoldPlan {
    name: String,
    root: PathBuf,
    files: Vec<(PathBuf, String)>,
}

impl SkillScaffoldPlan {
    /// Plan a scaffold under `output_dir`, seeded with the current built-in categories.
    ///
    /// # Errors
    ///
    /// Returns an error when the name is not a valid skill name, when neither `--output-dir` nor
    /// `[skills].authoring_root` names a destination, when the destination cannot be made
    /// absolute, or when it already exists.
    pub fn new(config: &Config, name: &str, output_dir: Option<&Path>) -> Result<Self> {
        if let Some(problem) = skill_name_problem(name) {
            bail!(
                "'{name}' is not a valid skill name: {problem}. The directory name and the \
                 SKILL.md name must be equal, so use 1-{MAX_NAME_CHARS} characters of lowercase \
                 letters, digits, and single interior hyphens"
            );
        }
        if name == crate::corrections::EMBEDDED_POLICY_NAME {
            bail!(
                "'{name}' is reserved for the policy built into this executable and cannot be \
                 shadowed; choose another name, such as '{name}-local'"
            );
        }

        let parent = match output_dir {
            Some(path) => path.to_path_buf(),
            None => {
                let configured = config.skills.authoring_root.as_deref().with_context(|| {
                    "no authoring destination; pass --output-dir <parent directory>, or set                      [skills].authoring_root in config.toml"
                        .to_string()
                })?;
                // Fallible: this path is WRITTEN to, so a silent fallback would create a
                // directory literally named `~` in the working directory.
                crate::util::expand_tilde_required(Path::new(configured))?
            }
        };
        let parent = if parent.is_absolute() {
            parent
        } else {
            std::env::current_dir()
                .context("failed to resolve a relative --output-dir against the current directory")?
                .join(parent)
        };
        let root = parent.join(name);
        if crate::durable_fs::entry_exists(&root)? {
            bail!(
                "refusing to create {}: it already exists. Creating INTO an existing directory \
                 could overwrite files you wrote, so pick a different name or --output-dir, or \
                 move the existing directory aside",
                root.display()
            );
        }

        Ok(Self {
            name: name.to_string(),
            root,
            files: scaffold_files(name)?,
        })
    }

    /// The directory that would be created.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// What the scaffold contains, without writing anything.
    pub fn preview(&self) -> SkillScaffoldReceipt {
        SkillScaffoldReceipt {
            name: self.name.clone(),
            root: self.root.display().to_string(),
            created: false,
            files: self
                .files
                .iter()
                .map(|(path, content)| ScaffoldedFile {
                    relative_path: path.display().to_string(),
                    bytes: content.len(),
                })
                .collect(),
        }
    }

    /// Stage every file, then publish the directory in one atomic rename.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent cannot be created, a file cannot be staged, or the
    /// destination appeared between planning and publication.
    pub fn publish(self) -> Result<SkillScaffoldReceipt> {
        let mut receipt = self.preview();
        let parent = self
            .root
            .parent()
            .context("skill destination has no parent directory")?;
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create the authoring directory {}",
                parent.display()
            )
        })?;
        let staging = crate::durable_fs::StagedDirectory::begin(parent, "skill")?;
        for (path, content) in &self.files {
            staging.write(path, content.as_bytes())?;
        }
        staging.publish(&self.root)?;
        receipt.created = true;
        Ok(receipt)
    }
}

/// The two files a new skill starts with, seeded from the current built-in categories.
///
/// Seeded rather than empty because an empty policy teaches nothing about the format, and
/// transcribing the shipped defaults is the most common reason to author a skill at all: start
/// from what `aise` already does, then change the parts you disagree with.
fn scaffold_files(name: &str) -> Result<Vec<(PathBuf, String)>> {
    let mut policy = format!(
        "# Correction categories for the `{name}` skill.\n\
         #\n\
         # Categories are evaluated top to bottom and the FIRST match wins, so a catch-all\n\
         # belongs last. Patterns within one category are ORed, and each category compiles to a\n\
         # single case-insensitive regex. Check this file with `aise skills validate`, then use\n\
         # it with `aise corrections --skill {name}`.\n\
         schema_version = {}\n\
         name = \"{name}\"\n\
         version = \"0.1.0\"\n",
        crate::corrections::CORRECTION_POLICY_SCHEMA_VERSION
    );
    for (category, patterns) in crate::analytics::default_correction_patterns() {
        policy.push_str(&format!(
            "\n[[categories]]\nname = \"{category}\"\npatterns = [\n"
        ));
        for pattern in patterns {
            // TOML multiline literal strings: no backslash doubling, so a regex stays readable.
            policy.push_str(&format!("  \'\'\'{pattern}\'\'\',\n"));
        }
        policy.push_str("]\n");
    }

    // Deliberately WITHOUT the managed marker: this copy belongs to whoever ran the command, and
    // its absence is what stops `aise skills update` and `integrations install` from rewriting it.
    let skill_md = format!(
        "---\n\
         name: {name}\n\
         description: Correction categories for {name}. Use when asked to find where a person \
         corrected the agent in past AI coding sessions, using project-specific wording that the \
         built-in categories do not cover.\n\
         metadata:\n\
         \x20 version: 0.1.0\n\
         ---\n\
         \n\
         # {name}\n\
         \n\
         Correction categories for `aise corrections`.\n\
         \n\
         ## How it works\n\
         \n\
         `corrections/policy.toml` lists named categories of regular expressions. `aise` scans the\n\
         user's own messages in indexed sessions and reports the first category each message\n\
         matches, with the exact text that matched.\n\
         \n\
         ## Usage\n\
         \n\
         ```sh\n\
         aise corrections --skill {name} --format json\n\
         ```\n\
         \n\
         Add this skill's parent directory to `[skills].search_paths` in the aise config so it is\n\
         discovered, then check it with `aise skills validate` after every edit.\n\
         \n\
         ## Editing the categories\n\
         \n\
         The seeded categories are the ones `aise` ships with. Order is behavior: the first\n\
         matching category wins, so keep any catch-all last. Narrow patterns beat broad ones -- a\n\
         bare `\\bstop\\b` matched ~98% false positives on real data, which is why the shipped\n\
         `other` category keys on imperative forms instead.\n"
    );

    Ok(vec![
        (PathBuf::from("SKILL.md"), skill_md),
        (PathBuf::from("corrections").join("policy.toml"), policy),
    ])
}

/// Render a validation result.
///
/// `table` gets a labeled block per diagnostic rather than a grid: a TOML parse error and the
/// sentence explaining how to fix it are prose, and a column-aligned grid pads every row to the
/// longest cell, which turns one multi-line error into an unreadable wall. `csv` and `plain` keep
/// the row shape, because those are parsed rather than read.
fn emit_validation(result: &SkillValidation, format: OutputFormat) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    match format {
        OutputFormat::Json | OutputFormat::Jsonl | OutputFormat::Csv | OutputFormat::Plain => {
            crate::render::render_record(result, &result.diagnostics, format, &mut out)?;
        }
        OutputFormat::Table => {
            writeln!(out, "skill  {}", result.name.as_deref().unwrap_or("-"))?;
            writeln!(out, "path   {}", result.path)?;
            writeln!(out, "owner  {}", result.ownership.as_str())?;
            if result.valid {
                // Success has to SAY it succeeded. An empty diagnostics table is indistinguishable
                // from a command that silently did nothing.
                writeln!(
                    out,
                    "\nvalid: frontmatter and correction policy both check out"
                )?;
            } else {
                for diagnostic in &result.diagnostics {
                    writeln!(out, "\n{}", diagnostic.file)?;
                    for line in diagnostic.problem.lines() {
                        writeln!(out, "  {line}")?;
                    }
                    writeln!(out, "  fix: {}", diagnostic.fix)?;
                }
            }
        }
    }
    out.flush()?;
    Ok(())
}

impl Row for crate::integrations::SkillWriteOutcome {
    fn headers() -> &'static [&'static str] {
        &["client", "result", "path"]
    }
    fn cells(&self) -> Vec<String> {
        vec![self.label.clone(), self.action.clone(), self.root.clone()]
    }
}

/// Run one `aise skills` verb.
///
/// `list`, `show`, and `validate` read only. `create`, `update`, and `restore` write, and each
/// writes only what aise owns: a directory the caller named that does not exist yet, or a
/// directory carrying aise's managed marker or install record.
///
/// # Errors
///
/// Returns an error when a configured search path cannot be read, when two directories claim one
/// skill name, when `show` names a skill that does not exist, or when output cannot be written.
pub fn run(config: &Config, cmd: SkillsCmd, receipt_path: &Path) -> Result<()> {
    match cmd {
        SkillsCmd::List(args) => {
            let rows = summaries(config)?;
            let stdout = io::stdout();
            let mut out = stdout.lock();
            render(&rows, args.format, &mut out)?;
            out.flush()?;
            Ok(())
        }
        SkillsCmd::Show(args) => {
            let found = detail(config, &args.name)?;
            let categories = found.categories.clone();
            let mut preamble = vec![
                ("skill", found.name.clone()),
                ("path", found.path.clone()),
                ("owner", found.ownership.as_str().to_string()),
                ("policy", found.policy_status.as_str().to_string()),
            ];
            if let Some(version) = &found.policy_version {
                preamble.push(("version", version.clone()));
            }
            if let Some(digest) = &found.policy_sha256 {
                preamble.push(("digest", digest.clone()));
            }
            if let Some(problem) = &found.problem {
                preamble.push(("problem", problem.clone()));
            }
            emit_record(
                &found,
                &categories,
                args.format,
                &preamble,
                "\nno correction policy: this skill defines no categories, so `--skill` cannot \
                 select it. Add corrections/policy.toml to give it rules.",
            )
        }
        SkillsCmd::Validate(args) => {
            let result = validate(&args.path)?;
            emit_validation(&result, args.format)?;
            if result.valid {
                return Ok(());
            }
            // A validator that exits 0 on invalid input cannot gate anything: every script using
            // it would report success. The report is already on stdout, so this only supplies the
            // verdict and the exit code.
            let count = result.diagnostics.len();
            anyhow::bail!(
                "{} is not a valid skill: {count} problem{} reported on stdout, each with its fix",
                args.path.display(),
                if count == 1 { "" } else { "s" }
            )
        }
        SkillsCmd::Create(args) => {
            let plan = SkillScaffoldPlan::new(config, &args.name, args.output_dir.as_deref())?;
            let receipt = if args.dry_run {
                plan.preview()
            } else {
                plan.publish()?
            };
            let files = receipt.files.clone();
            let verb = if receipt.created {
                "created"
            } else {
                "would create (--dry-run; nothing was written)"
            };
            emit_record(
                &receipt,
                &files,
                args.format,
                &[
                    ("skill", receipt.name.clone()),
                    (verb, receipt.root.clone()),
                ],
                "",
            )?;
            if receipt.created && args.format == OutputFormat::Table {
                println!(
                    "\nThis skill is yours: it carries no managed marker, so aise will never \
                     rewrite it.\nAdd {} to [skills].search_paths, then run \
                     `aise corrections --skill {}`.",
                    std::path::Path::new(&receipt.root)
                        .parent()
                        .unwrap_or_else(|| std::path::Path::new("."))
                        .display(),
                    receipt.name
                );
            }
            Ok(())
        }
        SkillsCmd::Update(args) => {
            let outcomes = crate::integrations::write_owned_skills(
                &args.skill_roots,
                receipt_path,
                args.dry_run,
                // Never overwrite changed bytes here. `update` is routine; destroying an edit
                // during a routine command is exactly the surprise `restore` exists to make
                // explicit instead.
                false,
            )?;
            report_write_outcomes(
                &outcomes,
                args.format,
                "No aise-owned skill was found to update.",
            )
        }
        SkillsCmd::Restore(args) => {
            if args.name != crate::corrections::EMBEDDED_POLICY_NAME {
                anyhow::bail!(
                    "cannot restore '{}': only '{}' has a copy embedded in this executable to \
                     restore from. A skill you wrote has no upstream aise could reinstall; edit \
                     it directly, or `aise skills create` a fresh one alongside it",
                    args.name,
                    crate::corrections::EMBEDDED_POLICY_NAME
                );
            }
            let outcomes = crate::integrations::write_owned_skills(
                std::slice::from_ref(&args.skill_root),
                receipt_path,
                args.dry_run,
                true,
            )?;
            report_write_outcomes(
                &outcomes,
                args.format,
                "Nothing to restore: that directory holds no aise-owned skill.",
            )
        }
    }
}

/// Print what a write verb did, and fail the process when nothing could be written.
///
/// A per-root `problem` is a refusal, not a crash: one hand-edited harness directory must not
/// stop the other three from being updated. But if EVERY root refused, the command achieved
/// nothing, and exiting 0 would tell a script it succeeded.
fn report_write_outcomes(
    outcomes: &[crate::integrations::SkillWriteOutcome],
    format: OutputFormat,
    empty_note: &str,
) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    match format {
        OutputFormat::Json | OutputFormat::Jsonl | OutputFormat::Csv | OutputFormat::Plain => {
            crate::render::render_record(&outcomes, outcomes, format, &mut out)?;
        }
        OutputFormat::Table => {
            if outcomes.is_empty() {
                writeln!(out, "{empty_note}")?;
            } else {
                render(outcomes, format, &mut out)?;
                for outcome in outcomes.iter().filter(|outcome| outcome.problem.is_some()) {
                    writeln!(out, "\n{}", outcome.root)?;
                    for line in outcome.problem.as_deref().unwrap_or_default().lines() {
                        writeln!(out, "  {line}")?;
                    }
                }
            }
        }
    }
    out.flush()?;

    let refused = outcomes.iter().filter(|o| o.problem.is_some()).count();
    if refused > 0 && refused == outcomes.len() {
        anyhow::bail!(
            "no skill directory could be written: all {refused} refused, each reported above"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(
        root: &Path,
        name: &str,
        frontmatter_name: &str,
        policy: Option<&str>,
    ) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {frontmatter_name}\ndescription: a test skill\n---\n\nbody\n"),
        )
        .unwrap();
        if let Some(policy) = policy {
            std::fs::create_dir_all(dir.join("corrections")).unwrap();
            std::fs::write(dir.join("corrections").join("policy.toml"), policy).unwrap();
        }
        dir
    }

    const VALID_POLICY: &str = "schema_version = 1\nname = \"team-rules\"\nversion = \"0.2.0\"\n\n\
                                [[categories]]\nname = \"clobber\"\npatterns = ['''\\byou overwrote\\b''']\n";

    #[test]
    fn frontmatter_reads_the_three_fields_this_build_needs() {
        let text = "---\nname: my-skill\ndescription: does a thing\nmetadata:\n  version: 1.2.3\n  author: someone\n---\n\nbody\n";
        let found = parse_frontmatter(text).expect("a fenced frontmatter block");
        assert_eq!(found.name.as_deref(), Some("my-skill"));
        assert_eq!(found.description.as_deref(), Some("does a thing"));
        assert_eq!(found.metadata_version.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn a_nested_version_is_only_read_under_metadata() {
        // `version:` indented under some OTHER key is not `metadata.version`, and reading it as
        // one would report a version the specification never declared.
        let text = "---\nname: my-skill\ncompatibility:\n  version: 9.9.9\n---\n";
        let found = parse_frontmatter(text).unwrap();
        assert_eq!(found.metadata_version, None);
    }

    #[test]
    fn a_document_without_a_fence_has_no_frontmatter() {
        assert!(parse_frontmatter("# Just a heading\n").is_none());
    }

    #[test]
    fn skill_names_follow_the_agent_skills_character_rules() {
        assert_eq!(skill_name_problem("my-skill"), None);
        assert_eq!(skill_name_problem("skill9"), None);
        for bad in [
            "",
            "My-Skill",
            "my_skill",
            "-lead",
            "trail-",
            "double--hyphen",
        ] {
            assert!(
                skill_name_problem(bad).is_some(),
                "{bad:?} must be rejected"
            );
        }
        assert!(skill_name_problem(&"a".repeat(65)).is_some());
    }

    #[test]
    fn list_reports_the_built_in_policy_and_every_discovered_skill() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("skills");
        write_skill(&root, "team-rules", "team-rules", Some(VALID_POLICY));
        write_skill(&root, "no-policy-skill", "no-policy-skill", None);

        let mut config = Config::default();
        config.skills.search_paths = vec![root.to_string_lossy().into_owned()];
        let rows = summaries(&config).unwrap();

        assert_eq!(
            rows.iter()
                .map(|row| (
                    row.name.as_str(),
                    row.ownership,
                    row.policy_status,
                    row.policy_version.as_deref()
                ))
                .collect::<Vec<_>>(),
            vec![
                (
                    crate::corrections::EMBEDDED_POLICY_NAME,
                    SkillOwnership::Aise,
                    SkillPolicyStatus::Ok,
                    // The POLICY's version, declared in corrections/policy.toml. Deliberately not
                    // the crate version: the rules version when the rules change, not when aise
                    // ships. Asserting `env!("CARGO_PKG_VERSION")` would pass today by
                    // coincidence and fail on the next release for no defect.
                    Some(embedded_policy_version())
                ),
                (
                    "no-policy-skill",
                    SkillOwnership::User,
                    SkillPolicyStatus::NoPolicy,
                    None
                ),
                (
                    "team-rules",
                    SkillOwnership::User,
                    SkillPolicyStatus::Ok,
                    Some("0.2.0")
                ),
            ],
            "the built-in policy leads, then discovered skills in sorted order"
        );
    }

    fn embedded_policy_version() -> &'static str {
        crate::corrections::EMBEDDED_POLICY_TOML
            .lines()
            .find_map(|line| line.strip_prefix("version = "))
            .map(|value| value.trim_matches('"'))
            .expect("the bundled policy declares a version")
    }

    /// The skill this repository ships must pass the validator this build runs on everyone else's.
    ///
    /// Three hand-maintained values have to agree -- the directory name, `SKILL.md` `name` and
    /// `metadata.version`, and the policy's `name`/`version` -- and nothing else forces them to.
    /// Without this, a version bump in one file and not the other ships a skill that `aise skills
    /// validate` rejects, which is exactly the diagnostic a user would report as a bug.
    #[test]
    fn the_bundled_skill_passes_this_build_s_own_validator() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("skills")
            .join(crate::corrections::EMBEDDED_POLICY_NAME);
        let result = validate(&root).unwrap();
        assert!(
            result.valid,
            "the skill this repo ships is invalid by its own rules: {:#?}",
            result.diagnostics
        );
        assert_eq!(
            result.ownership,
            SkillOwnership::Aise,
            "the bundled SKILL.md must carry the managed marker, or install refuses to update it"
        );
    }

    #[test]
    fn one_broken_policy_does_not_hide_the_other_skills() {
        // The failure mode this guards: propagating the first parse error empties the listing, so
        // `skills list` looks like it found nothing rather than like one file is malformed.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("skills");
        write_skill(
            &root,
            "aaa-broken",
            "aaa-broken",
            Some("schema_version = 99\n"),
        );
        write_skill(&root, "zzz-fine", "zzz-fine", Some(VALID_POLICY));

        let mut config = Config::default();
        config.skills.search_paths = vec![root.to_string_lossy().into_owned()];
        let rows = summaries(&config).unwrap();

        let broken = rows.iter().find(|row| row.name == "aaa-broken").unwrap();
        assert_eq!(broken.policy_status, SkillPolicyStatus::Invalid);
        assert!(
            broken
                .problem
                .as_deref()
                .is_some_and(|text| text.contains("schema_version")),
            "the row must say what is wrong: {:?}",
            broken.problem
        );
        assert!(
            rows.iter()
                .any(|row| row.name == "zzz-fine" && row.policy_status == SkillPolicyStatus::Ok),
            "a later, valid skill must still be listed"
        );
    }

    #[test]
    fn show_lists_categories_in_evaluation_order_with_the_regex_that_runs() {
        let detail = detail(&Config::default(), crate::corrections::EMBEDDED_POLICY_NAME).unwrap();
        assert_eq!(
            detail
                .categories
                .iter()
                .map(|row| (row.order, row.category.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (1, "regression"),
                (2, "skip_step"),
                (3, "misunderstanding"),
                (4, "incomplete"),
                (5, "other"),
            ],
            "`other` is a deliberate last catch-all; order IS behavior"
        );
        assert!(
            detail.categories[0].regex.starts_with("(?i)"),
            "one case-insensitive alternation per category is what actually runs"
        );
    }

    #[test]
    fn validate_reports_every_problem_at_once_and_names_a_fix_for_each() {
        let dir = tempfile::tempdir().unwrap();
        // Directory name, frontmatter name, and policy name all disagree, and the description is
        // absent: four independent problems in one directory.
        let root = dir.path().join("skills");
        let skill = root.join("Wrong_Name");
        std::fs::create_dir_all(skill.join("corrections")).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: other-name\n---\n\nbody\n",
        )
        .unwrap();
        std::fs::write(skill.join("corrections").join("policy.toml"), VALID_POLICY).unwrap();

        let result = validate(&skill).unwrap();
        assert!(!result.valid);
        assert!(
            result.diagnostics.iter().all(|d| !d.fix.is_empty()),
            "every diagnostic must name a fix, not only refuse"
        );
        let problems = result
            .diagnostics
            .iter()
            .map(|d| d.problem.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            problems.contains("does not match the directory name"),
            "{problems}"
        );
        assert!(problems.contains("no `description`"), "{problems}");
        assert!(
            problems.contains("does not match the SKILL.md name"),
            "{problems}"
        );
    }

    /// The strongest scaffold test there is: what `create` writes must pass what `validate`
    /// checks. A scaffold its own tool rejects sends every new author straight into a diagnostic.
    #[test]
    fn a_scaffolded_skill_passes_this_build_s_own_validator() {
        let dir = tempfile::tempdir().unwrap();
        let plan =
            SkillScaffoldPlan::new(&Config::default(), "my-rules", Some(dir.path())).unwrap();
        let receipt = plan.publish().unwrap();
        assert!(receipt.created);

        let result = validate(&dir.path().join("my-rules")).unwrap();
        assert!(
            result.valid,
            "the scaffold must be valid by this build's own rules: {:#?}",
            result.diagnostics
        );
        assert_eq!(
            result.ownership,
            SkillOwnership::User,
            "a scaffold belongs to whoever ran the command, so it must carry no managed marker"
        );
    }

    /// The seeded policy must be the categories `aise` actually ships, in order.
    #[test]
    fn a_scaffold_is_seeded_with_the_current_built_in_categories() {
        let dir = tempfile::tempdir().unwrap();
        SkillScaffoldPlan::new(&Config::default(), "my-rules", Some(dir.path()))
            .unwrap()
            .publish()
            .unwrap();
        let text =
            std::fs::read_to_string(dir.path().join("my-rules/corrections/policy.toml")).unwrap();
        let policy = CorrectionPolicy::parse_toml(&text, CorrectionPolicySource::Embedded).unwrap();
        assert_eq!(
            policy
                .rules()
                .iter()
                .map(|(category, _)| category.as_str())
                .collect::<Vec<_>>(),
            crate::analytics::default_correction_patterns()
                .iter()
                .map(|(category, _)| *category)
                .collect::<Vec<_>>(),
            "seeded categories must match the shipped defaults, in evaluation order"
        );
    }

    #[test]
    fn a_dry_run_previews_the_same_files_it_would_write_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let plan =
            SkillScaffoldPlan::new(&Config::default(), "my-rules", Some(dir.path())).unwrap();
        let preview = plan.preview();
        assert!(!preview.created);
        assert_eq!(
            preview
                .files
                .iter()
                .map(|file| file.relative_path.as_str())
                .collect::<Vec<_>>(),
            vec!["SKILL.md", "corrections/policy.toml"]
        );
        drop(plan);
        assert!(
            !dir.path().join("my-rules").exists(),
            "a preview must not create the directory"
        );
    }

    /// Refusing an existing destination is the whole safety property: creating INTO one could
    /// overwrite files the caller wrote.
    #[test]
    fn create_refuses_an_existing_destination_without_touching_it() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("my-rules");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(existing.join("mine.txt"), "do not lose me").unwrap();

        let error = SkillScaffoldPlan::new(&Config::default(), "my-rules", Some(dir.path()))
            .expect_err("an existing destination must be refused");
        let message = format!("{error:#}");
        assert!(
            message.contains("already exists") && message.contains("--output-dir"),
            "the refusal must name the cause and a way forward: {message}"
        );
        assert_eq!(
            std::fs::read_to_string(existing.join("mine.txt")).unwrap(),
            "do not lose me"
        );
    }

    #[test]
    fn create_refuses_invalid_and_reserved_names() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["My_Rules", "-leading", "double--hyphen", ""] {
            assert!(
                SkillScaffoldPlan::new(&Config::default(), bad, Some(dir.path())).is_err(),
                "{bad:?} is not a valid skill name"
            );
        }
        let error = SkillScaffoldPlan::new(
            &Config::default(),
            crate::corrections::EMBEDDED_POLICY_NAME,
            Some(dir.path()),
        )
        .expect_err("the reserved name cannot be shadowed");
        assert!(format!("{error:#}").contains("reserved"));
    }

    /// Omitting both `--output-dir` and `[skills].authoring_root` must say what to do, not guess
    /// a destination: writing a new directory into the current working directory by default is
    /// exactly the kind of surprise that leaves junk behind.
    #[test]
    fn create_without_any_destination_names_both_ways_to_supply_one() {
        let error = SkillScaffoldPlan::new(&Config::default(), "my-rules", None)
            .expect_err("no destination is configured by default");
        let message = format!("{error:#}");
        assert!(message.contains("--output-dir"), "{message}");
        assert!(message.contains("authoring_root"), "{message}");
    }

    #[test]
    fn a_configured_authoring_root_supplies_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.skills.authoring_root = Some(dir.path().to_string_lossy().into_owned());
        let plan = SkillScaffoldPlan::new(&config, "my-rules", None).unwrap();
        assert_eq!(plan.root(), dir.path().join("my-rules"));
    }

    /// Boundary shapes a policy file can take that are not "valid TOML" or "invalid TOML".
    ///
    /// Each must be REPORTED, never panic and never silently read as "no categories": a skill
    /// that quietly defines nothing looks identical to one that matched nothing.
    #[test]
    fn a_policy_file_that_is_empty_a_directory_or_not_utf8_is_reported_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("skills");

        // Empty file: parses as TOML but declares nothing, so compilation must refuse it.
        let empty = write_skill(&root, "empty-policy", "empty-policy", Some(""));
        let summary = summarize(&DiscoveredSkill {
            name: "empty-policy".into(),
            root: empty.clone(),
            policy_path: Some(empty.join("corrections/policy.toml")),
        });
        assert_eq!(summary.policy_status, SkillPolicyStatus::Invalid);
        assert!(
            summary.problem.is_some(),
            "an empty policy must say what is missing"
        );

        // A directory where a file belongs: `is_file()` is false, so discovery reports no policy
        // rather than trying to read it.
        let as_dir = write_skill(&root, "dir-policy", "dir-policy", None);
        std::fs::create_dir_all(as_dir.join("corrections/policy.toml")).unwrap();
        let discovered = discover_skills_in(&root).unwrap();
        let entry = discovered
            .iter()
            .find(|skill| skill.name == "dir-policy")
            .expect("the skill is still listed");
        assert!(
            entry.policy_path.is_none(),
            "a directory named policy.toml is not a policy"
        );

        // Non-UTF-8 bytes: reported as a read failure, not a panic and not an empty policy.
        #[cfg(unix)]
        {
            let invalid = write_skill(&root, "binary-policy", "binary-policy", Some(""));
            let path = invalid.join("corrections/policy.toml");
            std::fs::write(&path, [0xff_u8, 0xfe, 0xfd]).unwrap();
            let summary = summarize(&DiscoveredSkill {
                name: "binary-policy".into(),
                root: invalid,
                policy_path: Some(path),
            });
            assert_eq!(summary.policy_status, SkillPolicyStatus::Invalid);
            assert!(
                summary
                    .problem
                    .as_deref()
                    .is_some_and(|text| text.contains("failed to read")),
                "{:?}",
                summary.problem
            );
        }
    }

    /// A dangling symlink where a skill directory would be is not a skill, and not a crash.
    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_in_a_search_path_is_skipped_rather_than_failing_the_listing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("skills");
        write_skill(&root, "real-skill", "real-skill", Some(VALID_POLICY));
        std::os::unix::fs::symlink(dir.path().join("nowhere"), root.join("broken")).unwrap();

        let discovered = discover_skills_in(&root).unwrap();
        assert_eq!(
            discovered
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            vec!["real-skill"],
            "one broken entry must not hide the listing or fail it"
        );
    }

    /// A skill directory reached through a symlink is the SAME skill, not a duplicate name.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_root_dedupes_against_its_target_instead_of_colliding() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        write_skill(&real, "team-rules", "team-rules", Some(VALID_POLICY));
        let linked = dir.path().join("linked");
        std::os::unix::fs::symlink(&real, &linked).unwrap();

        let mut config = Config::default();
        config.skills.search_paths = vec![
            real.to_string_lossy().into_owned(),
            linked.to_string_lossy().into_owned(),
        ];
        // Canonicalization makes both paths one skill; without it this is a duplicate-name error.
        let discovered = crate::corrections::discover_skills(&config.skills.search_paths).unwrap();
        assert_eq!(discovered.len(), 1, "{discovered:#?}");
    }

    /// `validate` on a path that does not exist must say so, not report a valid empty skill.
    #[test]
    fn validating_a_missing_directory_is_not_silently_valid() {
        let dir = tempfile::tempdir().unwrap();
        let result = validate(&dir.path().join("no-such-skill")).unwrap();
        assert!(!result.valid);
        assert!(result.diagnostics[0].problem.contains("not a directory"));
    }

    #[test]
    fn tabular_diagnostic_cells_are_one_line_and_lose_nothing() {
        // `plain` is tab-separated and `csv` is line-oriented, so an embedded newline splits one
        // diagnostic into two malformed records -- and a TOML parse error is naturally several
        // lines long. Truncating instead would drop the fix, the only part a caller can act on.
        let diagnostic = SkillDiagnostic {
            file: "corrections/policy.toml".into(),
            problem: "TOML parse error at line 4\n  |\n4 | weights = 3\n  | ^^^^^".into(),
            fix: "correct the field named above;\nthen re-run".into(),
        };
        for cell in diagnostic.cells() {
            assert!(
                !cell.contains('\n') && !cell.contains('\r'),
                "every tabular cell must be one line: {cell:?}"
            );
        }
        assert!(
            diagnostic.cells()[2].ends_with("then re-run"),
            "the fix must survive intact, not be truncated"
        );
    }

    #[test]
    fn validate_accepts_a_well_formed_skill() {
        let dir = tempfile::tempdir().unwrap();
        let skill = write_skill(dir.path(), "team-rules", "team-rules", Some(VALID_POLICY));
        let result = validate(&skill).unwrap();
        assert!(
            result.valid,
            "expected no diagnostics, got {:#?}",
            result.diagnostics
        );
        assert_eq!(result.ownership, SkillOwnership::User);
    }

    #[test]
    fn validating_a_file_says_to_pass_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let skill = write_skill(dir.path(), "team-rules", "team-rules", None);
        let result = validate(&skill.join("SKILL.md")).unwrap();
        assert!(!result.valid);
        assert!(result.diagnostics[0].fix.contains("skill directory"));
    }
}
