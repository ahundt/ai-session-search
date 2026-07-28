//! Pure, provider-neutral classification and relationship analysis.
//!
//! This module consumes [`AnalysisDocument`] values from the
//! indexed service boundary. It does not discover providers, read session files, query SQLite,
//! or publish artifacts. Keeping policy pure makes it reusable from Rust, Python, CLI, and MCP
//! adapters without duplicating scanning or lifecycle behavior.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;

use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::hashing::FramedSha256;
use crate::models::{AnalysisDocument, SessionRecord};

/// Document field inspected by one classification rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassificationTarget {
    Title,
    Summary,
    FirstUserText,
    UserText,
    /// Test metadata fields first, followed by the selected session's user text.
    Any,
}

/// Serializable input for one classification rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct RelationshipRuleSpec {
    pub id: String,
    pub kind: RelationshipKind,
    pub pattern: String,
}

/// Validated, serialized policy for optional recurring-phrase aggregation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PhraseVocabularySpec {
    widths: BTreeSet<NonZeroUsize>,
    max_unique_phrases: NonZeroUsize,
    min_document_tokens: usize,
    excluded_tokens: BTreeSet<String>,
    exclude_numeric_tokens: bool,
    text_mode: PhraseTextMode,
}

/// Selects which normalized user content contributes to recurring phrases.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhraseTextMode {
    /// Analyze all normalized user text, including code and configuration snippets.
    #[default]
    UserText,
    /// Ignore fenced, indented, and structurally code-like lines before tokenization.
    ProseOnly,
}

/// Serializable, uncompiled recurring-phrase policy.
///
/// This transport type keeps JSON/TOML, CLI, MCP, and language bindings independent from the
/// validated [`PhraseVocabularySpec`] representation. Call [`Self::compile`] once at an adapter
/// boundary, then reuse the compiled policy for every document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhraseVocabularyPolicySpec {
    pub widths: Vec<usize>,
    pub max_unique_phrases: usize,
    #[serde(default)]
    pub min_document_tokens: usize,
    #[serde(default)]
    pub excluded_tokens: Vec<String>,
    #[serde(default = "default_true")]
    pub exclude_numeric_tokens: bool,
    #[serde(default)]
    pub text_mode: PhraseTextMode,
}

impl PhraseVocabularyPolicySpec {
    /// Validate and compile this transport representation.
    pub fn compile(&self) -> Result<PhraseVocabularySpec> {
        let widths = self
            .widths
            .iter()
            .copied()
            .map(|width| {
                NonZeroUsize::new(width)
                    .ok_or_else(|| anyhow!("phrase widths must be greater than zero"))
            })
            .collect::<Result<Vec<_>>>()?;
        let max_unique_phrases = NonZeroUsize::new(self.max_unique_phrases)
            .ok_or_else(|| anyhow!("max_unique_phrases must be greater than zero"))?;
        PhraseVocabularySpec::new(
            widths,
            max_unique_phrases,
            self.min_document_tokens,
            self.excluded_tokens.clone(),
            self.exclude_numeric_tokens,
        )
        .map(|spec| spec.with_text_mode(self.text_mode))
    }
}

fn default_true() -> bool {
    true
}

impl PhraseVocabularySpec {
    pub fn new(
        widths: impl IntoIterator<Item = NonZeroUsize>,
        max_unique_phrases: NonZeroUsize,
        min_document_tokens: usize,
        excluded_tokens: impl IntoIterator<Item = String>,
        exclude_numeric_tokens: bool,
    ) -> Result<Self> {
        let widths = widths.into_iter().collect::<BTreeSet<_>>();
        if widths.is_empty() {
            bail!("phrase vocabulary must contain at least one n-gram width");
        }
        let mut normalized_exclusions = BTreeSet::new();
        for token in excluded_tokens {
            let normalized = crate::analytics::normalized_tokens(&token);
            if normalized.len() != 1 {
                bail!("excluded phrase token '{token}' must normalize to exactly one token");
            }
            let normalized = normalized
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("excluded phrase token '{token}' normalized to nothing"))?;
            normalized_exclusions.insert(normalized);
        }
        Ok(Self {
            widths,
            max_unique_phrases,
            min_document_tokens,
            excluded_tokens: normalized_exclusions,
            exclude_numeric_tokens,
            text_mode: PhraseTextMode::UserText,
        })
    }

    pub fn with_text_mode(mut self, text_mode: PhraseTextMode) -> Self {
        self.text_mode = text_mode;
        self
    }

    pub fn widths(&self) -> impl ExactSizeIterator<Item = NonZeroUsize> + '_ {
        self.widths.iter().copied()
    }

    pub const fn max_unique_phrases(&self) -> NonZeroUsize {
        self.max_unique_phrases
    }

    pub const fn min_document_tokens(&self) -> usize {
        self.min_document_tokens
    }

    pub fn excluded_tokens(&self) -> impl ExactSizeIterator<Item = &str> {
        self.excluded_tokens.iter().map(String::as_str)
    }

    pub const fn exclude_numeric_tokens(&self) -> bool {
        self.exclude_numeric_tokens
    }

    pub const fn text_mode(&self) -> PhraseTextMode {
        self.text_mode
    }
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
    phrase_vocabulary: Option<PhraseVocabularySpec>,
    max_classification_chars: Option<NonZeroUsize>,
}

/// Serializable, provider-neutral input for one compiled analysis run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AnalysisPolicySpec {
    pub classification_rules: Vec<ClassificationRuleSpec>,
    pub relationship_rules: Vec<RelationshipRuleSpec>,
    pub phrase_vocabulary: Option<PhraseVocabularyPolicySpec>,
    pub max_classification_chars: Option<usize>,
}

