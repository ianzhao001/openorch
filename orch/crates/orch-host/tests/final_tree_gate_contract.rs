//! 红种子契约 · B305 · 唯一 merged-tree full 与 attempt-scoped miss budget。
//!
//! 首红：compile `E0432`。本种子冻结树逐字相等、ref CAS 不扣 miss、输入漂移按 attempt
//! 累计且第二次进入 approved-reattempt；真实 merge 仍须发生在 full green 之后。

use orch_host::close::{
    compare_actual_merge_tree_v1, decide_gate_reuse_miss_v1, FinalTreeDecisionV1,
    GateReuseMissDecisionV1, TestedMergeTreeV1, FINAL_TREE_GATE_CONTRACT_V1,
};

const CLOSE: &str = include_str!("../src/close.rs");
const COLLECT: &str = include_str!("../src/collect.rs");
const HOOK: &str = include_str!("../../../../.githooks/reference-transaction");

fn tested() -> TestedMergeTreeV1 {
    TestedMergeTreeV1 {
        tree_sha: "4e2490524f0b678eb5f4a24d31fce306c797596b".into(),
        main_sha: "12d0209784da41943f39dbbc067068672c4d18f1".into(),
        candidate_sha: "1426d7b316646da31a7875a44fff8dbf7a10ccd3".into(),
        input_identity_sha256: format!("{:064}", 7),
    }
}

#[test]
fn the_public_contract_anchor_is_version_one() {
    assert_eq!(FINAL_TREE_GATE_CONTRACT_V1, 1);
}

#[test]
fn only_the_exact_actual_tree_is_reused() {
    assert_eq!(
        compare_actual_merge_tree_v1(&tested(), &tested().tree_sha),
        FinalTreeDecisionV1::Reuse
    );
    assert!(matches!(
        compare_actual_merge_tree_v1(&tested(), &"0".repeat(40)),
        FinalTreeDecisionV1::RunPostMerge { .. }
    ));
}

#[test]
fn ref_cas_contention_does_not_burn_the_attempt_budget() {
    assert_eq!(
        decide_gate_reuse_miss_v1(1, false, true),
        GateReuseMissDecisionV1::RetryRefCas { miss_count: 1 }
    );
}

#[test]
fn input_drift_is_attempt_scoped_and_bounded() {
    assert_eq!(
        decide_gate_reuse_miss_v1(0, true, false),
        GateReuseMissDecisionV1::RetryInput { miss_count: 1 }
    );
    assert_eq!(
        decide_gate_reuse_miss_v1(1, true, false),
        GateReuseMissDecisionV1::ApprovedReattempt { miss_count: 2 }
    );
}

#[test]
fn production_order_keeps_the_irreversible_boundary_last() {
    let tested_at = CLOSE.find("TestedMergeTreeV1").expect("seal must build tested tree proof");
    let merge_at = CLOSE[tested_at..]
        .find("MergeStarted")
        .map(|offset| offset + tested_at)
        .expect("stable full proof must precede MergeStarted");
    assert!(tested_at < merge_at);
    assert!(HOOK.contains("seal") || HOOK.contains("MergeStarted"));
    let trial_start = COLLECT.find("fn execute_trial_merge_at").expect("cheap trial preflight remains");
    let trial = &COLLECT[trial_start..];
    assert!(!trial[..trial.len().min(5000)].contains("run_trial_gate_with_permit_and_identity"));
}

