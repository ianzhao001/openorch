//! 红种子契约 · B304 · root 采用 collect 的绿。
//!
//! 首红：compile `E0432`。只有同一 tree/contract/command/toolchain/environment 且全绿的
//! collect bundle 可被采用；main SHA 刻意不属于 identity。

use orch_host::verify::{
    plan_root_gate_reuse_v1, CollectGateBundleV1, ReusedGateV1, RootGateReuseDecisionV1,
    RootGateSubjectV1, ROOT_GATE_REUSE_CONTRACT_V1,
};

fn gates() -> Vec<ReusedGateV1> {
    ["seedTargets", "sourceReaderClosure", "check"]
        .iter()
        .enumerate()
        .map(|(index, name)| ReusedGateV1 {
            name: (*name).to_string(),
            exit_code: 0,
            source_event_id: format!("01ARZ3NDEKTSV4RRFFQ69G5F{index:02}"),
            log_sha256: format!("{index:064}"),
            log_bytes: 4096 + index as u64,
        })
        .collect()
}

fn bundle() -> CollectGateBundleV1 {
    CollectGateBundleV1 {
        subject_tree_sha: "1426d7b316646da31a7875a44fff8dbf7a10ccd3".into(),
        ir_revision: 1,
        validation_digest: format!("{:064}", 1),
        binding_sha256: format!("{:064}", 2),
        task_card_sha256: format!("{:064}", 3),
        resolved_command_digest: format!("{:064}", 4),
        toolchain_digest: format!("{:064}", 5),
        environment_digest: format!("{:064}", 6),
        gates: gates(),
    }
}

fn subject() -> RootGateSubjectV1 {
    let bundle = bundle();
    RootGateSubjectV1 {
        subject_tree_sha: bundle.subject_tree_sha,
        ir_revision: bundle.ir_revision,
        validation_digest: bundle.validation_digest,
        binding_sha256: bundle.binding_sha256,
        task_card_sha256: bundle.task_card_sha256,
        resolved_command_digest: bundle.resolved_command_digest,
        toolchain_digest: bundle.toolchain_digest,
        environment_digest: bundle.environment_digest,
    }
}

#[test]
fn the_public_contract_anchor_is_version_one() {
    assert_eq!(ROOT_GATE_REUSE_CONTRACT_V1, 1);
}

#[test]
fn the_same_green_identity_is_reused() {
    assert!(matches!(
        plan_root_gate_reuse_v1(&bundle(), &subject()),
        RootGateReuseDecisionV1::Reuse(ref gates) if gates.len() == 3
    ));
}

#[test]
fn every_identity_drift_runs_real_root_gates() {
    let mut cases = Vec::new();
    let mut tree = subject();
    tree.subject_tree_sha = "0".repeat(40);
    cases.push(tree);
    let mut contract = subject();
    contract.validation_digest = "7".repeat(64);
    cases.push(contract);
    let mut command = subject();
    command.resolved_command_digest = "8".repeat(64);
    cases.push(command);
    let mut environment = subject();
    environment.environment_digest = "9".repeat(64);
    cases.push(environment);
    for drifted in cases {
        assert!(matches!(
            plan_root_gate_reuse_v1(&bundle(), &drifted),
            RootGateReuseDecisionV1::Execute { .. }
        ));
    }
}

#[test]
fn a_red_or_unreadable_gate_is_never_reused() {
    let mut red = bundle();
    red.gates[0].exit_code = 101;
    assert!(matches!(
        plan_root_gate_reuse_v1(&red, &subject()),
        RootGateReuseDecisionV1::Execute { .. }
    ));
    let mut missing = bundle();
    missing.gates[0].source_event_id.clear();
    assert!(matches!(
        plan_root_gate_reuse_v1(&missing, &subject()),
        RootGateReuseDecisionV1::Execute { .. }
    ));
}

