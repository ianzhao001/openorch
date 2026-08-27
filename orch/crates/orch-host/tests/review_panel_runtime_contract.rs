//! 红种子契约 · B310 · 动态 panel、交付恢复与 dormant policy 激活。
//!
//! 首红必须是 compile `E0432`：本文件导入的 V1 公开合同在 B310 前不存在。
//! 复杂 git/进程/归档回归放本卡 non-seed 测试；本文件只冻结最小状态机与生产接线下界。

use orch_host::wake::{
    evaluate_review_panel_v1, ReviewPanelDecisionV1, ReviewPanelPolicyV1, ReviewPanelSeatStateV1,
    ReviewPanelSeatV1, REVIEW_PANEL_RUNTIME_CONTRACT_V1,
};

const WAKE: &str = include_str!("../src/wake.rs");
const VERIFY: &str = include_str!("../src/verify.rs");
const LEDGER: &str = include_str!("../src/ledger.rs");
const CORE: &str = include_str!("../../orch-core/src/lib.rs");
const HOOK: &str = include_str!("../../../../.githooks/reference-transaction");
const GUIDE: &str = include_str!("../../../docs/AI-MECHANICAL-GUIDE.md");

fn policy() -> ReviewPanelPolicyV1 {
    ReviewPanelPolicyV1 {
        minimum_passes: 2,
        require_primary_pass: true,
        maximum_business_retries: 1,
        nongate_substitutes_secondary_only: true,
    }
}

fn seat(id: &str, role: &str, primary: bool, state: ReviewPanelSeatStateV1) -> ReviewPanelSeatV1 {
    ReviewPanelSeatV1 {
        seat_id: id.to_string(),
        generation: 1,
        role: role.to_string(),
        agent: format!("executor-{id}"),
        primary_lineage: primary,
        retry_eligible: true,
        state,
    }
}

#[test]
fn the_public_contract_anchor_is_version_one() {
    assert_eq!(REVIEW_PANEL_RUNTIME_CONTRACT_V1, 1);
}

#[test]
fn two_passes_need_one_primary_lineage() {
    let seats = vec![
        seat("oc", "primary", true, ReviewPanelSeatStateV1::Pass),
        seat("pi", "secondary", false, ReviewPanelSeatStateV1::Pass),
        seat("agy", "nongate", false, ReviewPanelSeatStateV1::SystemInvalid),
    ];
    assert_eq!(
        evaluate_review_panel_v1(&policy(), &seats, 0, 0),
        ReviewPanelDecisionV1::Pass
    );

    let no_primary = vec![
        seat("pi", "secondary", false, ReviewPanelSeatStateV1::Pass),
        seat("agy", "nongate", false, ReviewPanelSeatStateV1::Pass),
        seat("dsh", "secondary", false, ReviewPanelSeatStateV1::SystemInvalid),
    ];
    assert_ne!(
        evaluate_review_panel_v1(&policy(), &no_primary, 0, 0),
        ReviewPanelDecisionV1::Pass
    );
}

#[test]
fn formal_findings_are_monotonic_vetoes() {
    let seats = vec![
        seat("oc", "primary", true, ReviewPanelSeatStateV1::Pass),
        seat("pi", "secondary", false, ReviewPanelSeatStateV1::Pass),
        seat("dsh", "primary", true, ReviewPanelSeatStateV1::Fail),
    ];
    assert_eq!(
        evaluate_review_panel_v1(&policy(), &seats, 0, 0),
        ReviewPanelDecisionV1::Veto
    );
}

#[test]
fn system_failures_do_not_consume_the_one_business_retry() {
    let seats = vec![
        seat("oc", "primary", true, ReviewPanelSeatStateV1::SystemInvalid),
        seat("pi", "secondary", false, ReviewPanelSeatStateV1::BusinessInvalid),
        seat("agy", "nongate", false, ReviewPanelSeatStateV1::Pending),
    ];
    assert!(matches!(
        evaluate_review_panel_v1(&policy(), &seats, 0, 1),
        ReviewPanelDecisionV1::Retry { ref seat_id } if seat_id == "pi"
    ));
}

#[test]
fn every_new_event_and_public_command_is_wired() {
    for kind in [
        "RuntimePolicyActivated",
        "RuntimePolicyDeactivated",
        "ReviewSpoolPromoted",
        "ReviewPanelSelected",
        "ReviewSeatRouted",
        "ReviewSeatTerminated",
        "ReviewPanelClosed",
        "GateLaneEscalated",
        "GateReused",
        "GateReuseMiss",
    ] {
        assert!(CORE.contains(kind), "known-event catalog missing {kind}");
        assert!(VERIFY.contains(kind), "verification surface missing {kind}");
    }
    for marker in ["review panel", "runtime-policy"] {
        assert!(GUIDE.contains(marker), "guide missing {marker}");
    }
    assert!(WAKE.contains("policyBaseSha"));
    assert!(LEDGER.contains("ReviewPanelSelected"));
    assert!(HOOK.contains("RuntimePolicyActivated") || HOOK.contains("ReviewPanelSelected"));
}

