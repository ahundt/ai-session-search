// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

use super::{fair_evidence_quotas, truncated_evidence, EvidenceSection, DEFAULT_EVIDENCE_LIMIT};

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
        truncated_evidence(&lengths, &quotas, &nested_lengths, &nested_quotas),
        vec![
            EvidenceSection::UserIntent,
            EvidenceSection::ToolActivity,
            EvidenceSection::ReferenceMessages,
            EvidenceSection::References,
        ]
    );
    assert_eq!(
        truncated_evidence(&[1, 1, 1, 1], &[1, 1, 1, 1], &[1], &[1]),
        Vec::<EvidenceSection>::new()
    );
}

#[test]
fn aggregate_budget_accepts_a_caller_selected_size() {
    assert_eq!(fair_evidence_quotas(&[9, 9, 9, 9], 8), vec![2, 2, 2, 2]);
    assert_eq!(fair_evidence_quotas(&[9, 0, 9, 0], 6), vec![3, 0, 3, 0]);
}
