// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

//! Neutral Agent Skill identity and descriptor loading.
//!
//! This module owns the standard `SKILL.md` package boundary. Capability-specific parsing and
//! execution stay in their domain modules so cataloging a skill never eagerly compiles every
//! deterministic capability it can see.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::str;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use yaml_rust2::parser::{Event, EventReceiver, Parser};
use yaml_rust2::{Yaml, YamlLoader};

/// Maximum bytes read while looking for the closing YAML frontmatter delimiter.
///
/// Skill bodies are intentionally outside this budget: catalog construction reads identity and
/// metadata only, so its memory use is bounded independently of instruction-body size.
pub(crate) const MAX_SKILL_FRONTMATTER_BYTES: usize = 64 * 1024;
/// Longest `description` accepted by the Agent Skills specification.
pub(crate) const MAX_DESCRIPTION_CHARS: usize = 1024;
/// Longest `name` accepted by the Agent Skills specification.
pub(crate) const MAX_NAME_CHARS: usize = 64;
/// Structural depth ceiling for metadata we inspect.
const MAX_YAML_DEPTH: usize = 32;

/// Standard Agent Skill metadata consumed by Aise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentSkillFrontmatter {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) license: Option<String>,
    pub(crate) compatibility: Option<String>,
    pub(crate) metadata: BTreeMap<String, String>,
    pub(crate) allowed_tools: Option<String>,
}

/// Adjacent deterministic capability-file state without compiling its domain policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CapabilityFileState {
    Absent,
    Available { path: PathBuf },
    Invalid { path: PathBuf, problem: String },
}

impl CapabilityFileState {
    /// The capability file to execute, or an error naming what to add.
    ///
    /// One definition because two callers — `AnalysisService::run_skill` and
    /// `message_classification` — need exactly this decision, and both previously spelled the same
    /// three-arm match and the same two sentences inline. A filename rename then had to be applied
    /// twice to stay truthful, which is precisely the drift [`CAPABILITY_FILE`] exists to prevent.
    pub(crate) fn require_path(self, skill_name: &str) -> Result<PathBuf> {
        match self {
            Self::Available { path } => Ok(path),
            Self::Absent => bail!(
                "skill {skill_name:?} has no adjacent message-classification capability; add \
                 {CAPABILITY_FILE} beside its SKILL.md, or load that SKILL.md in an agent harness \
                 instead"
            ),
            Self::Invalid { problem, .. } => {
                bail!("skill {skill_name:?} has an invalid capability: {problem}")
            }
        }
    }
}

/// Filename a skill package keeps its deterministic capability in.
///
/// Vendor-namespaced because the Agent Skills specification defines no capability concept: this is
/// an aise extension dropped into a shared directory layout, and the specification's own advice for
/// `metadata` keys — keep names unique enough to avoid accidental conflicts — applies equally to a
/// filename. Lowercase rather than styled like the specification's `SKILL.md` anchor, which would
/// imply standard status it does not have.
pub(crate) const CAPABILITY_FILE: &str = "aise-capability.toml";

