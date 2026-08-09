// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

//! `aise skills` — see every discovered skill, explain one, and validate a directory.
//!
//! `aise` must **see** every skill and **write** only what it owns. These three verbs are the
//! read-only half of that rule: they report aise-managed and user-authored skills alike, and they
//! never modify a directory. Ownership is reported, never assumed.
//!
//! Standard skill discovery and frontmatter parsing live in [`crate::skill_catalog`].
//! Capability parsing stays in its domain module; this module only adapts those shared results to
//! the command surface and renders rows.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::config::Config;
use crate::corrections::CorrectionPolicy;
use crate::render::{render, OutputFormat, Row};
use crate::skill_catalog::{
    load_skill_catalog, load_skill_descriptor, skill_name_problem, CapabilityFileState,
    SkillDescriptor, SkillRootState, MAX_NAME_CHARS,
};
use crate::util::truncate_for_display;
/// Compiled category regexes are one alternation over every pattern, so they get long. Table
/// output truncates; `--format json` always carries the full source.
const TABLE_REGEX_CHARS: usize = 72;

#[derive(Debug, Subcommand)]
pub enum SkillsCmd {
    /// List every discovered skill — aise-managed and user-authored alike — and whether its
    /// adjacent deterministic capability loads.
    ///
    /// Use `aise skills show` for one package's detail, or `aise skills validate` to check it
    /// parses.
    #[command(
        after_help = "Skills come from `[skills].search_paths` plus the built-in \
                            `ai-session-search` harness guidance and `corrections` capability. \
                            Diagnose one with `aise skills validate <path>`."
    )]
    List(SkillsListArgs),
    /// Explain one skill: where it resolved from and, when present, the categories its adjacent
    /// deterministic capability evaluates in order.
    ///
    /// Use `aise skills list` to find the name, or `aise skills run` to execute a capability.
    Show(SkillsShowArgs),
    /// Check one skill directory's frontmatter and adjacent capability, naming the fix for each
    /// problem rather than only refusing.
    ///
    /// Use `aise skills show` to read what parsed, or `aise skills update` after fixing it.
    Validate(SkillsValidateArgs),
    /// Scaffold a new harness-only skill directory you own.
    ///
    /// Use `aise skills validate` on the result, then `aise skills run` to execute it.
    #[command(
        after_help = "The scaffold is YOURS: it carries no managed marker, so \
                            `aise integrations install` and `aise skills update` will never \
                            rewrite it. Add `--capability message-classification` only when Aise \
                            should execute deterministic categories via `aise skills <name>`."
    )]
    Create(SkillsCreateArgs),
    /// Bring aise-owned installed skills up to this build's content. User-authored skills are
    /// only diagnosed, never rewritten.
    ///
    /// Use `aise skills validate` afterwards to confirm the package still parses.
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
    /// Run a read-only deterministic capability when its name collides with a management verb.
    ///
    /// Use `aise skills list` to find the capability name, or `aise skills show` for its parameters.
    #[command(
        after_help = "Selected packaged and direct capability definitions share a 1 MiB aggregate \
                      parsing safety ceiling. Exceeding it returns the consumed and attempted byte \
                      counts with guidance; Aise never truncates rules or results to fit."
    )]
    Run(SkillRunEscapeArgs),
    /// Run the named/path-selected skill's read-only deterministic capability.
    #[command(external_subcommand)]
    Inferred(Vec<OsString>),
}

#[derive(Debug, Args)]
pub struct SkillRunEscapeArgs {
    /// Skill name, skill directory, or exact SKILL.md path.
    pub selector: OsString,
    /// Capability-specific arguments parsed after the skill has been resolved.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub capability_args: Vec<OsString>,
}

#[derive(Debug)]
pub(crate) struct SkillExecution {
    pub(crate) selector: crate::skill_catalog::SkillSelector,
    pub(crate) args: crate::analytics::CorrectionsArgs,
}

#[derive(Debug, Parser)]
#[command(
    disable_help_subcommand = true,
    after_help = "Selected packaged and direct capability definitions share a 1 MiB aggregate \
                  parsing safety ceiling. Exceeding it returns the consumed and attempted byte \
                  counts with guidance; Aise never truncates rules or results to fit."
)]
struct MessageClassificationCommand {
    #[command(flatten)]
    args: crate::analytics::CorrectionsArgs,
}

impl SkillsCmd {
    pub(crate) fn is_management(&self) -> bool {
        matches!(
            self,
            Self::List(_)
                | Self::Show(_)
                | Self::Validate(_)
                | Self::Create(_)
                | Self::Update(_)
                | Self::Restore(_)
        )
    }

    /// Render second-stage capability help before configuration or the database is opened.
    ///
    /// Clap cannot parse an external subcommand's capability-specific flags in the root parser.
    /// Display-help is a successful control-flow outcome, so routing it through `anyhow` would
    /// incorrectly prefix it with `error:`, write it to stderr, and exit nonzero.
    pub(crate) fn print_execution_help_if_requested(&self) -> Result<bool> {
        let capability_args = match self {
            Self::Run(args) => args.capability_args.as_slice(),
            Self::Inferred(args) if !args.is_empty() => &args[1..],
            _ => return Ok(false),
        };
        if !capability_args
            .iter()
            .any(|arg| arg == "-h" || arg == "--help")
        {
            return Ok(false);
        }

        match MessageClassificationCommand::try_parse_from(
            std::iter::once(OsString::from("aise-skill-capability"))
                .chain(capability_args.iter().cloned()),
        ) {
            Err(error) if error.kind() == clap::error::ErrorKind::DisplayHelp => {
                error.print()?;
                Ok(true)
            }
            Err(error) => Err(anyhow::anyhow!(error.to_string())),
            Ok(_) => Ok(false),
        }
    }