impl AnalysisPolicySpec {
    /// Validate regexes, semantic identities, phrase bounds, and optional text bounds once.
    pub fn compile(&self) -> Result<AnalysisPolicy> {
        let mut policy = AnalysisPolicy::compile(
            self.classification_rules.clone(),
            self.relationship_rules.clone(),
        )?;
        if let Some(phrase_vocabulary) = &self.phrase_vocabulary {
            policy = policy.with_phrase_vocabulary(phrase_vocabulary.compile()?);
        }
        if let Some(max_chars) = self.max_classification_chars {
            let max_chars = NonZeroUsize::new(max_chars)
                .ok_or_else(|| anyhow!("max_classification_chars must be greater than zero"))?;
            policy = policy.with_max_classification_chars(max_chars);
        }
        Ok(policy)
    }
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
            phrase_vocabulary: None,
            max_classification_chars: None,
        })
    }

    /// Enable recurring-phrase aggregation with explicit memory and token policy.
    pub fn with_phrase_vocabulary(mut self, spec: PhraseVocabularySpec) -> Self {
        self.phrase_vocabulary = Some(spec);
        self
    }

    pub const fn phrase_vocabulary(&self) -> Option<&PhraseVocabularySpec> {
        self.phrase_vocabulary.as_ref()
    }

    /// Bound user-text characters examined by each classification rule.
    ///
    /// Session metadata remains unbounded because it is already bounded at ingestion.
    pub fn with_max_classification_chars(mut self, max_chars: NonZeroUsize) -> Self {
        self.max_classification_chars = Some(max_chars);
        self
    }

    pub const fn max_classification_chars(&self) -> Option<NonZeroUsize> {
        self.max_classification_chars
    }

    pub fn classification_specs(&self) -> impl ExactSizeIterator<Item = &ClassificationRuleSpec> {
        self.classifications.iter().map(|rule| &rule.spec)
    }

    pub fn relationship_specs(&self) -> impl ExactSizeIterator<Item = &RelationshipRuleSpec> {
        self.relationships.iter().map(|rule| &rule.spec)
    }

    /// Digest normalized executable policy semantics independently of source encoding and corpus.
    ///
    /// Rules are already sorted during compilation, so semantically equivalent input orderings
    /// share one identity. Runtime is linear in normalized policy bytes with `O(1)` hash state.
    pub fn policy_digest(&self) -> String {
        let mut digest = FramedSha256::new(b"aise-analysis-policy-v1");
        digest.update_u64(self.classifications.len() as u64);
        for rule in &self.classifications {
            digest.update_bytes(rule.spec.dimension.as_bytes());
            digest.update_bytes(rule.spec.label.as_bytes());
            digest.update_u8(classification_target_tag(rule.spec.target));
            digest.update_bytes(rule.spec.pattern.as_bytes());
            digest.update_i64(rule.spec.weight);
        }
        digest.update_u64(self.relationships.len() as u64);
        for rule in &self.relationships {
            digest.update_bytes(rule.spec.id.as_bytes());
            digest.update_u8(relationship_kind_tag(rule.spec.kind));
            digest.update_bytes(rule.spec.pattern.as_bytes());
        }
        if let Some(spec) = &self.phrase_vocabulary {
            digest.update_u8(1);
            digest.update_u64(spec.widths.len() as u64);
            for width in &spec.widths {
                digest.update_u64(width.get() as u64);
            }
            digest.update_u64(spec.max_unique_phrases.get() as u64);
            digest.update_u64(spec.min_document_tokens as u64);
            digest.update_u64(spec.excluded_tokens.len() as u64);
            for token in &spec.excluded_tokens {
                digest.update_bytes(token.as_bytes());
            }
            digest.update_u8(u8::from(spec.exclude_numeric_tokens));
            digest.update_u8(match spec.text_mode {
                PhraseTextMode::UserText => 0,
                PhraseTextMode::ProseOnly => 1,
            });
        } else {
            digest.update_u8(0);
        }
        if let Some(max_chars) = self.max_classification_chars {
            digest.update_u8(1);
            digest.update_u64(max_chars.get() as u64);
        } else {
            digest.update_u8(0);
        }
        digest.finish()
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
            vocabulary: BTreeMap::new(),
            failed: false,
        }
    }

    fn classify(&self, document: &AnalysisDocument) -> Vec<ClassificationMatch> {
        self.classifications
            .iter()
            .filter(|rule| classification_matches(rule, document, self.max_classification_chars))
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
    vocabulary: BTreeMap<String, PhraseFrequency>,
    failed: bool,
}

impl AnalysisAccumulator<'_> {
    /// Consume one normalized document and immediately release its aggregate user text.
    pub fn push(&mut self, document: AnalysisDocument) -> Result<()> {
        if self.failed {
            bail!("analysis accumulator is unusable after a previous error");
        }
        if let Err(error) = self.try_push(document) {
            self.failed = true;
            return Err(error);
        }
        Ok(())
    }

    /// Consume one session's user text as an ordered stream of per-message chunks.
    ///
    /// This is exactly equivalent to pushing an [`AnalysisDocument`] whose `user_text` joins the
    /// chunks with single spaces and whose `first_user_text` is the first chunk, without
    /// materializing that joined text. The space joiner participates in classification character
    /// bounds and prose line reconstruction, and phrase windows span chunk boundaries through
    /// rolling token state, so results are byte-identical to the document path.
    ///
    /// Memory is bounded by the policy's explicit bounds (`max_unique_phrases`,
    /// `min_document_tokens` × widths, `max_classification_chars`) plus one chunk, except when a
    /// `user_text`/`any` classification rule runs without `max_classification_chars`: the joined
    /// text is then retained for that rule, matching the document path.
    pub fn push_session_text_stream(
        &mut self,
        session: SessionRecord,
        message_count: i64,
        user_message_count: i64,
        chunks: impl IntoIterator<Item = Result<String>>,
    ) -> Result<()> {
        if self.failed {
            bail!("analysis accumulator is unusable after a previous error");
        }
        if let Err(error) = self.try_push_stream(session, message_count, user_message_count, chunks)
        {
            self.failed = true;
            return Err(error);
        }
        Ok(())
    }

    fn try_push_stream(
        &mut self,
        session: SessionRecord,
        message_count: i64,
        user_message_count: i64,
        chunks: impl IntoIterator<Item = Result<String>>,
    ) -> Result<()> {
        let session_id = session.id.clone();
        require_name("canonical session ID", &session_id)?;
        if self.sessions.contains_key(&session_id) {
            bail!("duplicate canonical session ID '{session_id}' in analysis input");
        }
        let mut state = StreamingDocumentText::new(self.policy);
        for chunk in chunks {
            state.push_chunk(&chunk?).with_context(|| {
                format!("failed to aggregate phrases for session '{session_id}'")
            })?;
        }
        let (text, phrase_delta) = state
            .finish()
            .with_context(|| format!("failed to aggregate phrases for session '{session_id}'"))?;
        let document = AnalysisDocument {
            session,
            user_text: text.classification_text,
            first_user_text: text.first_user_text,
            message_count,
            user_message_count,
        };
        let classifications = self.policy.classify(&document);
        self.merge_session(
            document.session,
            classifications,
            phrase_delta,
            text.has_user_text,
            message_count,
            user_message_count,
        )
    }

    fn try_push(&mut self, document: AnalysisDocument) -> Result<()> {
        let session_id = document.session.id.clone();
        require_name("canonical session ID", &session_id)?;
        if self.sessions.contains_key(&session_id) {
            bail!("duplicate canonical session ID '{session_id}' in analysis input");
        }
        let classifications = self.policy.classify(&document);
        let phrase_delta = self
            .policy
            .phrase_vocabulary
            .as_ref()
            .map(|spec| {
                phrase_delta(&document.user_text, spec).with_context(|| {
                    format!("failed to aggregate phrases for session '{session_id}'")
                })
            })
            .transpose()?;
        let has_user_text = !document.user_text.trim().is_empty();
        self.merge_session(
            document.session,
            classifications,
            phrase_delta,
            has_user_text,
            document.message_count,
            document.user_message_count,
        )
    }

    fn merge_session(
        &mut self,
        session: SessionRecord,
        classifications: Vec<ClassificationMatch>,
        phrase_delta: Option<BTreeMap<String, u64>>,
        has_user_text: bool,
        message_count: i64,
        user_message_count: i64,
    ) -> Result<()> {
        let session_id = session.id.clone();
        let score = classifications.iter().try_fold(0_i64, |total, item| {
            total
                .checked_add(item.weight)
                .ok_or_else(|| anyhow!("classification score overflow for session '{session_id}'"))
        })?;
        if let (Some(spec), Some(delta)) = (&self.policy.phrase_vocabulary, &phrase_delta) {
            let new_phrases = delta
                .keys()
                .filter(|phrase| !self.vocabulary.contains_key(*phrase))
                .count();
            let combined = self
                .vocabulary
                .len()
                .checked_add(new_phrases)
                .ok_or_else(|| anyhow!("phrase vocabulary size overflow"))?;
            if combined > spec.max_unique_phrases.get() {
                bail!(
                    "phrase vocabulary exceeded max_unique_phrases={} while analyzing session '{}'; increase the explicit bound or narrow the session scope",
                    spec.max_unique_phrases,
                    session_id
                );
            }
            for (phrase, occurrences) in delta {
                if let Some(existing) = self.vocabulary.get(phrase) {
                    existing
                        .occurrences
                        .checked_add(*occurrences)
                        .ok_or_else(|| {
                            anyhow!("phrase occurrence count overflow for '{phrase}'")
                        })?;
                    existing
                        .documents
                        .checked_add(1)
                        .ok_or_else(|| anyhow!("phrase document count overflow for '{phrase}'"))?;
                }
            }
            for (phrase, occurrences) in delta {
                let words = phrase.split_whitespace().count();
                let entry = self
                    .vocabulary
                    .entry(phrase.clone())
                    .or_insert(PhraseFrequency {
                        phrase: phrase.clone(),
                        words,
                        documents: 0,
                        occurrences: 0,
                    });
                entry.documents += 1;
                entry.occurrences += *occurrences;
            }
        }
        if let Some(title) = session.title.as_deref() {
            self.titles
                .entry(title.to_owned())
                .or_default()
                .push(session_id.clone());
        }
        self.sessions.insert(
            session_id,
            AnalyzedSession {
                has_user_text,
                session,
                classifications,
                score,
                relationship_hints: Vec::new(),
                message_count,
                user_message_count,
            },
        );
        Ok(())
    }

    /// Resolve relationship hints after every canonical ID/title is known.
    pub fn finish(mut self) -> Result<AnalysisResult> {
        if self.failed {
            bail!("analysis accumulator is unusable after a previous error");
        }
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

        let mut vocabulary = self.vocabulary.into_values().collect::<Vec<_>>();
        vocabulary.sort_by(|left, right| {
            right
                .occurrences
                .cmp(&left.occurrences)
                .then_with(|| right.documents.cmp(&left.documents))
                .then_with(|| right.words.cmp(&left.words))
                .then_with(|| left.phrase.cmp(&right.phrase))
        });
        Ok(AnalysisResult {
            sessions: self.sessions,
            vocabulary,
        })
    }
}

