// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

use std::num::NonZeroU64;
use std::path::Path;
use std::sync::{Arc, Barrier};

use ai_session_search::permission_store::{
    AdmissionInput, AdmissionOutcome, ClaimTerminal, GrantApproval, OverlayActivation,
    PendingRequestInput, PermissionRequestBounds, PermissionStore, PolicyGeneration,
};
use ai_session_search::search_scope::{SearchOperation, SessionPolicySelector};

fn generation(value: u64) -> PolicyGeneration {
    PolicyGeneration::new(value).unwrap()
}

#[test]
fn duplicate_delivery_spends_once_and_admitted_failure_is_not_refunded() {
    let root = tempfile::tempdir().unwrap();
    let store = PermissionStore::open(&root.path().join("permissions.sqlite")).unwrap();
    let selector =
        SessionPolicySelector::workspace_root(SearchOperation::Read, Path::new("/work/client"));
    let request = store
        .create_typed_pending_request(
            PendingRequestInput {
                policy_generation: generation(7),
                caller_context_binding: "caller-a".to_owned(),
                operation: SearchOperation::Read,
                selector_digest: selector.digest(),
            },
            &selector,
            PermissionRequestBounds {
                max_uses: NonZeroU64::new(2).unwrap(),
                max_expires_in_seconds: Some(90),
            },
            100,
        )
        .unwrap();
    assert_eq!(request.as_str().len(), 32);
    let grant = store
        .approve_request(
            request.as_str(),
            GrantApproval {
                uses: Some(NonZeroU64::new(2).unwrap()),
                expires_at_epoch_seconds: Some(200),
            },
            110,
        )
        .unwrap();
    assert_eq!(store.grant_selector(grant.as_str()).unwrap(), selector);

    let input = AdmissionInput {
        grant_id: grant.as_str(),
        operation_id: "rpc-1",
        policy_generation: generation(7),
        caller_context_binding: "caller-a",
        operation: SearchOperation::Read,
        now_epoch_seconds: 120,
    };
    assert_eq!(store.admit(&input).unwrap(), AdmissionOutcome::Admitted);
    assert_eq!(
        store.admit(&input).unwrap(),
        AdmissionOutcome::DuplicateInFlight
    );
    store
        .finish_claim(grant.as_str(), "rpc-1", ClaimTerminal::Failed)
        .unwrap();
    assert_eq!(
        store.admit(&input).unwrap(),
        AdmissionOutcome::DuplicateTerminal(ClaimTerminal::Failed)
    );

    assert_eq!(
        store
            .admit(&AdmissionInput {
                operation_id: "rpc-2",
                ..input
            })
            .unwrap(),
        AdmissionOutcome::Admitted
    );
    let error = store
        .admit(&AdmissionInput {
            operation_id: "rpc-3",
            ..input
        })
        .unwrap_err()
        .to_string();
    assert!(error.contains("no active permission grant"), "{error}");
}

#[test]
fn distinct_process_connections_cannot_spend_one_use_twice() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("permissions.sqlite");
    let store = PermissionStore::open(&path).unwrap();
    let request = store
        .create_pending_request(
            PendingRequestInput {
                policy_generation: generation(1),
                caller_context_binding: "caller".to_owned(),
                operation: SearchOperation::Read,
                selector_digest: "selector".to_owned(),
            },
            10,
        )
        .unwrap();
    let grant = store
        .approve_request(request.as_str(), GrantApproval::one_use(), 11)
        .unwrap();
    drop(store);

    let barrier = Arc::new(Barrier::new(3));
    let results = ["one", "two"].map(|operation_id| {
        let path = path.clone();
        let grant = grant.as_str().to_owned();
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            let store = PermissionStore::open(&path).unwrap();
            barrier.wait();
            store.admit(&AdmissionInput {
                grant_id: &grant,
                operation_id,
                policy_generation: generation(1),
                caller_context_binding: "caller",
                operation: SearchOperation::Read,
                now_epoch_seconds: 12,
            })
        })
    });
    barrier.wait();
    let outcomes = results.map(|thread| thread.join().unwrap());
    assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(outcomes.iter().filter(|result| result.is_err()).count(), 1);
}

#[test]
fn approval_cannot_exceed_the_persisted_request_envelope() {
    let root = tempfile::tempdir().unwrap();
    let store = PermissionStore::open(&root.path().join("permissions.sqlite")).unwrap();
    let selector =
        SessionPolicySelector::workspace_root(SearchOperation::Read, Path::new("/work/client"));
    let create = |now| {
        store
            .create_typed_pending_request(
                PendingRequestInput {
                    policy_generation: generation(3),
                    caller_context_binding: "caller".to_owned(),
                    operation: SearchOperation::Read,
                    selector_digest: selector.digest(),
                },
                &selector,
                PermissionRequestBounds {
                    max_uses: NonZeroU64::new(2).unwrap(),
                    max_expires_in_seconds: Some(60),
                },
                now,
            )
            .unwrap()
    };
    let request = create(100);
    let error = store
        .approve_request(
            request.as_str(),
            GrantApproval {
                uses: Some(NonZeroU64::new(3).unwrap()),
                expires_at_epoch_seconds: None,
            },
            110,
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("accepted maximum of 2"), "{error}");

    let request = create(200);
    let error = store
        .approve_request(
            request.as_str(),
            GrantApproval {
                uses: Some(NonZeroU64::MIN),
                expires_at_epoch_seconds: Some(271),
            },
            210,
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("accepted maximum of 60s"), "{error}");
}

