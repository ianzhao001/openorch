//! Pure collect-to-root gate reuse contract for the schema-3 verdict path.

use orch_host::verify::{
    plan_root_gate_reuse_v1, CollectGateBundleV1, ReusedGateV1, RootGateReuseDecisionV1,
    RootGateSubjectV1,
};

/// Build an exact matching synthetic collect bundle/subject without filesystem
/// state, so each reuse-identity component can be independently mutated.
pub fn fixture() -> (CollectGateBundleV1, RootGateSubjectV1) {
    let bundle = CollectGateBundleV1 {
        subject_tree_sha: "a".repeat(40),
        ir_revision: 11,
        validation_digest: "b".repeat(64),
        binding_sha256: "c".repeat(64),
        task_card_sha256: "d".repeat(64),
        resolved_command_digest: "e".repeat(64),
        toolchain_digest: "f".repeat(64),
        environment_digest: "1".repeat(64),
        gates: vec![ReusedGateV1 {
            name: "testFast".to_string(),
            exit_code: 0,
            source_event_id: "01M1ROOTGATEREUSE0000000000".to_string(),
            log_sha256: "2".repeat(64),
            log_bytes: 17,
        }],
    };
    let subject = RootGateSubjectV1 {
        subject_tree_sha: bundle.subject_tree_sha.clone(),
        ir_revision: bundle.ir_revision,
        validation_digest: bundle.validation_digest.clone(),
        binding_sha256: bundle.binding_sha256.clone(),
        task_card_sha256: bundle.task_card_sha256.clone(),
        resolved_command_digest: bundle.resolved_command_digest.clone(),
        toolchain_digest: bundle.toolchain_digest.clone(),
        environment_digest: bundle.environment_digest.clone(),
    };
    (bundle, subject)
}

#[test]
fn exact_green_collect_identity_is_reused() {
    let (bundle, subject) = fixture();
    assert!(matches!(
        plan_root_gate_reuse_v1(&bundle, &subject),
        RootGateReuseDecisionV1::Reuse(ref gates) if gates == &bundle.gates
    ));
}

#[test]
fn every_identity_drift_executes_instead_of_reusing() {
    let (bundle, subject) = fixture();
    let mut variants = Vec::new();
    let mut value = subject.clone();
    value.subject_tree_sha = "9".repeat(40);
    variants.push(value);
    let mut value = subject.clone();
    value.validation_digest = "9".repeat(64);
    variants.push(value);
    let mut value = subject.clone();
    value.resolved_command_digest = "9".repeat(64);
    variants.push(value);
    let mut value = subject.clone();
    value.toolchain_digest = "9".repeat(64);
    variants.push(value);
    let mut value = subject;
    value.environment_digest = "9".repeat(64);
    variants.push(value);
    for variant in variants {
        assert!(matches!(
            plan_root_gate_reuse_v1(&bundle, &variant),
            RootGateReuseDecisionV1::Execute { .. }
        ));
    }
}

#[test]
fn red_source_gate_is_never_reused() {
    let (mut bundle, subject) = fixture();
    bundle.gates[0].exit_code = 1;
    assert!(matches!(
        plan_root_gate_reuse_v1(&bundle, &subject),
        RootGateReuseDecisionV1::Execute { .. }
    ));
}
