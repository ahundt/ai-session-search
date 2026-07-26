//! Typed deterministic message-classification capability.
//!
//! Package identity belongs to the containing standard Agent Skill. This document therefore owns
//! only capability kind, schema version, ordered categories, and patterns.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::corrections::{
    CorrectionCategorySpec, CorrectionPolicy, CorrectionPolicySource, CorrectionPolicySpec,
    ResolvedCorrectionPolicySet,
};
use crate::hashing::sha256;

pub(crate) const MESSAGE_CLASSIFICATION_SCHEMA_VERSION: u32 = 1;
pub(crate) const MAX_LOADED_CAPABILITY_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub(crate) struct CapabilityLoadBudget {
    remaining_bytes: usize,
}

impl CapabilityLoadBudget {
    pub(crate) fn new() -> Self {
        Self {
            remaining_bytes: MAX_LOADED_CAPABILITY_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum CapabilityKind {
    MessageClassification,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MessageClassificationPolicySpec {
    pub(crate) schema_version: u32,
    pub(crate) kind: CapabilityKind,
    pub(crate) categories: Vec<CorrectionCategorySpec>,
}

impl MessageClassificationPolicySpec {
    pub(crate) fn parse_toml(source_text: &str) -> Result<Self> {
        toml::from_str(source_text).context("failed to parse message-classification capability")
    }

    pub(crate) fn compile(
        self,
        package_name: String,
        package_version: String,
        source: CorrectionPolicySource,
        exact_bytes: &[u8],
    ) -> Result<CorrectionPolicy> {
        if self.schema_version != MESSAGE_CLASSIFICATION_SCHEMA_VERSION {
            bail!(
                "unsupported message-classification schema_version {}; this build understands {}",
                self.schema_version,
                MESSAGE_CLASSIFICATION_SCHEMA_VERSION
            );
        }
        if self.kind != CapabilityKind::MessageClassification {
            bail!("capability kind is not message-classification");
        }
        CorrectionPolicySpec {
            schema_version: crate::corrections::CORRECTION_POLICY_SCHEMA_VERSION,
            name: package_name,
            version: package_version,
            categories: self.categories,
        }
        .compile(source, sha256(exact_bytes))
    }
}

#[cfg(test)]
pub(crate) fn load_and_compile(
    path: &Path,
    package_name: String,
    package_version: String,
) -> Result<CorrectionPolicy> {
    load_and_compile_with_budget(
        path,
        package_name,
        package_version,
        &mut CapabilityLoadBudget::new(),
    )
}

pub(crate) fn load_and_compile_with_budget(
    path: &Path,
    package_name: String,
    package_version: String,
    budget: &mut CapabilityLoadBudget,
) -> Result<CorrectionPolicy> {
    let available = budget.remaining_bytes;
    let consumed = MAX_LOADED_CAPABILITY_BYTES - available;
    let mut bytes = Vec::with_capacity(available.min(16 * 1024));
    File::open(path)
        .with_context(|| format!("failed to open capability document {}", path.display()))?
        .take((available + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read capability document {}", path.display()))?;
    if bytes.len() > available {
        bail!(
            "selected capability.toml documents exceed the 1 MiB ({} byte) aggregate safety \
             limit while reading {}: {} bytes were consumed by earlier selections and this file \
             contributes at least {} more; reduce comments or rules, split the capability, or \
             select fewer packages",
            MAX_LOADED_CAPABILITY_BYTES,
            path.display(),
            consumed,
            bytes.len()
        );
    }
    budget.remaining_bytes -= bytes.len();
    let text = std::str::from_utf8(&bytes)
        .with_context(|| format!("capability document {} is not UTF-8", path.display()))?;
    MessageClassificationPolicySpec::parse_toml(text)
        .with_context(|| {
            format!(
                "failed to parse message-classification capability {}",
                path.display()
            )
        })?
        .compile(
            package_name,
            package_version,
            CorrectionPolicySource::File {
                path: path.to_path_buf(),
            },
            &bytes,
        )
        .with_context(|| {
            format!(
                "failed to compile message-classification capability {}",
                path.display()
            )
        })
}

/// Compile an already-resolved ordered descriptor set under one aggregate byte budget.
///
/// Descriptor identity and duplicate validation belong to the neutral catalog. Capability kind,
/// schema, and compilation belong here, so CLI, Rust, Python, and MCP adapters do not repeat the
/// same frontmatter/version/file-state branching.
pub(crate) fn compile_skill_descriptors(
    descriptors: Vec<crate::skill_catalog::SkillDescriptor>,
) -> Result<ResolvedCorrectionPolicySet> {
    let mut budget = CapabilityLoadBudget::new();
    let mut policies = Vec::with_capacity(descriptors.len());
    for descriptor in descriptors {
        let frontmatter = descriptor
            .frontmatter
            .context("resolved skill has no valid frontmatter")?;
        let package_version = frontmatter
            .metadata
            .get("version")
            .cloned()
            .with_context(|| {
                format!(
                    "runnable skill {:?} must declare metadata.version in SKILL.md",
                    frontmatter.name
                )
            })?;
        let capability_path = match descriptor.capability {
            crate::skill_catalog::CapabilityFileState::Available { path } => path,
            crate::skill_catalog::CapabilityFileState::Absent => {
                bail!(
                    "skill {:?} has no adjacent message-classification capability.toml; load its \
                     SKILL.md in an agent harness instead",
                    frontmatter.name
                )
            }
            crate::skill_catalog::CapabilityFileState::Invalid { problem, .. } => {
                bail!(
                    "skill {:?} has an invalid capability: {problem}",
                    frontmatter.name
                )
            }
        };
        policies.push(load_and_compile_with_budget(
            &capability_path,
            frontmatter.name,
            package_version,
            &mut budget,
        )?);
    }
    Ok(ResolvedCorrectionPolicySet::from_policies(policies))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"schema_version = 1
kind = "message-classification"

[[categories]]
name = "clobber"
patterns = ['''\byou overwrote\b''']
"#;

    #[test]
    fn capability_uses_package_identity_and_digests_exact_document_bytes() {
        let policy = MessageClassificationPolicySpec::parse_toml(VALID)
            .unwrap()
            .compile(
                "my-review".to_string(),
                "2.1.0".to_string(),
                CorrectionPolicySource::Embedded,
                VALID.as_bytes(),
            )
            .unwrap();
        assert_eq!(policy.identity().name, "my-review");
        assert_eq!(policy.identity().version, "2.1.0");
        assert_eq!(policy.identity().sha256, sha256(VALID.as_bytes()));
        assert_eq!(
            policy.classify("you overwrote the notes").unwrap().category,
            "clobber"
        );
    }

    #[test]
    fn capability_rejects_package_identity_unknown_fields_and_empty_matching_patterns() {
        for (label, source) in [
            ("name", format!("{VALID}\nname = \"wrong-owner\"\n")),
            ("version", format!("{VALID}\nversion = \"9.9.9\"\n")),
            (
                "unknown",
                format!("{VALID}\nimplementation = \"arbitrary\"\n"),
            ),
        ] {
            let error = format!(
                "{:#}",
                MessageClassificationPolicySpec::parse_toml(&source)
                    .expect_err("unknown field must fail")
            );
            assert!(error.contains(label), "{error}");
        }

        for pattern in ["", "a*", "(?:x)?", "^$"] {
            let source = format!(
                "schema_version = 1\nkind = \"message-classification\"\n\n\
                 [[categories]]\nname = \"unsafe\"\npatterns = [{pattern:?}]\n"
            );
            let error = MessageClassificationPolicySpec::parse_toml(&source)
                .unwrap()
                .compile(
                    "my-review".to_string(),
                    "1.0.0".to_string(),
                    CorrectionPolicySource::Embedded,
                    source.as_bytes(),
                )
                .expect_err("empty-matching patterns must fail")
                .to_string();
            assert!(error.contains("empty"), "{pattern:?}: {error}");
        }
    }

    #[test]
    fn capability_file_read_is_bounded_before_toml_or_regex_allocation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("capability.toml");
        std::fs::write(&path, vec![b'x'; MAX_LOADED_CAPABILITY_BYTES + 1]).unwrap();
        let error = load_and_compile(&path, "my-review".to_string(), "1.0.0".to_string())
            .expect_err("oversized document must fail")
            .to_string();
        assert!(
            error.contains("1 MiB")
                && error.contains("1048576 byte")
                && error.contains("this file contributes at least 1048577")
                && error.contains("reduce comments or rules"),
            "{error}"
        );
    }

    #[test]
    fn selected_capability_documents_share_one_aggregate_byte_budget() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.toml");
        let second = temp.path().join("second.toml");
        let padded = |padding: usize| format!("{VALID}\n# {}\n", "x".repeat(padding));
        std::fs::write(&first, padded(600 * 1024)).unwrap();
        std::fs::write(&second, padded(500 * 1024)).unwrap();

        let mut budget = CapabilityLoadBudget::new();
        load_and_compile_with_budget(
            &first,
            "first".to_string(),
            "1.0.0".to_string(),
            &mut budget,
        )
        .unwrap();
        let error = load_and_compile_with_budget(
            &second,
            "second".to_string(),
            "1.0.0".to_string(),
            &mut budget,
        )
        .expect_err("the second document exceeds the remaining aggregate budget")
        .to_string();
        assert!(
            error.contains("aggregate")
                && error.contains("second.toml")
                && error.contains("bytes were consumed by earlier selections")
                && error.contains("select fewer packages"),
            "{error}"
        );
    }
}
