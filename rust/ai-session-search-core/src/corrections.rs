//! Pure correction-policy parsing, compilation, and classification.
//!
//! This module turns a `corrections/policy.toml` document into compiled, ordered categories and
//! classifies one message's text against them. Following the contract [`crate::analysis_pipeline`]
//! sets, it does not discover providers, read session files, query SQLite, or publish artifacts:
//! callers hand it bytes and it hands back a policy. Keeping it pure is what lets the CLI, Python,
//! MCP, and downstream Rust adapters share one definition of what a correction *is*.
//!
//! # Why this is not [`crate::analysis_pipeline::AnalysisPolicySpec`]
//!
//! The two look alike — named labels over regexes — and an earlier design deserialized corrections
//! straight into `AnalysisPolicySpec`. Reading the source shows that would silently change results:
//!
//! * [`crate::analysis_pipeline::AnalysisPolicy::compile`] **sorts** rules by `(dimension, label)`,
//!   so declaration order is destroyed. Corrections are ordered on purpose — `other` is a
//!   deliberately last catch-all — and a sorted list would evaluate `incomplete` first and `other`
//!   fourth.
//! * `AnalysisPolicy::classify` returns **every** matching rule with weights, over a session
//!   aggregate. Corrections return the **first** matching category for an **individual message**,
//!   plus the exact substring that matched.
//!
//! Sharing the validators is DRY; sharing the type would be a bug. So [`require_name`] and
//! [`compile_nonempty_regex`] are reused and the policy type is its own.

use anyhow::{bail, Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::analysis_pipeline::{compile_nonempty_regex, require_name};
use crate::config::Config;
use crate::hashing::sha256;

/// The only `schema_version` this build understands.
///
/// Bumping it is a breaking change to `policy.toml`; an unknown value is rejected by name rather
/// than ignored, so a policy written for a newer `aise` fails loudly instead of silently losing
/// categories.
pub const CORRECTION_POLICY_SCHEMA_VERSION: u32 = 1;

/// Where a resolved policy's bytes came from, for provenance and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CorrectionPolicySource {
    /// Compiled into the executable; always resolvable and never affected by a damaged install.
    Embedded,
    /// Read from a discovered skill directory.
    File { path: PathBuf },
    /// Synthesized from the legacy `[analytics].correction_patterns` config field.
    LegacyConfig,
}

/// A `corrections/policy.toml` document, before compilation.
///
/// `deny_unknown_fields` makes a typo a hard error rather than a silently ignored key: a
/// misspelled `patterns` would otherwise leave a category with no rules and no complaint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorrectionPolicySpec {
    pub schema_version: u32,
    pub name: String,
    pub version: String,
    pub categories: Vec<CorrectionCategorySpec>,
}

/// One named category. Patterns within a category are ORed; the category is the reported label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorrectionCategorySpec {
    pub name: String,
    pub patterns: Vec<String>,
}

/// Name, version, and digest of the exact bytes a result was produced from.
///
/// The digest is what makes a result reproducible. A name and version alone are not: a policy file
/// can be edited without bumping its version, and then two runs reporting `v1.0.0` would disagree
/// with no way to tell which rules produced which.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorrectionPolicyIdentity {
    pub name: String,
    pub version: String,
    pub sha256: String,
    pub source: CorrectionPolicySource,
}

/// A compiled, ordered correction policy.
///
/// Category order and within-category pattern order are both preserved from the document and are
/// the evaluation order.
#[derive(Debug, Clone)]
pub struct CorrectionPolicy {
    identity: CorrectionPolicyIdentity,
    /// Flattened `(category, regex)` in declaration order. Flattened because that is exactly the
    /// shape the classifier scans and the shape `Db::find_corrections` already accepts, so the hot
    /// loop keeps its current form and its parallel-vs-sequential equivalence test keeps holding.
    rules: Vec<(String, Regex)>,
    category_count: usize,
}

/// One classified message: which category matched, and the text that matched it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorrectionHit<'policy> {
    pub category: &'policy str,
    /// The matched substring, not the rule that matched. See [`CorrectionPolicy::classify`].
    pub matched_text: String,
}

impl CorrectionPolicySpec {
    /// Compile a spec that has no file behind it, deriving the digest from the spec itself.
    ///
    /// Use this whenever the bytes are constructed rather than read: the embedded default, the
    /// legacy-config bridge, and any caller assembling categories in code. The digest then still
    /// identifies exactly these rules, so an in-memory policy is as reproducible as a file one.
    ///
    /// # Errors
    ///
    /// Same as [`CorrectionPolicySpec::compile`].
    pub fn compile_in_memory(self, source: CorrectionPolicySource) -> Result<CorrectionPolicy> {
        let digest = sha256(&canonical_digest_input(&self));
        self.compile(source, digest)
    }

