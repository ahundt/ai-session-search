// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

//! Shared-layer contract for typed search permissions.

use std::path::Path;

use ai_session_search::config::Config;
use ai_session_search::models::Provider;
use ai_session_search::permission_store::PermissionStore;
use ai_session_search::search_scope::{
    CallerHarness, EffectiveSearchPolicy, GrantUntil, NativeAdapterAttestation, PolicyDecision,
    PolicyPreflightDecision, SearchOperation, SessionPolicySelector, SessionPolicyTarget,
    TemporaryGrantCoverage, TrustedPolicyInputs,
};

fn compile(source: &str) -> EffectiveSearchPolicy {
    let config: Config = toml::from_str(source).unwrap();
    config.validate().unwrap();
    EffectiveSearchPolicy::resolve(&config.search, TrustedPolicyInputs::default()).unwrap()
}

fn target<'a>(workspace: &'a str) -> SessionPolicyTarget<'a> {
    SessionPolicyTarget::new(
        SearchOperation::Read,
        Provider::Claude,
        Path::new(workspace),
        Path::new("/transcripts/session.jsonl"),
    )
}

#[test]
fn allow_block_hard_block_and_exception_have_order_independent_decisions() {
    let policy = compile(
        r#"
[search.permissions]
default_profile = "general"

[search.permissions.profiles.general]
default = "allow"

[[search.permissions.profiles.general.rules]]
rule_id = "old-client"
effect = "block"
resource = "session"
session_workspace_root = ["/archive/old-client"]

[[search.permissions.profiles.general.rules.exception]]
exception_id = "public"
session_workspace_root = ["/archive/old-client/public"]

[[search.permissions.profiles.general.rules]]
rule_id = "credentials"
effect = "hard-block"
resource = "session"
session_workspace_root = ["/secrets"]

[search.permissions.profiles.general.grant_envelope]
session_workspace_root = ["/archive/old-client"]
operation = ["read"]
max_uses = 3
"#,
    );

    assert_eq!(
        policy.evaluate_session(&target("/ordinary"), TemporaryGrantCoverage::None),
        PolicyDecision::Allow
    );
    assert_eq!(
        policy.evaluate_session(
            &target("/archive/old-client/private"),
            TemporaryGrantCoverage::None,
        ),
        PolicyDecision::Block
    );
    assert_eq!(
        policy.evaluate_session(
            &target("/archive/old-client/private"),
            TemporaryGrantCoverage::Exact,
        ),
        PolicyDecision::Allow
    );
    assert_eq!(
        policy.evaluate_session(
            &target("/archive/old-client/public"),
            TemporaryGrantCoverage::None,
        ),
        PolicyDecision::Allow
    );
    assert_eq!(
        policy.evaluate_session(&target("/secrets/key"), TemporaryGrantCoverage::Exact),
        PolicyDecision::HardBlock
    );
}

#[test]
fn default_block_is_an_allowlist_and_grants_stay_inside_the_envelope() {
    let policy = compile(
        r#"
[search.permissions]
default_profile = "project"

[search.permissions.profiles.project]
default = "block"

[[search.permissions.profiles.project.rules]]
rule_id = "project-read"
effect = "allow"
resource = "session"
operation = ["read"]
session_workspace_root = ["/work/project"]

[search.permissions.profiles.project.grant_envelope]
operation = ["read"]
session_workspace_root = ["/work/shared"]
max_uses = 1
"#,
    );

    assert_eq!(
        policy.evaluate_session(&target("/work/project/api"), TemporaryGrantCoverage::None),
        PolicyDecision::Allow
    );
    assert_eq!(
        policy.evaluate_session(&target("/work/other"), TemporaryGrantCoverage::Exact),
        PolicyDecision::Block
    );
    assert_eq!(
        policy.evaluate_session(&target("/work/shared/api"), TemporaryGrantCoverage::Exact),
        PolicyDecision::Allow
    );
}

