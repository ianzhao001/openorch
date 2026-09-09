//! B323: retain driver receipt/parser truth while retiring legacy review bridges.

use orch_host::wake::{BackendReceiptKind, R82_CHANNEL_CLOSURE_CONTRACT_V1};

const WAKE: &str = include_str!("../src/wake.rs");
const LEDGER: &str = include_str!("../src/ledger.rs");
const DSH_WRAPPER: &str = include_str!("../../../scripts/wake-dsh-stream.sh");
const GUIDE: &str = include_str!("../../../docs/AI-MECHANICAL-GUIDE.md");

#[test]
fn historical_channel_anchor_and_driver_receipt_kind_remain_stable() {
    assert_eq!(R82_CHANNEL_CLOSURE_CONTRACT_V1, 1);
    assert_eq!(BackendReceiptKind::Dsh.as_str(), "dsh");
    assert!(DSH_WRAPPER.contains("dsh.session"));
    assert!(DSH_WRAPPER.contains("request/header"));
}

#[test]
fn panel_and_dsh_legacy_writer_bridges_are_physically_absent() {
    for retired in [
        "fn smartclaw_session_terminal_from_payloads_v1",
        "fn smartclaw_panel_artifact_v1",
        "fn dsh_legacy_spool_promotion_v1",
        "fn reconcile_dsh_legacy_spool_promotions_filtered_v1",
        "fn panel_route_awaits_spool_transition_v1",
    ] {
        assert!(!WAKE.contains(retired), "legacy bridge survived: {retired}");
    }
    assert!(WAKE.contains("fn parse_smartclaw_payload_terminal_v1"));
    assert!(WAKE.contains("legacy formal-review attach 已退役"));
}

#[test]
fn automatic_review_policy_callers_and_append_bypass_are_closed() {
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for name in ["serve.rs", "runloop.rs"] {
        assert!(!source.join(name).exists(), "automatic review caller module survived");
    }
    assert!(LEDGER.contains("是只读 legacy 历史事实，拒绝追加新的 production event"));
}

#[test]
fn guide_describes_generic_review_and_legacy_audit_only() {
    assert!(GUIDE.contains("generic review"));
    assert!(!GUIDE.contains("orch-guide-command:review panel"));
    assert!(!GUIDE.contains("orch-guide-command:runtime-policy"));
}