    /// Validate and compile against a digest the caller already has — typically of the exact file
    /// bytes — preserving declaration order. Prefer [`CorrectionPolicySpec::compile_in_memory`]
    /// when there is no such file.
    ///
    /// # Errors
    ///
    /// Returns an error naming the offending category or pattern when the schema version is
    /// unknown, a name or version is empty, there are no categories, a category name repeats, a
    /// category has no patterns, or a pattern is empty, invalid, or matches empty text.
    pub fn compile(
        self,
        source: CorrectionPolicySource,
        digest: String,
    ) -> Result<CorrectionPolicy> {
        if self.schema_version != CORRECTION_POLICY_SCHEMA_VERSION {
            bail!(
                "unsupported correction policy schema_version {}; this build understands {}. \
                 Upgrade aise, or set schema_version = {}",
                self.schema_version,
                CORRECTION_POLICY_SCHEMA_VERSION,
                CORRECTION_POLICY_SCHEMA_VERSION
            );
        }
        require_name("correction policy name", &self.name)?;
        require_name("correction policy version", &self.version)?;
        if self.categories.is_empty() {
            bail!(
                "correction policy '{}' defines no categories; add at least one [[categories]] \
                 table with a name and one or more patterns",
                self.name
            );
        }

        let mut seen = BTreeSet::new();
        let mut rules = Vec::new();
        for category in &self.categories {
            require_name("correction category name", &category.name)?;
            if !seen.insert(category.name.clone()) {
                bail!(
                    "duplicate correction category '{}'; merge its patterns into one \
                     [[categories]] table, because the first match wins and the second would \
                     never be reached",
                    category.name
                );
            }
            if category.patterns.is_empty() {
                bail!(
                    "correction category '{}' has no patterns; give it at least one, or remove \
                     the category",
                    category.name
                );
            }
            // Validate each pattern on its own FIRST, so a failure names the offending pattern
            // rather than an opaque joined blob the author never wrote.
            for pattern in &category.patterns {
                compile_nonempty_regex("correction", &category.name, pattern)?;
            }

            // Then compile ONE case-insensitive alternation per category, byte-for-byte the shape
            // `analytics.rs:compile_category_patterns:120` already produces:
            // `Regex::new(&format!("(?i){}", patterns.join("|")))`.
            //
            // This is not a stylistic choice — compiling each pattern separately and trying them
            // in order changes results two ways:
            //   * Case. The existing `(?i)` prefix makes every built-in category
            //     case-insensitive, so "You Forgot" classifies today and would stop.
            //   * Position. One alternation returns the LEFTMOST match in the message; separate
            //     regexes tried in order return the first *pattern's* match wherever it sits. For
            //     "you missed X, you forgot Y" with patterns ordered [forgot, missed], the
            //     alternation reports "you missed" and per-pattern iteration reports "you forgot".
            //     Both the reported `matched_text` and any position-sensitive downstream use would
            //     differ.
            let joined = format!("(?i){}", category.patterns.join("|"));
            let regex = Regex::new(&joined).with_context(|| {
                format!(
                    "correction category '{}' does not compile once its {} patterns are combined; \
                     each is valid alone, so check for an unbalanced group spanning them",
                    category.name,
                    category.patterns.len()
                )
            })?;
            rules.push((category.name.clone(), regex));
        }

        Ok(CorrectionPolicy {
            identity: CorrectionPolicyIdentity {
                name: self.name,
                version: self.version,
                sha256: digest,
                source,
            },
            rules,
            category_count: self.categories.len(),
        })
    }
}

impl CorrectionPolicy {
    /// Parse and compile a `policy.toml` document, digesting the exact bytes supplied.
    ///
    /// The digest covers `source_text` verbatim rather than the re-serialized spec, so a
    /// whitespace-only or comment-only edit still produces a different identity. That is the
    /// point: the question a digest answers is "were these the same bytes?", not "did the parsed
    /// values happen to match?".
    ///
    /// # Errors
    ///
    /// Returns an error when the document is not valid TOML, carries an unknown field, or fails
    /// any check in [`CorrectionPolicySpec::compile`].
    pub fn parse_toml(source_text: &str, source: CorrectionPolicySource) -> Result<Self> {
        let spec: CorrectionPolicySpec = toml::from_str(source_text).with_context(|| {
            format!(
                "failed to parse correction policy from {}",
                describe_source(&source)
            )
        })?;
        let digest = sha256(source_text.as_bytes());
        spec.compile(source, digest)
    }

    /// Classify one message's text, returning the FIRST matching category.
    ///
    /// Categories are tried in declaration order and patterns within a category in their own
    /// order, so a deliberately-last catch-all such as `other` only fires when nothing earlier
    /// matched. `matched_text` is the substring the regex matched, not the regex source: for the
    /// rule `\byou forgot\b` over "ok you forgot the tests" the value is `you forgot`.
    pub fn classify(&self, text: &str) -> Option<CorrectionHit<'_>> {
        self.rules.iter().find_map(|(category, regex)| {
            regex.find(text).map(|matched| CorrectionHit {
                category: category.as_str(),
                matched_text: matched.as_str().to_string(),
            })
        })
    }

    /// Name, version, digest, and origin of the bytes this policy was compiled from.
    pub fn identity(&self) -> &CorrectionPolicyIdentity {
        &self.identity
    }

    /// Declared categories, in evaluation order.
    pub fn category_count(&self) -> usize {
        self.category_count
    }

    /// Compiled `(category, regex)` rules, in evaluation order.
    pub fn rules(&self) -> &[(String, Regex)] {
        &self.rules
    }
}

/// One skill directory found on disk, before its policy is compiled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSkill {
    /// Directory basename, which the Agent Skills spec requires to equal the `SKILL.md` `name`.
    pub name: String,
    /// Canonicalized skill root, so two paths reaching one directory compare equal.
    pub root: PathBuf,
    /// `corrections/policy.toml`, when the skill has one. A skill without it is still listed —
    /// `aise skills list` must show every skill it can see, or the tool looks broken — but it
    /// cannot be selected for corrections.
    pub policy_path: Option<PathBuf>,
}