#[test]
fn required_caller_context_is_unavailable_until_a_trusted_origin_supplies_it() {
    let source = r#"
[search.permissions]
default_profile = "pi-only"

[search.permissions.profiles.pi-only]
default = "hard-block"

[[search.permissions.profiles.pi-only.rules]]
rule_id = "attested-pi"
effect = "allow"
resource = "session"
caller_harness = ["pi"]
session_workspace_root = ["/work/project"]
"#;
    let config: Config = toml::from_str(source).unwrap();
    config.validate().unwrap();
    let error = EffectiveSearchPolicy::resolve(&config.search, TrustedPolicyInputs::default())
        .unwrap_err()
        .to_string();
    assert!(error.contains("caller_harness"), "{error}");
    assert!(error.contains("unavailable"), "{error}");

    let policy = EffectiveSearchPolicy::resolve(
        &config.search,
        TrustedPolicyInputs::default().with_launch_harness(CallerHarness::Pi),
    )
    .unwrap();
    assert_eq!(
        policy.evaluate_session(&target("/work/project"), TemporaryGrantCoverage::None),
        PolicyDecision::Allow
    );
}

#[test]
fn native_attestation_and_observed_context_have_distinct_trust_contracts() {
    let config: Config = toml::from_str(
        r#"
[search.permissions]
default_profile = "native"

[search.permissions.profiles.native]
default = "hard-block"

[[search.permissions.profiles.native.rules]]
rule_id = "native-model"
effect = "allow"
resource = "session"
caller_harness = ["pi"]
caller_model_id = ["openai/gpt-5"]
session_workspace_relation = ["within-caller-working-directory"]
"#,
    )
    .unwrap();
    let inputs = TrustedPolicyInputs::default()
        .with_native_adapter_attestation(NativeAdapterAttestation {
            version: 1,
            generation: 7,
            harness: CallerHarness::Pi,
            model_id: "openai/gpt-5".to_owned(),
            working_directory: Some(Path::new("/work/project").to_owned()),
            session_binding: Some("pi-session-1".to_owned()),
        })
        .unwrap();
    let policy = EffectiveSearchPolicy::resolve(&config.search, inputs).unwrap();
    assert_eq!(
        policy.evaluate_session(&target("/work/project/src"), TemporaryGrantCoverage::None),
        PolicyDecision::Allow
    );
    assert!(TrustedPolicyInputs::default()
        .with_native_adapter_attestation(NativeAdapterAttestation {
            version: 2,
            generation: 7,
            harness: CallerHarness::Pi,
            model_id: "openai/gpt-5".to_owned(),
            working_directory: None,
            session_binding: None,
        })
        .unwrap_err()
        .to_string()
        .contains("attestation version"));
}

#[test]
fn declarations_can_narrow_but_never_satisfy_allow_rules() {
    let allow_config: Config = toml::from_str(
        r#"
[search.permissions]
default_profile = "pi-only"

[search.permissions.profiles.pi-only]
default = "hard-block"

[[search.permissions.profiles.pi-only.rules]]
rule_id = "pi"
effect = "allow"
resource = "session"
caller_harness = ["pi"]
session_workspace_root = ["/work"]
"#,
    )
    .unwrap();
    let error = EffectiveSearchPolicy::resolve(
        &allow_config.search,
        TrustedPolicyInputs::default().with_declared_harness(CallerHarness::Pi),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("declared") && error.contains("does not authorize"),
        "{error}"
    );

    let block_config: Config = toml::from_str(
        r#"
[search.permissions]
default_profile = "general"

[search.permissions.profiles.general]
default = "allow"

[[search.permissions.profiles.general.rules]]
rule_id = "declared-risk"
effect = "hard-block"
resource = "session"
caller_model_id = ["untrusted-model"]
session_workspace_root = ["/work"]
"#,
    )
    .unwrap();
    let policy = EffectiveSearchPolicy::resolve(
        &block_config.search,
        TrustedPolicyInputs::default().with_declared_model_id("untrusted-model".to_owned()),
    )
    .unwrap();
    assert_eq!(
        policy.evaluate_session(&target("/work"), TemporaryGrantCoverage::None),
        PolicyDecision::HardBlock
    );
}