    pub(crate) fn into_execution(self) -> Result<SkillExecution> {
        let (selector, capability_args) = match self {
            Self::Run(args) => (args.selector, args.capability_args),
            Self::Inferred(mut args) => {
                if args.is_empty() {
                    bail!("`aise skills` requires a management command or skill selector");
                }
                let selector = args.remove(0);
                (selector, args)
            }
            _ => bail!("skill management commands do not execute deterministic capabilities"),
        };
        let selector = parse_skill_selector(selector)?;
        let args = MessageClassificationCommand::try_parse_from(
            std::iter::once(OsString::from("aise-skill-capability")).chain(capability_args),
        )
        .map_err(|error| anyhow::anyhow!(error.to_string()))?
        .args;
        Ok(SkillExecution { selector, args })
    }
}

pub(crate) fn parse_skill_selector(value: OsString) -> Result<crate::skill_catalog::SkillSelector> {
    let path = PathBuf::from(&value);
    let rendered = value.to_string_lossy();
    let explicit_path = path.is_absolute()
        || rendered == "SKILL.md"
        || rendered.starts_with("./")
        || rendered.starts_with("../")
        || rendered.starts_with("~/")
        || rendered.contains(std::path::MAIN_SEPARATOR)
        || (cfg!(windows)
            && (rendered.contains('\\')
                || rendered.starts_with(r"\\")
                || rendered.get(1..2) == Some(":")));
    if explicit_path {
        return Ok(crate::skill_catalog::SkillSelector::Path(
            crate::skill_catalog::SkillPathSelector { path },
        ));
    }
    let name = value
        .into_string()
        .map_err(|_| anyhow::anyhow!("skill names must be valid UTF-8; use an explicit path"))?;
    Ok(crate::skill_catalog::SkillSelector::Name(
        crate::skill_catalog::SkillNameSelector {
            name: crate::skill_catalog::SkillName::try_from(name).map_err(anyhow::Error::msg)?,
        },
    ))
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
    /// Capability file to create. Accepts `message-classification`, which adds aise-capability.toml
    /// seeded with the built-in ordered categories; omit this flag to create only SKILL.md for
    /// agent-harness use.
    #[arg(long, value_enum)]
    pub capability: Option<ScaffoldCapability>,
    /// Print what would be created without creating anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ScaffoldCapability {
    MessageClassification,
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

/// Whether a discovered skill can currently supply a deterministic capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkillCapabilityStatus {
    /// `aise-capability.toml` is present and compiles.
    Ok,
    /// No `aise-capability.toml`. A valid harness-only skill.
    HarnessOnly,
    /// A capability file is present but does not load. `problem` says why.
    Invalid,
}

impl SkillCapabilityStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::HarnessOnly => "harness-only",
            Self::Invalid => "invalid",
        }
    }
}

/// One skill as `aise skills list` reports it.
#[derive(Debug, Clone, Serialize)]
pub struct SkillSummary {
    pub name: String,
    pub ownership: SkillOwnership,
    pub capability_status: SkillCapabilityStatus,
    /// Package version from `SKILL.md metadata.version`. Absent for harness-only or invalid skills.
    pub package_version: Option<String>,
    /// Digest of the exact capability bytes. Absent when there is no loadable capability.
    pub capability_sha256: Option<String>,
    pub category_count: Option<usize>,
    pub path: String,
    /// Why `capability_status` is `invalid`. Table output shows only the status token, so this is
    /// where `--format json` keeps the detail a caller needs to act.
    pub problem: Option<String>,
}