/// Scan one configured root for skills, one level deep, in a deterministic order.
///
/// The root may be a skill directory itself (it holds `SKILL.md`) or a directory of skill
/// directories. Both spellings are accepted because a user pointing at `~/.claude/skills` and a
/// user pointing at one specific skill both expect it to work.
///
/// Entries are sorted by file name, so traversal order never becomes hidden precedence: two runs
/// on the same disk must produce the same list, and `read_dir` order is not guaranteed. A missing
/// root is not an error — a configured path that does not exist yet is a normal state, not a
/// failure — but an unreadable one is, because silently returning nothing would look identical to
/// "no skills installed".
///
/// # Errors
///
/// Returns an error when the root exists but cannot be read, or when an entry cannot be
/// canonicalized.
pub fn discover_skills_in(root: &Path) -> Result<Vec<DiscoveredSkill>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    if let Some(skill) = skill_at(root)? {
        return Ok(vec![skill]);
    }

    let mut entries: Vec<PathBuf> = std::fs::read_dir(root)
        .with_context(|| format!("failed to read skill search path {}", root.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to list skill search path {}", root.display()))?
        .into_iter()
        .map(|entry| entry.path())
        .collect();
    entries.sort();

    let mut found = Vec::new();
    for entry in entries {
        if let Some(skill) = skill_at(&entry)? {
            found.push(skill);
        }
    }
    Ok(found)
}

/// Recognize one directory as a skill, or return `None` if it is not one.
fn skill_at(candidate: &Path) -> Result<Option<DiscoveredSkill>> {
    if !candidate.is_dir() || !candidate.join("SKILL.md").is_file() {
        return Ok(None);
    }
    // Canonicalize so a symlinked root and its target dedupe against each other rather than
    // appearing as two skills with the same name and racing to be "the" one.
    let root = candidate
        .canonicalize()
        .with_context(|| format!("failed to resolve skill directory {}", candidate.display()))?;
    let Some(name) = root.file_name().and_then(|name| name.to_str()) else {
        // A non-UTF-8 directory name cannot equal a `SKILL.md` `name`, which the spec restricts to
        // `[a-z0-9-]`, so this is not a skill rather than an error.
        return Ok(None);
    };
    let policy = root.join("corrections").join("policy.toml");
    Ok(Some(DiscoveredSkill {
        name: name.to_string(),
        policy_path: policy.is_file().then_some(policy),
        root,
    }))
}

/// Every skill across every configured search path, deduplicated and checked for name collisions.
///
/// Two rules make results reproducible rather than dependent on configuration order:
///
/// * The same canonical directory reached through two configured paths is **one** skill, dropped
///   silently, exactly as [`crate::integrations`] treats a destination selected twice the same way.
/// * Two **different** directories declaring the same name is a hard error naming both paths. The
///   tempting alternative — first one wins — makes the answer depend on config order, which is
///   precisely the hidden precedence this avoids.
///
/// # Errors
///
/// Returns an error when a search path cannot be read, or when one name resolves to two roots.
pub fn discover_skills(search_paths: &[String]) -> Result<Vec<DiscoveredSkill>> {
    let mut by_name: BTreeMap<String, DiscoveredSkill> = BTreeMap::new();
    for configured in search_paths {
        let root = crate::util::expand_tilde(configured);
        for skill in discover_skills_in(&root)? {
            match by_name.get(&skill.name) {
                Some(existing) if existing.root == skill.root => {}
                Some(existing) => bail!(
                    "two different directories both provide the skill '{}':\n  {}\n  {}\n\
                     Rename one directory, or drop one entry from [skills].search_paths — \
                     picking by search order would make results depend on config order",
                    skill.name,
                    existing.root.display(),
                    skill.root.display()
                ),
                None => {
                    by_name.insert(skill.name.clone(), skill);
                }
            }
        }
    }
    Ok(by_name.into_values().collect())
}

/// Reserved name of the policy compiled into the executable.
///
/// It is always resolvable and cannot be shadowed by a discovered directory: a damaged, stale, or
/// hostile skill install must never be able to redefine what the product means by "a correction".
pub const EMBEDDED_POLICY_NAME: &str = "ai-session-search";

/// Receipt name used when results came from the legacy `[analytics].correction_patterns` field.
pub const LEGACY_POLICY_NAME: &str = "legacy-analytics-config";

/// The ordered set of policies one `corrections` call evaluates, with provenance.
#[derive(Debug, Clone)]
pub struct ResolvedCorrectionPolicySet {
    policies: Vec<CorrectionPolicy>,
}

impl ResolvedCorrectionPolicySet {
    /// Use exactly these already-compiled policies, in this order.
    ///
    /// The escape hatch from name-based resolution, for callers that built their policies in code
    /// rather than selecting them from disk. It deliberately performs no duplicate-name or
    /// reserved-name check: those exist to stop a *discovered directory* from redefining what the
    /// product means by a correction, and a caller passing an explicit list has already said what
    /// it wants. Prefer [`ResolvedCorrectionPolicySet::resolve`] whenever the selection comes from
    /// configuration or a user-supplied name.
    pub fn from_policies(policies: Vec<CorrectionPolicy>) -> Self {
        Self { policies }
    }

    /// Decide which policies apply, in order.
    ///
    /// Precedence, highest first. Each rung is a *replacement*, never a merge, so no invocation
    /// half-inherits a configured set:
    ///
    /// 1. A non-empty `requested` list (CLI `--skill`), in argument order.
    /// 2. `[skills].enabled`, including an explicit `[]` meaning **no policies at all**.
    /// 3. Legacy `[analytics].correction_patterns`, keeping its exact replace-all meaning.
    /// 4. The embedded policy — the product default.
    ///
    /// # Errors
    ///
    /// Returns an error when a selected name matches no discovered skill, when a selected skill
    /// has no `corrections/policy.toml`, when any policy fails to compile, or when the legacy
    /// field is combined with an explicit new selector.
    pub fn resolve(config: &Config, requested: Option<&[String]>) -> Result<Self> {
        let cli = requested.filter(|names| !names.is_empty());
        let selection = cli
            .map(<[String]>::to_vec)
            .or_else(|| config.skills.enabled.clone());
        let legacy = &config.analytics.correction_patterns;

        if selection.is_some() && !legacy.is_empty() {
            bail!(
                "[analytics].correction_patterns cannot be combined with a skill selection, \
                 because the two express the same thing and merging them would invent a \
                 precedence nobody chose.\n\
                 Migrate: run `aise skills create <name>` to turn those patterns into a skill, \
                 then delete correction_patterns and select the skill by name.\n\
                 Or drop the skill selection to keep using the legacy field unchanged."
            );
        }

        let Some(names) = selection else {
            // No selection at all: legacy field if set, otherwise the product default.
            let policy = if legacy.is_empty() {
                embedded_policy()?
            } else {
                legacy_policy(legacy)?
            };
            return Ok(Self {
                policies: vec![policy],
            });
        };

        // An explicit empty list is a real state, not a mistake: "scan nothing".
        if names.is_empty() {
            return Ok(Self {
                policies: Vec::new(),
            });
        }

        let discovered = discover_skills(&config.skills.search_paths)?;
        let mut policies = Vec::with_capacity(names.len());
        for name in &names {
            policies.push(resolve_one(name, &discovered)?);
        }
        Ok(Self { policies })
    }

    /// Classify one message against every selected policy, in order, returning the first hit.
    ///
    /// Policies are tried in selection order and categories within a policy in declaration order,
    /// so "which policy did this come from" is answerable and stable.
    pub fn classify(&self, text: &str) -> Option<(&CorrectionPolicyIdentity, CorrectionHit<'_>)> {
        self.policies
            .iter()
            .find_map(|policy| policy.classify(text).map(|hit| (policy.identity(), hit)))
    }

    /// Provenance for every selected policy, in evaluation order.
    ///
    /// Carried once per report rather than repeated on every match: the digest is what makes a
    /// result reproducible, and it is a property of the run, not of each row.
    pub fn receipts(&self) -> Vec<CorrectionPolicyReceipt> {
        self.policies
            .iter()
            .map(|policy| {
                let identity = policy.identity();
                CorrectionPolicyReceipt {
                    name: identity.name.clone(),
                    version: identity.version.clone(),
                    sha256: identity.sha256.clone(),
                }
            })
            .collect()
    }

    /// Selected policies, in evaluation order.
    pub fn policies(&self) -> &[CorrectionPolicy] {
        &self.policies
    }

    /// True when nothing was selected, so `corrections` must return no matches.
    pub fn is_empty(&self) -> bool {
        self.policies.is_empty()
    }
}

/// Name, version, and digest of one policy that contributed to a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorrectionPolicyReceipt {
    pub name: String,
    pub version: String,
    /// Digest of the exact resolved policy bytes. Version alone is not reproducible.
    pub sha256: String,
}

