//! Pure final-tree proof retained after schema-1/2 seal writers retire.

use orch_host::close::{
    compare_actual_merge_tree_v1, decide_gate_reuse_miss_v1, FinalTreeDecisionV1,
    GateReuseMissDecisionV1, TestedMergeTreeV1,
};

fn tested(tree: &str) -> TestedMergeTreeV1 {
    TestedMergeTreeV1 {
        tree_sha: tree.to_string(),
        main_sha: "b".repeat(40),
        candidate_sha: "c".repeat(40),
        input_identity_sha256: "d".repeat(64),
    }
}

#[test]
fn exact_full_tree_identity_reuses_the_proof() {
    let tree = "a".repeat(40);
    assert_eq!(
        compare_actual_merge_tree_v1(&tested(&tree), &tree),
        FinalTreeDecisionV1::Reuse
    );
}

#[test]
fn drift_or_abbreviation_forces_the_real_postmerge_lane() {
    let tree = "a".repeat(40);
    assert!(matches!(
        compare_actual_merge_tree_v1(&tested(&tree), &"e".repeat(40)),
        FinalTreeDecisionV1::RunPostMerge { .. }
    ));
    assert!(matches!(
        compare_actual_merge_tree_v1(&tested(&tree), &tree[..7]),
        FinalTreeDecisionV1::RunPostMerge { .. }
    ));
}

#[test]
fn input_drift_budget_is_attempt_scoped_and_monotonic() {
    assert_eq!(
        decide_gate_reuse_miss_v1(0, false, false),
        GateReuseMissDecisionV1::Stable { miss_count: 0 }
    );
    assert_eq!(
        decide_gate_reuse_miss_v1(0, true, false),
        GateReuseMissDecisionV1::RetryInput { miss_count: 1 }
    );
    assert_eq!(
        decide_gate_reuse_miss_v1(1, true, false),
        GateReuseMissDecisionV1::ApprovedReattempt { miss_count: 2 }
    );
}
