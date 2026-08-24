// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::num::NonZeroU64;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{SearchConfig, SearchScopeConfig, SearchScopeMode};
use crate::models::Provider;

pub const MAX_PERMISSION_PROFILES: usize = 64;
pub const MAX_PERMISSION_RULES_PER_PROFILE: usize = 256;
pub const MAX_PERMISSION_EXCEPTIONS_PER_RULE: usize = 64;
pub const MAX_PERMISSION_VALUES_PER_FIELD: usize = 256;
pub const MAX_PERMISSION_PREDICATES: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum RuleEffect {
    Allow,
    Block,
    HardBlock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ProfileDefault {
    Allow,
    Block,
    HardBlock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyResource {
    Session,
    Message,
    FileEdit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum SearchOperation {
    Read,
    Analyze,
    Export,
    Resume,
    RestoreFiles,
    Schema,
    AdminMetadata,
    IndexAdmin,
    PolicyAdmin,
}

impl SearchOperation {
    pub const ALL: [Self; 9] = [
        Self::Read,
        Self::Analyze,
        Self::Export,
        Self::Resume,
        Self::RestoreFiles,
        Self::Schema,
        Self::AdminMetadata,
        Self::IndexAdmin,
        Self::PolicyAdmin,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Analyze => "analyze",
            Self::Export => "export",
            Self::Resume => "resume",
            Self::RestoreFiles => "restore-files",
            Self::Schema => "schema",
            Self::AdminMetadata => "admin-metadata",
            Self::IndexAdmin => "index-admin",
            Self::PolicyAdmin => "policy-admin",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchOperationSpec {
    pub operation: SearchOperation,
    pub resource: Option<PolicyResource>,
    pub side_effect: bool,
    pub raw_sql: bool,
    pub metadata_exposure: bool,
}

pub const SEARCH_OPERATION_REGISTRY: [SearchOperationSpec; 9] = [
    SearchOperationSpec {
        operation: SearchOperation::Read,
        resource: Some(PolicyResource::Session),
        side_effect: false,
        raw_sql: false,
        metadata_exposure: false,
    },
    SearchOperationSpec {
        operation: SearchOperation::Analyze,
        resource: Some(PolicyResource::Session),
        side_effect: false,
        raw_sql: false,
        metadata_exposure: false,
    },
    SearchOperationSpec {
        operation: SearchOperation::Export,
        resource: Some(PolicyResource::Session),
        side_effect: true,
        raw_sql: false,
        metadata_exposure: false,
    },
    SearchOperationSpec {
        operation: SearchOperation::Resume,
        resource: Some(PolicyResource::Session),
        side_effect: true,
        raw_sql: false,
        metadata_exposure: true,
    },
    SearchOperationSpec {
        operation: SearchOperation::RestoreFiles,
        resource: Some(PolicyResource::FileEdit),
        side_effect: true,
        raw_sql: false,
        metadata_exposure: false,
    },
    SearchOperationSpec {
        operation: SearchOperation::Schema,
        resource: None,
        side_effect: false,
        raw_sql: true,
        metadata_exposure: true,
    },
    SearchOperationSpec {
        operation: SearchOperation::AdminMetadata,
        resource: None,
        side_effect: false,
        raw_sql: false,
        metadata_exposure: true,
    },
    SearchOperationSpec {
        operation: SearchOperation::IndexAdmin,
        resource: None,
        side_effect: true,
        raw_sql: false,
        metadata_exposure: true,
    },
    SearchOperationSpec {
        operation: SearchOperation::PolicyAdmin,
        resource: None,
        side_effect: true,
        raw_sql: false,
        metadata_exposure: true,
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum CallerHarness {
    Claude,
    Codex,
    Gemini,
    Antigravity,
    Pi,
    PrimeAgent,
    Cursor,
    Windsurf,
    Vscode,
    Zed,
    Opencode,
    Openclaw,
    Kilocode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionWorkspaceRelation {
    WithinCallerWorkingDirectory,
    WithinLiveWorkspaceRoots,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Deserialize,
    Serialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "kebab-case")]
pub enum GrantUntil {
    ConnectionClose,
    SessionEnd,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicySelectorConfig {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub operation: Vec<SearchOperation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub caller_harness: Vec<CallerHarness>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub caller_model_id: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub caller_working_directory_root: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub session_provider: Vec<Provider>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub session_workspace_root: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub session_transcript_root: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub session_workspace_relation: Vec<SessionWorkspaceRelation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub message_role: Vec<crate::models::Role>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub message_kind: Vec<crate::models::MessageKind>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub message_tool_name: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub file_path: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub file_tool: Vec<String>,
}

impl PolicySelectorConfig {
    fn is_empty(&self) -> bool {
        self.operation.is_empty()
            && self.caller_harness.is_empty()
            && self.caller_model_id.is_empty()
            && self.caller_working_directory_root.is_empty()
            && self.session_provider.is_empty()
            && self.session_workspace_root.is_empty()
            && self.session_transcript_root.is_empty()
            && self.session_workspace_relation.is_empty()
            && self.message_role.is_empty()
            && self.message_kind.is_empty()
            && self.message_tool_name.is_empty()
            && self.file_path.is_empty()
            && self.file_tool.is_empty()
    }

    fn predicate_count(&self) -> usize {
        self.operation.len()
            + self.caller_harness.len()
            + self.caller_model_id.len()
            + self.caller_working_directory_root.len()
            + self.session_provider.len()
            + self.session_workspace_root.len()
            + self.session_transcript_root.len()
            + self.session_workspace_relation.len()
            + self.message_role.len()
            + self.message_kind.len()
            + self.message_tool_name.len()
            + self.file_path.len()
            + self.file_tool.len()
    }

    fn validate(&self, context: &str) -> Result<()> {
        for (name, length) in [
            ("operation", self.operation.len()),
            ("caller_harness", self.caller_harness.len()),
            ("caller_model_id", self.caller_model_id.len()),
            (
                "caller_working_directory_root",
                self.caller_working_directory_root.len(),
            ),
            ("session_provider", self.session_provider.len()),
            ("session_workspace_root", self.session_workspace_root.len()),
            (
                "session_transcript_root",
                self.session_transcript_root.len(),
            ),
            (
                "session_workspace_relation",
                self.session_workspace_relation.len(),
            ),
            ("message_role", self.message_role.len()),
            ("message_kind", self.message_kind.len()),
            ("message_tool_name", self.message_tool_name.len()),
            ("file_path", self.file_path.len()),
            ("file_tool", self.file_tool.len()),
        ] {
            if length > MAX_PERMISSION_VALUES_PER_FIELD {
                bail!(
                    "{context}.{name} has {length} values; at most {MAX_PERMISSION_VALUES_PER_FIELD} are allowed"
                );
            }
        }
        for model in &self.caller_model_id {
            if model.is_empty()
                || model.len() > 256
                || model.chars().any(|character| character.is_control())
            {
                bail!(
                    "{context}.caller_model_id values must contain 1 through 256 UTF-8 bytes and no control characters"
                );
            }
        }
        for value in self.message_tool_name.iter().chain(self.file_tool.iter()) {
            if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
                bail!("{context} tool names must contain 1 through 256 UTF-8 bytes and no control characters");
            }
        }
        for value in &self.file_path {
            lexical_absolute(Path::new(value))
                .map_err(|error| anyhow!("{context}.file_path entry {value:?}: {error}"))?;
        }
        for (name, values) in [
            (
                "caller_working_directory_root",
                &self.caller_working_directory_root,
            ),
            ("session_workspace_root", &self.session_workspace_root),
            ("session_transcript_root", &self.session_transcript_root),
        ] {
            for value in values {
                validate_configured_root(Path::new(value))
                    .map_err(|error| anyhow!("{context}.{name} entry {value:?}: {error}"))?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyExceptionConfig {
    pub exception_id: String,
    #[serde(flatten)]
    pub selector: PolicySelectorConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyRuleConfig {
    pub rule_id: String,
    pub effect: RuleEffect,
    pub resource: PolicyResource,
    #[serde(default, rename = "exception", skip_serializing_if = "Vec::is_empty")]
    pub exceptions: Vec<PolicyExceptionConfig>,
    #[serde(flatten)]
    pub selector: PolicySelectorConfig,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct GrantEnvelopeConfig {
    #[serde(flatten)]
    pub selector: PolicySelectorConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_uses: Option<NonZeroU64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_expires_in: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allowed_until: Vec<GrantUntil>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyProfileConfig {
    pub default: ProfileDefault,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<PolicyRuleConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grant_envelope: Option<GrantEnvelopeConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchPermissionsConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_state_database: Option<String>,
    pub ceiling_profile: String,
    pub default_profile: String,
    pub profiles: BTreeMap<String, PolicyProfileConfig>,
}

impl Default for SearchPermissionsConfig {
    fn default() -> Self {
        Self {
            permission_state_database: None,
            ceiling_profile: "unrestricted".to_owned(),
            default_profile: "unrestricted".to_owned(),
            profiles: BTreeMap::new(),
        }
    }
}

impl SearchPermissionsConfig {
    pub fn validate(&self) -> Result<()> {
        if self.profiles.len() > MAX_PERMISSION_PROFILES {
            bail!(
                "search.permissions.profiles has {} profiles; at most {MAX_PERMISSION_PROFILES} are allowed",
                self.profiles.len()
            );
        }
        validate_policy_identifier("ceiling_profile", &self.ceiling_profile)?;
        validate_policy_identifier("default_profile", &self.default_profile)?;
        for selected in [&self.ceiling_profile, &self.default_profile] {
            if selected != "unrestricted" && !self.profiles.contains_key(selected) {
                bail!("search.permissions profile {selected:?} is selected but not defined");
            }
        }
        if let Some(path) = &self.permission_state_database {
            if !Path::new(path).is_absolute() {
                bail!("search.permissions.permission_state_database must be an absolute path");
            }
        }

        let mut predicates = 0usize;
        for (profile_name, profile) in &self.profiles {
            validate_policy_identifier("profile", profile_name)?;
            if profile.rules.len() > MAX_PERMISSION_RULES_PER_PROFILE {
                bail!(
                    "search.permissions.profiles.{profile_name}.rules has {} rules; at most {MAX_PERMISSION_RULES_PER_PROFILE} are allowed",
                    profile.rules.len()
                );
            }
            if profile_name == &self.ceiling_profile
                && (profile.default == ProfileDefault::Block
                    || profile
                        .rules
                        .iter()
                        .any(|rule| rule.effect == RuleEffect::Block))
            {
                bail!(
                    "ceiling profile {profile_name:?} cannot use overrideable block; use hard-block"
                );
            }
            let mut rule_ids = std::collections::BTreeSet::new();
            for rule in &profile.rules {
                validate_policy_identifier("rule_id", &rule.rule_id)?;
                if !rule_ids.insert(&rule.rule_id) {
                    bail!(
                        "profile {profile_name:?} repeats rule_id {:?}",
                        rule.rule_id
                    );
                }
                let has_message_fields = !rule.selector.message_role.is_empty()
                    || !rule.selector.message_kind.is_empty()
                    || !rule.selector.message_tool_name.is_empty();
                let has_file_fields =
                    !rule.selector.file_path.is_empty() || !rule.selector.file_tool.is_empty();
                match rule.resource {
                    PolicyResource::Session if has_message_fields || has_file_fields => bail!(
                        "profile {profile_name:?} session rule {:?} contains child-resource fields",
                        rule.rule_id
                    ),
                    PolicyResource::Message if has_file_fields => bail!(
                        "profile {profile_name:?} message rule {:?} contains file-edit fields",
                        rule.rule_id
                    ),
                    PolicyResource::FileEdit if has_message_fields => bail!(
                        "profile {profile_name:?} file-edit rule {:?} contains message fields",
                        rule.rule_id
                    ),
                    _ => {}
                }
                if rule.selector.is_empty() {
                    bail!("rule {:?} has no selector or operation", rule.rule_id);
                }
                rule.selector.validate(&format!(
                    "search.permissions.profiles.{profile_name}.rules.{}",
                    rule.rule_id
                ))?;
                predicates = predicates.saturating_add(rule.selector.predicate_count());
                if rule.exceptions.len() > MAX_PERMISSION_EXCEPTIONS_PER_RULE {
                    bail!(
                        "rule {:?} has {} exceptions; at most {MAX_PERMISSION_EXCEPTIONS_PER_RULE} are allowed",
                        rule.rule_id,
                        rule.exceptions.len()
                    );
                }
                let mut exception_ids = std::collections::BTreeSet::new();
                for exception in &rule.exceptions {
                    validate_policy_identifier("exception_id", &exception.exception_id)?;
                    if !exception_ids.insert(&exception.exception_id) {
                        bail!(
                            "rule {:?} repeats exception_id {:?}",
                            rule.rule_id,
                            exception.exception_id
                        );
                    }
                    if exception.selector.is_empty() {
                        bail!(
                            "rule {:?} exception {:?} has no selector",
                            rule.rule_id,
                            exception.exception_id
                        );
                    }
                    exception.selector.validate(&format!(
                        "search.permissions.profiles.{profile_name}.rules.{}.exception.{}",
                        rule.rule_id, exception.exception_id
                    ))?;
                    predicates = predicates.saturating_add(exception.selector.predicate_count());
                }
            }
            if let Some(envelope) = &profile.grant_envelope {
                envelope.selector.validate(&format!(
                    "search.permissions.profiles.{profile_name}.grant_envelope"
                ))?;
                predicates = predicates.saturating_add(envelope.selector.predicate_count());
                if let Some(duration) = envelope.max_expires_in.as_deref() {
                    parse_permission_duration_seconds(duration).with_context(|| {
                        format!(
                            "search.permissions.profiles.{profile_name}.grant_envelope.max_expires_in is invalid"
                        )
                    })?;
                }
                let unique_until = envelope
                    .allowed_until
                    .iter()
                    .copied()
                    .collect::<std::collections::BTreeSet<_>>();
                if unique_until.len() != envelope.allowed_until.len() {
                    bail!(
                        "search.permissions.profiles.{profile_name}.grant_envelope.allowed_until contains duplicates"
                    );
                }
            }
        }
        if predicates > MAX_PERMISSION_PREDICATES {
            bail!(
                "search.permissions compiles {predicates} predicate atoms; at most {MAX_PERMISSION_PREDICATES} are allowed"
            );
        }
        Ok(())
    }
}

pub fn parse_permission_duration_seconds(value: &str) -> Result<i64> {
    let (digits, multiplier) = match value.as_bytes().last().copied() {
        Some(b's') => (&value[..value.len() - 1], 1_i64),
        Some(b'm') => (&value[..value.len() - 1], 60_i64),
        Some(b'h') => (&value[..value.len() - 1], 60_i64 * 60),
        Some(b'd') => (&value[..value.len() - 1], 24_i64 * 60 * 60),
        _ => bail!("duration must end in s, m, h, or d, got {value:?}"),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("duration must contain an integer followed by s, m, h, or d, got {value:?}");
    }
    let amount = digits
        .parse::<i64>()
        .with_context(|| format!("duration integer is out of range: {value:?}"))?;
    let seconds = amount
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow!("duration overflows seconds: {value:?}"))?;
    if seconds == 0 || seconds > 30 * 24 * 60 * 60 {
        bail!("duration must be from 1s through 30d, got {value:?}");
    }
    Ok(seconds)
}

fn validate_policy_identifier(field: &str, value: &str) -> Result<()> {
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 64
        || !bytes[0].is_ascii_alphanumeric()
        || !bytes[bytes.len() - 1].is_ascii_alphanumeric()
        || bytes
            .iter()
            .any(|byte| !byte.is_ascii_lowercase() && !byte.is_ascii_digit() && *byte != b'-')
    {
        bail!(
            "{field} must contain 1 through 64 lowercase ASCII letters, digits, or hyphens and start/end alphanumeric, got {value:?}"
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccessRootOrigin {
    HarnessRoots,
    ExplicitConfig,
    InvocationDirectory,
}

impl AccessRootOrigin {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HarnessRoots => "harness-roots",
            Self::ExplicitConfig => "explicit-config",
            Self::InvocationDirectory => "invocation-directory",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct AccessRootSource {
    configured_path: PathBuf,
    canonicalized_at_startup: bool,
    origin: AccessRootOrigin,
}

impl AccessRootSource {
    pub fn configured_path(&self) -> &Path {
        &self.configured_path
    }

    pub const fn origin(&self) -> AccessRootOrigin {
        self.origin
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct AccessRoot {
    canonical_path: PathBuf,
    match_paths: Vec<PathBuf>,
    sources: Vec<AccessRootSource>,
}

impl AccessRoot {
    pub fn path(&self) -> &Path {
        &self.canonical_path
    }

    pub fn configured_path(&self) -> &Path {
        self.sources[0].configured_path()
    }

    pub fn origin(&self) -> AccessRootOrigin {
        self.sources[0].origin()
    }

    pub fn sources(&self) -> &[AccessRootSource] {
        &self.sources
    }

    fn database_prefixes(&self) -> impl Iterator<Item = &str> {
        self.match_paths
            .iter()
            .map(|path| path.to_str().expect("validated access roots are UTF-8"))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustedAccessInputs {
    pub harness_roots: Vec<PathBuf>,
    pub invocation_directory: Option<PathBuf>,
}

impl TrustedAccessInputs {
    /// Capture process-owned legacy scope inputs at an application boundary.
    ///
    /// Embedding hosts must pass only roots received from a protected adapter protocol; request
    /// arguments, transcript metadata, environment model labels, and model self-report are not
    /// trusted roots.
    pub fn capture(config: &SearchScopeConfig, harness_roots: Vec<PathBuf>) -> Result<Self> {
        let invocation_directory = if config.include_invocation_directory {
            Some(std::env::current_dir().map_err(|error| {
                anyhow!("cannot resolve invocation directory for allowed-roots scope: {error}")
            })?)
        } else {
            None
        };
        Ok(Self {
            harness_roots,
            invocation_directory,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "kebab-case")]
pub enum EffectiveAccessScope {
    All,
    AllowedRoots { roots: Vec<AccessRoot> },
}

impl EffectiveAccessScope {
    pub fn resolve(config: &SearchScopeConfig, inputs: TrustedAccessInputs) -> Result<Self> {
        if config.mode == SearchScopeMode::All {
            return Ok(Self::All);
        }

        let mut candidates = Vec::new();
        candidates.extend(
            inputs
                .harness_roots
                .into_iter()
                .map(|path| (path, AccessRootOrigin::HarnessRoots)),
        );
        candidates.extend(
            config
                .roots
                .iter()
                .map(|path| (PathBuf::from(path), AccessRootOrigin::ExplicitConfig)),
        );
        if config.include_invocation_directory {
            if let Some(path) = inputs.invocation_directory {
                candidates.push((path, AccessRootOrigin::InvocationDirectory));
            }
        }

        let mut roots = Vec::new();
        for (path, origin) in candidates {
            let candidate = normalize_authority_root(&path, origin).with_context(|| {
                format!("invalid {} access root {:?}", origin_name(origin), path)
            })?;
            if let Some(existing) = roots
                .iter_mut()
                .find(|root: &&mut AccessRoot| root.canonical_path == candidate.canonical_path)
            {
                for path in candidate.match_paths {
                    if !existing.match_paths.contains(&path) {
                        existing.match_paths.push(path);
                    }
                }
                existing.sources.extend(candidate.sources);
            } else {
                roots.push(candidate);
            }
        }

        if roots.is_empty() {
            bail!(
                "search scope mode allowed-roots resolved no authoritative roots; configure search.scope.roots, enable include_invocation_directory, or supply trusted harness roots"
            );
        }
        Ok(Self::AllowedRoots { roots })
    }

    pub const fn is_unrestricted(&self) -> bool {
        matches!(self, Self::All)
    }

    pub fn roots(&self) -> &[AccessRoot] {
        match self {
            Self::All => &[],
            Self::AllowedRoots { roots } => roots,
        }
    }

    pub fn workspace_prefixes(&self) -> impl Iterator<Item = &str> {
        self.roots().iter().flat_map(AccessRoot::database_prefixes)
    }

    pub fn allows_workspace_path(&self, path: &Path) -> bool {
        match self {
            Self::All => true,
            Self::AllowedRoots { roots } => lexical_absolute(path).is_ok_and(|path| {
                roots
                    .iter()
                    .flat_map(|root| &root.match_paths)
                    .any(|root| path.starts_with(root))
            }),
        }
    }

    pub fn validate_stable(&self) -> Result<()> {
        for root in self.roots() {
            for source in &root.sources {
                let current = match fs::canonicalize(&source.configured_path) {
                    Ok(current) => current,
                    Err(error)
                        if error.kind() == ErrorKind::NotFound
                            && !source.canonicalized_at_startup =>
                    {
                        continue;
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!(
                                "allowed root {:?} disappeared after scope resolution",
                                source.configured_path
                            )
                        });
                    }
                };
                if !current.is_dir() {
                    bail!(
                        "allowed root {:?} is no longer a directory; refusing to widen access",
                        source.configured_path
                    );
                }
                if current != root.canonical_path {
                    bail!(
                        "allowed root {:?} changed target after scope resolution; refusing to widen access",
                        source.configured_path
                    );
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyDecision {
    Allow,
    Block,
    HardBlock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemporaryGrantCoverage {
    None,
    Exact,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPolicySelector {
    pub operation: SearchOperation,
    pub workspace_root: PathBuf,
}

impl SessionPolicySelector {
    pub fn workspace_root(operation: SearchOperation, workspace_root: &Path) -> Self {
        Self {
            operation,
            workspace_root: workspace_root.to_owned(),
        }
    }

    pub fn digest(&self) -> String {
        let identity = format!(
            "{}\0{}",
            self.operation.as_str(),
            self.workspace_root.display()
        );
        crate::hashing::sha256(identity.as_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyPreflightDecision {
    Allow,
    Request {
        max_uses: u64,
        max_expires_in_seconds: Option<i64>,
        allowed_until: Vec<GrantUntil>,
        selector_digest: String,
    },
    HardBlock,
    FilterSilently,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CallerContextOrigin {
    LaunchBinding,
    NativeAdapter,
    EmbeddingHost,
    CliObserved,
    McpRoots,
    DeclaredUnverified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TrustedContextValue<T> {
    pub value: T,
    pub origin: CallerContextOrigin,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeAdapterAttestation {
    pub version: u32,
    pub generation: u64,
    pub harness: CallerHarness,
    pub model_id: String,
    pub working_directory: Option<PathBuf>,
    pub session_binding: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ResolvedCallerContext {
    pub harness: Option<TrustedContextValue<CallerHarness>>,
    pub model_id: Option<TrustedContextValue<String>>,
    pub working_directory: Option<TrustedContextValue<PathBuf>>,
    pub live_workspace_roots: Vec<PathBuf>,
    pub live_workspace_roots_origin: Option<CallerContextOrigin>,
    pub live_workspace_roots_generation: u64,
    pub session_binding: Option<TrustedContextValue<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TrustedPolicyInputs {
    pub caller_context: ResolvedCallerContext,
    pub policy_generation: Option<crate::permission_store::PolicyGeneration>,
    pub overlay_profiles: Vec<String>,
    pub admitted_grant_selectors: Vec<SessionPolicySelector>,
    /// Trusted application/service operation for this immutable policy snapshot.
    pub operation: Option<SearchOperation>,
}

impl TrustedPolicyInputs {
    pub fn with_launch_harness(mut self, harness: CallerHarness) -> Self {
        self.caller_context.harness = Some(TrustedContextValue {
            value: harness,
            origin: CallerContextOrigin::LaunchBinding,
            generation: 0,
        });
        self
    }

    pub fn caller_context_binding(&self) -> Result<String> {
        let encoded = serde_json::to_vec(&self.caller_context)
            .context("cannot serialize trusted caller context")?;
        Ok(crate::hashing::sha256(&encoded))
    }

    pub fn with_native_adapter_attestation(
        mut self,
        attestation: NativeAdapterAttestation,
    ) -> Result<Self> {
        if attestation.version != 1 {
            bail!(
                "unsupported native adapter attestation version {}; expected version 1",
                attestation.version
            );
        }
        if attestation.model_id.trim().is_empty() {
            bail!("native adapter model_id must not be empty");
        }
        if attestation
            .session_binding
            .as_ref()
            .is_some_and(|binding| binding.trim().is_empty())
        {
            bail!("native adapter session_binding must not be empty");
        }
        self.caller_context.harness = Some(TrustedContextValue {
            value: attestation.harness,
            origin: CallerContextOrigin::NativeAdapter,
            generation: attestation.generation,
        });
        self.caller_context.model_id = Some(TrustedContextValue {
            value: attestation.model_id,
            origin: CallerContextOrigin::NativeAdapter,
            generation: attestation.generation,
        });
        self.caller_context.working_directory = attestation
            .working_directory
            .map(|path| lexical_absolute(&path))
            .transpose()?
            .map(|value| TrustedContextValue {
                value,
                origin: CallerContextOrigin::NativeAdapter,
                generation: attestation.generation,
            });
        self.caller_context.session_binding =
            attestation
                .session_binding
                .map(|value| TrustedContextValue {
                    value,
                    origin: CallerContextOrigin::NativeAdapter,
                    generation: attestation.generation,
                });
        Ok(self)
    }

    pub fn with_mcp_live_roots(mut self, roots: Vec<PathBuf>, generation: u64) -> Result<Self> {
        if roots.is_empty() {
            bail!("MCP live workspace roots must not be empty");
        }
        self.caller_context.live_workspace_roots = roots
            .into_iter()
            .map(|root| lexical_absolute(&root))
            .collect::<Result<Vec<_>>>()?;
        self.caller_context.live_workspace_roots.sort();
        self.caller_context.live_workspace_roots.dedup();
        self.caller_context.live_workspace_roots_origin = Some(CallerContextOrigin::McpRoots);
        self.caller_context.live_workspace_roots_generation = generation;
        Ok(self)
    }

    pub fn with_declared_harness(mut self, harness: CallerHarness) -> Self {
        self.caller_context.harness = Some(TrustedContextValue {
            value: harness,
            origin: CallerContextOrigin::DeclaredUnverified,
            generation: 0,
        });
        self
    }

    pub fn with_declared_model_id(mut self, model_id: String) -> Self {
        self.caller_context.model_id = Some(TrustedContextValue {
            value: model_id,
            origin: CallerContextOrigin::DeclaredUnverified,
            generation: 0,
        });
        self
    }

    pub fn with_declared_working_directory(mut self, path: PathBuf) -> Result<Self> {
        self.caller_context.working_directory = Some(TrustedContextValue {
            value: lexical_absolute(&path)?,
            origin: CallerContextOrigin::DeclaredUnverified,
            generation: 0,
        });
        Ok(self)
    }

    pub fn with_observed_working_directory(mut self, path: PathBuf) -> Result<Self> {
        let path = lexical_absolute(&path)?;
        self.caller_context.working_directory = Some(TrustedContextValue {
            value: path,
            origin: CallerContextOrigin::CliObserved,
            generation: 0,
        });
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SessionPolicyTarget<'a> {
    pub operation: SearchOperation,
    pub provider: Provider,
    pub workspace: &'a Path,
    pub transcript: &'a Path,
}

impl<'a> SessionPolicyTarget<'a> {
    pub const fn new(
        operation: SearchOperation,
        provider: Provider,
        workspace: &'a Path,
        transcript: &'a Path,
    ) -> Self {
        Self {
            operation,
            provider,
            workspace,
            transcript,
        }
    }
}

#[derive(Debug, Clone)]
struct CompiledSelector {
    operation: Vec<SearchOperation>,
    caller_harness: Vec<CallerHarness>,
    caller_model_id: Vec<String>,
    caller_working_directory_root: Vec<PathBuf>,
    session_provider: Vec<Provider>,
    session_workspace_root: Vec<PathBuf>,
    session_transcript_root: Vec<PathBuf>,
    session_workspace_relation: Vec<SessionWorkspaceRelation>,
    message_role: Vec<crate::models::Role>,
    message_kind: Vec<crate::models::MessageKind>,
    message_tool_name: Vec<String>,
    file_path: Vec<PathBuf>,
    file_tool: Vec<String>,
}

impl CompiledSelector {
    fn compile(selector: &PolicySelectorConfig) -> Result<Self> {
        Ok(Self {
            operation: selector.operation.clone(),
            caller_harness: selector.caller_harness.clone(),
            caller_model_id: selector.caller_model_id.clone(),
            caller_working_directory_root: compile_roots(&selector.caller_working_directory_root)?,
            session_provider: selector.session_provider.clone(),
            session_workspace_root: compile_roots(&selector.session_workspace_root)?,
            session_transcript_root: compile_roots(&selector.session_transcript_root)?,
            session_workspace_relation: selector.session_workspace_relation.clone(),
            message_role: selector.message_role.clone(),
            message_kind: selector.message_kind.clone(),
            message_tool_name: selector.message_tool_name.clone(),
            file_path: selector
                .file_path
                .iter()
                .map(|path| lexical_absolute(Path::new(path)))
                .collect::<Result<_>>()?,
            file_tool: selector.file_tool.clone(),
        })
    }

    fn matches(&self, target: &SessionPolicyTarget<'_>, context: &ResolvedCallerContext) -> bool {
        if !self.operation.is_empty() && !self.operation.contains(&target.operation) {
            return false;
        }
        if !self.session_provider.is_empty() && !self.session_provider.contains(&target.provider) {
            return false;
        }
        if !matches_any_root(target.workspace, &self.session_workspace_root)
            || !matches_any_root(target.transcript, &self.session_transcript_root)
        {
            return false;
        }
        if !self.caller_harness.is_empty()
            && !context
                .harness
                .as_ref()
                .is_some_and(|value| self.caller_harness.contains(&value.value))
        {
            return false;
        }
        if !self.caller_model_id.is_empty()
            && !context
                .model_id
                .as_ref()
                .is_some_and(|value| self.caller_model_id.contains(&value.value))
        {
            return false;
        }
        if !self.caller_working_directory_root.is_empty()
            && !context.working_directory.as_ref().is_some_and(|value| {
                matches_any_root(&value.value, &self.caller_working_directory_root)
            })
        {
            return false;
        }
        for relation in &self.session_workspace_relation {
            let matches = match relation {
                SessionWorkspaceRelation::WithinCallerWorkingDirectory => context
                    .working_directory
                    .as_ref()
                    .is_some_and(|value| target.workspace.starts_with(&value.value)),
                SessionWorkspaceRelation::WithinLiveWorkspaceRoots => context
                    .live_workspace_roots
                    .iter()
                    .any(|root| target.workspace.starts_with(root)),
            };
            if !matches {
                return false;
            }
        }
        true
    }

    fn uses_declared_context(&self, context: &ResolvedCallerContext) -> bool {
        (!self.caller_harness.is_empty()
            && context
                .harness
                .as_ref()
                .is_some_and(|value| value.origin == CallerContextOrigin::DeclaredUnverified))
            || (!self.caller_model_id.is_empty()
                && context
                    .model_id
                    .as_ref()
                    .is_some_and(|value| value.origin == CallerContextOrigin::DeclaredUnverified))
            || (!self.caller_working_directory_root.is_empty()
                && context
                    .working_directory
                    .as_ref()
                    .is_some_and(|value| value.origin == CallerContextOrigin::DeclaredUnverified))
            || (self
                .session_workspace_relation
                .contains(&SessionWorkspaceRelation::WithinCallerWorkingDirectory)
                && context
                    .working_directory
                    .as_ref()
                    .is_some_and(|value| value.origin == CallerContextOrigin::DeclaredUnverified))
    }

    fn applies_to_operation(&self, operation: SearchOperation) -> bool {
        self.operation.is_empty() || self.operation.contains(&operation)
    }

    fn requires_harness(&self) -> bool {
        !self.caller_harness.is_empty()
    }

    fn requires_model(&self) -> bool {
        !self.caller_model_id.is_empty()
    }

    fn requires_working_directory(&self) -> bool {
        !self.caller_working_directory_root.is_empty()
            || self
                .session_workspace_relation
                .contains(&SessionWorkspaceRelation::WithinCallerWorkingDirectory)
    }

    fn requires_live_roots(&self) -> bool {
        self.session_workspace_relation
            .contains(&SessionWorkspaceRelation::WithinLiveWorkspaceRoots)
    }
}

fn compile_roots(values: &[String]) -> Result<Vec<PathBuf>> {
    values
        .iter()
        .map(|value| lexical_absolute(Path::new(value)))
        .collect()
}

fn matches_any_root(path: &Path, roots: &[PathBuf]) -> bool {
    roots.is_empty()
        || lexical_absolute(path).is_ok_and(|path| roots.iter().any(|root| path.starts_with(root)))
}

#[derive(Debug, Clone)]
struct CompiledRule {
    effect: RuleEffect,
    resource: PolicyResource,
    selector: CompiledSelector,
    exceptions: Vec<CompiledSelector>,
}

impl CompiledRule {
    fn matches(&self, target: &SessionPolicyTarget<'_>, context: &ResolvedCallerContext) -> bool {
        if self.effect == RuleEffect::Allow && self.selector.uses_declared_context(context) {
            return false;
        }
        self.selector.matches(target, context)
            && !self
                .exceptions
                .iter()
                .any(|exception| exception.matches(target, context))
    }
}

#[derive(Debug, Clone)]
struct CompiledProfile {
    default: ProfileDefault,
    rules: Vec<CompiledRule>,
    grant_envelope: Option<CompiledSelector>,
    grant_max_uses: u64,
    grant_max_expires_in_seconds: Option<i64>,
    grant_allowed_until: Vec<GrantUntil>,
}

impl CompiledProfile {
    fn unrestricted() -> Self {
        Self {
            default: ProfileDefault::Allow,
            rules: Vec::new(),
            grant_envelope: None,
            grant_max_uses: 1,
            grant_max_expires_in_seconds: None,
            grant_allowed_until: Vec::new(),
        }
    }

    fn is_unrestricted(&self) -> bool {
        self.default == ProfileDefault::Allow && self.rules.is_empty()
    }

    fn compile(profile: &PolicyProfileConfig) -> Result<Self> {
        Ok(Self {
            default: profile.default,
            rules: profile
                .rules
                .iter()
                .map(|rule| {
                    Ok(CompiledRule {
                        effect: rule.effect,
                        resource: rule.resource,
                        selector: CompiledSelector::compile(&rule.selector)?,
                        exceptions: rule
                            .exceptions
                            .iter()
                            .map(|exception| CompiledSelector::compile(&exception.selector))
                            .collect::<Result<_>>()?,
                    })
                })
                .collect::<Result<_>>()?,
            grant_envelope: profile
                .grant_envelope
                .as_ref()
                .map(|envelope| CompiledSelector::compile(&envelope.selector))
                .transpose()?,
            grant_max_uses: profile
                .grant_envelope
                .as_ref()
                .and_then(|envelope| envelope.max_uses)
                .map_or(1, NonZeroU64::get),
            grant_max_expires_in_seconds: profile
                .grant_envelope
                .as_ref()
                .and_then(|envelope| envelope.max_expires_in.as_deref())
                .map(parse_permission_duration_seconds)
                .transpose()?,
            grant_allowed_until: profile
                .grant_envelope
                .as_ref()
                .map(|envelope| envelope.allowed_until.clone())
                .unwrap_or_default(),
        })
    }

    fn authority_selectors(&self) -> impl Iterator<Item = &CompiledSelector> {
        self.rules
            .iter()
            .filter(|rule| rule.effect == RuleEffect::Allow)
            .map(|rule| &rule.selector)
            .chain(self.grant_envelope.iter())
    }

    fn evaluate(
        &self,
        target: &SessionPolicyTarget<'_>,
        context: &ResolvedCallerContext,
        grant: TemporaryGrantCoverage,
    ) -> PolicyDecision {
        let mut matching_allow = false;
        let mut matching_block = false;
        let mut matching_hard_block = false;
        for rule in &self.rules {
            if rule.resource != PolicyResource::Session || !rule.matches(target, context) {
                continue;
            }
            match rule.effect {
                RuleEffect::Allow => matching_allow = true,
                RuleEffect::Block => matching_block = true,
                RuleEffect::HardBlock => matching_hard_block = true,
            }
        }
        let default_hard_block = self.default == ProfileDefault::HardBlock && !matching_allow;
        if matching_hard_block || default_hard_block {
            return PolicyDecision::HardBlock;
        }
        let allow = self.default == ProfileDefault::Allow || matching_allow;
        let block = matching_block || (self.default == ProfileDefault::Block && !matching_allow);
        let grant_applies = grant == TemporaryGrantCoverage::Exact
            && self.grant_envelope.as_ref().is_some_and(|envelope| {
                !envelope.uses_declared_context(context) && envelope.matches(target, context)
            });
        if grant_applies || (allow && !block) {
            PolicyDecision::Allow
        } else {
            PolicyDecision::Block
        }
    }
}

#[derive(Debug, Clone)]
pub struct EffectiveSearchPolicy {
    ceiling_name: String,
    profile_name: String,
    ceiling: CompiledProfile,
    profile: CompiledProfile,
    overlays: Vec<(String, CompiledProfile)>,
    admitted_grant_selectors: Vec<SessionPolicySelector>,
    operation: Option<SearchOperation>,
    generation: Option<crate::permission_store::PolicyGeneration>,
    caller_context: ResolvedCallerContext,
}

pub(crate) struct PolicySqlPredicate {
    pub(crate) expression: String,
    pub(crate) parameters: Vec<String>,
}

fn trusted_authorization_value<T>(value: Option<&TrustedContextValue<T>>) -> bool {
    value.is_some_and(|value| value.origin != CallerContextOrigin::DeclaredUnverified)
}

fn caller_context_unavailable<T>(
    field: &str,
    value: Option<&TrustedContextValue<T>>,
) -> anyhow::Error {
    if value.is_some_and(|value| value.origin == CallerContextOrigin::DeclaredUnverified) {
        anyhow!(
            "caller-context-unavailable: {field} was declared by the caller; declared context can narrow results but does not authorize access"
        )
    } else {
        anyhow!(
            "caller-context-unavailable: {field} is required by the effective permission policy"
        )
    }
}

impl EffectiveSearchPolicy {
    pub(crate) fn unrestricted() -> Self {
        Self {
            ceiling_name: "unrestricted".to_owned(),
            profile_name: "unrestricted".to_owned(),
            ceiling: CompiledProfile::unrestricted(),
            profile: CompiledProfile::unrestricted(),
            overlays: Vec::new(),
            admitted_grant_selectors: Vec::new(),
            operation: None,
            generation: None,
            caller_context: ResolvedCallerContext::default(),
        }
    }

    pub fn resolve(config: &SearchConfig, inputs: TrustedPolicyInputs) -> Result<Self> {
        let Some(permissions) = &config.permissions else {
            if !inputs.overlay_profiles.is_empty() {
                bail!("restrictive overlays require a [search.permissions] panel");
            }
            let mut policy = Self::unrestricted();
            policy.generation = inputs.policy_generation;
            policy.operation = inputs.operation;
            policy.caller_context = inputs.caller_context;
            return Ok(policy);
        };
        permissions.validate()?;
        let compile_named = |name: &str| -> Result<CompiledProfile> {
            if name == "unrestricted" {
                Ok(CompiledProfile::unrestricted())
            } else {
                CompiledProfile::compile(
                    permissions
                        .profiles
                        .get(name)
                        .ok_or_else(|| anyhow!("permission profile {name:?} is not defined"))?,
                )
            }
        };
        let ceiling = compile_named(&permissions.ceiling_profile)?;
        let profile = compile_named(&permissions.default_profile)?;
        let overlays = inputs
            .overlay_profiles
            .iter()
            .map(|name| Ok((name.clone(), compile_named(name)?)))
            .collect::<Result<Vec<_>>>()?;
        let authority_selectors = ceiling
            .authority_selectors()
            .chain(profile.authority_selectors())
            .chain(
                overlays
                    .iter()
                    .flat_map(|(_, profile)| profile.authority_selectors()),
            );
        for selector in authority_selectors.filter(|selector| {
            inputs
                .operation
                .is_none_or(|operation| selector.applies_to_operation(operation))
        }) {
            if selector.requires_harness()
                && !trusted_authorization_value(inputs.caller_context.harness.as_ref())
            {
                bail!(caller_context_unavailable(
                    "caller_harness",
                    inputs.caller_context.harness.as_ref()
                ));
            }
            if selector.requires_model()
                && !trusted_authorization_value(inputs.caller_context.model_id.as_ref())
            {
                bail!(caller_context_unavailable(
                    "caller_model_id",
                    inputs.caller_context.model_id.as_ref()
                ));
            }
            if selector.requires_working_directory()
                && !trusted_authorization_value(inputs.caller_context.working_directory.as_ref())
            {
                bail!(caller_context_unavailable(
                    "caller_working_directory",
                    inputs.caller_context.working_directory.as_ref()
                ));
            }
            if selector.requires_live_roots()
                && inputs.caller_context.live_workspace_roots.is_empty()
            {
                bail!(
                    "caller-context-unavailable: live workspace roots are required by the effective permission policy"
                );
            }
        }
        Ok(Self {
            ceiling_name: permissions.ceiling_profile.clone(),
            profile_name: permissions.default_profile.clone(),
            ceiling,
            profile,
            overlays,
            admitted_grant_selectors: inputs.admitted_grant_selectors,
            operation: inputs.operation,
            generation: inputs.policy_generation,
            caller_context: inputs.caller_context,
        })
    }

    pub fn evaluate_session(
        &self,
        target: &SessionPolicyTarget<'_>,
        grant: TemporaryGrantCoverage,
    ) -> PolicyDecision {
        if self
            .ceiling
            .evaluate(target, &self.caller_context, TemporaryGrantCoverage::None)
            != PolicyDecision::Allow
        {
            return PolicyDecision::HardBlock;
        }
        let admitted = self.admitted_grant_selectors.iter().any(|selector| {
            selector.operation == target.operation
                && target.workspace.starts_with(&selector.workspace_root)
        });
        let grant = if grant == TemporaryGrantCoverage::Exact || admitted {
            TemporaryGrantCoverage::Exact
        } else {
            TemporaryGrantCoverage::None
        };
        let decision = self.profile.evaluate(target, &self.caller_context, grant);
        if decision != PolicyDecision::Allow {
            return decision;
        }
        for (_, overlay) in &self.overlays {
            let decision =
                overlay.evaluate(target, &self.caller_context, TemporaryGrantCoverage::None);
            if decision != PolicyDecision::Allow {
                return decision;
            }
        }
        PolicyDecision::Allow
    }

    pub fn preflight_session_selector(
        &self,
        selector: &SessionPolicySelector,
    ) -> PolicyPreflightDecision {
        let workspace = match lexical_absolute(&selector.workspace_root) {
            Ok(workspace) => workspace,
            Err(_) => return PolicyPreflightDecision::FilterSilently,
        };
        let profiles = std::iter::once(&self.ceiling)
            .chain(std::iter::once(&self.profile))
            .chain(self.overlays.iter().map(|(_, profile)| profile));
        if profiles.clone().any(|profile| {
            profile.rules.iter().any(|rule| {
                rule.resource == PolicyResource::Session
                    && (!rule.selector.session_provider.is_empty()
                        || !rule.selector.session_transcript_root.is_empty()
                        || !rule.exceptions.is_empty()
                        || rule.selector.session_workspace_root.iter().any(|root| {
                            root.starts_with(&workspace) && !workspace.starts_with(root)
                        }))
            })
        }) {
            return PolicyPreflightDecision::FilterSilently;
        }
        let transcript = workspace.join(".aise-policy-preflight.jsonl");
        let target = SessionPolicyTarget::new(
            selector.operation,
            Provider::Claude,
            &workspace,
            &transcript,
        );
        if self
            .ceiling
            .evaluate(&target, &self.caller_context, TemporaryGrantCoverage::None)
            != PolicyDecision::Allow
        {
            return PolicyPreflightDecision::HardBlock;
        }
        for (_, overlay) in &self.overlays {
            match overlay.evaluate(&target, &self.caller_context, TemporaryGrantCoverage::None) {
                PolicyDecision::Allow => {}
                PolicyDecision::HardBlock => return PolicyPreflightDecision::HardBlock,
                PolicyDecision::Block => return PolicyPreflightDecision::FilterSilently,
            }
        }
        match self
            .profile
            .evaluate(&target, &self.caller_context, TemporaryGrantCoverage::None)
        {
            PolicyDecision::Allow => PolicyPreflightDecision::Allow,
            PolicyDecision::HardBlock => PolicyPreflightDecision::HardBlock,
            PolicyDecision::Block => {
                let inside_envelope = self
                    .profile
                    .grant_envelope
                    .as_ref()
                    .is_some_and(|envelope| envelope.matches(&target, &self.caller_context));
                if !inside_envelope {
                    return PolicyPreflightDecision::FilterSilently;
                }
                PolicyPreflightDecision::Request {
                    max_uses: self.profile.grant_max_uses,
                    max_expires_in_seconds: self.profile.grant_max_expires_in_seconds,
                    allowed_until: self.profile.grant_allowed_until.clone(),
                    selector_digest: SessionPolicySelector::workspace_root(
                        selector.operation,
                        &workspace,
                    )
                    .digest(),
                }
            }
        }
    }

    pub(crate) fn session_sql_predicate(&self, operation: SearchOperation) -> PolicySqlPredicate {
        let operation = self.operation.unwrap_or(operation);
        if self.ceiling.is_unrestricted()
            && self.profile.is_unrestricted()
            && self.overlays.is_empty()
        {
            return PolicySqlPredicate {
                expression: "1".to_owned(),
                parameters: Vec::new(),
            };
        }
        let mut parameters = Vec::new();
        let ceiling = profile_sql_expression(
            &self.ceiling,
            operation,
            &self.caller_context,
            &mut parameters,
        );
        let profile = profile_sql_expression_with_grants(
            &self.profile,
            operation,
            &self.caller_context,
            &self.admitted_grant_selectors,
            &mut parameters,
        );
        let mut expressions = vec![ceiling, profile];
        expressions.extend(self.overlays.iter().map(|(_, overlay)| {
            profile_sql_expression(overlay, operation, &self.caller_context, &mut parameters)
        }));
        PolicySqlPredicate {
            expression: expressions
                .into_iter()
                .map(|expression| format!("({expression})"))
                .collect::<Vec<_>>()
                .join(" and "),
            parameters,
        }
    }

    pub(crate) fn message_sql_predicate(&self, operation: SearchOperation) -> PolicySqlPredicate {
        self.child_sql_predicate(PolicyResource::Message, operation)
    }

    pub(crate) fn file_edit_sql_predicate(&self, operation: SearchOperation) -> PolicySqlPredicate {
        self.child_sql_predicate(PolicyResource::FileEdit, operation)
    }

    fn child_sql_predicate(
        &self,
        resource: PolicyResource,
        operation: SearchOperation,
    ) -> PolicySqlPredicate {
        let operation = self.operation.unwrap_or(operation);
        let mut parameters = Vec::new();
        let ceiling = child_profile_sql_expression(
            &self.ceiling,
            resource,
            operation,
            &self.caller_context,
            &mut parameters,
        );
        let profile = child_profile_sql_expression_with_grants(
            &self.profile,
            resource,
            operation,
            &self.caller_context,
            &self.admitted_grant_selectors,
            &mut parameters,
        );
        let mut expressions = vec![ceiling, profile];
        expressions.extend(self.overlays.iter().map(|(_, overlay)| {
            child_profile_sql_expression(
                overlay,
                resource,
                operation,
                &self.caller_context,
                &mut parameters,
            )
        }));
        PolicySqlPredicate {
            expression: expressions
                .into_iter()
                .map(|expression| format!("({expression})"))
                .collect::<Vec<_>>()
                .join(" and "),
            parameters,
        }
    }

    pub fn is_restricted(&self) -> bool {
        !self.ceiling.is_unrestricted()
            || !self.profile.is_unrestricted()
            || !self.overlays.is_empty()
            || !self.admitted_grant_selectors.is_empty()
    }

    pub fn has_message_rules(&self) -> bool {
        self.ceiling
            .rules
            .iter()
            .chain(self.profile.rules.iter())
            .chain(
                self.overlays
                    .iter()
                    .flat_map(|(_, profile)| profile.rules.iter()),
            )
            .any(|rule| rule.resource == PolicyResource::Message)
    }

    pub fn has_file_edit_rules(&self) -> bool {
        self.ceiling
            .rules
            .iter()
            .chain(self.profile.rules.iter())
            .chain(
                self.overlays
                    .iter()
                    .flat_map(|(_, profile)| profile.rules.iter()),
            )
            .any(|rule| rule.resource == PolicyResource::FileEdit)
    }

    pub fn profile_name(&self) -> &str {
        &self.profile_name
    }

    pub fn ceiling_name(&self) -> &str {
        &self.ceiling_name
    }
}

fn child_profile_sql_expression_with_grants(
    profile: &CompiledProfile,
    resource: PolicyResource,
    operation: SearchOperation,
    context: &ResolvedCallerContext,
    grants: &[SessionPolicySelector],
    parameters: &mut Vec<String>,
) -> String {
    let allows = child_rule_group_sql(
        profile,
        resource,
        RuleEffect::Allow,
        operation,
        context,
        parameters,
    );
    let blocks = child_rule_group_sql(
        profile,
        resource,
        RuleEffect::Block,
        operation,
        context,
        parameters,
    );
    let hard_blocks = child_rule_group_sql(
        profile,
        resource,
        RuleEffect::HardBlock,
        operation,
        context,
        parameters,
    );
    let has_allowlist = profile
        .rules
        .iter()
        .any(|rule| rule.resource == resource && rule.effect == RuleEffect::Allow);
    let base_allow = if has_allowlist {
        allows
    } else {
        "1".to_owned()
    };
    let grant = child_grant_group_sql(resource, grants, operation, parameters);
    let granted_hard_blocks = child_rule_group_sql(
        profile,
        resource,
        RuleEffect::HardBlock,
        operation,
        context,
        parameters,
    );
    format!(
        "(({base_allow}) and not ({blocks}) and not ({hard_blocks})) or (({grant}) and not ({granted_hard_blocks}))"
    )
}

fn child_grant_group_sql(
    resource: PolicyResource,
    grants: &[SessionPolicySelector],
    operation: SearchOperation,
    parameters: &mut Vec<String>,
) -> String {
    let roots = grants
        .iter()
        .filter(|selector| selector.operation == operation)
        .map(|selector| selector.workspace_root.clone())
        .collect::<Vec<_>>();
    if roots.is_empty() {
        return "0".to_owned();
    }
    let session_id = match resource {
        PolicyResource::Message => "m.session_id",
        PolicyResource::FileEdit => "session_id",
        PolicyResource::Session => unreachable!("session grants use grant_group_sql"),
    };
    let session = path_group_sql(&roots, SessionPathColumns::Workspace, parameters);
    format!("{session_id} in (select s.id from sessions s where {session})")
}

fn child_profile_sql_expression(
    profile: &CompiledProfile,
    resource: PolicyResource,
    operation: SearchOperation,
    context: &ResolvedCallerContext,
    parameters: &mut Vec<String>,
) -> String {
    let allows = child_rule_group_sql(
        profile,
        resource,
        RuleEffect::Allow,
        operation,
        context,
        parameters,
    );
    let blocks = child_rule_group_sql(
        profile,
        resource,
        RuleEffect::Block,
        operation,
        context,
        parameters,
    );
    let hard_blocks = child_rule_group_sql(
        profile,
        resource,
        RuleEffect::HardBlock,
        operation,
        context,
        parameters,
    );
    let has_allowlist = profile
        .rules
        .iter()
        .any(|rule| rule.resource == resource && rule.effect == RuleEffect::Allow);
    let base_allow = if has_allowlist {
        allows
    } else {
        "1".to_owned()
    };
    format!("({base_allow}) and not ({blocks}) and not ({hard_blocks})")
}

fn child_rule_group_sql(
    profile: &CompiledProfile,
    resource: PolicyResource,
    effect: RuleEffect,
    operation: SearchOperation,
    context: &ResolvedCallerContext,
    parameters: &mut Vec<String>,
) -> String {
    let expressions = profile
        .rules
        .iter()
        .filter(|rule| rule.resource == resource && rule.effect == effect)
        .map(|rule| {
            if effect == RuleEffect::Allow && rule.selector.uses_declared_context(context) {
                return "0".to_owned();
            }
            let selector = child_selector_sql_expression(
                &rule.selector,
                resource,
                operation,
                context,
                parameters,
            );
            let exceptions = rule
                .exceptions
                .iter()
                .map(|exception| {
                    child_selector_sql_expression(
                        exception, resource, operation, context, parameters,
                    )
                })
                .collect::<Vec<_>>();
            if exceptions.is_empty() {
                selector
            } else {
                format!("({selector}) and not ({})", exceptions.join(" or "))
            }
        })
        .collect::<Vec<_>>();
    if expressions.is_empty() {
        "0".to_owned()
    } else {
        expressions
            .into_iter()
            .map(|expression| format!("({expression})"))
            .collect::<Vec<_>>()
            .join(" or ")
    }
}

fn child_selector_sql_expression(
    selector: &CompiledSelector,
    resource: PolicyResource,
    operation: SearchOperation,
    context: &ResolvedCallerContext,
    parameters: &mut Vec<String>,
) -> String {
    let session = selector_sql_expression(selector, operation, context, parameters);
    if session == "0" {
        return session;
    }
    let mut terms = Vec::new();
    if session != "1" {
        let session_id = match resource {
            PolicyResource::Message => "m.session_id",
            PolicyResource::FileEdit => "session_id",
            PolicyResource::Session => unreachable!("session selectors use profile_sql_expression"),
        };
        terms.push(format!(
            "{session_id} in (select s.id from sessions s where {session})"
        ));
    }
    match resource {
        PolicyResource::Message => {
            push_text_set_term(
                &mut terms,
                parameters,
                "m.role",
                selector.message_role.iter().map(|value| value.as_str()),
            );
            push_text_set_term(
                &mut terms,
                parameters,
                "m.kind",
                selector.message_kind.iter().map(|value| value.as_str()),
            );
            push_text_set_term(
                &mut terms,
                parameters,
                "m.tool_name",
                selector.message_tool_name.iter().map(String::as_str),
            );
        }
        PolicyResource::FileEdit => {
            push_text_set_term(
                &mut terms,
                parameters,
                "file_path",
                selector
                    .file_path
                    .iter()
                    .map(|value| value.to_str().expect("validated file paths are UTF-8")),
            );
            push_text_set_term(
                &mut terms,
                parameters,
                "tool",
                selector.file_tool.iter().map(String::as_str),
            );
        }
        PolicyResource::Session => unreachable!("session selectors use profile_sql_expression"),
    }
    if terms.is_empty() {
        "1".to_owned()
    } else {
        terms.join(" and ")
    }
}

fn push_text_set_term<'a>(
    terms: &mut Vec<String>,
    parameters: &mut Vec<String>,
    column: &str,
    values: impl Iterator<Item = &'a str>,
) {
    let values = values.collect::<Vec<_>>();
    if values.is_empty() {
        return;
    }
    terms.push(format!(
        "{column} in ({})",
        vec!["?"; values.len()].join(", ")
    ));
    parameters.extend(values.into_iter().map(str::to_owned));
}

fn profile_sql_expression_with_grants(
    profile: &CompiledProfile,
    operation: SearchOperation,
    context: &ResolvedCallerContext,
    grants: &[SessionPolicySelector],
    parameters: &mut Vec<String>,
) -> String {
    let allows = rule_group_sql(
        profile,
        PolicyResource::Session,
        RuleEffect::Allow,
        operation,
        context,
        parameters,
    );
    let blocks = rule_group_sql(
        profile,
        PolicyResource::Session,
        RuleEffect::Block,
        operation,
        context,
        parameters,
    );
    let hard_blocks = rule_group_sql(
        profile,
        PolicyResource::Session,
        RuleEffect::HardBlock,
        operation,
        context,
        parameters,
    );
    let base_allow = if profile.default == ProfileDefault::Allow {
        "1".to_owned()
    } else {
        allows
    };
    let grant = grant_group_sql(grants, operation, parameters);
    let granted_hard_blocks = rule_group_sql(
        profile,
        PolicyResource::Session,
        RuleEffect::HardBlock,
        operation,
        context,
        parameters,
    );
    format!(
        "(({base_allow}) and not ({blocks}) and not ({hard_blocks})) or (({grant}) and not ({granted_hard_blocks}))"
    )
}

fn grant_group_sql(
    grants: &[SessionPolicySelector],
    operation: SearchOperation,
    parameters: &mut Vec<String>,
) -> String {
    let roots = grants
        .iter()
        .filter(|selector| selector.operation == operation)
        .map(|selector| selector.workspace_root.clone())
        .collect::<Vec<_>>();
    if roots.is_empty() {
        "0".to_owned()
    } else {
        path_group_sql(&roots, SessionPathColumns::Workspace, parameters)
    }
}

fn profile_sql_expression(
    profile: &CompiledProfile,
    operation: SearchOperation,
    context: &ResolvedCallerContext,
    parameters: &mut Vec<String>,
) -> String {
    let allows = rule_group_sql(
        profile,
        PolicyResource::Session,
        RuleEffect::Allow,
        operation,
        context,
        parameters,
    );
    let blocks = rule_group_sql(
        profile,
        PolicyResource::Session,
        RuleEffect::Block,
        operation,
        context,
        parameters,
    );
    let hard_blocks = rule_group_sql(
        profile,
        PolicyResource::Session,
        RuleEffect::HardBlock,
        operation,
        context,
        parameters,
    );
    let base_allow = if profile.default == ProfileDefault::Allow {
        "1".to_owned()
    } else {
        allows
    };
    format!("({base_allow}) and not ({blocks}) and not ({hard_blocks})")
}

fn rule_group_sql(
    profile: &CompiledProfile,
    resource: PolicyResource,
    effect: RuleEffect,
    operation: SearchOperation,
    context: &ResolvedCallerContext,
    parameters: &mut Vec<String>,
) -> String {
    let expressions: Vec<String> = profile
        .rules
        .iter()
        .filter(|rule| rule.resource == resource && rule.effect == effect)
        .map(|rule| {
            if effect == RuleEffect::Allow && rule.selector.uses_declared_context(context) {
                return "0".to_owned();
            }
            let selector = selector_sql_expression(&rule.selector, operation, context, parameters);
            let exceptions: Vec<String> = rule
                .exceptions
                .iter()
                .map(|exception| selector_sql_expression(exception, operation, context, parameters))
                .collect();
            if exceptions.is_empty() {
                selector
            } else {
                format!("({selector}) and not ({})", exceptions.join(" or "))
            }
        })
        .collect();
    if expressions.is_empty() {
        "0".to_owned()
    } else {
        expressions
            .into_iter()
            .map(|expression| format!("({expression})"))
            .collect::<Vec<_>>()
            .join(" or ")
    }
}

fn selector_sql_expression(
    selector: &CompiledSelector,
    operation: SearchOperation,
    context: &ResolvedCallerContext,
    parameters: &mut Vec<String>,
) -> String {
    if (!selector.operation.is_empty() && !selector.operation.contains(&operation))
        || (!selector.caller_harness.is_empty()
            && !context
                .harness
                .as_ref()
                .is_some_and(|value| selector.caller_harness.contains(&value.value)))
        || (!selector.caller_model_id.is_empty()
            && !context
                .model_id
                .as_ref()
                .is_some_and(|value| selector.caller_model_id.contains(&value.value)))
        || (!selector.caller_working_directory_root.is_empty()
            && !context.working_directory.as_ref().is_some_and(|value| {
                matches_any_root(&value.value, &selector.caller_working_directory_root)
            }))
    {
        return "0".to_owned();
    }

    let mut terms = Vec::new();
    if !selector.session_provider.is_empty() {
        let placeholders = vec!["?"; selector.session_provider.len()].join(", ");
        terms.push(format!("s.provider in ({placeholders})"));
        parameters.extend(
            selector
                .session_provider
                .iter()
                .map(|provider| provider.as_str().to_owned()),
        );
    }
    if !selector.session_workspace_root.is_empty() {
        terms.push(path_group_sql(
            &selector.session_workspace_root,
            SessionPathColumns::Workspace,
            parameters,
        ));
    }
    if !selector.session_transcript_root.is_empty() {
        terms.push(path_group_sql(
            &selector.session_transcript_root,
            SessionPathColumns::Transcript,
            parameters,
        ));
    }
    for relation in &selector.session_workspace_relation {
        let roots: Vec<PathBuf> = match relation {
            SessionWorkspaceRelation::WithinCallerWorkingDirectory => context
                .working_directory
                .as_ref()
                .map(|value| vec![value.value.clone()])
                .unwrap_or_default(),
            SessionWorkspaceRelation::WithinLiveWorkspaceRoots => {
                context.live_workspace_roots.clone()
            }
        };
        if roots.is_empty() {
            return "0".to_owned();
        }
        terms.push(path_group_sql(
            &roots,
            SessionPathColumns::Workspace,
            parameters,
        ));
    }
    if terms.is_empty() {
        "1".to_owned()
    } else {
        terms.join(" and ")
    }
}

#[derive(Clone, Copy)]
enum SessionPathColumns {
    Workspace,
    Transcript,
}

fn path_group_sql(
    roots: &[PathBuf],
    columns: SessionPathColumns,
    parameters: &mut Vec<String>,
) -> String {
    roots
        .iter()
        .map(|root| {
            let prefix = root.to_str().expect("validated permission roots are UTF-8");
            path_condition_sql(prefix, columns, parameters)
        })
        .map(|expression| format!("({expression})"))
        .collect::<Vec<_>>()
        .join(" or ")
}

fn path_condition_sql(
    prefix: &str,
    columns: SessionPathColumns,
    parameters: &mut Vec<String>,
) -> String {
    let (exact, child_pattern) = path_prefix_patterns(prefix);
    match columns {
        SessionPathColumns::Workspace => {
            parameters.push(exact.clone());
            parameters.push(child_pattern.clone());
            parameters.push(exact);
            parameters.push(child_pattern);
            "(coalesce(s.cwd, '') = ? or coalesce(s.cwd, '') like ? escape '\\' or coalesce(s.repo_root, '') = ? or coalesce(s.repo_root, '') like ? escape '\\')".to_owned()
        }
        SessionPathColumns::Transcript => {
            parameters.push(exact);
            parameters.push(child_pattern);
            "(coalesce(s.source_path, '') = ? or coalesce(s.source_path, '') like ? escape '\\')"
                .to_owned()
        }
    }
}

pub(crate) fn path_prefix_patterns(prefix: &str) -> (String, String) {
    let bytes = prefix.as_bytes();
    let windows_style = prefix.starts_with(r"\\")
        || matches!(bytes, [drive, b':', b'\\' | b'/', ..] if drive.is_ascii_alphabetic());
    let separator = if windows_style { '\\' } else { '/' };
    let exact = prefix.trim_end_matches(separator).to_string();
    let child = format!("{exact}{separator}");
    (exact, literal_like_prefix_pattern(&child))
}

fn literal_like_prefix_pattern(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    for character in value.chars() {
        match character {
            '%' | '_' | '\\' => {
                escaped.push('\\');
                escaped.push(character);
            }
            other => escaped.push(other),
        }
    }
    escaped.push('%');
    escaped
}

fn origin_name(origin: AccessRootOrigin) -> &'static str {
    origin.as_str()
}

fn normalize_authority_root(path: &Path, origin: AccessRootOrigin) -> Result<AccessRoot> {
    let lexical = lexical_absolute(path)?;
    let (canonical, canonicalized_at_startup) = match fs::canonicalize(&lexical) {
        Ok(path) => {
            if !path.is_dir() {
                bail!("access root exists but is not a directory");
            }
            (lexical_absolute(&path)?, true)
        }
        Err(error)
            if error.kind() == ErrorKind::NotFound
                && origin == AccessRootOrigin::ExplicitConfig =>
        {
            (lexical.clone(), false)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            bail!("trusted runtime access root does not exist")
        }
        Err(error) => {
            return Err(error).with_context(|| format!("cannot resolve access root {lexical:?}"));
        }
    };
    if canonical.parent().is_none() {
        bail!("filesystem roots are not valid allowed-roots entries");
    }
    if lexical.to_str().is_none() || canonical.to_str().is_none() {
        bail!("access roots must be valid UTF-8 for the SQLite path model");
    }
    let mut match_paths = vec![canonical.clone()];
    if lexical != canonical {
        match_paths.push(lexical.clone());
    }
    Ok(AccessRoot {
        canonical_path: canonical,
        match_paths,
        sources: vec![AccessRootSource {
            configured_path: lexical,
            canonicalized_at_startup,
            origin,
        }],
    })
}

pub(crate) fn validate_configured_root(path: &Path) -> Result<()> {
    normalize_authority_root(path, AccessRootOrigin::ExplicitConfig).map(|_| ())
}

pub(crate) fn ensure_raw_sql_allowed(config: &SearchScopeConfig, operation: &str) -> Result<()> {
    if config.mode == SearchScopeMode::AllowedRoots {
        bail!(
            "{operation} is unavailable while search.scope.mode is allowed-roots because arbitrary SQL cannot enforce workspace authority; use typed search, session, message, analysis, file, or export operations"
        );
    }
    Ok(())
}

pub(crate) fn ensure_search_raw_sql_allowed(config: &SearchConfig, operation: &str) -> Result<()> {
    ensure_raw_sql_allowed(&config.scope, operation)?;
    if config.permissions.is_some() {
        bail!(
            "{operation} is unavailable while search.permissions is configured because arbitrary SQL cannot enforce typed row policy; use typed search, session, message, analysis, file, or export operations"
        );
    }
    Ok(())
}

fn lexical_absolute(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("access roots must be absolute paths");
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() || !normalized.has_root() {
                    return Err(anyhow!("access root escapes its filesystem root"));
                }
            }
        }
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn restricted(roots: &[&Path]) -> SearchScopeConfig {
        SearchScopeConfig {
            mode: SearchScopeMode::AllowedRoots,
            roots: roots
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            include_invocation_directory: false,
        }
    }

    #[test]
    fn search_operation_registry_classifies_every_closed_enum_value_once() {
        let registered = SEARCH_OPERATION_REGISTRY
            .iter()
            .map(|spec| spec.operation)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(registered.len(), SEARCH_OPERATION_REGISTRY.len());
        assert_eq!(registered, SearchOperation::ALL.into_iter().collect());
    }

    #[test]
    fn unrestricted_scope_preserves_every_path_and_ignores_unused_inputs() {
        let scope = EffectiveAccessScope::resolve(
            &SearchScopeConfig::default(),
            TrustedAccessInputs {
                harness_roots: vec![PathBuf::from("relative-is-unused")],
                invocation_directory: None,
            },
        )
        .unwrap();
        assert!(scope.is_unrestricted());
        assert!(scope.allows_workspace_path(Path::new("relative-is-still-visible")));
    }

    #[test]
    fn restricted_scope_unions_deduplicates_and_records_origins() {
        let root = tempfile::tempdir().unwrap();
        let configured = root.path().join("configured");
        let harness = root.path().join("harness");
        let invocation = root.path().join("invocation");
        fs::create_dir_all(&configured).unwrap();
        fs::create_dir_all(&harness).unwrap();
        fs::create_dir_all(&invocation).unwrap();
        let mut config = restricted(&[&configured]);
        config.include_invocation_directory = true;

        let scope = EffectiveAccessScope::resolve(
            &config,
            TrustedAccessInputs {
                harness_roots: vec![harness.clone(), configured.clone()],
                invocation_directory: Some(invocation.clone()),
            },
        )
        .unwrap();

        assert_eq!(scope.roots().len(), 3);
        assert_eq!(scope.roots()[0].origin(), AccessRootOrigin::HarnessRoots);
        assert_eq!(scope.roots()[1].origin(), AccessRootOrigin::HarnessRoots);
        assert_eq!(scope.roots()[1].sources().len(), 2);
        assert_eq!(
            scope.roots()[1].sources()[1].origin(),
            AccessRootOrigin::ExplicitConfig
        );
        assert_eq!(
            scope.roots()[2].origin(),
            AccessRootOrigin::InvocationDirectory
        );
        assert!(scope.allows_workspace_path(&harness.join("child")));
        assert!(scope.allows_workspace_path(&configured));
        assert!(scope.allows_workspace_path(&invocation.join("child")));
        assert!(!scope.allows_workspace_path(root.path().join("outside").as_path()));
    }

    #[test]
    fn restricted_scope_fails_closed_without_authoritative_roots() {
        let error = EffectiveAccessScope::resolve(&restricted(&[]), TrustedAccessInputs::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("resolved no authoritative roots"), "{error}");
    }

    #[test]
    fn roots_require_absolute_non_root_directory_paths() {
        for value in [Path::new("relative"), Path::new("/")] {
            let error = EffectiveAccessScope::resolve(
                &restricted(&[value]),
                TrustedAccessInputs::default(),
            )
            .unwrap_err()
            .to_string();
            assert!(
                error.contains("invalid explicit-config access root"),
                "{error}"
            );
        }
    }

    #[test]
    fn arbitrary_sql_is_allowed_only_for_unrestricted_scope() {
        ensure_raw_sql_allowed(&SearchScopeConfig::default(), "test query").unwrap();
        let error = ensure_raw_sql_allowed(&restricted(&[Path::new("/configured")]), "test query")
            .unwrap_err()
            .to_string();
        assert!(error.contains("test query is unavailable"));
        assert!(error.contains("arbitrary SQL cannot enforce workspace authority"));

        let search = SearchConfig {
            permissions: Some(SearchPermissionsConfig::default()),
            ..SearchConfig::default()
        };
        let error = ensure_search_raw_sql_allowed(&search, "test query")
            .unwrap_err()
            .to_string();
        assert!(error.contains("search.permissions"), "{error}");
        assert!(error.contains("typed row policy"), "{error}");
    }

    #[test]
    fn lexical_normalization_preserves_component_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let allowed = root.path().join("a/inside/../allowed");
        fs::create_dir_all(root.path().join("a/allowed")).unwrap();
        let scope =
            EffectiveAccessScope::resolve(&restricted(&[&allowed]), TrustedAccessInputs::default())
                .unwrap();
        assert!(scope.allows_workspace_path(&root.path().join("a/allowed/child")));
        assert!(!scope.allows_workspace_path(&root.path().join("a/allowed-sibling")));
    }

    #[cfg(unix)]
    #[test]
    fn existing_symlink_keeps_lexical_alias_and_fails_closed_after_retarget() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let alias = root.path().join("alias");
        fs::create_dir(&target).unwrap();
        symlink(&target, &alias).unwrap();

        let scope =
            EffectiveAccessScope::resolve(&restricted(&[&alias]), TrustedAccessInputs::default())
                .unwrap();
        assert_eq!(scope.roots()[0].path(), target.canonicalize().unwrap());
        assert!(scope.allows_workspace_path(&scope.roots()[0].path().join("child")));
        assert!(scope.allows_workspace_path(&alias.join("child")));
        scope.validate_stable().unwrap();

        let other = root.path().join("other");
        fs::create_dir(&other).unwrap();
        fs::remove_file(&alias).unwrap();
        symlink(&other, &alias).unwrap();
        let error = scope.validate_stable().unwrap_err().to_string();
        assert!(error.contains("changed target"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn every_deduplicated_symlink_alias_is_revalidated() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let other = root.path().join("other");
        let configured_alias = root.path().join("configured-alias");
        let harness_alias = root.path().join("harness-alias");
        fs::create_dir(&target).unwrap();
        fs::create_dir(&other).unwrap();
        symlink(&target, &configured_alias).unwrap();
        symlink(&target, &harness_alias).unwrap();

        let scope = EffectiveAccessScope::resolve(
            &restricted(&[&configured_alias]),
            TrustedAccessInputs {
                harness_roots: vec![harness_alias],
                invocation_directory: None,
            },
        )
        .unwrap();
        assert_eq!(scope.roots().len(), 1);
        assert_eq!(scope.roots()[0].sources().len(), 2);

        fs::remove_file(&configured_alias).unwrap();
        symlink(&other, &configured_alias).unwrap();
        let error = scope.validate_stable().unwrap_err().to_string();
        assert!(error.contains("changed target"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn configured_root_missing_at_startup_rejects_later_symlink_target() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let configured = root.path().join("future-root");
        let outside = root.path().join("outside");
        fs::create_dir(&outside).unwrap();

        let scope = EffectiveAccessScope::resolve(
            &restricted(&[&configured]),
            TrustedAccessInputs::default(),
        )
        .unwrap();
        scope.validate_stable().unwrap();

        symlink(&outside, &configured).unwrap();
        let error = scope.validate_stable().unwrap_err().to_string();
        assert!(error.contains("changed target"), "{error}");
    }
}