/// Text-derived facts produced by [`StreamingDocumentText::finish`].
struct StreamedDocumentFacts {
    /// The exact text classification rules must see: empty when no rule reads user text,
    /// a `max_classification_chars` prefix of the joined text when bounded, or the full
    /// joined text when a user-text rule runs unbounded.
    classification_text: String,
    first_user_text: Option<String>,
    has_user_text: bool,
}

/// How much of the joined user text classification rules require.
enum ClassificationTextState {
    /// No classification rule reads user text: retain nothing.
    Unused,
    /// Retain at most the first `remaining_chars` characters of the joined text.
    Bounded {
        text: String,
        remaining_chars: usize,
    },
    /// A user-text rule runs without `max_classification_chars`: retain the joined text.
    Full { text: String },
}

impl ClassificationTextState {
    fn push_chunk(&mut self, chunk: &str, joiner: bool) {
        match self {
            Self::Unused => {}
            Self::Full { text } => {
                if joiner {
                    text.push(' ');
                }
                text.push_str(chunk);
            }
            Self::Bounded {
                text,
                remaining_chars,
            } => {
                if joiner && *remaining_chars > 0 {
                    text.push(' ');
                    *remaining_chars -= 1;
                }
                for character in chunk.chars() {
                    if *remaining_chars == 0 {
                        break;
                    }
                    text.push(character);
                    *remaining_chars -= 1;
                }
            }
        }
    }

    fn into_text(self) -> String {
        match self {
            Self::Unused => String::new(),
            Self::Full { text } | Self::Bounded { text, .. } => text,
        }
    }
}

/// Streaming replica of the joined-`user_text` document semantics.
///
/// Feeding chunks `c1..cn` is exactly equivalent to evaluating the policy against
/// `c1 + " " + c2 + ... + " " + cn` without materializing that string.
struct StreamingDocumentText<'policy> {
    saw_first_chunk: bool,
    /// Whether the joined text is non-empty so far. The batch path only inserts the space
    /// joiner when its accumulated text is non-empty, so leading empty chunks add nothing.
    joined_nonempty: bool,
    has_user_text: bool,
    first_user_text: Option<String>,
    classification: ClassificationTextState,
    phrases: Option<StreamingPhraseAggregator<'policy>>,
}

impl<'policy> StreamingDocumentText<'policy> {
    fn new(policy: &'policy AnalysisPolicy) -> Self {
        let needs_user_text = policy.classifications.iter().any(|rule| {
            matches!(
                rule.spec.target,
                ClassificationTarget::UserText | ClassificationTarget::Any
            )
        });
        let classification = if !needs_user_text {
            ClassificationTextState::Unused
        } else if let Some(max_chars) = policy.max_classification_chars {
            ClassificationTextState::Bounded {
                text: String::new(),
                remaining_chars: max_chars.get(),
            }
        } else {
            ClassificationTextState::Full {
                text: String::new(),
            }
        };
        Self {
            saw_first_chunk: false,
            joined_nonempty: false,
            has_user_text: false,
            first_user_text: None,
            classification,
            phrases: policy
                .phrase_vocabulary
                .as_ref()
                .map(StreamingPhraseAggregator::new),
        }
    }

    fn push_chunk(&mut self, chunk: &str) -> Result<()> {
        let joiner = self.joined_nonempty;
        if !self.saw_first_chunk {
            self.saw_first_chunk = true;
            self.first_user_text = Some(chunk.to_owned());
        }
        self.joined_nonempty = self.joined_nonempty || !chunk.is_empty();
        if !self.has_user_text {
            self.has_user_text = chunk.chars().any(|character| !character.is_whitespace());
        }
        self.classification.push_chunk(chunk, joiner);
        if let Some(phrases) = &mut self.phrases {
            phrases.push_chunk(chunk, joiner)?;
        }
        Ok(())
    }