/// One `corrections` request: which messages to scan, and whose rules to scan them with.
///
/// A request type rather than two loose arguments, because every adapter — CLI, Python, MCP, and
/// downstream Rust — has to spell the same two halves, and each half has a non-obvious default
/// that a bare parameter list would leave undocumented at the call site.
#[derive(Debug, Clone, Default)]
pub struct CorrectionQuery {
    /// Which messages to scan: session, provider, path prefix, dates, paging, session class.
    ///
    /// [`crate::db::Db::find_corrections`] overrides `role` to `user` and defaults
    /// `session_kinds` to `[SessionKind::User]`, because both state what a correction *is* rather
    /// than what a caller prefers. Setting `session_kinds` here opts other classes back in.
    pub filters: crate::models::MessageFilters,
    /// Named policies to evaluate, in the order given.
    ///
    /// Empty means "no per-request selection", which falls back to `[skills].enabled`, then
    /// legacy `[analytics].correction_patterns`, then the embedded `ai-session-search` policy —
    /// see [`ResolvedCorrectionPolicySet::resolve`]. There is deliberately no per-request
    /// spelling for "evaluate nothing": an empty vector already means "defer", and selecting no
    /// rules at all is a configuration state (`[skills].enabled = []`), not a per-call one.
    pub skills: Vec<String>,
}

/// The result of one `corrections` request: what matched, and which rules were in force.
///
/// The receipts are not derivable from the matches — a policy that matched nothing still shaped
/// the result, and its digest is what makes the run reproducible — so they travel together.
#[derive(Debug, Clone, Serialize)]
pub struct CorrectionReport {
    /// Every evaluated policy, in evaluation order, including those that matched nothing.
    ///
    /// Carried once per report rather than repeated per match: name, version, and digest describe
    /// the run. Each match separately names only the policy it came from.
    pub policies: Vec<CorrectionPolicyReceipt>,
    /// Matches in newest-first order, after `filters.offset` is skipped and `filters.limit` taken.
    pub matches: Vec<crate::models::CorrectionMatch>,
}

fn resolve_one(name: &str, discovered: &[DiscoveredSkill]) -> Result<CorrectionPolicy> {
    if name == EMBEDDED_POLICY_NAME {
        // Checked before discovery so an external directory of the same name cannot shadow it.
        return embedded_policy();
    }
    let Some(skill) = discovered.iter().find(|skill| skill.name == name) else {
        bail!(
            "unknown skill '{name}'; run `aise skills list` to see discovered skills, or add its \
             parent directory to [skills].search_paths"
        );
    };
    let Some(policy_path) = &skill.policy_path else {
        // Distinct from "unknown name": the skill exists and the fix is different.
        bail!(
            "skill '{name}' at {} has no corrections/policy.toml, so it defines no correction \
             categories; run `aise skills validate {}` for details",
            skill.root.display(),
            skill.root.display()
        );
    };
    let text = std::fs::read_to_string(policy_path)
        .with_context(|| format!("failed to read {}", policy_path.display()))?;
    CorrectionPolicy::parse_toml(
        &text,
        CorrectionPolicySource::File {
            path: policy_path.clone(),
        },
    )
}

/// The product-default correction policy, compiled into the executable.
///
/// Embedded rather than read from an installed skill directory so that a damaged, stale, or
/// missing install cannot change what `aise` measures, and so install itself needs no network, no
/// data directory, and no package-relative lookup. Same mechanism `config.example.toml` already
/// uses (`config.rs:CONFIG_EXAMPLE_TOML`).
///
/// `include_str!` resolves relative to THIS source file, so it reads the crate-local mirror at
/// `rust/ai-session-search-core/skills/`, not the repo-root `skills/` a human edits. The two are
/// held byte-identical by `tests/test_repository_contracts.py`.
///
/// The digest covers the exact file bytes, so editing a comment in the policy changes the reported
/// SHA-256 even when the compiled rules are identical. That is the intended reading of
/// "reproducible": the receipt identifies the bytes that ran.
pub(crate) fn embedded_policy() -> Result<CorrectionPolicy> {
    CorrectionPolicy::parse_toml(EMBEDDED_POLICY_TOML, CorrectionPolicySource::Embedded)
}

/// Bytes of the bundled `corrections/policy.toml`.
pub(crate) const EMBEDDED_POLICY_TOML: &str =
    include_str!("../skills/ai-session-search/corrections/policy.toml");

