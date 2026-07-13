//! Pure, provider-neutral classification and relationship analysis.
//!
//! This module consumes [`AnalysisDocument`](crate::models::AnalysisDocument) values from the
//! indexed service boundary. It does not discover providers, read session files, query SQLite,
//! or publish artifacts. Keeping policy pure makes it reusable from Rust, Python, CLI, and MCP
//! adapters without duplicating scanning or lifecycle behavior.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::models::{AnalysisDocument, SessionRecord};

/// Document field inspected by one classification rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassificationTarget {
    Title,
    Summary,
    FirstUserText,
    UserText,
    /// Test metadata fields first, followed by bounded-page user text.
    Any,
}

/// Serializable input for one classification rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassificationRuleSpec {
    pub dimension: String,
    pub label: String,
    pub target: ClassificationTarget,
    pub pattern: String,
    pub weight: i64,
}

/// Semantic meaning of an explicit, name-derived relationship hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipKind {
    Branch,
    Copy,
    Version,
}

/// Serializable relationship rule. `pattern` must contain a named `parent` capture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationshipRuleSpec {
    pub id: String,
    pub kind: RelationshipKind,
    pub pattern: String,
}

#[derive(Debug, Clone)]
struct ClassificationRule {
    spec: ClassificationRuleSpec,
    regex: Regex,
}

#[derive(Debug, Clone)]
struct RelationshipRule {
    spec: RelationshipRuleSpec,
    regex: Regex,
}

/// Validated, compiled analysis policy.
///
/// Construction rejects empty identifiers, duplicate semantic keys, invalid or empty-matching
/// regexes, and relationship expressions without a named `parent` capture. Regex compilation is
/// therefore paid once rather than once per document.
#[derive(Debug, Clone)]
pub struct AnalysisPolicy {
    classifications: Vec<ClassificationRule>,
    relationships: Vec<RelationshipRule>,
}

impl AnalysisPolicy {
    pub fn compile(
        classification_specs: Vec<ClassificationRuleSpec>,
        relationship_specs: Vec<RelationshipRuleSpec>,
    ) -> Result<Self> {
        let mut classification_keys = BTreeSet::new();
        let mut classifications = Vec::with_capacity(classification_specs.len());
        for spec in classification_specs {
            require_name("classification dimension", &spec.dimension)?;
            require_name("classification label", &spec.label)?;
            if !classification_keys.insert((spec.dimension.clone(), spec.label.clone())) {
                bail!(
                    "duplicate classification rule for dimension '{}' and label '{}'",
                    spec.dimension,
                    spec.label
                );
            }
            let regex = compile_nonempty_regex("classification", &spec.label, &spec.pattern)?;
            classifications.push(ClassificationRule { spec, regex });
        }
        classifications.sort_by(|left, right| {
            left.spec
                .dimension
                .cmp(&right.spec.dimension)
                .then_with(|| left.spec.label.cmp(&right.spec.label))
        });

        let mut relationship_ids = BTreeSet::new();
        let mut relationships = Vec::with_capacity(relationship_specs.len());
        for spec in relationship_specs {
            require_name("relationship rule id", &spec.id)?;
            if !relationship_ids.insert(spec.id.clone()) {
                bail!("duplicate relationship rule id '{}'", spec.id);
            }
            let regex = compile_nonempty_regex("relationship", &spec.id, &spec.pattern)?;
            if !regex.capture_names().flatten().any(|name| name == "parent") {
                bail!(
                    "relationship rule '{}' must define a named 'parent' capture",
                    spec.id
                );
            }
            relationships.push(RelationshipRule { spec, regex });
        }
        relationships.sort_by(|left, right| left.spec.id.cmp(&right.spec.id));

        Ok(Self {
            classifications,
            relationships,
        })
    }

    pub fn classification_specs(&self) -> impl ExactSizeIterator<Item = &ClassificationRuleSpec> {
        self.classifications.iter().map(|rule| &rule.spec)
    }