    fn finish(self) -> Result<(StreamedDocumentFacts, Option<BTreeMap<String, u64>>)> {
        let phrase_delta = self
            .phrases
            .map(StreamingPhraseAggregator::finish)
            .transpose()?;
        Ok((
            StreamedDocumentFacts {
                classification_text: self.classification.into_text(),
                first_user_text: self.first_user_text,
                has_user_text: self.has_user_text,
            },
            phrase_delta,
        ))
    }
}

/// Streaming n-gram aggregation equivalent to [`phrase_delta`] over the joined text.
///
/// Windows span chunk (and, in prose mode, kept-line) boundaries through `recent_tokens`.
/// The `max_unique_phrases` error is deferred until the `min_document_tokens` gate is
/// known to pass, exactly matching the batch path's "short documents return an empty
/// delta without error" behavior; once the unique bound is exceeded the counts map stops
/// growing, so memory stays bounded by the policy's explicit bounds.
struct StreamingPhraseAggregator<'policy> {
    spec: &'policy PhraseVocabularySpec,
    /// Prose mode only: incomplete final line of the text seen so far, plus fence state.
    prose: Option<(String, ProseLineFilter)>,
    recent_tokens: std::collections::VecDeque<String>,
    max_width: usize,
    total_tokens: usize,
    counts: BTreeMap<String, u64>,
    exceeded_unique_bound: bool,
}

impl<'policy> StreamingPhraseAggregator<'policy> {
    fn new(spec: &'policy PhraseVocabularySpec) -> Self {
        let max_width = spec
            .widths()
            .map(NonZeroUsize::get)
            .max()
            .expect("compiled phrase vocabulary always has at least one width");
        Self {
            spec,
            prose: matches!(spec.text_mode, PhraseTextMode::ProseOnly)
                .then(|| (String::new(), ProseLineFilter::default())),
            recent_tokens: std::collections::VecDeque::with_capacity(max_width),
            max_width,
            total_tokens: 0,
            counts: BTreeMap::new(),
            exceeded_unique_bound: false,
        }
    }

    fn push_chunk(&mut self, chunk: &str, joiner: bool) -> Result<()> {
        if self.prose.is_some() {
            // Rebuild the joined text's lines exactly: the space joiner extends the pending
            // line, and `str::lines` semantics (split at '\n', strip one '\r' before it,
            // optional final line) decide what the fence filter sees.
            if joiner {
                self.prose_pending().push(' ');
            }
            let mut parts = chunk.split('\n');
            if let Some(first) = parts.next() {
                self.prose_pending().push_str(first);
            }
            for part in parts {
                let mut line = std::mem::take(self.prose_pending());
                if line.ends_with('\r') {
                    line.pop();
                }
                self.process_prose_line(&line)?;
                self.prose_pending().push_str(part);
            }
        } else {
            // Tokens are maximal alphanumeric runs, so the space joiner can never extend a
            // token across chunks: per-chunk tokenization equals joined-text tokenization.
            for token in crate::analytics::normalized_tokens(chunk) {
                self.push_token(token)?;
            }
        }
        Ok(())
    }

    fn prose_pending(&mut self) -> &mut String {
        &mut self
            .prose
            .as_mut()
            .expect("prose state is present in prose mode")
            .0
    }

    fn process_prose_line(&mut self, line: &str) -> Result<()> {
        let keep = self
            .prose
            .as_mut()
            .expect("prose state is present in prose mode")
            .1
            .keep(line);
        if keep {
            // The batch path joins kept lines with '\n' before tokenizing; newlines never
            // extend a token, so feeding per-line tokens into the shared rolling window is
            // identical (windows still span kept lines through `recent_tokens`).
            for token in crate::analytics::normalized_tokens(line) {
                self.push_token(token)?;
            }
        }
        Ok(())
    }

    fn push_token(&mut self, token: String) -> Result<()> {
        self.total_tokens = self
            .total_tokens
            .checked_add(1)
            .ok_or_else(|| anyhow!("phrase token count overflow"))?;
        self.recent_tokens.push_back(token);
        if self.recent_tokens.len() > self.max_width {
            self.recent_tokens.pop_front();
        }
        for width in self.spec.widths() {
            let width = width.get();
            if self.recent_tokens.len() < width {
                continue;
            }
            let window_start = self.recent_tokens.len() - width;
            let window: Vec<&str> = self
                .recent_tokens
                .range(window_start..)
                .map(String::as_str)
                .collect();
            if self.spec.exclude_numeric_tokens
                && window
                    .iter()
                    .any(|token| token.chars().any(char::is_numeric))
            {
                continue;
            }
            if window
                .first()
                .is_some_and(|token| self.spec.excluded_tokens.contains(*token))
                || window
                    .iter()
                    .all(|token| self.spec.excluded_tokens.contains(*token))
            {
                continue;
            }
            if self.exceeded_unique_bound {
                continue;
            }
            let phrase = window.join(" ");
            if !self.counts.contains_key(&phrase)
                && self.counts.len() >= self.spec.max_unique_phrases.get()
            {
                // The batch path only reports this error for documents that pass the
                // min_document_tokens gate; defer until that is certain.
                self.exceeded_unique_bound = true;
                continue;
            }
            let count = self.counts.entry(phrase.clone()).or_default();
            *count = count
                .checked_add(1)
                .ok_or_else(|| anyhow!("phrase occurrence count overflow for '{phrase}'"))?;
        }
        if self.exceeded_unique_bound && self.total_tokens >= self.spec.min_document_tokens {
            bail!(
                "one document exceeded max_unique_phrases={}",
                self.spec.max_unique_phrases
            );
        }
        Ok(())
    }

    fn finish(mut self) -> Result<BTreeMap<String, u64>> {
        if self.prose.is_some() {
            // `str::lines` yields no final line for text ending in '\n' and does not strip a
            // '\r' that is not followed by '\n'; the pending buffer replicates both.
            let line = std::mem::take(self.prose_pending());
            if !line.is_empty() {
                self.process_prose_line(&line)?;
            }
        }
        if self.total_tokens < self.spec.min_document_tokens {
            return Ok(BTreeMap::new());
        }
        if self.exceeded_unique_bound {
            bail!(
                "one document exceeded max_unique_phrases={}",
                self.spec.max_unique_phrases
            );
        }
        Ok(self.counts)
    }
}