/// Translate the legacy `CATEGORY:REGEX` list into a policy, preserving its exact meaning.
///
/// Same grouping rule as `analytics::compile_category_patterns`: same-category entries are ORed
/// and categories keep first-seen order. The legacy field replaces the built-ins entirely, which
/// is why nothing else is merged in.
fn legacy_policy(entries: &[String]) -> Result<CorrectionPolicy> {
    let mut order: Vec<String> = Vec::new();
    let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for entry in entries {
        let (category, pattern) = entry.split_once(':').ok_or_else(|| {
            anyhow::anyhow!("invalid correction pattern '{entry}': expected CATEGORY:REGEX")
        })?;
        if !grouped.contains_key(category) {
            order.push(category.to_string());
        }
        grouped
            .entry(category.to_string())
            .or_default()
            .push(pattern.to_string());
    }
    let spec = CorrectionPolicySpec {
        schema_version: CORRECTION_POLICY_SCHEMA_VERSION,
        name: LEGACY_POLICY_NAME.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        categories: order
            .into_iter()
            .map(|name| CorrectionCategorySpec {
                patterns: grouped[&name].clone(),
                name,
            })
            .collect(),
    };
    spec.compile_in_memory(CorrectionPolicySource::LegacyConfig)
}

/// Length-delimited encoding of a spec, for policies that have no file to digest.
///
/// Length prefixes rather than separators, so no category or pattern containing the separator can
/// collide with a different spec — `["a:b", "c"]` and `["a", "b:c"]` must not digest alike.
fn canonical_digest_input(spec: &CorrectionPolicySpec) -> Vec<u8> {
    let mut out = Vec::new();
    let mut push = |value: &str| {
        out.extend_from_slice(&(value.len() as u64).to_le_bytes());
        out.extend_from_slice(value.as_bytes());
    };
    push(&spec.schema_version.to_string());
    push(&spec.name);
    push(&spec.version);
    for category in &spec.categories {
        push(&category.name);
        for pattern in &category.patterns {
            push(pattern);
        }
    }
    out
}

fn describe_source(source: &CorrectionPolicySource) -> String {
    match source {
        CorrectionPolicySource::Embedded => "the policy embedded in this executable".to_string(),
        CorrectionPolicySource::File { path } => path.display().to_string(),
        CorrectionPolicySource::LegacyConfig => {
            "[analytics].correction_patterns in config.toml".to_string()
        }
    }
}

#[cfg(test)]
mod tests {

    /// Boundary category counts. One and many must both work; zero must be refused.
    #[test]
    fn a_policy_may_have_one_category_or_many_but_never_zero() {
        let spec = |count: usize| CorrectionPolicySpec {
            schema_version: CORRECTION_POLICY_SCHEMA_VERSION,
            name: "boundary".to_string(),
            version: "1.0.0".to_string(),
            categories: (0..count)
                .map(|index| CorrectionCategorySpec {
                    name: format!("c{index}"),
                    patterns: vec![format!(r"\bmarker{index}\b")],
                })
                .collect(),
        };

        let error = spec(0)
            .compile_in_memory(CorrectionPolicySource::Embedded)
            .expect_err("a policy with no categories matches nothing, silently");
        assert!(format!("{error:#}").contains("[[categories]]"), "{error:#}");

        for count in [1_usize, 64] {
            let policy = spec(count)
                .compile_in_memory(CorrectionPolicySource::Embedded)
                .unwrap_or_else(|error| panic!("{count} categories must compile: {error:#}"));
            assert_eq!(policy.category_count(), count);
            // The LAST category is still reachable: order is preserved, not truncated.
            let hit = policy
                .classify(&format!("marker{}", count - 1))
                .expect("the final category still matches");
            assert_eq!(hit.category, format!("c{}", count - 1));
        }
    }

    /// A pattern that matches everything would label every message, so it is refused at compile
    /// time rather than discovered when every user message becomes a "correction".
    #[test]
    fn a_pattern_matching_empty_text_is_refused() {
        let spec = CorrectionPolicySpec {
            schema_version: CORRECTION_POLICY_SCHEMA_VERSION,
            name: "greedy".to_string(),
            version: "1.0.0".to_string(),
            categories: vec![CorrectionCategorySpec {
                name: "everything".to_string(),
                patterns: vec![".*".to_string()],
            }],
        };
        let error = spec
            .compile_in_memory(CorrectionPolicySource::Embedded)
            .expect_err("a rule that matches the empty string matches every message");
        let message = format!("{error:#}");
        assert!(
            message.contains(".*"),
            "name the offending pattern: {message}"
        );
    }

    /// Whitespace-only is not a value. Each blank field is refused by name, so a caller does not
    /// have to guess which of three strings was the empty one.
    #[test]
    fn blank_names_and_versions_are_refused_by_field() {
        for (name, version, category, expected) in [
            ("", "1.0.0", "c", "correction policy name"),
            ("   ", "1.0.0", "c", "correction policy name"),
            ("ok", "", "c", "correction policy version"),
            ("ok", "  ", "c", "correction policy version"),
            ("ok", "1.0.0", "  ", "correction category name"),
        ] {
            let spec = CorrectionPolicySpec {
                schema_version: CORRECTION_POLICY_SCHEMA_VERSION,
                name: name.to_string(),
                version: version.to_string(),
                categories: vec![CorrectionCategorySpec {
                    name: category.to_string(),
                    patterns: vec![r"\bx\b".to_string()],
                }],
            };
            let error = spec
                .compile_in_memory(CorrectionPolicySource::Embedded)
                .expect_err(&format!(
                    "{name:?}/{version:?}/{category:?} must be refused"
                ));
            let message = format!("{error:#}");
            assert!(
                message.contains(expected),
                "the message must name WHICH field is blank: {message}"
            );
        }
    }

    use super::*;

    fn policy(body: &str) -> String {
        format!(
            "schema_version = 1\nname = \"fixture\"\nversion = \"1.0.0\"\n\n{body}",
            body = body
        )
    }

    fn compile(body: &str) -> Result<CorrectionPolicy> {
        CorrectionPolicy::parse_toml(&policy(body), CorrectionPolicySource::Embedded)
    }

    /// Render a rejected policy the way `main.rs:20` renders it for the user: `{:#}`, which walks
    /// the whole `anyhow` chain. Plain `to_string()` shows only the outermost context, so an
    /// assertion written against it would pass while the detail the caller needs -- which key was
    /// misspelled, which regex failed -- sat unread in a source error.
    fn message(result: Result<CorrectionPolicy>) -> String {
        format!(
            "{:#}",
            result.expect_err("policy should have been rejected")
        )
    }