    pub fn relationship_specs(&self) -> impl ExactSizeIterator<Item = &RelationshipRuleSpec> {
        self.relationships.iter().map(|rule| &rule.spec)
    }

    /// Analyze provider-normalized documents without retaining their aggregate user text.
    ///
    /// Results and candidates are sorted by canonical session ID. Runtime is linear in document
    /// text times the configured rule count; relationship resolution uses an indexed title map
    /// rather than an all-pairs comparison.
    pub fn analyze(
        &self,
        documents: impl IntoIterator<Item = AnalysisDocument>,
    ) -> Result<AnalysisResult> {
        let mut accumulator = self.accumulator();
        for document in documents {
            accumulator.push(document)?;
        }
        accumulator.finish()
    }

    /// Start a bounded-memory analysis that accepts documents one at a time.
    pub fn accumulator(&self) -> AnalysisAccumulator<'_> {
        AnalysisAccumulator {
            policy: self,
            sessions: BTreeMap::new(),
            titles: BTreeMap::new(),
        }
    }

    fn classify(&self, document: &AnalysisDocument) -> Vec<ClassificationMatch> {
        self.classifications
            .iter()
            .filter(|rule| classification_matches(rule, document))
            .map(|rule| ClassificationMatch {
                dimension: rule.spec.dimension.clone(),
                label: rule.spec.label.clone(),
                target: rule.spec.target,
                weight: rule.spec.weight,
            })
            .collect()
    }
}

/// Streaming pure-policy state. Dropping it discards partial output without side effects.
pub struct AnalysisAccumulator<'policy> {
    policy: &'policy AnalysisPolicy,
    sessions: BTreeMap<String, AnalyzedSession>,
    titles: BTreeMap<String, Vec<String>>,
}

impl AnalysisAccumulator<'_> {
    /// Consume one normalized document and immediately release its aggregate user text.
    pub fn push(&mut self, document: AnalysisDocument) -> Result<()> {
        let session_id = document.session.id.clone();
        require_name("canonical session id", &session_id)?;
        if self.sessions.contains_key(&session_id) {
            bail!("duplicate canonical session id '{session_id}' in analysis input");
        }
        let classifications = self.policy.classify(&document);
        let score = classifications.iter().try_fold(0_i64, |total, item| {
            total
                .checked_add(item.weight)
                .ok_or_else(|| anyhow!("classification score overflow for session '{session_id}'"))
        })?;
        if let Some(title) = document.session.title.as_deref() {
            self.titles
                .entry(title.to_owned())
                .or_default()
                .push(session_id.clone());
        }
        self.sessions.insert(
            session_id,
            AnalyzedSession {
                session: document.session,
                classifications,
                score,
                relationship_hints: Vec::new(),
            },
        );
        Ok(())
    }

    /// Resolve relationship hints after every canonical ID/title is known.
    pub fn finish(mut self) -> Result<AnalysisResult> {
        for candidates in self.titles.values_mut() {
            candidates.sort();
            candidates.dedup();
        }

        for (session_id, analyzed) in &mut self.sessions {
            let Some(title) = analyzed.session.title.as_deref() else {
                continue;
            };
            for rule in &self.policy.relationships {
                let Some(captures) = rule.regex.captures(title) else {
                    continue;
                };
                let parent_title = captures
                    .name("parent")
                    .map(|capture| capture.as_str().trim())
                    .filter(|value| !value.is_empty())
                    .with_context(|| {
                        format!(
                            "relationship rule '{}' captured an empty parent for session '{}'",
                            rule.spec.id, session_id
                        )
                    })?
                    .to_owned();
                let candidates = self
                    .titles
                    .get(&parent_title)
                    .into_iter()
                    .flatten()
                    .filter(|candidate| *candidate != session_id)
                    .cloned()
                    .collect::<Vec<_>>();
                let resolution = match candidates.as_slice() {
                    [] => RelationshipResolution::Unresolved,
                    [candidate] => RelationshipResolution::Resolved {
                        session_id: candidate.clone(),
                    },
                    _ => RelationshipResolution::Ambiguous {
                        session_ids: candidates,
                    },
                };
                analyzed.relationship_hints.push(RelationshipHint {
                    rule_id: rule.spec.id.clone(),
                    kind: rule.spec.kind,
                    parent_title,
                    resolution,
                });
            }
        }

        Ok(AnalysisResult {
            sessions: self.sessions,
        })
    }
}