fn phrase_delta(content: &str, spec: &PhraseVocabularySpec) -> Result<BTreeMap<String, u64>> {
    let prose;
    let content = match spec.text_mode {
        PhraseTextMode::UserText => content,
        PhraseTextMode::ProseOnly => {
            prose = prose_only(content);
            &prose
        }
    };
    let tokens = crate::analytics::normalized_tokens(content);
    let mut counts = BTreeMap::<String, u64>::new();
    if tokens.len() < spec.min_document_tokens {
        return Ok(counts);
    }
    for width in spec.widths() {
        for window in tokens.windows(width.get()) {
            if spec.exclude_numeric_tokens
                && window
                    .iter()
                    .any(|token| token.chars().any(char::is_numeric))
            {
                continue;
            }
            if window
                .first()
                .is_some_and(|token| spec.excluded_tokens.contains(token))
                || window
                    .iter()
                    .all(|token| spec.excluded_tokens.contains(token))
            {
                continue;
            }
            let phrase = window.join(" ");
            if !counts.contains_key(&phrase) && counts.len() >= spec.max_unique_phrases.get() {
                bail!(
                    "one document exceeded max_unique_phrases={}",
                    spec.max_unique_phrases
                );
            }
            let count = counts.entry(phrase.clone()).or_default();
            *count = count
                .checked_add(1)
                .ok_or_else(|| anyhow!("phrase occurrence count overflow for '{phrase}'"))?;
        }
    }
    Ok(counts)
}

fn prose_only(content: &str) -> String {
    let mut prose = String::with_capacity(content.len());
    let mut filter = ProseLineFilter::default();
    for line in content.lines() {
        if filter.keep(line) {
            prose.push_str(line);
            prose.push('\n');
        }
    }
    prose
}

/// Stateful line classifier shared by the batch and streaming prose paths.
///
/// Both paths must agree on fence state, so the decision logic lives in exactly one place.
#[derive(Default)]
struct ProseLineFilter {
    in_fence: bool,
}

impl ProseLineFilter {
    fn keep(&mut self, line: &str) -> bool {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            self.in_fence = !self.in_fence;
            return false;
        }
        if self.in_fence || line.starts_with("    ") || line.starts_with('\t') {
            return false;
        }
        let first_token = trimmed
            .split(|character: char| !character.is_alphanumeric() && character != '_')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let keyword_code = matches!(
            first_token.as_str(),
            "import"
                | "from"
                | "def"
                | "class"
                | "return"
                | "if"
                | "elif"
                | "else"
                | "for"
                | "while"
                | "try"
                | "except"
                | "with"
                | "async"
                | "await"
        );
        let assignment = trimmed.split_once('=').is_some_and(|(left, _)| {
            let identifier = left.trim();
            !identifier.is_empty()
                && identifier
                    .chars()
                    .all(|character| character.is_alphanumeric() || character == '_')
                && identifier
                    .chars()
                    .next()
                    .is_some_and(|character| character.is_alphabetic() || character == '_')
        });
        let function_call = trimmed.split_once('(').is_some_and(|(left, _)| {
            let identifier = left.trim();
            !identifier.is_empty()
                && identifier
                    .chars()
                    .all(|character| character.is_alphanumeric() || character == '_')
        });
        let structural_code = trimmed.starts_with('{')
            || trimmed.starts_with('[')
            || trimmed.starts_with("</")
            || trimmed.starts_with('#')
            || assignment
            || function_call;
        !(keyword_code || structural_code)
    }
}

pub(crate) fn require_name(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{label} must not be empty");
    }
    Ok(())
}

pub(crate) fn compile_nonempty_regex(kind: &str, id: &str, pattern: &str) -> Result<Regex> {
    require_name(&format!("{kind} rule pattern"), pattern)?;
    let regex =
        Regex::new(pattern).with_context(|| format!("invalid {kind} regex for rule '{id}'"))?;
    if regex.is_match("") {
        // Name the PATTERN, not only the rule it belongs to. A rule with several patterns reports
        // the same message for each of them otherwise, and the caller has to bisect by hand to
        // find which one matches everything.
        bail!(
            "{kind} rule '{id}' has a pattern that matches empty text, so it would label every \
             message: {pattern}. Anchor it with \\b word boundaries or a literal prefix"
        );
    }
    Ok(regex)
}

fn classification_matches(
    rule: &ClassificationRule,
    document: &AnalysisDocument,
    max_user_chars: Option<NonZeroUsize>,
) -> bool {
    let matches = |value: Option<&str>| value.is_some_and(|value| rule.regex.is_match(value));
    let user_text = char_prefix(&document.user_text, max_user_chars);
    let first_user_text = document
        .first_user_text
        .as_deref()
        .map(|value| char_prefix(value, max_user_chars));
    match rule.spec.target {
        ClassificationTarget::Title => matches(document.session.title.as_deref()),
        ClassificationTarget::Summary => matches(document.session.summary.as_deref()),
        ClassificationTarget::FirstUserText => matches(first_user_text),
        ClassificationTarget::UserText => rule.regex.is_match(user_text),
        ClassificationTarget::Any => {
            matches(document.session.title.as_deref())
                || matches(document.session.summary.as_deref())
                || matches(first_user_text)
                || rule.regex.is_match(user_text)
        }
    }
}

fn char_prefix(value: &str, max_chars: Option<NonZeroUsize>) -> &str {
    max_chars.map_or(value, |limit| {
        value
            .char_indices()
            .nth(limit.get())
            .map_or(value, |(end, _)| &value[..end])
    })
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
    pub has_user_text: bool,
    pub classifications: Vec<ClassificationMatch>,
    pub score: i64,
    pub relationship_hints: Vec<RelationshipHint>,
    pub message_count: i64,
    pub user_message_count: i64,
}

/// One recurring normalized phrase aggregated without retaining source messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhraseFrequency {
    pub phrase: String,
    pub words: usize,
    pub documents: u64,
    pub occurrences: u64,
}

/// Pure analysis result keyed by canonical session ID, never by mutable/display titles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisResult {
    pub sessions: BTreeMap<String, AnalyzedSession>,
    pub vocabulary: Vec<PhraseFrequency>,
}

/// Canonical session metadata retained by the provider-neutral graph projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGraphNode {
    pub session_id: String,
    pub provider: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub repo_root: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub score: i64,
    pub classifications: Vec<ClassificationMatch>,
}

/// A resolved, directed provenance relationship between two canonical sessions.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SessionGraphEdge {
    pub source_session_id: String,
    pub target_session_id: String,
    pub kind: RelationshipKind,
    pub rule_id: String,
}

/// Set-valued membership that must not be interpreted as parent-child lineage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGraphGroup {
    pub dimension: String,
    pub key: String,
    pub session_ids: Vec<String>,
}

/// Deterministic graph projection with canonical identity and no inferred all-pairs edges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGraph {
    pub nodes: BTreeMap<String, SessionGraphNode>,
    pub edges: Vec<SessionGraphEdge>,
    pub groups: Vec<SessionGraphGroup>,
}