    // THE reason this type exists rather than reusing `AnalysisPolicySpec`. That type's `compile`
    // sorts by (dimension, label), which would reorder these categories alphabetically to
    // [incomplete, other, regression] and make the catch-all fire before the specific rule.
    #[test]
    fn declaration_order_is_evaluation_order() {
        let compiled = compile(
            r#"
[[categories]]
name = "regression"
patterns = ['''\byou broke\b''']

[[categories]]
name = "incomplete"
patterns = ['''\bstill need\b''']

[[categories]]
name = "other"
patterns = ['''\bstop\b''']
"#,
        )
        .unwrap();

        let order: Vec<&str> = compiled
            .rules()
            .iter()
            .map(|(category, _)| category.as_str())
            .collect();
        assert_eq!(
            order,
            ["regression", "incomplete", "other"],
            "categories must evaluate in declaration order, not sorted order"
        );
    }

    #[test]
    fn first_matching_category_wins_and_later_ones_are_not_reported() {
        let compiled = compile(
            r#"
[[categories]]
name = "specific"
patterns = ['''\byou broke the build\b''']

[[categories]]
name = "catch_all"
patterns = ['''\bbroke\b''']
"#,
        )
        .unwrap();

        let hit = compiled.classify("you broke the build again").unwrap();
        assert_eq!(hit.category, "specific");
        assert_eq!(hit.matched_text, "you broke the build");

        // The catch-all still fires when nothing earlier matches.
        let hit = compiled.classify("the deploy broke overnight").unwrap();
        assert_eq!(hit.category, "catch_all");
    }

    #[test]
    fn patterns_within_a_category_are_ored() {
        let compiled = compile(
            r#"
[[categories]]
name = "skip_step"
patterns = ['''\byou forgot\b''', '''\byou missed\b''', '''\byou skipped\b''']
"#,
        )
        .unwrap();

        for (text, expected) in [
            ("you forgot the tests", "you forgot"),
            ("you missed a step", "you missed"),
            ("you skipped the lint", "you skipped"),
        ] {
            let hit = compiled.classify(text).unwrap();
            assert_eq!(hit.category, "skip_step");
            assert_eq!(hit.matched_text, expected);
        }
        assert!(compiled.classify("everything looks fine").is_none());
    }

    // Two properties that fall out of compiling ONE alternation per category rather than one
    // regex per pattern. Both are existing behavior from
    // `analytics.rs:compile_category_patterns:120`, and both would have broken silently under
    // per-pattern iteration — no test covered either before.
    #[test]
    fn categories_are_case_insensitive_and_report_the_leftmost_match() {
        let compiled = compile(
            r#"
[[categories]]
name = "skip_step"
patterns = ['''\byou forgot\b''', '''\byou missed\b''']
"#,
        )
        .unwrap();

        // (?i) is prefixed to the joined alternation, so capitalization does not change results.
        assert_eq!(
            compiled
                .classify("You Forgot the tests")
                .unwrap()
                .matched_text,
            "You Forgot",
            "matching is case-insensitive, and matched_text preserves the ORIGINAL casing"
        );

        // "you missed" appears first in the text but SECOND in the pattern list. One alternation
        // reports the leftmost match; trying patterns in order would report "you forgot" instead.
        assert_eq!(
            compiled
                .classify("you missed X, and you forgot Y")
                .unwrap()
                .matched_text,
            "you missed",
            "within a category the leftmost match in the message wins, not the first pattern"
        );
    }

    // The value is the OUTPUT substring, not the rule that produced it. Naming it after the rule
    // is exactly the S16 defect this plan renamed away from.
    #[test]
    fn matched_text_is_the_substring_not_the_rule() {
        let compiled = compile(
            r#"
[[categories]]
name = "regression"
patterns = ['''\byou (deleted|removed|reverted)\b''']
"#,
        )
        .unwrap();

        let hit = compiled.classify("wait, you reverted my fix").unwrap();
        assert_eq!(hit.matched_text, "you reverted");
        assert_ne!(
            hit.matched_text, r"\byou (deleted|removed|reverted)\b",
            "the field must not carry the regex source"
        );
    }

    #[test]
    fn identity_digests_the_exact_source_bytes() {
        let body = r#"
[[categories]]
name = "regression"
patterns = ['''\byou broke\b''']
"#;
        let first = compile(body).unwrap();
        let second = compile(body).unwrap();
        assert_eq!(
            first.identity().sha256,
            second.identity().sha256,
            "identical bytes must digest identically"
        );

        // A comment-only edit parses to the same spec but is NOT the same bytes, and the digest
        // must say so — that is what makes a result reproducible after an untracked edit.
        let commented = compile(&format!("# a note\n{body}")).unwrap();
        assert_ne!(
            first.identity().sha256,
            commented.identity().sha256,
            "a byte-level edit must change the digest even when the parsed values match"
        );
        assert_eq!(first.identity().name, "fixture");
        assert_eq!(first.identity().version, "1.0.0");
        assert_eq!(first.identity().source, CorrectionPolicySource::Embedded);
    }

    #[test]
    fn unknown_schema_version_is_rejected_by_name() {
        let source = "schema_version = 2\nname = \"f\"\nversion = \"1\"\n\n[[categories]]\nname = \"c\"\npatterns = ['''x''']\n";
        let err = CorrectionPolicy::parse_toml(source, CorrectionPolicySource::Embedded);
        let err = message(err);
        assert!(err.contains("schema_version 2"), "{err}");
        assert!(
            err.contains("Upgrade aise"),
            "must say what to do, not only what is wrong: {err}"
        );
    }

    #[test]
    fn unknown_field_is_a_hard_error_not_a_silent_drop() {
        let err = compile(
            r#"
[[categories]]
name = "regression"
pattern = ['''\byou broke\b''']
"#,
        );
        let err = message(err);
        assert!(
            err.contains("pattern"),
            "a misspelled key must be named, not ignored: {err}"
        );
    }

    #[test]
    fn duplicate_category_names_are_rejected_because_the_second_is_unreachable() {
        let err = compile(
            r#"
[[categories]]
name = "regression"
patterns = ['''\byou broke\b''']

[[categories]]
name = "regression"
patterns = ['''\byou deleted\b''']
"#,
        );
        let err = message(err);
        assert!(
            err.contains("duplicate correction category 'regression'"),
            "{err}"
        );
        assert!(err.contains("never be reached"), "must explain why: {err}");
    }