#[test]
fn mcp_live_roots_are_trusted_only_through_the_roots_adapter() {
    let config: Config = toml::from_str(
        r#"
[search.permissions]
default_profile = "live"

[search.permissions.profiles.live]
default = "hard-block"

[[search.permissions.profiles.live.rules]]
rule_id = "live-root"
effect = "allow"
resource = "session"
session_workspace_relation = ["within-live-workspace-roots"]
"#,
    )
    .unwrap();
    let policy = EffectiveSearchPolicy::resolve(
        &config.search,
        TrustedPolicyInputs::default()
            .with_mcp_live_roots(vec![Path::new("/work/live").to_owned()], 4)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        policy.evaluate_session(&target("/work/live/repo"), TemporaryGrantCoverage::None),
        PolicyDecision::Allow
    );
    assert!(
        EffectiveSearchPolicy::resolve(&config.search, TrustedPolicyInputs::default())
            .unwrap_err()
            .to_string()
            .contains("live workspace roots")
    );
}

#[test]
fn policy_generation_is_monotonic_even_when_config_bytes_revert() {
    let root = tempfile::tempdir().unwrap();
    let store = PermissionStore::open(&root.path().join("permissions.sqlite")).unwrap();
    let config_path = root.path().join("config.toml");

    let first = store
        .observe_valid_policy_source(&config_path, b"default = 'block'")
        .unwrap();
    let unchanged = store
        .observe_valid_policy_source(&config_path, b"default = 'block'")
        .unwrap();
    assert_eq!(unchanged, first);

    let second = store
        .observe_valid_policy_source(&config_path, b"default = 'allow'")
        .unwrap();
    assert!(second > first);
    let reverted = store
        .observe_valid_policy_source(&config_path, b"default = 'block'")
        .unwrap();
    assert!(
        reverted > second,
        "reverting bytes must not reactivate old grants"
    );
}

#[test]
fn restrictive_overlay_profiles_intersect_the_selected_profile() {
    let config: Config = toml::from_str(
        r#"
[search.permissions]
default_profile = "general"

[search.permissions.profiles.general]
default = "allow"

[search.permissions.profiles.focus]
default = "hard-block"

[[search.permissions.profiles.focus.rules]]
rule_id = "focus-root"
effect = "allow"
resource = "session"
session_workspace_root = ["/work/focus"]
"#,
    )
    .unwrap();
    config.validate().unwrap();
    let policy = EffectiveSearchPolicy::resolve(
        &config.search,
        TrustedPolicyInputs {
            overlay_profiles: vec!["focus".to_owned()],
            ..TrustedPolicyInputs::default()
        },
    )
    .unwrap();
    assert_eq!(
        policy.evaluate_session(&target("/work/focus/api"), TemporaryGrantCoverage::None),
        PolicyDecision::Allow
    );
    assert_eq!(
        policy.evaluate_session(&target("/work/other"), TemporaryGrantCoverage::None),
        PolicyDecision::HardBlock
    );
}