impl AnalysisResult {
    /// Digest canonical analysis semantics independently of corpus, policy source, and publication.
    ///
    /// `AnalysisResult` uses ordered maps and deterministically sorted vectors. Its compact Serde
    /// projection is therefore a stable versioned digest input. A counting pass frames the exact
    /// byte length, then a second pass hashes serializer chunks directly. Time is
    /// `O(result bytes)` with a constant factor of two and additional memory is `O(1)`;
    /// publication formats never enter it.
    pub fn result_digest(&self) -> String {
        let mut digest = FramedSha256::new(b"aise-analysis-result-v1");
        digest
            .update_json(self)
            .expect("AnalysisResult contains only JSON-serializable domain values");
        digest.finish()
    }

    /// Project analyzed sessions into provenance edges and project memberships.
    ///
    /// Only explicitly resolved relationship hints become edges. Ambiguous and unresolved hints
    /// remain available on [`AnalyzedSession`] as evidence and are never guessed into lineage.
    /// Working directories and repository roots are groups, not temporal relationships. Runtime
    /// is `O(N log N + E log E)` and does not compare every pair of sessions.
    pub fn session_graph(&self) -> SessionGraph {
        let nodes = self
            .sessions
            .iter()
            .map(|(session_id, analyzed)| {
                let session = &analyzed.session;
                (
                    session_id.clone(),
                    SessionGraphNode {
                        session_id: session_id.clone(),
                        provider: session.provider.as_str().to_owned(),
                        title: session.title.clone(),
                        cwd: session.cwd.clone(),
                        repo_root: session.repo_root.clone(),
                        created_at: session.created_at.map(|value| value.to_rfc3339()),
                        updated_at: session.updated_at.map(|value| value.to_rfc3339()),
                        score: analyzed.score,
                        classifications: analyzed.classifications.clone(),
                    },
                )
            })
            .collect();

        let mut edges = self
            .sessions
            .iter()
            .flat_map(|(target_session_id, analyzed)| {
                analyzed.relationship_hints.iter().filter_map(move |hint| {
                    let RelationshipResolution::Resolved { session_id } = &hint.resolution else {
                        return None;
                    };
                    Some(SessionGraphEdge {
                        source_session_id: session_id.clone(),
                        target_session_id: target_session_id.clone(),
                        kind: hint.kind,
                        rule_id: hint.rule_id.clone(),
                    })
                })
            })
            .collect::<Vec<_>>();
        edges.sort();
        edges.dedup();

        let mut memberships = BTreeMap::<(String, String), BTreeSet<String>>::new();
        for (session_id, analyzed) in &self.sessions {
            for (dimension, value) in [
                ("working_directory", analyzed.session.cwd.as_deref()),
                ("repository", analyzed.session.repo_root.as_deref()),
            ] {
                if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
                    memberships
                        .entry((dimension.to_owned(), value.to_owned()))
                        .or_default()
                        .insert(session_id.clone());
                }
            }
        }
        let groups = memberships
            .into_iter()
            .filter(|(_, session_ids)| session_ids.len() > 1)
            .map(|((dimension, key), session_ids)| SessionGraphGroup {
                dimension,
                key,
                session_ids: session_ids.into_iter().collect(),
            })
            .collect();

        SessionGraph {
            nodes,
            edges,
            groups,
        }
    }
}

const fn classification_target_tag(target: ClassificationTarget) -> u8 {
    match target {
        ClassificationTarget::Title => 0,
        ClassificationTarget::Summary => 1,
        ClassificationTarget::FirstUserText => 2,
        ClassificationTarget::UserText => 3,
        ClassificationTarget::Any => 4,
    }
}