    #[test]
    fn structural_emptiness_is_rejected_at_every_level() {
        let no_categories = CorrectionPolicy::parse_toml(
            "schema_version = 1\nname = \"f\"\nversion = \"1\"\ncategories = []\n",
            CorrectionPolicySource::Embedded,
        );
        let no_categories = message(no_categories);
        assert!(
            no_categories.contains("defines no categories"),
            "{no_categories}"
        );

        let no_patterns = message(compile(
            "[[categories]]\nname = \"regression\"\npatterns = []\n",
        ));
        assert!(no_patterns.contains("has no patterns"), "{no_patterns}");

        let blank_name = message(compile(
            "[[categories]]\nname = \"   \"\npatterns = ['''x''']\n",
        ));
        assert!(blank_name.contains("must not be empty"), "{blank_name}");

        let blank_policy_name = message(CorrectionPolicy::parse_toml(
            "schema_version = 1\nname = \"\"\nversion = \"1\"\n\n[[categories]]\nname = \"c\"\npatterns = ['''x''']\n",
            CorrectionPolicySource::Embedded,
        ));
        assert!(
            blank_policy_name.contains("must not be empty"),
            "{blank_policy_name}"
        );
    }

    #[test]
    fn invalid_and_empty_matching_regexes_are_rejected() {
        let invalid = message(compile(
            "[[categories]]\nname = \"c\"\npatterns = ['''(unclosed''']\n",
        ));
        assert!(invalid.contains("invalid correction regex"), "{invalid}");

        // A pattern matching empty text would classify EVERY message into that category and
        // starve every later one, because the first match wins.
        let empty_matching = message(compile(
            "[[categories]]\nname = \"c\"\npatterns = ['''a*''']\n",
        ));
        assert!(
            empty_matching.contains("matches empty text"),
            "{empty_matching}"
        );
        assert!(
            empty_matching.contains("a*"),
            "and it names WHICH pattern, since a category may hold several: {empty_matching}"
        );

        let empty_pattern = message(compile(
            "[[categories]]\nname = \"c\"\npatterns = ['''''']\n",
        ));
        assert!(
            empty_pattern.contains("must not be empty"),
            "{empty_pattern}"
        );
    }