/// One standard-shaped skill package loaded from a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillDescriptor {
    pub(crate) root: PathBuf,
    pub(crate) directory_name: String,
    pub(crate) frontmatter: Option<AgentSkillFrontmatter>,
    pub(crate) capability: CapabilityFileState,
    pub(crate) diagnostics: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkillRootState {
    Available,
    Missing,
    Unreadable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillSearchRootStatus {
    pub(crate) configured_path: PathBuf,
    pub(crate) canonical_path: Option<PathBuf>,
    pub(crate) state: SkillRootState,
    pub(crate) problem: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillCatalog {
    pub(crate) roots: Vec<SkillSearchRootStatus>,
    pub(crate) skills: Vec<SkillDescriptor>,
}

/// Valid standard Agent Skill name shared by every public adapter.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct SkillName(String);

impl SkillName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SkillName {
    type Error = String;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        match skill_name_problem(&value) {
            Some(problem) => Err(format!("invalid skill name {value:?}: {problem}")),
            None => Ok(Self(value)),
        }
    }
}

impl<'de> Deserialize<'de> for SkillName {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_from(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillNameSelector {
    pub name: SkillName,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillPathSelector {
    pub path: PathBuf,
}

/// Canonical wire selector: exactly one name or path object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SkillSelector {
    Name(SkillNameSelector),
    Path(SkillPathSelector),
}

impl SkillSelector {
    /// Build a validated name selector without exposing the wire-shape wrapper types.
    pub fn name(value: impl Into<String>) -> Result<Self> {
        let name = SkillName::try_from(value.into()).map_err(anyhow::Error::msg)?;
        Ok(Self::Name(SkillNameSelector { name }))
    }

    /// Build a path selector. Existence and canonicalization are checked at resolution time.
    pub fn path(value: impl Into<PathBuf>) -> Self {
        Self::Path(SkillPathSelector { path: value.into() })
    }
}

/// Resolve one name/path selector to the same strict descriptor type.
pub(crate) fn resolve_skill_selector(
    selector: &SkillSelector,
    catalog: &SkillCatalog,
) -> Result<SkillDescriptor> {
    match selector {
        SkillSelector::Name(selector) => {
            let matches = catalog
                .skills
                .iter()
                .filter(|skill| {
                    skill
                        .frontmatter
                        .as_ref()
                        .is_some_and(|frontmatter| frontmatter.name == selector.name.as_str())
                })
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [] => bail!(
                    "unknown skill {:?}; run `aise skills list` to inspect the skill catalog and \
                     available names",
                    selector.name.as_str()
                ),
                [descriptor] if descriptor.diagnostics.is_empty() => Ok((*descriptor).clone()),
                [descriptor] => bail!(
                    "skill {:?} is invalid: {}",
                    selector.name.as_str(),
                    descriptor.diagnostics.join("; ")
                ),
                _ => bail!(
                    "skill name {:?} is ambiguous across {}",
                    selector.name.as_str(),
                    matches
                        .iter()
                        .map(|skill| skill.root.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }
        SkillSelector::Path(selector) => {
            if selector.path.as_os_str().is_empty() {
                bail!("skill path is empty; pass a skill directory or exact SKILL.md path");
            }
            let expanded = crate::util::expand_tilde_required(&selector.path)?;
            let root = if expanded.is_dir() {
                expanded
            } else {
                let file_name = expanded.file_name().and_then(|name| name.to_str());
                if file_name != Some("SKILL.md") {
                    bail!(
                        "skill path {} must be a directory or a file named exactly SKILL.md",
                        expanded.display()
                    );
                }
                if !expanded.is_file() {
                    bail!("skill path {} is not a readable file", expanded.display());
                }
                expanded
                    .parent()
                    .context("SKILL.md path has no containing skill directory")?
                    .to_path_buf()
            };
            let descriptor = load_skill_descriptor(&root)?;
            if !descriptor.diagnostics.is_empty() {
                bail!(
                    "skill at {} is invalid: {}",
                    descriptor.root.display(),
                    descriptor.diagnostics.join("; ")
                );
            }
            Ok(descriptor)
        }
    }
}

/// Resolve ordered selectors and reject canonical duplicates.
///
/// For `K` selectors and `N` catalog entries, name resolution is `O(K * N)` and canonical
/// duplicate detection is expected `O(K)`. Skill selections are ordinarily one or a few packages,
/// so retaining the catalog's deterministic vector avoids a second name index and its drift risk.
pub(crate) fn resolve_skill_selectors(
    selectors: &[SkillSelector],
    catalog: &SkillCatalog,
) -> Result<Vec<SkillDescriptor>> {
    let mut resolved = Vec::with_capacity(selectors.len());
    let mut seen = HashSet::with_capacity(selectors.len());
    for selector in selectors {
        let descriptor = resolve_skill_selector(selector, catalog)?;
        if !seen.insert(descriptor.root.clone()) {
            bail!(
                "skill {} was selected more than once; remove the duplicate name or path",
                descriptor.root.display()
            );
        }
        resolved.push(descriptor);
    }
    Ok(resolved)
}

/// Load one descriptor without reading the Markdown instruction body or compiling capability data.
pub(crate) fn load_skill_descriptor(root: &Path) -> Result<SkillDescriptor> {
    let metadata = std::fs::symlink_metadata(root)
        .with_context(|| format!("failed to inspect skill directory {}", root.display()))?;
    if !metadata.is_dir() && !metadata.file_type().is_symlink() {
        bail!("skill path {} must resolve to a directory", root.display());
    }
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("failed to resolve skill directory {}", root.display()))?;
    let directory_name = canonical_root
        .file_name()
        .and_then(|name| name.to_str())
        .context("skill directory name is not valid UTF-8")?
        .to_string();

    let skill_md_path = canonical_root.join("SKILL.md");
    let mut diagnostics = Vec::new();
    let frontmatter = match std::fs::symlink_metadata(&skill_md_path) {
        Err(error) => {
            diagnostics.push(format!("SKILL.md: cannot inspect file: {error}"));
            None
        }
        Ok(metadata) if metadata.file_type().is_symlink() => {
            diagnostics.push("SKILL.md must be a regular file, not a symlink".to_string());
            None
        }
        Ok(metadata) if !metadata.is_file() => {
            diagnostics.push("SKILL.md must be a regular file".to_string());
            None
        }
        Ok(_) => match read_frontmatter_prefix(&skill_md_path)
            .and_then(|bytes| parse_skill_frontmatter(&bytes))
        {
            Ok(frontmatter) => {
                if frontmatter.name != directory_name {
                    diagnostics.push(format!(
                        "SKILL.md name {:?} does not match directory name {:?}",
                        frontmatter.name, directory_name
                    ));
                }
                Some(frontmatter)
            }
            Err(error) => {
                diagnostics.push(format!("SKILL.md: {error:#}"));
                None
            }
        },
    };

    let capability_path = canonical_root.join(CAPABILITY_FILE);
    let capability = match std::fs::symlink_metadata(&capability_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => CapabilityFileState::Absent,
        Err(error) => CapabilityFileState::Invalid {
            path: capability_path,
            problem: format!("cannot inspect {CAPABILITY_FILE}: {error}"),
        },
        Ok(metadata) if metadata.file_type().is_symlink() => CapabilityFileState::Invalid {
            path: capability_path,
            problem: format!("{CAPABILITY_FILE} must be a regular file, not a symlink"),
        },
        Ok(metadata) if !metadata.is_file() => CapabilityFileState::Invalid {
            path: capability_path,
            problem: format!("{CAPABILITY_FILE} must be a regular file"),
        },
        Ok(_) => CapabilityFileState::Available {
            path: capability_path,
        },
    };
    if let CapabilityFileState::Invalid { problem, .. } = &capability {
        diagnostics.push(problem.clone());
    }

    Ok(SkillDescriptor {
        root: canonical_root,
        directory_name,
        frontmatter,
        capability,
        diagnostics,
    })
}

/// Catalog standard skills one level below each root in deterministic order.
///
/// Root failures and malformed skills are retained as diagnostics. Only catastrophic allocation
/// failures can abort the process, so one broken neighbor never hides the rest of the catalog.
pub(crate) fn load_skill_catalog(search_roots: &[PathBuf]) -> SkillCatalog {
    let mut roots = Vec::new();
    let mut skills = Vec::new();
    let mut seen_roots = BTreeSet::new();
    let mut seen_skills = BTreeSet::new();

    for configured_path in search_roots {
        if !configured_path.exists() {
            roots.push(SkillSearchRootStatus {
                configured_path: configured_path.clone(),
                canonical_path: None,
                state: SkillRootState::Missing,
                problem: None,
            });
            continue;
        }
        let canonical_path = match configured_path.canonicalize() {
            Ok(path) => path,
            Err(error) => {
                roots.push(SkillSearchRootStatus {
                    configured_path: configured_path.clone(),
                    canonical_path: None,
                    state: SkillRootState::Unreadable,
                    problem: Some(format!("cannot resolve root: {error}")),
                });
                continue;
            }
        };
        if !seen_roots.insert(canonical_path.clone()) {
            continue;
        }
        if !canonical_path.is_dir() {
            roots.push(SkillSearchRootStatus {
                configured_path: configured_path.clone(),
                canonical_path: Some(canonical_path),
                state: SkillRootState::Unreadable,
                problem: Some("skill search root is not a directory".to_string()),
            });
            continue;
        }

        let candidates = if canonical_path.join("SKILL.md").is_file() {
            Ok(vec![canonical_path.clone()])
        } else {
            std::fs::read_dir(&canonical_path).and_then(|entries| {
                let mut candidates = entries
                    .collect::<std::io::Result<Vec<_>>>()?
                    .into_iter()
                    .map(|entry| entry.path())
                    .filter(|entry| entry.join("SKILL.md").is_file())
                    .collect::<Vec<_>>();
                candidates.sort();
                Ok(candidates)
            })
        };
        let candidates = match candidates {
            Ok(candidates) => candidates,
            Err(error) => {
                roots.push(SkillSearchRootStatus {
                    configured_path: configured_path.clone(),
                    canonical_path: Some(canonical_path),
                    state: SkillRootState::Unreadable,
                    problem: Some(format!("cannot list root: {error}")),
                });
                continue;
            }
        };
        roots.push(SkillSearchRootStatus {
            configured_path: configured_path.clone(),
            canonical_path: Some(canonical_path),
            state: SkillRootState::Available,
            problem: None,
        });

        for candidate in candidates {
            match load_skill_descriptor(&candidate) {
                Ok(descriptor) if seen_skills.insert(descriptor.root.clone()) => {
                    skills.push(descriptor);
                }
                Ok(_) => {}
                Err(error) => {
                    let directory_name = candidate
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("<invalid-name>")
                        .to_string();
                    skills.push(SkillDescriptor {
                        root: candidate,
                        directory_name,
                        frontmatter: None,
                        capability: CapabilityFileState::Absent,
                        diagnostics: vec![format!("{error:#}")],
                    });
                }
            }
        }
    }

    let mut names = BTreeMap::<String, Vec<usize>>::new();
    for (index, skill) in skills.iter().enumerate() {
        names
            .entry(skill.directory_name.clone())
            .or_default()
            .push(index);
    }
    for (name, indexes) in names {
        if indexes.len() < 2 {
            continue;
        }
        let locations = indexes
            .iter()
            .map(|index| skills[*index].root.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        for index in indexes {
            skills[index].diagnostics.push(format!(
                "skill name {name:?} is ambiguous across: {locations}"
            ));
        }
    }

    SkillCatalog { roots, skills }
}

fn read_frontmatter_prefix(path: &Path) -> Result<Vec<u8>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut bytes = Vec::with_capacity(MAX_SKILL_FRONTMATTER_BYTES.min(8 * 1024));
    file.take((MAX_SKILL_FRONTMATTER_BYTES + 16) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read bounded frontmatter from {}", path.display()))?;
    Ok(bytes)
}

/// Explain why a proposed Agent Skill name is invalid.
pub(crate) fn skill_name_problem(name: &str) -> Option<String> {
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

/// Parse one leading, bounded `SKILL.md` YAML frontmatter mapping.
pub(crate) fn parse_skill_frontmatter(bytes: &[u8]) -> Result<AgentSkillFrontmatter> {
    let yaml_bytes = frontmatter_bytes(bytes)?;
    let yaml = str::from_utf8(yaml_bytes).context("SKILL.md frontmatter is not valid UTF-8")?;
    validate_yaml_events(yaml)?;

    let documents =
        YamlLoader::load_from_str(yaml).context("failed to parse SKILL.md YAML frontmatter")?;
    if documents.len() != 1 {
        bail!("SKILL.md frontmatter contains multiple documents; use one top-level mapping");
    }
    let mapping = documents[0]
        .as_hash()
        .context("SKILL.md frontmatter must be one YAML mapping")?;

    let required_string = |key: &str| -> Result<String> {
        yaml_field(mapping, key)
            .with_context(|| format!("SKILL.md frontmatter is missing `{key}`"))?
            .as_str()
            .map(str::to_owned)
            .with_context(|| format!("SKILL.md frontmatter `{key}` must be a string"))
    };
    let optional_string = |key: &str| -> Result<Option<String>> {
        yaml_field(mapping, key)
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .with_context(|| format!("SKILL.md frontmatter `{key}` must be a string"))
            })
            .transpose()
    };

    let name = required_string("name")?;
    if let Some(problem) = skill_name_problem(&name) {
        bail!("invalid SKILL.md `name` {name:?}: {problem}");
    }
    let description = required_string("description")?;
    let description_chars = description.chars().count();
    if description.trim().is_empty() {
        bail!("SKILL.md frontmatter `description` must not be empty");
    }
    if description_chars > MAX_DESCRIPTION_CHARS {
        bail!(
            "SKILL.md frontmatter `description` is {description_chars} characters; the limit is \
             {MAX_DESCRIPTION_CHARS}"
        );
    }

    let metadata = match yaml_field(mapping, "metadata") {
        None => BTreeMap::new(),
        Some(value) => {
            let values = value
                .as_hash()
                .context("SKILL.md frontmatter `metadata` must be a mapping")?;
            let mut metadata = BTreeMap::new();
            for (key, value) in values {
                let key = key
                    .as_str()
                    .context("SKILL.md frontmatter metadata keys must be strings")?;
                let value = value.as_str().with_context(|| {
                    format!("SKILL.md frontmatter `metadata.{key}` must be a string")
                })?;
                metadata.insert(key.to_string(), value.to_string());
            }
            metadata
        }
    };

    Ok(AgentSkillFrontmatter {
        name,
        description,
        license: optional_string("license")?,
        compatibility: optional_string("compatibility")?,
        metadata,
        allowed_tools: optional_string("allowed-tools")?,
    })
}

fn yaml_field<'a>(mapping: &'a yaml_rust2::yaml::Hash, name: &str) -> Option<&'a Yaml> {
    mapping.get(&Yaml::String(name.to_string()))
}

/// Return only the YAML bytes between the leading and closing fences.
///
/// The scan is linear in at most `MAX_SKILL_FRONTMATTER_BYTES` bytes and never allocates the
/// instruction body.
fn frontmatter_bytes(bytes: &[u8]) -> Result<&[u8]> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let Some((opening, mut cursor)) = next_line(bytes, 0) else {
        bail!("SKILL.md has no leading YAML frontmatter fence");
    };
    if opening != b"---" {
        bail!("SKILL.md has no leading YAML frontmatter fence");
    }
    let content_start = cursor;
    loop {
        if cursor.saturating_sub(content_start) > MAX_SKILL_FRONTMATTER_BYTES {
            bail!("SKILL.md frontmatter exceeds the 64 KiB safety limit");
        }
        let Some((line, next)) = next_line(bytes, cursor) else {
            bail!("SKILL.md frontmatter has no closing `---` delimiter");
        };
        if line == b"---" {
            if cursor.saturating_sub(content_start) > MAX_SKILL_FRONTMATTER_BYTES {
                bail!("SKILL.md frontmatter exceeds the 64 KiB safety limit");
            }
            return Ok(&bytes[content_start..cursor]);
        }
        cursor = next;
    }
}

fn next_line(bytes: &[u8], start: usize) -> Option<(&[u8], usize)> {
    if start >= bytes.len() {
        return None;
    }
    let relative_end = bytes[start..].iter().position(|byte| *byte == b'\n');
    let (end, next) = match relative_end {
        Some(offset) => (start + offset, start + offset + 1),
        None => (bytes.len(), bytes.len()),
    };
    let line = bytes[start..end]
        .strip_suffix(b"\r")
        .unwrap_or(&bytes[start..end]);
    Some((line, next))
}

#[derive(Debug)]
enum Container {
    Mapping {
        expecting_key: bool,
        keys: BTreeSet<String>,
    },
    Sequence,
}

#[derive(Default)]
struct EventSink {
    events: Vec<Event>,
}

impl EventReceiver for EventSink {
    fn on_event(&mut self, event: Event) {
        self.events.push(event);
    }
}

/// Reject YAML features that make identity ambiguous or permit expansive alias graphs.
fn validate_yaml_events(yaml: &str) -> Result<()> {
    let mut sink = EventSink::default();
    Parser::new_from_str(yaml)
        .load(&mut sink, true)
        .context("failed to parse SKILL.md YAML frontmatter")?;

    let document_count = sink
        .events
        .iter()
        .filter(|event| matches!(event, Event::DocumentStart))
        .count();
    if document_count != 1 {
        bail!("SKILL.md frontmatter contains multiple documents; use one top-level mapping");
    }

    let mut stack = Vec::<Container>::new();
    let mut root_is_mapping = false;
    for event in sink.events {
        match event {
            Event::Alias(_) => {
                bail!("SKILL.md frontmatter contains an alias; aliases are not allowed")
            }
            Event::Scalar(value, _, anchor, tag) => {
                reject_anchor_or_tag(anchor, tag.is_some())?;
                match stack.last_mut() {
                    Some(Container::Mapping {
                        expecting_key,
                        keys,
                    }) if *expecting_key => {
                        if !keys.insert(value.clone()) {
                            bail!("SKILL.md frontmatter contains duplicate key {value:?}");
                        }
                        *expecting_key = false;
                    }
                    Some(Container::Mapping { expecting_key, .. }) => *expecting_key = true,
                    Some(Container::Sequence) => {}
                    None => {}
                }
            }
            Event::MappingStart(anchor, tag) => {
                reject_anchor_or_tag(anchor, tag.is_some())?;
                consume_container_value(&mut stack)?;
                if stack.is_empty() {
                    root_is_mapping = true;
                }
                stack.push(Container::Mapping {
                    expecting_key: true,
                    keys: BTreeSet::new(),
                });
                if stack.len() > MAX_YAML_DEPTH {
                    bail!("SKILL.md frontmatter exceeds the {MAX_YAML_DEPTH}-level depth limit");
                }
            }
            Event::SequenceStart(anchor, tag) => {
                reject_anchor_or_tag(anchor, tag.is_some())?;
                consume_container_value(&mut stack)?;
                stack.push(Container::Sequence);
                if stack.len() > MAX_YAML_DEPTH {
                    bail!("SKILL.md frontmatter exceeds the {MAX_YAML_DEPTH}-level depth limit");
                }
            }
            Event::MappingEnd => {
                let Some(Container::Mapping { expecting_key, .. }) = stack.pop() else {
                    bail!("SKILL.md frontmatter has an unbalanced mapping");
                };
                if !expecting_key {
                    bail!("SKILL.md frontmatter mapping has a key without a value");
                }
            }
            Event::SequenceEnd => {
                if !matches!(stack.pop(), Some(Container::Sequence)) {
                    bail!("SKILL.md frontmatter has an unbalanced sequence");
                }
            }
            Event::Nothing
            | Event::StreamStart
            | Event::StreamEnd
            | Event::DocumentStart
            | Event::DocumentEnd => {}
        }
    }
    if !stack.is_empty() {
        bail!("SKILL.md frontmatter has an unclosed YAML container");
    }
    if !root_is_mapping {
        bail!("SKILL.md frontmatter must be one YAML mapping");
    }
    Ok(())
}

fn consume_container_value(stack: &mut [Container]) -> Result<()> {
    if let Some(Container::Mapping { expecting_key, .. }) = stack.last_mut() {
        if *expecting_key {
            bail!("SKILL.md frontmatter contains a complex key; keys must be strings");
        }
        *expecting_key = true;
    }
    Ok(())
}

fn reject_anchor_or_tag(anchor: usize, has_tag: bool) -> Result<()> {
    if anchor != 0 {
        bail!("SKILL.md frontmatter contains an anchor; anchors are not allowed");
    }
    if has_tag {
        bail!("SKILL.md frontmatter contains a tag; tags are not allowed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_accepts_standard_scalar_styles_bom_and_line_endings() {
        let fixtures = [
            (
                b"---\nname: my-skill\ndescription: plain text\n---\nbody\n".as_slice(),
                "plain text",
            ),
            (
                b"---\nname: 'my-skill'\ndescription: \"quoted text\"\n---\n".as_slice(),
                "quoted text",
            ),
            (
                b"---\nname: my-skill\ndescription: >\n  folded\n  text\n---\n".as_slice(),
                "folded text\n",
            ),
            (
                b"---\nname: my-skill\ndescription: |\n  literal\n  text\n---\n".as_slice(),
                "literal\ntext\n",
            ),
            (
                b"\xEF\xBB\xBF---\r\nname: my-skill\r\ndescription: windows\r\n---\r\n".as_slice(),
                "windows",
            ),
        ];

        for (source, expected_description) in fixtures {
            let parsed = parse_skill_frontmatter(source)
                .unwrap_or_else(|error| panic!("fixture should parse: {error:#}"));
            assert_eq!(parsed.name, "my-skill");
            assert_eq!(parsed.description, expected_description);
        }
    }

    #[test]
    fn frontmatter_rejects_ambiguous_or_expansive_yaml_features() {
        for (label, source) in [
            (
                "duplicate key",
                "---\nname: first\nname: second\ndescription: test\n---\n",
            ),
            (
                "anchor",
                "---\nname: &name my-skill\ndescription: *name\n---\n",
            ),
            (
                "tag",
                "---\nname: my-skill\ndescription: !custom value\n---\n",
            ),
            (
                "multiple documents",
                "---\nname: my-skill\ndescription: one\n...\nname: other\n---\n",
            ),
        ] {
            let error = parse_skill_frontmatter(source.as_bytes())
                .expect_err(label)
                .to_string();
            assert!(
                error.contains(label),
                "{label} should be named in the diagnostic: {error}"
            );
        }
    }

    #[test]
    fn frontmatter_size_is_bounded_before_yaml_allocation() {
        let source = format!(
            "---\nname: my-skill\ndescription: {}\n---\n",
            "x".repeat(MAX_SKILL_FRONTMATTER_BYTES)
        );
        let error = parse_skill_frontmatter(source.as_bytes())
            .expect_err("oversized frontmatter must fail")
            .to_string();
        assert!(error.contains("64 KiB"), "{error}");
    }

    fn write_skill(root: &Path, directory_name: &str, declared_name: &str) -> PathBuf {
        let skill = root.join(directory_name);
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            format!(
                "---\nname: {declared_name}\ndescription: descriptor fixture\n---\n{}",
                "instruction body\n".repeat(10_000)
            ),
        )
        .unwrap();
        skill
    }

    #[test]
    fn descriptor_reports_identity_disagreement_without_reading_the_instruction_body() {
        let temp = tempfile::tempdir().unwrap();
        let skill = write_skill(temp.path(), "directory-name", "declared-name");
        let descriptor = load_skill_descriptor(&skill).unwrap();

        assert_eq!(descriptor.directory_name, "directory-name");
        assert_eq!(
            descriptor
                .frontmatter
                .as_ref()
                .map(|frontmatter| frontmatter.name.as_str()),
            Some("declared-name")
        );
        assert!(
            descriptor
                .diagnostics
                .iter()
                .any(|problem| problem.contains("does not match directory name")),
            "{:?}",
            descriptor.diagnostics
        );
        assert_eq!(descriptor.capability, CapabilityFileState::Absent);
    }

    #[cfg(unix)]
    #[test]
    fn adjacent_capability_distinguishes_absent_regular_directory_and_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let skill = write_skill(temp.path(), "my-skill", "my-skill");
        assert_eq!(
            load_skill_descriptor(&skill).unwrap().capability,
            CapabilityFileState::Absent
        );

        let capability = skill.join(CAPABILITY_FILE);
        std::fs::write(
            &capability,
            "schema_version = 1\nkind = \"message-classification\"\n",
        )
        .unwrap();
        assert!(matches!(
            load_skill_descriptor(&skill).unwrap().capability,
            CapabilityFileState::Available { .. }
        ));

        std::fs::remove_file(&capability).unwrap();
        std::fs::create_dir(&capability).unwrap();
        let directory_state = load_skill_descriptor(&skill).unwrap();
        assert!(matches!(
            directory_state.capability,
            CapabilityFileState::Invalid { .. }
        ));
        assert!(directory_state
            .diagnostics
            .iter()
            .any(|problem| problem.contains("regular file")));

        std::fs::remove_dir(&capability).unwrap();
        let target = temp.path().join("outside.toml");
        std::fs::write(&target, "kind = \"message-classification\"\n").unwrap();
        symlink(&target, &capability).unwrap();
        let symlink_state = load_skill_descriptor(&skill).unwrap();
        assert!(matches!(
            symlink_state.capability,
            CapabilityFileState::Invalid { .. }
        ));
        assert!(symlink_state
            .diagnostics
            .iter()
            .any(|problem| problem.contains("symlink")));
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_rejects_a_final_skill_md_symlink_without_reading_its_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let skill = temp.path().join("my-skill");
        std::fs::create_dir_all(&skill).unwrap();
        let outside = temp.path().join("outside-secret.md");
        std::fs::write(
            &outside,
            "---\nname: my-skill\ndescription: sentinel-secret\n---\n",
        )
        .unwrap();
        symlink(&outside, skill.join("SKILL.md")).unwrap();

        let descriptor = load_skill_descriptor(&skill).unwrap();
        assert!(descriptor.frontmatter.is_none());
        assert!(
            descriptor
                .diagnostics
                .iter()
                .any(|problem| problem.contains("SKILL.md must be a regular file, not a symlink")),
            "{:?}",
            descriptor.diagnostics
        );
        assert!(
            descriptor
                .diagnostics
                .iter()
                .all(|problem| !problem.contains("sentinel-secret")),
            "target bytes must not be parsed into diagnostics"
        );
    }

    #[cfg(unix)]
    #[test]
    fn catalog_reports_root_states_dedupes_aliases_and_retains_name_conflicts() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let first_root = temp.path().join("first");
        let second_root = temp.path().join("second");
        std::fs::create_dir_all(&first_root).unwrap();
        std::fs::create_dir_all(&second_root).unwrap();
        write_skill(&first_root, "same-name", "same-name");
        write_skill(&second_root, "same-name", "same-name");
        let first_alias = temp.path().join("first-alias");
        symlink(&first_root, &first_alias).unwrap();
        let missing = temp.path().join("missing");
        let not_directory = temp.path().join("not-directory");
        std::fs::write(&not_directory, "not a root").unwrap();

        let catalog = load_skill_catalog(&[
            first_root.clone(),
            first_alias,
            missing.clone(),
            not_directory.clone(),
            second_root,
        ]);

        assert_eq!(
            catalog
                .roots
                .iter()
                .map(|root| (&root.configured_path, root.state))
                .collect::<Vec<_>>(),
            vec![
                (&first_root, SkillRootState::Available),
                (&missing, SkillRootState::Missing),
                (&not_directory, SkillRootState::Unreadable),
                (&temp.path().join("second"), SkillRootState::Available),
            ]
        );
        assert_eq!(catalog.skills.len(), 2);
        assert!(catalog.skills.iter().all(|skill| skill
            .diagnostics
            .iter()
            .any(|problem| problem.contains("ambiguous"))));
    }

    #[test]
    fn selector_wire_shape_requires_exactly_one_valid_name_or_path() {
        let named: SkillSelector =
            serde_json::from_value(serde_json::json!({"name": "my-skill"})).unwrap();
        assert_eq!(
            named,
            SkillSelector::Name(SkillNameSelector {
                name: SkillName::try_from("my-skill".to_string()).unwrap()
            })
        );
        let path: SkillSelector =
            serde_json::from_value(serde_json::json!({"path": "./my-skill"})).unwrap();
        assert_eq!(
            path,
            SkillSelector::Path(SkillPathSelector {
                path: PathBuf::from("./my-skill")
            })
        );

        for invalid in [
            serde_json::json!({}),
            serde_json::json!({"name": "my-skill", "path": "./my-skill"}),
            serde_json::json!({"name": "Bad_Name"}),
            serde_json::json!({"name": "my-skill", "extra": true}),
        ] {
            assert!(
                serde_json::from_value::<SkillSelector>(invalid.clone()).is_err(),
                "{invalid} must be rejected"
            );
        }
    }

    #[test]
    fn name_and_path_selectors_resolve_one_identity_and_duplicates_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("skills");
        let skill = write_skill(&root, "my-skill", "my-skill");
        let catalog = load_skill_catalog(std::slice::from_ref(&root));
        let selectors = [
            SkillSelector::Name(SkillNameSelector {
                name: SkillName::try_from("my-skill".to_string()).unwrap(),
            }),
            SkillSelector::Path(SkillPathSelector {
                path: skill.join("SKILL.md"),
            }),
        ];

        let named = resolve_skill_selector(&selectors[0], &catalog).unwrap();
        let by_path = resolve_skill_selector(&selectors[1], &catalog).unwrap();
        assert_eq!(named.root, by_path.root);
        let error = resolve_skill_selectors(&selectors, &catalog)
            .expect_err("canonical duplicate must fail")
            .to_string();
        assert!(error.contains("selected more than once"), "{error}");
    }
}