#[test]
fn explicit_blocked_selector_requests_only_inside_the_envelope() {
    let policy = compile(
        r#"
[search.permissions]
default_profile = "general"

[search.permissions.profiles.general]
default = "allow"

[[search.permissions.profiles.general.rules]]
rule_id = "archive"
effect = "block"
resource = "session"
session_workspace_root = ["/archive"]

[[search.permissions.profiles.general.rules]]
rule_id = "secrets"
effect = "hard-block"
resource = "session"
session_workspace_root = ["/archive/secrets"]

[search.permissions.profiles.general.grant_envelope]
operation = ["read"]
session_workspace_root = ["/archive"]
max_uses = 2
max_expires_in = "2h"
allowed_until = ["connection-close"]
"#,
    );
    assert!(matches!(
        policy.preflight_session_selector(&SessionPolicySelector::workspace_root(
            SearchOperation::Read,
            Path::new("/archive/client"),
        )),
        PolicyPreflightDecision::Request {
            max_uses: 2,
            max_expires_in_seconds: Some(7200),
            ref allowed_until,
            ..
        } if allowed_until == &[GrantUntil::ConnectionClose]
    ));
    assert_eq!(
        policy.preflight_session_selector(&SessionPolicySelector::workspace_root(
            SearchOperation::Read,
            Path::new("/archive/secrets"),
        )),
        PolicyPreflightDecision::HardBlock
    );
    assert_eq!(
        policy.preflight_session_selector(&SessionPolicySelector::workspace_root(
            SearchOperation::Read,
            Path::new("/outside"),
        )),
        PolicyPreflightDecision::Allow
    );

    let granted = EffectiveSearchPolicy::resolve(
        &toml::from_str::<Config>(
            r#"
[search.permissions]
default_profile = "general"

[search.permissions.profiles.general]
default = "block"

[[search.permissions.profiles.general.rules]]
rule_id = "secrets"
effect = "hard-block"
resource = "session"
session_workspace_root = ["/archive/secrets"]

[search.permissions.profiles.general.grant_envelope]
operation = ["read"]
session_workspace_root = ["/archive"]
max_uses = 2
"#,
        )
        .unwrap()
        .search,
        TrustedPolicyInputs {
            admitted_grant_selectors: vec![SessionPolicySelector::workspace_root(
                SearchOperation::Read,
                Path::new("/archive/client"),
            )],
            ..TrustedPolicyInputs::default()
        },
    )
    .unwrap();
    assert_eq!(
        granted.evaluate_session(&target("/archive/client/api"), TemporaryGrantCoverage::None),
        PolicyDecision::Allow
    );
    assert_eq!(
        granted.evaluate_session(&target("/archive/other"), TemporaryGrantCoverage::None),
        PolicyDecision::Block
    );
    assert_eq!(
        granted.evaluate_session(&target("/archive/secrets"), TemporaryGrantCoverage::None),
        PolicyDecision::HardBlock
    );
}

#[test]
fn invalid_grant_duration_is_rejected_during_config_validation() {
    let config: Config = toml::from_str(
        r#"
[search.permissions]
default_profile = "restricted"

[search.permissions.profiles.restricted]
default = "block"

[search.permissions.profiles.restricted.grant_envelope]
operation = ["read"]
session_workspace_root = ["/work"]
max_expires_in = "31d"
"#,
    )
    .unwrap();
    let error = config.validate().unwrap_err().to_string();
    assert!(error.contains("max_expires_in is invalid"), "{error}");
}

#[test]
fn invalid_policy_never_falls_back_to_unrestricted() {
    let unknown = toml::from_str::<Config>(
        r#"
[search.permissions]
default_profile = "work"

[search.permissions.profiles.work]
default = "block"
typo = true
"#,
    )
    .unwrap_err()
    .to_string();
    assert!(unknown.contains("unknown field"), "{unknown}");

    let mut empty_rule: Config = toml::from_str(
        r#"
[search.permissions]
default_profile = "work"

[search.permissions.profiles.work]
default = "block"

[[search.permissions.profiles.work.rules]]
rule_id = "empty"
effect = "allow"
resource = "session"
"#,
    )
    .unwrap();
    let error = empty_rule.validate().unwrap_err().to_string();
    assert!(
        error.contains("rule \"empty\" has no selector or operation"),
        "{error}"
    );

    empty_rule.search.scope.mode = ai_session_search::config::SearchScopeMode::AllowedRoots;
    empty_rule.search.scope.roots = vec!["/legacy".to_owned()];
    let error = empty_rule.validate().unwrap_err().to_string();
    assert!(error.contains("search.scope"), "{error}");
    assert!(error.contains("search.permissions"), "{error}");
}
