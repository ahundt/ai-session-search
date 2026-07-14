use super::{
    evidence_truncation, fair_evidence_quotas, EvidenceTruncation, DEFAULT_EVIDENCE_LIMIT,
};

#[test]
fn aggregate_evidence_budget_is_fair_and_reuses_unused_section_capacity() {
    assert_eq!(
        fair_evidence_quotas(&[12, 12, 12, 2], DEFAULT_EVIDENCE_LIMIT),
        vec![4, 3, 3, 2]
    );
    assert_eq!(
        fair_evidence_quotas(&[12, 0, 6, 0], DEFAULT_EVIDENCE_LIMIT),
        vec![6, 0, 6, 0]
    );
}

#[test]
fn aggregate_evidence_budget_never_exceeds_available_items_or_budget() {
    for lengths in [[0, 0, 0, 0], [1, 1, 1, 1], [100, 2, 7, 0]] {
        let quotas = fair_evidence_quotas(&lengths, DEFAULT_EVIDENCE_LIMIT);
        assert!(quotas
            .iter()
            .zip(lengths)
            .all(|(quota, length)| *quota <= length));
        assert!(quotas.iter().sum::<usize>() <= DEFAULT_EVIDENCE_LIMIT);
    }
}

#[test]
fn truncation_metadata_identifies_only_sections_with_more_evidence() {
    let lengths = [12, 12, 5, 2];
    let quotas = fair_evidence_quotas(&lengths, DEFAULT_EVIDENCE_LIMIT);
    let nested_lengths = [12, 4, 4];
    let nested_quotas = fair_evidence_quotas(&nested_lengths, DEFAULT_EVIDENCE_LIMIT);

    assert_eq!(
        evidence_truncation(&lengths, &quotas, &nested_lengths, &nested_quotas),
        EvidenceTruncation {
            is_truncated: true,
            user_intent: true,
            tool_activity: true,
            refs: true,
            nested_refs: true,
            changed_files: false,
        }
    );
    assert_eq!(
        evidence_truncation(&[1, 1, 1, 1], &[1, 1, 1, 1], &[1], &[1]),
        EvidenceTruncation::default()
    );
}

#[test]
fn aggregate_budget_accepts_a_caller_selected_size() {
    assert_eq!(fair_evidence_quotas(&[9, 9, 9, 9], 8), vec![2, 2, 2, 2]);
    assert_eq!(fair_evidence_quotas(&[9, 0, 9, 0], 6), vec![3, 0, 3, 0]);
}