fn require_name(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{label} must not be empty");
    }
    Ok(())
}

fn compile_nonempty_regex(kind: &str, id: &str, pattern: &str) -> Result<Regex> {
    require_name(&format!("{kind} rule pattern"), pattern)?;
    let regex =
        Regex::new(pattern).with_context(|| format!("invalid {kind} regex for rule '{id}'"))?;
    if regex.is_match("") {
        bail!("{kind} rule '{id}' must not match empty text");
    }
    Ok(regex)
}

fn classification_matches(rule: &ClassificationRule, document: &AnalysisDocument) -> bool {
    let matches = |value: Option<&str>| value.is_some_and(|value| rule.regex.is_match(value));
    match rule.spec.target {
        ClassificationTarget::Title => matches(document.session.title.as_deref()),
        ClassificationTarget::Summary => matches(document.session.summary.as_deref()),
        ClassificationTarget::FirstUserText => matches(document.first_user_text.as_deref()),
        ClassificationTarget::UserText => rule.regex.is_match(&document.user_text),
        ClassificationTarget::Any => {
            matches(document.session.title.as_deref())
                || matches(document.session.summary.as_deref())
                || matches(document.first_user_text.as_deref())
                || rule.regex.is_match(&document.user_text)
        }
    }
}

/// One matched codebook rule. No source text is retained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassificationMatch {
    pub dimension: String,
    pub label: String,
    pub target: ClassificationTarget,
    pub weight: i64,
}

/// Deterministic resolution of one explicit relationship hint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RelationshipResolution {
    Unresolved,
    Resolved { session_id: String },
    Ambiguous { session_ids: Vec<String> },
}

/// Name-derived evidence and its canonical-ID resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationshipHint {
    pub rule_id: String,
    pub kind: RelationshipKind,
    pub parent_title: String,
    pub resolution: RelationshipResolution,
}

/// Analysis metadata for one canonical session. Aggregate user text is intentionally absent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyzedSession {
    pub session: SessionRecord,
    pub classifications: Vec<ClassificationMatch>,
    pub score: i64,
    pub relationship_hints: Vec<RelationshipHint>,
}