    fn write_skill(root: &Path, name: &str, with_policy: bool) -> PathBuf {
        let skill = root.join(name);
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), format!("---\nname: {name}\n---\n")).unwrap();
        if with_policy {
            std::fs::create_dir_all(skill.join("corrections")).unwrap();
            std::fs::write(
                skill.join("corrections/policy.toml"),
                policy("[[categories]]\nname = \"c\"\npatterns = ['''x''']\n"),
            )
            .unwrap();
        }
        skill
    }

    #[test]
    fn discovery_accepts_a_container_or_a_skill_directory_and_sorts_deterministically() {
        let dir = tempfile::tempdir().unwrap();
        // Deliberately created out of alphabetical order: `read_dir` order is not guaranteed, and
        // traversal order must never become hidden precedence between two skills.
        write_skill(dir.path(), "zulu", true);
        write_skill(dir.path(), "alpha", true);
        let bravo = write_skill(dir.path(), "bravo", false);
        std::fs::create_dir_all(dir.path().join("not-a-skill")).unwrap();

        let found = discover_skills_in(dir.path()).unwrap();
        let names: Vec<&str> = found.iter().map(|skill| skill.name.as_str()).collect();
        assert_eq!(
            names,
            ["alpha", "bravo", "zulu"],
            "a directory without SKILL.md is not a skill, and order is sorted, not read_dir order"
        );

        // A skill WITHOUT a correction policy is still discovered. `aise skills list` showing
        // every skill it can see is the point; hiding one makes the tool look broken.
        let listed_without_policy = found.iter().find(|s| s.name == "bravo").unwrap();
        assert_eq!(listed_without_policy.policy_path, None);
        assert!(found.iter().all(|s| s.name != "not-a-skill"));

        // Pointing directly AT one skill works too, because a user with one skill expects it to.
        let direct = discover_skills_in(&bravo).unwrap();
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].name, "bravo");
    }

    #[test]
    fn a_missing_search_path_is_normal_but_an_unreadable_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            discover_skills_in(&dir.path().join("does-not-exist"))
                .unwrap()
                .is_empty(),
            "a configured path that does not exist yet is a normal state, not a failure"
        );

        // A FILE where a directory was configured is a real mistake and must not read as "empty".
        let file = dir.path().join("a-file");
        std::fs::write(&file, "not a directory\n").unwrap();
        assert!(
            discover_skills_in(&file).is_err(),
            "an unreadable search path must not be indistinguishable from no skills installed"
        );
    }

    #[test]
    fn one_directory_reached_twice_dedupes_but_two_directories_one_name_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "shared", true);
        let root = dir.path().to_string_lossy().to_string();

        let deduped = discover_skills(&[root.clone(), root.clone()]).unwrap();
        assert_eq!(
            deduped.len(),
            1,
            "the same canonical directory reached twice is one skill, dropped silently"
        );

        let other = tempfile::tempdir().unwrap();
        write_skill(other.path(), "shared", true);
        let err = discover_skills(&[root, other.path().to_string_lossy().to_string()])
            .expect_err("one name from two different roots must not resolve silently");
        let err = format!("{err:#}");
        assert!(err.contains("both provide the skill 'shared'"), "{err}");
        assert!(
            err.contains("would make results depend on config order"),
            "must explain why first-wins was rejected: {err}"
        );
    }

    fn config_with(skills: crate::config::SkillsConfig, legacy: Vec<String>) -> Config {
        Config {
            skills,
            analytics: crate::config::AnalyticsConfig {
                correction_patterns: legacy,
                ..crate::config::AnalyticsConfig::default()
            },
            ..Config::default()
        }
    }

    // THE backward-compatibility contract. With nothing configured, the resolved policy must
    // compile to exactly what `analytics::compile_patterns` produces today -- same categories, in
    // the same order, with the same regex source. Comparing at the COMPILED level rather than
    // comparing output text is what makes this robust: it fails if a category is renamed,
    // reordered, dropped, or has a pattern edited, without needing a session fixture.
    #[test]
    fn the_default_resolution_compiles_to_todays_built_in_rules() {
        let config = Config::default();
        let resolved = ResolvedCorrectionPolicySet::resolve(&config, None).unwrap();
        assert_eq!(resolved.policies().len(), 1);

        let new_rules: Vec<(String, String)> = resolved.policies()[0]
            .rules()
            .iter()
            .map(|(category, regex)| (category.clone(), regex.as_str().to_string()))
            .collect();
        let old_rules: Vec<(String, String)> = crate::analytics::compile_patterns(&config)
            .unwrap()
            .iter()
            .map(|(category, regex)| (category.clone(), regex.as_str().to_string()))
            .collect();

        assert_eq!(
            new_rules, old_rules,
            "the embedded policy must be indistinguishable from the built-ins it replaces"
        );
        assert_eq!(resolved.receipts()[0].name, EMBEDDED_POLICY_NAME);
    }

    #[test]
    fn cli_selection_replaces_configured_selection_and_keeps_argument_order() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "alpha", true);
        write_skill(dir.path(), "bravo", true);
        let config = config_with(
            crate::config::SkillsConfig {
                search_paths: vec![dir.path().to_string_lossy().to_string()],
                enabled: Some(vec!["alpha".to_string()]),
                ..Default::default()
            },
            Vec::new(),
        );

        let configured = ResolvedCorrectionPolicySet::resolve(&config, None).unwrap();
        assert_eq!(
            configured
                .receipts()
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            ["fixture"],
            "with no CLI list the configured selection applies"
        );

        // A CLI list REPLACES rather than merges, and argument order is evaluation order.
        let overridden = ResolvedCorrectionPolicySet::resolve(
            &config,
            Some(&["bravo".to_string(), "alpha".to_string()]),
        )
        .unwrap();
        assert_eq!(
            overridden.policies().len(),
            2,
            "both selected skills apply, and neither the configured one nor the embedded default \
             is silently added"
        );
    }

    #[test]
    fn an_explicit_empty_selection_matches_nothing() {
        let config = config_with(
            crate::config::SkillsConfig {
                enabled: Some(Vec::new()),
                ..Default::default()
            },
            Vec::new(),
        );
        let resolved = ResolvedCorrectionPolicySet::resolve(&config, None).unwrap();
        assert!(resolved.is_empty());
        assert!(
            resolved.classify("you forgot the tests").is_none(),
            "`enabled = []` is a deliberate no-policy state, not a fallback to the default"
        );
    }

    #[test]
    fn the_legacy_field_keeps_working_alone_but_never_alongside_a_selection() {
        let legacy = vec![
            "custom:^\\s*nope\\b".to_string(),
            "custom:\\bwrong turn\\b".to_string(),
        ];
        let config = config_with(Default::default(), legacy.clone());
        let resolved = ResolvedCorrectionPolicySet::resolve(&config, None).unwrap();

        let receipt = &resolved.receipts()[0];
        assert_eq!(receipt.name, LEGACY_POLICY_NAME);
        assert!(
            !receipt.sha256.is_empty(),
            "even the compatibility path must be reproducible"
        );
        // Same-category entries are ORed and the field REPLACES the built-ins entirely.
        let hit = resolved.classify("that was a wrong turn").unwrap().1;
        assert_eq!(hit.category, "custom");
        assert!(
            resolved.classify("you forgot the tests").is_none(),
            "the legacy field replaces the built-ins rather than adding to them"
        );

        // Combining old and new is refused rather than given an invented precedence.
        let both = config_with(
            crate::config::SkillsConfig {
                enabled: Some(vec!["ai-session-search".to_string()]),
                ..Default::default()
            },
            legacy,
        );
        let err = format!(
            "{:#}",
            ResolvedCorrectionPolicySet::resolve(&both, None)
                .expect_err("combining the legacy field with a selection must be refused")
        );
        assert!(err.contains("cannot be combined"), "{err}");
        assert!(
            err.contains("aise skills create"),
            "must name the migration: {err}"
        );
    }

    #[test]
    fn selection_errors_distinguish_unknown_from_policyless_and_protect_the_reserved_name() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "no-policy", false);
        // A directory that tries to shadow the embedded name.
        write_skill(dir.path(), EMBEDDED_POLICY_NAME, true);
        let config = config_with(
            crate::config::SkillsConfig {
                search_paths: vec![dir.path().to_string_lossy().to_string()],
                ..Default::default()
            },
            Vec::new(),
        );

        let unknown = format!(
            "{:#}",
            ResolvedCorrectionPolicySet::resolve(&config, Some(&["ghost".to_string()]))
                .expect_err("an unknown name must be refused")
        );
        assert!(unknown.contains("unknown skill 'ghost'"), "{unknown}");
        assert!(unknown.contains("aise skills list"), "{unknown}");

        let policyless = format!(
            "{:#}",
            ResolvedCorrectionPolicySet::resolve(&config, Some(&["no-policy".to_string()]))
                .expect_err("a skill without a policy must be refused")
        );
        assert!(
            policyless.contains("no corrections/policy.toml"),
            "the fix differs from an unknown name, so the message must too: {policyless}"
        );

        // The reserved name resolves to the EMBEDDED policy even though a directory of that name
        // is discoverable: a stale or hostile install must not redefine the product default.
        let reserved = ResolvedCorrectionPolicySet::resolve(
            &config,
            Some(&[EMBEDDED_POLICY_NAME.to_string()]),
        )
        .unwrap();
        assert_eq!(
            reserved.policies()[0].identity().source,
            CorrectionPolicySource::Embedded,
            "the embedded name must not be shadowable by a discovered directory"
        );
    }

    #[test]
    fn malformed_toml_names_where_it_came_from() {
        let err = CorrectionPolicy::parse_toml(
            "schema_version = ",
            CorrectionPolicySource::File {
                path: PathBuf::from("/skills/mine/corrections/policy.toml"),
            },
        );
        let err = message(err);
        assert!(
            err.contains("/skills/mine/corrections/policy.toml"),
            "a parse failure must name the file: {err}"
        );
    }
}
