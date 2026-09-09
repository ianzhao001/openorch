//! Pure recovery-budget decisions for the retained schema-3 seal path.

use orch_host::close::{decide_gate_reuse_miss_v1, GateReuseMissDecisionV1};

const CLOSE_SOURCE: &str = include_str!("../src/close.rs");
const GUIDE: &str = include_str!("../../../docs/AI-MECHANICAL-GUIDE.md");

#[test]
fn ref_cas_contention_never_consumes_an_input_miss() {
    assert_eq!(
        decide_gate_reuse_miss_v1(0, false, true),
        GateReuseMissDecisionV1::RetryRefCas { miss_count: 0 }
    );
    assert_eq!(
        decide_gate_reuse_miss_v1(1, false, true),
        GateReuseMissDecisionV1::RetryRefCas { miss_count: 1 }
    );
}

#[test]
fn repeated_invocation_without_drift_is_stable() {
    assert_eq!(
        decide_gate_reuse_miss_v1(1, false, false),
        GateReuseMissDecisionV1::Stable { miss_count: 1 }
    );
}

#[test]
fn the_second_real_input_drift_requires_an_approved_successor() {
    assert_eq!(
        decide_gate_reuse_miss_v1(0, true, false),
        GateReuseMissDecisionV1::RetryInput { miss_count: 1 }
    );
    assert_eq!(
        decide_gate_reuse_miss_v1(1, true, false),
        GateReuseMissDecisionV1::ApprovedReattempt { miss_count: 2 }
    );
    assert_eq!(
        decide_gate_reuse_miss_v1(u32::MAX, true, false),
        GateReuseMissDecisionV1::ApprovedReattempt { miss_count: 2 }
    );
}

#[test]
fn public_contract_and_guide_keep_the_final_tree_explanations() {
    for marker in [
        "/// Version of the seal-time final-tree proof contract.",
        "/// Immutable proof subject captured after the seal lifecycle",
        "/// Decision made after the real no-ff merge exposes its actual tree.",
        "/// Attempt-scoped response to a seal input recheck.",
        "/// Compare complete Git object identities",
        "/// Apply the V1 miss budget",
    ] {
        assert!(
            CLOSE_SOURCE.contains(marker),
            "missing rustdoc marker {marker}"
        );
    }
    for marker in [
        "final-tree-v1",
        "GateReused(trial→postmerge)",
        "approved-reattempt",
        "carryForward",
        "70% SLO",
    ] {
        assert!(GUIDE.contains(marker), "missing guide marker {marker}");
    }
}