/// Pure analysis result keyed by canonical session ID, never by mutable/display titles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisResult {
    pub sessions: BTreeMap<String, AnalyzedSession>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Provider;
    use crate::util::minimal_record;
    use std::path::Path;

    fn document(id: &str, provider: Provider, title: &str, user_text: &str) -> AnalysisDocument {
        let mut parsed =
            minimal_record(provider, Path::new("/fixture/session.jsonl"), "test".into());
        parsed.session.id = id.into();
        parsed.session.title = Some(title.into());
        parsed.session.preview_text.clear();
        AnalysisDocument {
            session: parsed.session,
            user_text: user_text.into(),
            first_user_text: (!user_text.is_empty()).then(|| user_text.into()),
            message_count: usize::from(!user_text.is_empty()) as i64,
            user_message_count: usize::from(!user_text.is_empty()) as i64,
        }
    }

    fn policy() -> AnalysisPolicy {
        AnalysisPolicy::compile(
            vec![
                ClassificationRuleSpec {
                    dimension: "technique".into(),
                    label: "tdd".into(),
                    target: ClassificationTarget::UserText,
                    pattern: "(?i)\\btdd\\b".into(),
                    weight: 7,
                },
                ClassificationRuleSpec {
                    dimension: "role".into(),
                    label: "maintainer".into(),
                    target: ClassificationTarget::Any,
                    pattern: "(?i)maintainer".into(),
                    weight: 11,
                },
            ],
            vec![RelationshipRuleSpec {
                id: "branch_of".into(),
                kind: RelationshipKind::Branch,
                pattern: "^Branch of (?P<parent>.+)$".into(),
            }],
        )
        .unwrap()
    }

    #[test]
    fn classifies_any_provider_without_retaining_user_text() {
        let result = policy()
            .analyze([
                document("claude:a", Provider::Claude, "Maintainer task", "Use TDD"),
                document("codex:b", Provider::Codex, "Other", "Use TDD"),
            ])
            .unwrap();

        assert_eq!(result.sessions["claude:a"].score, 18);
        assert_eq!(result.sessions["codex:b"].score, 7);
        assert_eq!(
            result
                .sessions
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["claude:a", "codex:b"]
        );
        let json = serde_json::to_string(&result).unwrap();
        assert!(!json.contains("Use TDD"));
    }

    #[test]
    fn duplicate_parent_titles_are_reported_as_ambiguous_canonical_ids() {
        let result = policy()
            .analyze([
                document("codex:z", Provider::Codex, "Root", ""),
                document("claude:a", Provider::Claude, "Root", ""),
                document("gemini:child", Provider::GeminiCli, "Branch of Root", ""),
            ])
            .unwrap();

        assert_eq!(
            result.sessions["gemini:child"].relationship_hints[0].resolution,
            RelationshipResolution::Ambiguous {
                session_ids: vec!["claude:a".into(), "codex:z".into()]
            }
        );
    }

    #[test]
    fn relationship_resolution_never_creates_self_loops() {
        let self_policy = AnalysisPolicy::compile(
            vec![],
            vec![RelationshipRuleSpec {
                id: "identity".into(),
                kind: RelationshipKind::Version,
                pattern: "^(?P<parent>.+)$".into(),
            }],
        )
        .unwrap();
        let result = self_policy
            .analyze([document("claude:self", Provider::Claude, "Same", "")])
            .unwrap();
        assert_eq!(
            result.sessions["claude:self"].relationship_hints[0].resolution,
            RelationshipResolution::Unresolved
        );
    }

    #[test]
    fn invalid_and_duplicate_rules_fail_before_analysis() {
        let duplicate = ClassificationRuleSpec {
            dimension: "role".into(),
            label: "maintainer".into(),
            target: ClassificationTarget::Title,
            pattern: "maintainer".into(),
            weight: 1,
        };
        let error = AnalysisPolicy::compile(vec![duplicate.clone(), duplicate], vec![])
            .unwrap_err()
            .to_string();
        assert!(error.contains("duplicate classification rule"));

        let error = AnalysisPolicy::compile(
            vec![],
            vec![RelationshipRuleSpec {
                id: "broken".into(),
                kind: RelationshipKind::Copy,
                pattern: "Copy of (.+)".into(),
            }],
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("named 'parent' capture"));
    }

    #[test]
    fn duplicate_session_ids_and_score_overflow_are_actionable_errors() {
        let duplicate = document("claude:same", Provider::Claude, "One", "");
        let error = policy()
            .analyze([duplicate.clone(), duplicate])
            .unwrap_err()
            .to_string();
        assert!(error.contains("duplicate canonical session id 'claude:same'"));

        let overflow_policy = AnalysisPolicy::compile(
            vec![
                ClassificationRuleSpec {
                    dimension: "a".into(),
                    label: "max".into(),
                    target: ClassificationTarget::Title,
                    pattern: "One".into(),
                    weight: i64::MAX,
                },
                ClassificationRuleSpec {
                    dimension: "b".into(),
                    label: "one".into(),
                    target: ClassificationTarget::Title,
                    pattern: "One".into(),
                    weight: 1,
                },
            ],
            vec![],
        )
        .unwrap();
        let error = overflow_policy
            .analyze([document("claude:overflow", Provider::Claude, "One", "")])
            .unwrap_err()
            .to_string();
        assert!(error.contains("classification score overflow"));
    }
}
