//! Typed deterministic capability requests and reports.
//!
//! This is the single wire contract shared by Rust and the CLI, and later by the thin Python and
//! MCP adapters. Skill discovery, descriptor validation, and capability compilation remain in
//! their domain modules; this module only defines the closed request/output vocabulary.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::corrections::{CapabilityReceipt, MessageClassificationReport};
use crate::models::MessageFilters;
use crate::skill_catalog::{SkillName, SkillSelector};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillRunQuery {
    pub skill: SkillSelector,
    pub input: SkillCapabilityInput,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "capability",
    content = "arguments",
    rename_all = "kebab-case",
    deny_unknown_fields
)]
pub enum SkillCapabilityInput {
    MessageClassification(MessageClassificationQuery),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MessageClassificationQuery {
    pub filters: MessageFilters,
    pub additional_skills: Vec<SkillSelector>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillRunReport {
    pub requested_selector: SkillSelector,
    pub resolved_skill: ResolvedSkillReceipt,
    pub output: SkillCapabilityOutput,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "capability",
    content = "result",
    rename_all = "kebab-case",
    deny_unknown_fields
)]
pub enum SkillCapabilityOutput {
    MessageClassification(MessageClassificationResult),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageClassificationResult {
    /// Receipt for the primary selected skill's compiled classification policy.
    ///
    /// This is derived from, and byte-for-byte equal to, the first entry in
    /// `report.policies`. The explicit primary receipt keeps the generic capability envelope
    /// self-describing; `report.policies` additionally records every ordered `--skill` policy.
    pub receipt: CapabilityReceipt,
    pub report: MessageClassificationReport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedSkillReceipt {
    pub name: SkillName,
    pub package_version: Option<String>,
    pub selected_location: SelectedSkillLocation,
    pub execution_source: CapabilityExecutionSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum SelectedSkillLocation {
    Embedded,
    Path { canonical_skill_md: PathBuf },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum CapabilityExecutionSource {
    Embedded,
    Path { canonical_capability_toml: PathBuf },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corrections::CapabilityReceipt;
    use crate::models::{MessageClassificationMatch, Provider};
    use crate::skill_catalog::{SkillNameSelector, SkillPathSelector};

    #[test]
    fn request_wire_shape_is_tagged_and_rejects_ambiguous_or_duplicate_fields() {
        let query: SkillRunQuery = serde_json::from_value(serde_json::json!({
            "skill": {"name": "corrections"},
            "input": {
                "capability": "message-classification",
                "arguments": {
                    "filters": {},
                    "additional_skills": [
                        {"path": "./my-review"}
                    ]
                }
            }
        }))
        .unwrap();
        assert!(matches!(
            query.skill,
            SkillSelector::Name(SkillNameSelector { .. })
        ));
        let SkillCapabilityInput::MessageClassification(input) = query.input;
        assert!(matches!(
            input.additional_skills.as_slice(),
            [SkillSelector::Path(SkillPathSelector { .. })]
        ));

        for invalid in [
            serde_json::json!({
                "skill": {"name": "corrections", "path": "./corrections"},
                "input": {
                    "capability": "message-classification",
                    "arguments": {}
                }
            }),
            serde_json::json!({
                "skill": {"name": "corrections"},
                "additional_skills": [],
                "input": {
                    "capability": "message-classification",
                    "arguments": {}
                }
            }),
        ] {
            assert!(
                serde_json::from_value::<SkillRunQuery>(invalid).is_err(),
                "ambiguous selectors and root-level additional_skills must fail"
            );
        }
    }

    #[test]
    fn generalized_result_types_preserve_the_message_classification_wire_shape() {
        let result = MessageClassificationResult {
            receipt: CapabilityReceipt {
                name: "corrections".to_string(),
                version: "1.0.0".to_string(),
                sha256: "digest".to_string(),
            },
            report: MessageClassificationReport {
                policies: vec![CapabilityReceipt {
                    name: "corrections".to_string(),
                    version: "1.0.0".to_string(),
                    sha256: "digest".to_string(),
                }],
                matches: vec![MessageClassificationMatch {
                    session_id: "claude:session".to_string(),
                    provider: Provider::Claude,
                    ts: None,
                    policy_name: "corrections".to_string(),
                    category: "accuracy".to_string(),
                    matched_text: "actually".to_string(),
                    content: "Actually, use the other command.".to_string(),
                }],
            },
        };

        assert_eq!(
            serde_json::to_value(result).unwrap(),
            serde_json::json!({
                "receipt": {
                    "name": "corrections",
                    "version": "1.0.0",
                    "sha256": "digest"
                },
                "report": {
                    "policies": [{
                        "name": "corrections",
                        "version": "1.0.0",
                        "sha256": "digest"
                    }],
                    "matches": [{
                        "session_id": "claude:session",
                        "provider": "claude",
                        "ts": null,
                        "policy_name": "corrections",
                        "category": "accuracy",
                        "matched_text": "actually",
                        "content": "Actually, use the other command."
                    }]
                }
            })
        );
    }
}