const fn relationship_kind_tag(kind: RelationshipKind) -> u8 {
    match kind {
        RelationshipKind::Branch => 0,
        RelationshipKind::Copy => 1,
        RelationshipKind::Version => 2,
    }
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

    /// The batch join the streaming path must replicate: a space joiner only when the
    /// accumulated text is already non-empty (mirrors `analysis_document_page`).
    fn joined(chunks: &[&str]) -> String {
        let mut text = String::new();
        for chunk in chunks {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(chunk);
        }
        text
    }

    fn run_document_path(policy: &AnalysisPolicy, chunks: &[&str]) -> Result<AnalysisResult> {
        let mut reference = policy.accumulator();
        let mut doc = document(
            "claude:diff",
            Provider::Claude,
            "Differential",
            &joined(chunks),
        );
        doc.first_user_text = chunks.first().map(|chunk| (*chunk).to_string());
        doc.message_count = chunks.len() as i64;
        doc.user_message_count = chunks.len() as i64;
        reference.push(doc)?;
        reference.finish()
    }

    fn run_streaming_path(policy: &AnalysisPolicy, chunks: &[&str]) -> Result<AnalysisResult> {
        let mut streaming = policy.accumulator();
        let session = document("claude:diff", Provider::Claude, "Differential", "").session;
        streaming.push_session_text_stream(
            session,
            chunks.len() as i64,
            chunks.len() as i64,
            chunks.iter().map(|chunk| Ok((*chunk).to_string())),
        )?;
        streaming.finish()
    }

    #[test]
    fn analysis_policy_digest_uses_normalized_executable_semantics() {
        let left = AnalysisPolicySpec {
            classification_rules: vec![
                ClassificationRuleSpec {
                    dimension: "workflow".into(),
                    label: "tdd".into(),
                    target: ClassificationTarget::UserText,
                    pattern: "(?i)tdd".into(),
                    weight: 2,
                },
                ClassificationRuleSpec {
                    dimension: "architecture".into(),
                    label: "reuse".into(),
                    target: ClassificationTarget::Title,
                    pattern: "(?i)reuse".into(),
                    weight: 1,
                },
            ],
            relationship_rules: vec![],
            phrase_vocabulary: None,
            max_classification_chars: Some(200),
        }
        .compile()
        .unwrap();
        let reordered = AnalysisPolicySpec {
            classification_rules: vec![
                ClassificationRuleSpec {
                    dimension: "architecture".into(),
                    label: "reuse".into(),
                    target: ClassificationTarget::Title,
                    pattern: "(?i)reuse".into(),
                    weight: 1,
                },
                ClassificationRuleSpec {
                    dimension: "workflow".into(),
                    label: "tdd".into(),
                    target: ClassificationTarget::UserText,
                    pattern: "(?i)tdd".into(),
                    weight: 2,
                },
            ],
            relationship_rules: vec![],
            phrase_vocabulary: None,
            max_classification_chars: Some(200),
        }
        .compile()
        .unwrap();
        let changed = AnalysisPolicySpec {
            max_classification_chars: Some(201),
            ..AnalysisPolicySpec {
                classification_rules: vec![
                    ClassificationRuleSpec {
                        dimension: "architecture".into(),
                        label: "reuse".into(),
                        target: ClassificationTarget::Title,
                        pattern: "(?i)reuse".into(),
                        weight: 1,
                    },
                    ClassificationRuleSpec {
                        dimension: "workflow".into(),
                        label: "tdd".into(),
                        target: ClassificationTarget::UserText,
                        pattern: "(?i)tdd".into(),
                        weight: 2,
                    },
                ],
                relationship_rules: vec![],
                phrase_vocabulary: None,
                max_classification_chars: None,
            }
        }
        .compile()
        .unwrap();

        assert_eq!(left.policy_digest(), reordered.policy_digest());
        assert_ne!(left.policy_digest(), changed.policy_digest());
    }

    #[test]
    fn analysis_result_digest_is_deterministic_and_result_sensitive() {
        let result = policy()
            .analyze([document(
                "claude:digest",
                Provider::Claude,
                "Digest",
                "use TDD",
            )])
            .unwrap();
        let same = result.clone();
        let mut changed = result.clone();
        changed.sessions.get_mut("claude:digest").unwrap().score += 1;

        assert_eq!(result.result_digest(), same.result_digest());
        assert_ne!(result.result_digest(), changed.result_digest());
    }

    #[track_caller]
    fn assert_stream_matches_document(policy: &AnalysisPolicy, chunks: &[&str]) {
        let expected = run_document_path(policy, chunks).unwrap();
        let actual = run_streaming_path(policy, chunks).unwrap();
        assert_eq!(
            serde_json::to_value(&expected).unwrap(),
            serde_json::to_value(&actual).unwrap(),
            "streaming result diverged from joined-document result for chunks {chunks:?}"
        );
    }

    fn phrase_policy(
        min_document_tokens: usize,
        max_unique: usize,
        text_mode: PhraseTextMode,
    ) -> AnalysisPolicy {
        AnalysisPolicy::compile(vec![], vec![])
            .unwrap()
            .with_phrase_vocabulary(
                PhraseVocabularySpec::new(
                    [NonZeroUsize::new(2).unwrap(), NonZeroUsize::new(3).unwrap()],
                    NonZeroUsize::new(max_unique).unwrap(),
                    min_document_tokens,
                    ["the".to_string()],
                    true,
                )
                .unwrap()
                .with_text_mode(text_mode),
            )
    }

    #[test]
    fn streaming_chunks_match_joined_document_for_classification() {
        let unbounded = policy();
        let bounded = policy().with_max_classification_chars(NonZeroUsize::new(7).unwrap());
        for chunks in [
            &["use tdd across chunks", "tdd again maintainer"][..],
            &["", "", "leading empties then maintainer"],
            &["alpha beta", "", "gamma"],
            &["héllo wörld", "maintainer tdd"],
            &[""],
            &[],
        ] {
            assert_stream_matches_document(&unbounded, chunks);
            assert_stream_matches_document(&bounded, chunks);
        }
        // The 7-char prefix of "héllo maintainer" is "héllo m": the joiner and multibyte
        // characters count toward the bound on both paths, so neither matches "maintainer".
        let result = run_streaming_path(&bounded, &["héllo", "maintainer"]).unwrap();
        assert_eq!(result.sessions["claude:diff"].score, 0);
        let result = run_streaming_path(&unbounded, &["héllo", "maintainer"]).unwrap();
        assert_eq!(result.sessions["claude:diff"].score, 11);
    }

    #[test]
    fn streaming_chunks_match_joined_document_for_phrase_windows() {
        let user_text = phrase_policy(0, 1000, PhraseTextMode::UserText);
        let prose = phrase_policy(0, 1000, PhraseTextMode::ProseOnly);
        for chunks in [
            // n-gram spanning the chunk junction ("chunks tdd").
            &["use tdd across chunks", "tdd again across chunks"][..],
            // Fence opened in one chunk and closed in the next: the junction line
            // "``` code hidden" toggles the fence exactly as in the joined text.
            &["prose one\n```", "code hidden\n```\nprose two"],
            // Fence marker merged mid-line at the junction must NOT toggle the fence.
            &["alpha beta", "``` gamma\ndelta"],
            // CRLF line endings and a carriage return absorbed into a joined line.
            &["line one\r\nline two\r", "tail words here"],
            // Excluded token at window start, numeric exclusion, empty chunks.
            &["the quick brown", "fox 99 jumps", "", "over the lazy dog"],
            &["", "leading empty"],
            &[""],
        ] {
            assert_stream_matches_document(&user_text, chunks);
            assert_stream_matches_document(&prose, chunks);
        }
    }

    #[test]
    fn streaming_phrase_bounds_match_joined_document_errors_exactly() {
        let chunks = &["alpha beta gamma", "delta epsilon zeta"][..];

        // Enough unique windows to exceed max_unique_phrases=2 once the token gate passes.
        let overflowing = phrase_policy(0, 2, PhraseTextMode::UserText);
        let expected = run_document_path(&overflowing, chunks).unwrap_err();
        let actual = run_streaming_path(&overflowing, chunks).unwrap_err();
        assert_eq!(format!("{expected:#}"), format!("{actual:#}"));
        assert!(format!("{actual:#}").contains("one document exceeded max_unique_phrases=2"));

        // Below min_document_tokens both paths return an EMPTY delta without an error,
        // even though the unique bound was crossed along the way.
        let short_gate = phrase_policy(50, 2, PhraseTextMode::UserText);
        let expected = run_document_path(&short_gate, chunks).unwrap();
        let actual = run_streaming_path(&short_gate, chunks).unwrap();
        assert!(actual.vocabulary.is_empty());
        assert_eq!(
            serde_json::to_value(&expected).unwrap(),
            serde_json::to_value(&actual).unwrap()
        );

        // Gate crossed only after the unique bound was exceeded: both paths still error.
        let late_gate = phrase_policy(6, 2, PhraseTextMode::UserText);
        let expected = run_document_path(&late_gate, chunks).unwrap_err();
        let actual = run_streaming_path(&late_gate, chunks).unwrap_err();
        assert_eq!(format!("{expected:#}"), format!("{actual:#}"));
    }

    #[test]
    fn streaming_push_rejects_duplicates_and_poisons_like_document_push() {
        let policy = policy();
        let mut accumulator = policy.accumulator();
        let session = document("claude:dup", Provider::Claude, "Dup", "").session;
        accumulator
            .push_session_text_stream(session.clone(), 1, 1, [Ok("one".to_string())])
            .unwrap();
        let error = accumulator
            .push_session_text_stream(session.clone(), 1, 1, [Ok("two".to_string())])
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("duplicate canonical session ID 'claude:dup'"));
        let error = accumulator
            .push_session_text_stream(session, 1, 1, [Ok("three".to_string())])
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("analysis accumulator is unusable after a previous error"));
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
    fn graph_uses_canonical_ids_resolved_edges_and_set_valued_groups() {
        let mut root = document("claude:root", Provider::Claude, "Root", "");
        root.session.cwd = Some("/repo/worktree".into());
        root.session.repo_root = Some("/repo".into());
        let mut child = document("gemini:child", Provider::GeminiCli, "Branch of Root", "");
        child.session.cwd = Some("/repo/worktree".into());
        child.session.repo_root = Some("/repo".into());

        let result = policy().analyze([child, root]).unwrap();
        let graph = result.session_graph();

        assert_eq!(
            graph.nodes.keys().map(String::as_str).collect::<Vec<_>>(),
            ["claude:root", "gemini:child"]
        );
        assert_eq!(
            graph.edges,
            [SessionGraphEdge {
                source_session_id: "claude:root".into(),
                target_session_id: "gemini:child".into(),
                kind: RelationshipKind::Branch,
                rule_id: "branch_of".into(),
            }]
        );
        assert_eq!(graph.groups.len(), 2);
        assert!(graph
            .groups
            .iter()
            .all(|group| { group.session_ids == ["claude:root", "gemini:child"] }));
    }

    #[test]
    fn graph_never_promotes_ambiguous_relationship_evidence_to_an_edge() {
        let result = policy()
            .analyze([
                document("codex:z", Provider::Codex, "Root", ""),
                document("claude:a", Provider::Claude, "Root", ""),
                document("gemini:child", Provider::GeminiCli, "Branch of Root", ""),
            ])
            .unwrap();

        assert!(result.session_graph().edges.is_empty());
        assert!(matches!(
            result.sessions["gemini:child"].relationship_hints[0].resolution,
            RelationshipResolution::Ambiguous { .. }
        ));
    }

    #[test]
    fn serializable_policy_spec_compiles_once_and_rejects_invalid_bounds() {
        let spec: AnalysisPolicySpec = serde_json::from_value(serde_json::json!({
            "classification_rules": [{
                "dimension": "workflow",
                "label": "planning",
                "target": "user_text",
                "pattern": "(?i)plan",
                "weight": 3
            }],
            "relationship_rules": [],
            "phrase_vocabulary": {
                "widths": [3, 4],
                "max_unique_phrases": 100,
                "excluded_tokens": ["the"],
                "text_mode": "prose_only"
            },
            "max_classification_chars": 4096
        }))
        .unwrap();
        let policy = spec.compile().unwrap();
        assert_eq!(policy.classification_specs().len(), 1);
        assert_eq!(policy.max_classification_chars().unwrap().get(), 4096);
        let phrases = policy.phrase_vocabulary().unwrap();
        assert_eq!(
            phrases.widths().map(NonZeroUsize::get).collect::<Vec<_>>(),
            [3, 4]
        );
        assert_eq!(phrases.text_mode(), PhraseTextMode::ProseOnly);
        assert!(phrases.exclude_numeric_tokens());

        let zero_bound = AnalysisPolicySpec {
            max_classification_chars: Some(0),
            ..AnalysisPolicySpec::default()
        };
        assert!(zero_bound.compile().is_err());
        assert!(
            serde_json::from_value::<AnalysisPolicySpec>(serde_json::json!({
                "unknown": true
            }))
            .is_err()
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
    fn phrase_vocabulary_is_bounded_configurable_and_deterministic() {
        let spec = PhraseVocabularySpec::new(
            [NonZeroUsize::new(3).unwrap(), NonZeroUsize::new(4).unwrap()],
            NonZeroUsize::new(100).unwrap(),
            0,
            ["the".into()],
            true,
        )
        .unwrap();
        let result = policy()
            .with_phrase_vocabulary(spec)
            .analyze([
                document(
                    "claude:a",
                    Provider::Claude,
                    "One",
                    "the quick brown fox quick brown fox 2026",
                ),
                document("codex:b", Provider::Codex, "Two", "quick brown fox"),
            ])
            .unwrap();

        let repeated = result
            .vocabulary
            .iter()
            .find(|item| item.phrase == "quick brown fox")
            .unwrap();
        assert_eq!(repeated.words, 3);
        assert_eq!(repeated.documents, 2);
        assert_eq!(repeated.occurrences, 3);
        assert_eq!(result.sessions["claude:a"].message_count, 1);
        assert_eq!(result.sessions["claude:a"].user_message_count, 1);
        assert!(result.sessions["claude:a"].has_user_text);
        assert!(!result
            .vocabulary
            .iter()
            .any(|item| item.phrase.starts_with("the ") || item.phrase.contains("2026")));
    }

    #[test]
    fn phrase_limit_error_poisons_accumulator_without_partial_publication() {
        let spec = PhraseVocabularySpec::new(
            [NonZeroUsize::new(1).unwrap()],
            NonZeroUsize::new(1).unwrap(),
            0,
            Vec::new(),
            false,
        )
        .unwrap();
        let policy = policy().with_phrase_vocabulary(spec);
        let mut accumulator = policy.accumulator();
        accumulator
            .push(document("claude:a", Provider::Claude, "One", "alpha"))
            .unwrap();
        let error = accumulator
            .push(document("codex:b", Provider::Codex, "Two", "beta"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("max_unique_phrases=1"));
        assert!(accumulator
            .finish()
            .unwrap_err()
            .to_string()
            .contains("unusable after a previous error"));

        assert!(PhraseVocabularySpec::new(
            Vec::new(),
            NonZeroUsize::new(1).unwrap(),
            0,
            Vec::new(),
            false,
        )
        .unwrap_err()
        .to_string()
        .contains("at least one n-gram width"));
    }

    #[test]
    fn phrase_prose_mode_excludes_code_and_classification_window_is_bounded() {
        let spec = PhraseVocabularySpec::new(
            [NonZeroUsize::new(2).unwrap()],
            NonZeroUsize::new(100).unwrap(),
            0,
            Vec::new(),
            false,
        )
        .unwrap()
        .with_text_mode(PhraseTextMode::ProseOnly);
        let result = policy()
            .with_phrase_vocabulary(spec)
            .with_max_classification_chars(NonZeroUsize::new(8).unwrap())
            .analyze([document(
                "claude:a",
                Provider::Claude,
                "One",
                "plain prose\nelsewhere text\nhttps://example.test?a=b remains prose\n```rust\nlet secret_code = true;\n```\ntdd after window",
            )])
            .unwrap();

        assert!(result.sessions["claude:a"].classifications.is_empty());
        assert!(result
            .vocabulary
            .iter()
            .any(|item| item.phrase == "plain prose"));
        assert!(result
            .vocabulary
            .iter()
            .any(|item| item.phrase == "elsewhere text"));
        assert!(result
            .vocabulary
            .iter()
            .any(|item| item.phrase == "remains prose"));
        assert!(!result
            .vocabulary
            .iter()
            .any(|item| item.phrase.contains("secret") || item.phrase.contains("code")));
    }

    #[test]
    fn duplicate_session_ids_and_score_overflow_are_actionable_errors() {
        let duplicate = document("claude:same", Provider::Claude, "One", "");
        let error = policy()
            .analyze([duplicate.clone(), duplicate])
            .unwrap_err()
            .to_string();
        assert!(error.contains("duplicate canonical session ID 'claude:same'"));

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