#[test]
fn status_reports_active_authority_and_pruning_preserves_only_live_rows() {
    let root = tempfile::tempdir().unwrap();
    let store = PermissionStore::open(&root.path().join("permissions.sqlite")).unwrap();
    let request = store
        .create_pending_request(
            PendingRequestInput {
                policy_generation: generation(9),
                caller_context_binding: "caller".to_owned(),
                operation: SearchOperation::Read,
                selector_digest: "selector".to_owned(),
            },
            100,
        )
        .unwrap();
    assert_eq!(
        store.status(generation(9), 110).unwrap().pending_requests,
        1
    );
    let grant = store
        .approve_request(request.as_str(), GrantApproval::one_use(), 120)
        .unwrap();
    assert_eq!(store.status(generation(9), 121).unwrap().active_grants, 1);
    store
        .admit(&AdmissionInput {
            grant_id: grant.as_str(),
            operation_id: "operation",
            policy_generation: generation(9),
            caller_context_binding: "caller",
            operation: SearchOperation::Read,
            now_epoch_seconds: 130,
        })
        .unwrap();
    store
        .finish_claim(grant.as_str(), "operation", ClaimTerminal::Succeeded)
        .unwrap();
    let prune_at = chrono::Utc::now().timestamp() + 40 * 24 * 60 * 60;
    let removed = store
        .prune_terminal_state(prune_at, 30 * 24 * 60 * 60)
        .unwrap();
    assert!(removed >= 2, "removed {removed}");
    let status = store.status(generation(9), prune_at).unwrap();
    assert_eq!(status.active_grants, 0);
    assert_eq!(status.in_flight_claims, 0);
}

#[test]
fn restrictive_overlays_bind_generation_lifecycle_and_deactivation() {
    let root = tempfile::tempdir().unwrap();
    let store = PermissionStore::open(&root.path().join("permissions.sqlite")).unwrap();
    let global = store
        .activate_overlay(
            OverlayActivation {
                profile: "focus".to_owned(),
                policy_generation: generation(5),
                expires_at_epoch_seconds: 200,
                session_binding: None,
            },
            100,
        )
        .unwrap();
    let session = store
        .activate_overlay(
            OverlayActivation {
                profile: "incident".to_owned(),
                policy_generation: generation(5),
                expires_at_epoch_seconds: 300,
                session_binding: Some("session-a".to_owned()),
            },
            100,
        )
        .unwrap();
    assert_eq!(global.as_str().len(), 32);
    assert_eq!(
        store
            .active_overlay_profiles(generation(5), None, 150)
            .unwrap(),
        vec!["focus"]
    );
    assert_eq!(
        store
            .active_overlay_profiles(generation(5), Some("session-a"), 150)
            .unwrap(),
        vec!["focus", "incident"]
    );
    assert!(store
        .active_overlay_profiles(generation(6), Some("session-a"), 150)
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .active_overlay_profiles(generation(5), Some("session-a"), 200)
            .unwrap(),
        vec!["incident"]
    );
    store.deactivate_overlay(session.as_str(), 160).unwrap();
    assert_eq!(
        store
            .active_overlay_profiles(generation(5), Some("session-a"), 170)
            .unwrap(),
        vec!["focus"]
    );
}

#[test]
fn generation_context_expiry_and_revocation_are_rechecked_at_admission() {
    let root = tempfile::tempdir().unwrap();
    let store = PermissionStore::open(&root.path().join("permissions.sqlite")).unwrap();
    let request = store
        .create_pending_request(
            PendingRequestInput {
                policy_generation: generation(3),
                caller_context_binding: "caller".to_owned(),
                operation: SearchOperation::Read,
                selector_digest: "selector".to_owned(),
            },
            100,
        )
        .unwrap();
    let grant = store
        .approve_request(
            request.as_str(),
            GrantApproval {
                uses: Some(NonZeroU64::new(4).unwrap()),
                expires_at_epoch_seconds: Some(120),
            },
            101,
        )
        .unwrap();

    for (generation, caller, now, expected) in [
        (generation(4), "caller", 110, "policy generation"),
        (generation(3), "other", 110, "caller context"),
        (generation(3), "caller", 120, "expired"),
    ] {
        let error = store
            .admit(&AdmissionInput {
                grant_id: grant.as_str(),
                operation_id: expected,
                policy_generation: generation,
                caller_context_binding: caller,
                operation: SearchOperation::Read,
                now_epoch_seconds: now,
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{error}");
    }

    store.revoke_grant(grant.as_str(), 111).unwrap();
    let error = store
        .admit(&AdmissionInput {
            grant_id: grant.as_str(),
            operation_id: "revoked",
            policy_generation: generation(3),
            caller_context_binding: "caller",
            operation: SearchOperation::Read,
            now_epoch_seconds: 112,
        })
        .unwrap_err()
        .to_string();
    assert!(error.contains("revoked"), "{error}");
}