impl Row for SkillSummary {
    fn headers() -> &'static [&'static str] {
        &[
            "skill",
            "owner",
            "capability",
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
            self.capability_status.as_str().to_string(),
            self.package_version.clone().unwrap_or_else(|| "-".into()),
            self.capability_sha256
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
    pub capability_status: SkillCapabilityStatus,
    pub package_version: Option<String>,
    pub capability_sha256: Option<String>,
    pub capability_source: Option<crate::skill_run::CapabilityExecutionSource>,
    /// Categories in evaluation order. Empty when no capability loaded.
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

/// Read `SKILL.md` and decide who owns the directory.
fn ownership_of(root: &Path) -> SkillOwnership {
    match std::fs::read_to_string(root.join("SKILL.md")) {
        Ok(text) if crate::integrations::is_managed_skill_anchor(&text) => SkillOwnership::Aise,
        Ok(_) => SkillOwnership::User,
        Err(_) => SkillOwnership::Unknown,
    }
}

/// Load one descriptor's adjacent capability, keeping the failure rather than propagating it.
///
/// `skills list` must show every skill it can see even when one is broken: propagating the first
/// error would hide every other row, and a listing that disappears because one file is malformed
/// looks like the tool is broken rather than the file.
fn load_policy(skill: &SkillDescriptor) -> Result<Option<CorrectionPolicy>, String> {
    if !skill.diagnostics.is_empty() {
        return Err(skill.diagnostics.join("; "));
    }
    let path = match &skill.capability {
        CapabilityFileState::Absent => return Ok(None),
        CapabilityFileState::Available { path } => path,
        CapabilityFileState::Invalid { problem, .. } => return Err(problem.clone()),
    };
    let frontmatter = skill
        .frontmatter
        .as_ref()
        .ok_or_else(|| "SKILL.md has no valid frontmatter".to_string())?;
    let version = frontmatter
        .metadata
        .get("version")
        .cloned()
        .ok_or_else(|| "runnable skills must declare `metadata.version` in SKILL.md".to_string())?;
    crate::message_classification::load_and_compile_with_budget(
        path,
        frontmatter.name.clone(),
        version,
        &mut crate::message_classification::CapabilityLoadBudget::new(),
    )
    .map(Some)
    .map_err(|error| format!("{error:#}"))
}

fn summarize(skill: &SkillDescriptor) -> SkillSummary {
    let ownership = ownership_of(&skill.root);
    let path = skill.root.display().to_string();
    let name = skill.frontmatter.as_ref().map_or_else(
        || skill.directory_name.clone(),
        |frontmatter| frontmatter.name.clone(),
    );
    match load_policy(skill) {
        Ok(Some(policy)) => {
            let identity = policy.identity();
            SkillSummary {
                name,
                ownership,
                capability_status: SkillCapabilityStatus::Ok,
                package_version: Some(identity.version.clone()),
                capability_sha256: Some(identity.sha256.clone()),
                category_count: Some(policy.category_count()),
                path,
                problem: None,
            }
        }
        Ok(None) => SkillSummary {
            name,
            ownership,
            capability_status: SkillCapabilityStatus::HarnessOnly,
            package_version: None,
            capability_sha256: None,
            category_count: None,
            path,
            problem: None,
        },
        Err(problem) => SkillSummary {
            name,
            ownership,
            capability_status: SkillCapabilityStatus::Invalid,
            package_version: None,
            capability_sha256: None,
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

fn descriptor_matches_name(skill: &SkillDescriptor, name: &str) -> bool {
    skill
        .frontmatter
        .as_ref()
        .is_some_and(|frontmatter| frontmatter.name == name)
        || skill.directory_name == name
}

fn canonical_skill_catalog(receipt_path: &Path) -> crate::skill_catalog::SkillCatalog {
    let app_root = receipt_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    load_skill_catalog(&[app_root.join("skills")])
}

fn installed_builtin<'a>(
    configured: &crate::skill_catalog::SkillCatalog,
    canonical: &'a crate::skill_catalog::SkillCatalog,
    name: &str,
) -> Option<&'a SkillDescriptor> {
    if name == crate::corrections::EMBEDDED_POLICY_NAME {
        return None;
    }
    if configured
        .skills
        .iter()
        .any(|skill| descriptor_matches_name(skill, name))
    {
        return None;
    }
    canonical
        .skills
        .iter()
        .find(|skill| descriptor_matches_name(skill, name))
}

fn installed_builtin_summary(skill: &SkillDescriptor) -> SkillSummary {
    let mut summary = summarize(skill);
    if summary.package_version.is_none() {
        summary.package_version = skill
            .frontmatter
            .as_ref()
            .and_then(|frontmatter| frontmatter.metadata.get("version"))
            .cloned();
    }
    summary
}

/// Every skill `aise` can see: the embedded policy first, then the search paths.
///
/// The embedded row is synthesized rather than discovered, because it has no directory. Listing it
/// matters: it is what `corrections` uses by default, so a listing that omitted it would answer
/// "which rules run?" with everything except the answer.
fn summaries_at(config: &Config, receipt_path: Option<&Path>) -> Result<Vec<SkillSummary>> {
    let embedded = crate::corrections::embedded_policy()?;
    let embedded_identity = embedded.identity();
    let mut rows = vec![
        SkillSummary {
            name: embedded_identity.name.clone(),
            ownership: SkillOwnership::Aise,
            capability_status: SkillCapabilityStatus::Ok,
            package_version: Some(embedded_identity.version.clone()),
            capability_sha256: Some(embedded_identity.sha256.clone()),
            category_count: Some(embedded.category_count()),
            path: "(built in)".to_string(),
            problem: None,
        },
        SkillSummary {
            name: crate::integrations::AI_SESSION_SEARCH_SKILL_NAME.to_string(),
            ownership: SkillOwnership::Aise,
            capability_status: SkillCapabilityStatus::HarnessOnly,
            package_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            capability_sha256: None,
            category_count: None,
            path: "(built in)".to_string(),
            problem: None,
        },
    ];
    let search_roots = config
        .skills
        .search_paths
        .iter()
        .map(|path| crate::util::expand_tilde(path))
        .collect::<Vec<_>>();
    let catalog = load_skill_catalog(&search_roots);
    let canonical = receipt_path.map(canonical_skill_catalog);
    if let Some(status) = catalog
        .roots
        .iter()
        .find(|status| status.state == SkillRootState::Unreadable)
    {
        bail!(
            "failed to read skill search path {}: {}",
            status.configured_path.display(),
            status.problem.as_deref().unwrap_or("unreadable skill root")
        );
    }
    if let Some(canonical) = &canonical {
        if let Some(skill) = installed_builtin(
            &catalog,
            canonical,
            crate::corrections::EMBEDDED_POLICY_NAME,
        ) {
            rows[0] = installed_builtin_summary(skill);
        }
        if let Some(skill) = installed_builtin(
            &catalog,
            canonical,
            crate::integrations::AI_SESSION_SEARCH_SKILL_NAME,
        ) {
            rows[1] = installed_builtin_summary(skill);
        }
    }
    for skill in &catalog.skills {
        // The reserved name cannot be shadowed, so a directory claiming it is reported at its
        // real path and marked, rather than silently omitted or silently winning.
        if skill
            .frontmatter
            .as_ref()
            .is_some_and(|frontmatter| frontmatter.name == crate::corrections::EMBEDDED_POLICY_NAME)
        {
            let mut row = summarize(skill);
            row.capability_status = SkillCapabilityStatus::Invalid;
            row.problem = Some(format!(
                "'{}' is reserved for the built-in policy and cannot be selected from disk; \
                     rename this directory and its SKILL.md name to use it",
                crate::corrections::EMBEDDED_POLICY_NAME
            ));
            rows.push(row);
            continue;
        }
        rows.push(summarize(skill));
    }
    Ok(rows)
}

fn detail_at(config: &Config, name: &str, receipt_path: Option<&Path>) -> Result<SkillDetail> {
    let roots = config
        .skills
        .search_paths
        .iter()
        .map(|path| crate::util::expand_tilde(path))
        .collect::<Vec<_>>();
    let catalog = load_skill_catalog(&roots);
    let canonical = receipt_path.map(canonical_skill_catalog);
    let installed = canonical
        .as_ref()
        .and_then(|canonical| installed_builtin(&catalog, canonical, name));

    if name == crate::integrations::AI_SESSION_SEARCH_SKILL_NAME {
        if let Some(skill) = installed {
            return Ok(installed_builtin_detail(skill, None));
        }
        return Ok(SkillDetail {
            name: name.to_string(),
            path: "(built in)".to_string(),
            ownership: SkillOwnership::Aise,
            capability_status: SkillCapabilityStatus::HarnessOnly,
            package_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            capability_sha256: None,
            capability_source: None,
            categories: Vec::new(),
            problem: None,
        });
    }
    if name == crate::corrections::EMBEDDED_POLICY_NAME {
        if let Some(skill) = installed {
            return Ok(installed_builtin_detail(
                skill,
                Some(crate::skill_run::CapabilityExecutionSource::Embedded),
            ));
        }
        let policy = crate::corrections::embedded_policy()?;
        let identity = policy.identity();
        return Ok(SkillDetail {
            name: identity.name.clone(),
            path: "(built in)".to_string(),
            ownership: SkillOwnership::Aise,
            capability_status: SkillCapabilityStatus::Ok,
            package_version: Some(identity.version.clone()),
            capability_sha256: Some(identity.sha256.clone()),
            capability_source: Some(crate::skill_run::CapabilityExecutionSource::Embedded),
            categories: category_rows(&policy),
            problem: None,
        });
    }

    let matches = catalog
        .skills
        .iter()
        .filter(|skill| descriptor_matches_name(skill, name))
        .collect::<Vec<_>>();
    let skill = match matches.as_slice() {
        [] => {
            bail!(
                "unknown skill '{name}'; run `aise skills list` to see discovered skills, or add \
                 its parent directory to [skills].search_paths"
            )
        }
        [skill] => *skill,
        duplicates => {
            let mut locations = duplicates
                .iter()
                .map(|skill| skill.root.display().to_string())
                .collect::<Vec<_>>();
            locations.sort();
            bail!(
                "skill name {name:?} is ambiguous across {}; pass a unique skill name after \
                 removing the duplicate identity",
                locations.join(", ")
            )
        }
    };
    Ok(detail_from_descriptor(skill, None))
}

fn installed_builtin_detail(
    skill: &SkillDescriptor,
    capability_source_override: Option<crate::skill_run::CapabilityExecutionSource>,
) -> SkillDetail {
    let mut detail = detail_from_descriptor(skill, capability_source_override);
    if detail.package_version.is_none() {
        detail.package_version = skill
            .frontmatter
            .as_ref()
            .and_then(|frontmatter| frontmatter.metadata.get("version"))
            .cloned();
    }
    detail
}

fn detail_from_descriptor(
    skill: &SkillDescriptor,
    capability_source_override: Option<crate::skill_run::CapabilityExecutionSource>,
) -> SkillDetail {
    let summary = summarize(skill);
    let categories = match load_policy(skill) {
        Ok(Some(policy)) => category_rows(&policy),
        _ => Vec::new(),
    };
    let capability_source = capability_source_override.or_else(|| match &skill.capability {
        CapabilityFileState::Available { path } => {
            Some(crate::skill_run::CapabilityExecutionSource::Path {
                canonical_capability_toml: path.clone(),
            })
        }
        CapabilityFileState::Absent | CapabilityFileState::Invalid { .. } => None,
    });
    SkillDetail {
        name: summary.name,
        path: summary.path,
        ownership: summary.ownership,
        capability_status: summary.capability_status,
        package_version: summary.package_version,
        capability_sha256: summary.capability_sha256,
        capability_source,
        categories,
        problem: summary.problem,
    }
}

/// Check one directory, collecting every problem rather than stopping at the first.
///
/// Reporting all of them at once is the difference between one fix-and-rerun cycle and five.
fn validate(path: &Path) -> Result<SkillValidation> {
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

    let descriptor = match load_skill_descriptor(path) {
        Ok(descriptor) => descriptor,
        Err(error) => {
            return Ok(SkillValidation {
                path: display,
                name: path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string),
                ownership: SkillOwnership::Unknown,
                valid: false,
                diagnostics: vec![SkillDiagnostic {
                    file: ".".to_string(),
                    problem: format!("{error:#}"),
                    fix: "pass a readable skill directory containing a regular SKILL.md"
                        .to_string(),
                }],
            });
        }
    };
    let mut diagnostics = descriptor
        .diagnostics
        .iter()
        .map(|problem| {
            let capability = problem.contains(crate::skill_catalog::CAPABILITY_FILE);
            SkillDiagnostic {
                file: if capability {
                    crate::skill_catalog::CAPABILITY_FILE
                } else {
                    "SKILL.md"
                }
                .to_string(),
                problem: problem.clone(),
                fix: if capability {
                    format!(
                        "make {} a readable regular file, or remove it for a harness-only skill",
                        crate::skill_catalog::CAPABILITY_FILE
                    )
                } else {
                    "correct SKILL.md YAML frontmatter so name matches the directory and \
                     description is valid"
                        .to_string()
                },
            }
        })
        .collect::<Vec<_>>();
    match &descriptor.capability {
        CapabilityFileState::Available { path: _ } if descriptor.diagnostics.is_empty() => {
            if let Err(problem) = load_policy(&descriptor) {
                diagnostics.push(SkillDiagnostic {
                    file: crate::skill_catalog::CAPABILITY_FILE.to_string(),
                    problem,
                    fix: "correct the capability field named above; compare with the built-in ai-session-search/aise-capability.toml"
                        .to_string(),
                });
            }
        }
        CapabilityFileState::Available { path } => {
            let result = crate::message_classification::load_and_compile_with_budget(
                path,
                descriptor.directory_name.clone(),
                "validation".to_string(),
                &mut crate::message_classification::CapabilityLoadBudget::new(),
            );
            if let Err(error) = result {
                diagnostics.push(SkillDiagnostic {
                    file: crate::skill_catalog::CAPABILITY_FILE.to_string(),
                    problem: format!("{error:#}"),
                    fix: "correct the capability field named above; compare with the built-in ai-session-search/aise-capability.toml"
                        .to_string(),
                });
            }
        }
        CapabilityFileState::Absent | CapabilityFileState::Invalid { .. } => {}
    }
    let name = descriptor.frontmatter.as_ref().map_or_else(
        || Some(descriptor.directory_name.clone()),
        |frontmatter| Some(frontmatter.name.clone()),
    );

    Ok(SkillValidation {
        path: display,
        name,
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
    /// Plan a scaffold under `output_dir`, optionally seeded with the built-in capability.
    ///
    /// # Errors
    ///
    /// Returns an error when the name is not a valid skill name, when neither `--output-dir` nor
    /// `[skills].authoring_root` names a destination, when the destination cannot be made
    /// absolute, or when it already exists.
    pub fn new(
        config: &Config,
        name: &str,
        output_dir: Option<&Path>,
        capability: Option<ScaffoldCapability>,
    ) -> Result<Self> {
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
            files: scaffold_files(name, capability),
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

/// Files for a new harness-only skill, plus an optional deterministic capability.
fn scaffold_files(name: &str, capability: Option<ScaffoldCapability>) -> Vec<(PathBuf, String)> {
    // Deliberately WITHOUT the managed marker: this copy belongs to whoever ran the command, and
    // its absence is what stops `aise skills update` and `integrations install` from rewriting it.
    let skill_md = format!(
        "---\n\
         name: {name}\n\
         description: Instructions for {name}. Use when a task needs this skill's specialized \
         workflow or domain guidance.\n\
         metadata:\n\
         \x20 version: 0.1.0\n\
         ---\n\
         \n\
         # {name}\n\
         \n\
         Add the instructions an agent harness should follow when this skill is selected.\n\
         \n\
         Add this skill's parent directory to `[skills].search_paths`, then check it with\n\
         `aise skills validate` after every edit.\n"
    );
    let mut files = vec![(PathBuf::from("SKILL.md"), skill_md)];
    if capability != Some(ScaffoldCapability::MessageClassification) {
        return files;
    }

    let mut policy = format!(
        "# Message-classification categories for the `{name}` skill.\n\
         #\n\
         # Categories are evaluated top to bottom and the FIRST match wins, so a catch-all\n\
         # belongs last. Patterns within one category are ORed, and each category compiles to a\n\
         # single case-insensitive regex. Check this file with `aise skills validate`, then use\n\
         # it with `aise skills {name}`.\n\
         schema_version = {}\n\
         kind = \"message-classification\"\n",
        crate::message_classification::MESSAGE_CLASSIFICATION_SCHEMA_VERSION
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
    files.push((PathBuf::from(crate::skill_catalog::CAPABILITY_FILE), policy));
    files
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
                    "\nvalid: SKILL.md frontmatter and optional {} both check out",
                    crate::skill_catalog::CAPABILITY_FILE
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
            let rows = summaries_at(config, Some(receipt_path))?;
            let stdout = io::stdout();
            let mut out = stdout.lock();
            render(&rows, args.format, &mut out)?;
            out.flush()?;
            Ok(())
        }
        SkillsCmd::Show(args) => {
            let found = detail_at(config, &args.name, Some(receipt_path))?;
            let categories = found.categories.clone();
            let mut preamble = vec![
                ("skill", found.name.clone()),
                ("path", found.path.clone()),
                ("owner", found.ownership.as_str().to_string()),
                ("capability", found.capability_status.as_str().to_string()),
            ];
            if let Some(version) = &found.package_version {
                preamble.push(("version", version.clone()));
            }
            if let Some(digest) = &found.capability_sha256 {
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
                &format!(
                    "\nno deterministic capability: load this harness-only skill's SKILL.md in an \
                     agent harness, or add {} to make `aise skills <name>` executable.",
                    crate::skill_catalog::CAPABILITY_FILE
                ),
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
            let plan = SkillScaffoldPlan::new(
                config,
                &args.name,
                args.output_dir.as_deref(),
                args.capability,
            )?;
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
                     `aise skills {}` when it has a capability, or load SKILL.md in an agent \
                     harness.",
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
        SkillsCmd::Run(_) | SkillsCmd::Inferred(_) => {
            anyhow::bail!("skill execution must run through the indexed read lifecycle")
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

    #[test]
    fn inferred_and_explicit_run_keep_the_first_capability_option() {
        for command in [
            SkillsCmd::Inferred(vec![
                OsString::from("corrections"),
                OsString::from("--when"),
                OsString::from("7d"),
            ]),
            SkillsCmd::Run(SkillRunEscapeArgs {
                selector: OsString::from("corrections"),
                capability_args: vec![OsString::from("--when"), OsString::from("7d")],
            }),
        ] {
            let execution = command.into_execution().unwrap();
            assert_eq!(
                execution.args.dates.when.as_deref(),
                Some("7d"),
                "the synthetic argv[0] must keep --when from being consumed as the program name"
            );
        }
    }

    #[test]
    fn skill_execution_accepts_standard_presentation_controls() {
        let execution = SkillsCmd::Inferred(vec![
            OsString::from("corrections"),
            OsString::from("--field-view-chars"),
            OsString::from("80"),
            OsString::from("--match-view-chars"),
            OsString::from("minimal"),
            OsString::from("--format"),
            OsString::from("json"),
        ])
        .into_execution()
        .unwrap();

        assert_eq!(
            execution.args.field_view_chars,
            Some(crate::messages::CliFieldViewChars::MaxChars(
                std::num::NonZeroUsize::new(80).unwrap()
            ))
        );
        assert_eq!(
            execution.args.match_view_chars,
            Some(crate::messages::CliMatchViewChars::Minimal)
        );
    }

    #[test]
    fn selector_lexing_keeps_names_and_explicit_paths_unambiguous() {
        assert!(matches!(
            parse_skill_selector(OsString::from("my-review")).unwrap(),
            crate::skill_catalog::SkillSelector::Name(_)
        ));
        for path in [
            "./my-review",
            "../my-review",
            "~/my-review",
            "my-review/SKILL.md",
            "SKILL.md",
        ] {
            assert!(
                matches!(
                    parse_skill_selector(OsString::from(path)).unwrap(),
                    crate::skill_catalog::SkillSelector::Path(_)
                ),
                "{path}"
            );
        }
    }

    fn write_skill(
        root: &Path,
        name: &str,
        frontmatter_name: &str,
        capability: Option<&str>,
    ) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!(
                "---\nname: {frontmatter_name}\ndescription: a test skill\nmetadata:\n  version: \
                 0.2.0\n---\n\nbody\n"
            ),
        )
        .unwrap();
        if let Some(capability) = capability {
            std::fs::write(dir.join("aise-capability.toml"), capability).unwrap();
        }
        dir
    }

    const VALID_POLICY: &str = "schema_version = 1\nkind = \"message-classification\"\n\n\
                                [[categories]]\nname = \"clobber\"\npatterns = ['''\\byou overwrote\\b''']\n";

    #[test]
    fn list_and_validate_accept_yaml_block_descriptions_from_the_shared_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join("block-description");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: block-description\ndescription: |\n  First line.\n  Use when a task needs \
             the second line.\nmetadata:\n  version: 1.2.3\n---\n",
        )
        .unwrap();
        let result = validate(&skill).unwrap();
        assert!(result.valid, "{:#?}", result.diagnostics);

        let mut config = Config::default();
        config.skills.search_paths = vec![dir.path().to_string_lossy().into_owned()];
        let rows = summaries_at(&config, None).unwrap();
        assert!(rows.iter().any(|row| {
            row.name == "block-description"
                && row.capability_status == SkillCapabilityStatus::HarnessOnly
        }));
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
        let rows = summaries_at(&config, None).unwrap();

        assert_eq!(
            rows.iter()
                .map(|row| (
                    row.name.as_str(),
                    row.ownership,
                    row.capability_status,
                    row.package_version.as_deref()
                ))
                .collect::<Vec<_>>(),
            vec![
                (
                    crate::corrections::EMBEDDED_POLICY_NAME,
                    SkillOwnership::Aise,
                    SkillCapabilityStatus::Ok,
                    // The built-in skill package version identifies the embedded capability.
                    Some(embedded_policy_version())
                ),
                (
                    "ai-session-search",
                    SkillOwnership::Aise,
                    SkillCapabilityStatus::HarnessOnly,
                    Some(embedded_policy_version())
                ),
                (
                    "no-policy-skill",
                    SkillOwnership::User,
                    SkillCapabilityStatus::HarnessOnly,
                    None
                ),
                (
                    "team-rules",
                    SkillOwnership::User,
                    SkillCapabilityStatus::Ok,
                    Some("0.2.0")
                ),
            ],
            "the built-in policy leads, then discovered skills in sorted order"
        );
    }

    #[test]
    fn list_and_show_report_canonical_installs_beside_the_config_before_embedded_fallbacks() {
        let dir = tempfile::tempdir().unwrap();
        let app_root = dir.path().join("app");
        let skills_root = app_root.join("skills");
        let general = skills_root.join(crate::integrations::AI_SESSION_SEARCH_SKILL_NAME);
        std::fs::create_dir_all(&general).unwrap();
        std::fs::write(
            general.join("SKILL.md"),
            include_str!("../skills/ai-session-search/SKILL.md"),
        )
        .unwrap();
        std::fs::write(
            general.join(crate::skill_catalog::CAPABILITY_FILE),
            include_str!("../skills/ai-session-search/aise-capability.toml"),
        )
        .unwrap();
        write_skill(
            &skills_root,
            crate::corrections::EMBEDDED_POLICY_NAME,
            crate::corrections::EMBEDDED_POLICY_NAME,
            None,
        );
        let receipt = app_root.join(".ai-session-search-mcp-transaction.json");
        let config = Config::default();
        assert!(
            config.skills.search_paths.is_empty(),
            "the canonical install must not require a redundant configured search root"
        );

        let rows = summaries_at(&config, Some(&receipt)).unwrap();
        assert_eq!(rows.len(), 2, "canonical install replaces its fallback row");
        let general_path = general.canonicalize().unwrap().display().to_string();
        assert_eq!(
            rows.iter()
                .map(|row| (row.name.as_str(), row.path.as_str()))
                .collect::<Vec<_>>(),
            vec![
                // The embedded classification policy keeps its own row and stays built in: it is a
                // runnable capability, not a second installed package.
                (crate::corrections::EMBEDDED_POLICY_NAME, "(built in)"),
                (
                    crate::integrations::AI_SESSION_SEARCH_SKILL_NAME,
                    general_path.as_str()
                ),
            ]
        );

        let shown = detail_at(
            &config,
            crate::integrations::AI_SESSION_SEARCH_SKILL_NAME,
            Some(&receipt),
        )
        .unwrap();
        assert_eq!(shown.path, general_path);
        assert_eq!(shown.ownership, SkillOwnership::Aise);
        assert_eq!(
            shown.capability_status,
            SkillCapabilityStatus::Ok,
            "the installed package now ships its capability as a side file, so it is runnable"
        );
        assert_eq!(
            shown.package_version.as_deref(),
            Some(embedded_policy_version()),
            "switching from the embedded fallback to its installed package must retain version metadata"
        );

        let shown_corrections = detail_at(
            &config,
            crate::corrections::EMBEDDED_POLICY_NAME,
            Some(&receipt),
        )
        .unwrap();
        assert_eq!(shown_corrections.path, "(built in)");
        assert_eq!(shown_corrections.ownership, SkillOwnership::Aise);
        assert!(
            matches!(
                shown_corrections.capability_source,
                Some(crate::skill_run::CapabilityExecutionSource::Embedded)
            ),
            "reporting the installed package path must not change reserved-name execution"
        );
    }

    #[test]
    fn show_resolves_the_embedded_harness_only_skill_promised_by_help() {
        let detail = detail_at(&Config::default(), "ai-session-search", None).unwrap();
        assert_eq!(detail.name, "ai-session-search");
        assert_eq!(detail.path, "(built in)");
        assert_eq!(detail.ownership, SkillOwnership::Aise);
        assert_eq!(detail.capability_status, SkillCapabilityStatus::HarnessOnly);
        assert_eq!(
            detail.package_version.as_deref(),
            Some(embedded_policy_version())
        );
        assert!(detail.capability_sha256.is_none());
        assert!(detail.capability_source.is_none());
        assert!(detail.categories.is_empty());
        assert!(detail.problem.is_none());
    }

    fn embedded_policy_version() -> &'static str {
        env!("CARGO_PKG_VERSION")
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
            .join(crate::integrations::AI_SESSION_SEARCH_SKILL_NAME);
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
        let rows = summaries_at(&config, None).unwrap();

        let broken = rows.iter().find(|row| row.name == "aaa-broken").unwrap();
        assert_eq!(broken.capability_status, SkillCapabilityStatus::Invalid);
        assert!(
            broken
                .problem
                .as_deref()
                .is_some_and(|text| text.contains("schema_version")),
            "the row must say what is wrong: {:?}",
            broken.problem
        );
        assert!(
            rows.iter().any(|row| {
                row.name == "zzz-fine" && row.capability_status == SkillCapabilityStatus::Ok
            }),
            "a later, valid skill must still be listed"
        );
        let shown = detail_at(&config, "aaa-broken", None)
            .expect("one invalid descriptor is still explainable by its unique identity");
        assert_eq!(shown.capability_status, SkillCapabilityStatus::Invalid);
        assert!(shown
            .problem
            .as_deref()
            .is_some_and(|problem| problem.contains("schema_version")));
    }

    #[test]
    fn show_lists_categories_in_evaluation_order_with_the_regex_that_runs() {
        let detail = detail_at(
            &Config::default(),
            crate::corrections::EMBEDDED_POLICY_NAME,
            None,
        )
        .unwrap();
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
    fn summary_and_detail_json_use_package_version_and_generalized_capability_source() {
        let summary = summaries_at(&Config::default(), None).unwrap().remove(0);
        let summary_json = serde_json::to_value(summary).unwrap();
        assert_eq!(
            summary_json["package_version"],
            serde_json::json!(env!("CARGO_PKG_VERSION"))
        );
        assert!(
            summary_json.get("capability_version").is_none(),
            "the package-owned version must not be exposed under a capability-owned key"
        );

        let embedded_detail = detail_at(
            &Config::default(),
            crate::corrections::EMBEDDED_POLICY_NAME,
            None,
        )
        .unwrap();
        let detail_json = serde_json::to_value(embedded_detail).unwrap();
        assert_eq!(
            detail_json["package_version"],
            serde_json::json!(env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(
            detail_json["capability_source"],
            serde_json::json!({"kind": "embedded"})
        );
        assert!(detail_json.get("capability_version").is_none());

        let dir = tempfile::tempdir().unwrap();
        let skill = write_skill(dir.path(), "team-rules", "team-rules", Some(VALID_POLICY));
        let mut config = Config::default();
        config.skills.search_paths = vec![dir.path().to_string_lossy().into_owned()];
        let path_detail =
            serde_json::to_value(detail_at(&config, "team-rules", None).unwrap()).unwrap();
        assert_eq!(path_detail["package_version"], serde_json::json!("0.2.0"));
        assert_eq!(
            path_detail["capability_source"],
            serde_json::json!({
                "kind": "path",
                "canonical_capability_toml": skill
                    .join("aise-capability.toml")
                    .canonicalize()
                    .unwrap()
            })
        );
    }

    #[test]
    fn show_rejects_duplicate_names_in_deterministic_path_order() {
        let dir = tempfile::tempdir().unwrap();
        let first_root = dir.path().join("a");
        let second_root = dir.path().join("b");
        let first = write_skill(&first_root, "team-rules", "team-rules", Some(VALID_POLICY));
        let second = write_skill(&second_root, "team-rules", "team-rules", Some(VALID_POLICY));
        let mut config = Config::default();
        config.skills.search_paths = vec![
            second_root.to_string_lossy().into_owned(),
            first_root.to_string_lossy().into_owned(),
        ];

        let error = detail_at(&config, "team-rules", None)
            .expect_err("show must not silently choose one duplicate skill")
            .to_string();
        assert!(error.contains("ambiguous"), "{error}");
        let first_path = first.canonicalize().unwrap().display().to_string();
        let second_path = second.canonicalize().unwrap().display().to_string();
        let first_index = error.find(&first_path).expect("first path is reported");
        let second_index = error.find(&second_path).expect("second path is reported");
        assert!(
            first_index < second_index,
            "duplicate locations must be sorted independently of search-path order: {error}"
        );
    }

    #[test]
    fn validate_reports_every_problem_at_once_and_names_a_fix_for_each() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("skills");
        let skill = root.join("Wrong_Name");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: other-name\n---\n\nbody\n",
        )
        .unwrap();
        std::fs::write(skill.join("aise-capability.toml"), "schema_version = 99\n").unwrap();

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
        assert!(problems.contains("description"), "{problems}");
        assert!(problems.contains("schema_version"), "{problems}");
    }

    /// The strongest scaffold test there is: what `create` writes must pass what `validate`
    /// checks. A scaffold its own tool rejects sends every new author straight into a diagnostic.
    #[test]
    fn a_scaffolded_skill_passes_this_build_s_own_validator() {
        let dir = tempfile::tempdir().unwrap();
        let plan =
            SkillScaffoldPlan::new(&Config::default(), "my-rules", Some(dir.path()), None).unwrap();
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

    /// The optional capability must be seeded with the categories `aise` actually ships, in order.
    #[test]
    fn a_scaffold_is_seeded_with_the_current_built_in_categories() {
        let dir = tempfile::tempdir().unwrap();
        SkillScaffoldPlan::new(
            &Config::default(),
            "my-rules",
            Some(dir.path()),
            Some(ScaffoldCapability::MessageClassification),
        )
        .unwrap()
        .publish()
        .unwrap();
        let text =
            std::fs::read_to_string(dir.path().join("my-rules/aise-capability.toml")).unwrap();
        let policy =
            crate::message_classification::MessageClassificationPolicySpec::parse_toml(&text)
                .unwrap()
                .compile(
                    "my-rules".to_string(),
                    "0.1.0".to_string(),
                    crate::corrections::CorrectionPolicySource::Embedded,
                    text.as_bytes(),
                )
                .unwrap();
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
            SkillScaffoldPlan::new(&Config::default(), "my-rules", Some(dir.path()), None).unwrap();
        let preview = plan.preview();
        assert!(!preview.created);
        assert_eq!(
            preview
                .files
                .iter()
                .map(|file| file.relative_path.as_str())
                .collect::<Vec<_>>(),
            vec!["SKILL.md"]
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

        let error = SkillScaffoldPlan::new(&Config::default(), "my-rules", Some(dir.path()), None)
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
                SkillScaffoldPlan::new(&Config::default(), bad, Some(dir.path()), None).is_err(),
                "{bad:?} is not a valid skill name"
            );
        }
        let error = SkillScaffoldPlan::new(
            &Config::default(),
            crate::corrections::EMBEDDED_POLICY_NAME,
            Some(dir.path()),
            None,
        )
        .expect_err("the reserved name cannot be shadowed");
        assert!(format!("{error:#}").contains("reserved"));
    }

    /// Omitting both `--output-dir` and `[skills].authoring_root` must say what to do, not guess
    /// a destination: writing a new directory into the current working directory by default is
    /// exactly the kind of surprise that leaves junk behind.
    #[test]
    fn create_without_any_destination_names_both_ways_to_supply_one() {
        let error = SkillScaffoldPlan::new(&Config::default(), "my-rules", None, None)
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
        let plan = SkillScaffoldPlan::new(&config, "my-rules", None, None).unwrap();
        assert_eq!(plan.root(), dir.path().join("my-rules"));
    }

    /// Boundary shapes aise-capability.toml can take must be reported, never treated as harness-only.
    #[test]
    fn an_empty_directory_or_non_utf8_capability_is_reported_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("skills");

        let empty = write_skill(&root, "empty-policy", "empty-policy", Some(""));
        let summary = summarize(&load_skill_descriptor(&empty).unwrap());
        assert_eq!(summary.capability_status, SkillCapabilityStatus::Invalid);
        assert!(
            summary.problem.is_some(),
            "an empty capability must say what is missing"
        );

        let as_dir = write_skill(&root, "dir-policy", "dir-policy", None);
        std::fs::create_dir_all(as_dir.join("aise-capability.toml")).unwrap();
        let descriptor = load_skill_descriptor(&as_dir).unwrap();
        assert!(
            matches!(descriptor.capability, CapabilityFileState::Invalid { .. }),
            "a directory named aise-capability.toml is invalid, not harness-only"
        );

        #[cfg(unix)]
        {
            let invalid = write_skill(&root, "binary-policy", "binary-policy", Some(""));
            let path = invalid.join("aise-capability.toml");
            std::fs::write(&path, [0xff_u8, 0xfe, 0xfd]).unwrap();
            let summary = summarize(&load_skill_descriptor(&invalid).unwrap());
            assert_eq!(summary.capability_status, SkillCapabilityStatus::Invalid);
            assert!(
                summary
                    .problem
                    .as_deref()
                    .is_some_and(|text| text.contains("UTF-8")),
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

        let discovered = load_skill_catalog(std::slice::from_ref(&root));
        assert_eq!(
            discovered
                .skills
                .iter()
                .map(|skill| skill.directory_name.as_str())
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
        let roots = config
            .skills
            .search_paths
            .iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        let catalog = load_skill_catalog(&roots);
        assert_eq!(catalog.skills.len(), 1, "{catalog:#?}");
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
            file: "aise-capability.toml".into(),
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
