//! B323 legacy boundary: Panel evaluation remains read-only while writers retire.

use orch_host::legacy::{
    evaluate_review_panel_v1, ReviewPanelDecisionV1, ReviewPanelPolicyV1, ReviewPanelSeatStateV1,
    ReviewPanelSeatV1, REVIEW_PANEL_RUNTIME_CONTRACT_V1,
};
use orch_host::{ledger, plan, wake};
use std::fs;

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
fn historical_event_codec_remains_while_live_writers_fail_closed() {
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
    for retired in [
        "review Panel select production action 已退役",
        "review Panel retry production action 已退役",
        "review Panel backfill production action 已退役",
        "automatic review fallback transition 已退役",
    ] {
        assert!(WAKE.contains(retired), "missing retired boundary: {retired}");
    }
    assert!(!GUIDE.contains("orch-guide-command:review panel"));
    assert!(!GUIDE.contains("orch-guide-command:runtime-policy"));
    assert!(LEDGER.contains("ReviewPanelSelected"));
    assert!(LEDGER.contains("只读 legacy 历史事实"));
    assert!(HOOK.contains("RuntimePolicyActivated") || HOOK.contains("ReviewPanelSelected"));
}

#[test]
fn schema3_acceptance_has_no_evidence_provider_smartclaw_or_primary_magic() {
    let start = VERIFY
        .find("fn required_file_bindings(")
        .expect("schema-3 verdict binding entry remains present");
    let scope = &VERIFY[start..];
    let end = scope
        .find("let mode = review_contract_mode_for_attempt(")
        .expect("legacy review-mode branch follows the schema-3 early return");
    let schema3 = &scope[..end];

    assert_eq!(
        schema3
            .matches("validate_generic_review_facts_for_verdict(")
            .count(),
        1,
        "schema 3 must consume exactly the generic review validator"
    );
    for forbidden in [
        "enforce_signed_smartclaw_fixed_primary_pass_v1(",
        "SMARTCLAW_FIXED_PRIMARY_EVIDENCE_V1",
        "required_evidence.iter().any",
    ] {
        assert!(
            !schema3.contains(forbidden),
            "schema-3 acceptance revived legacy magic: {forbidden}"
        );
    }
    let lowercase = schema3.to_ascii_lowercase();
    for forbidden in ["provider", "smartclaw", "primary"] {
        assert!(
            !lowercase.contains(forbidden),
            "schema-3 acceptance contains participant magic: {forbidden}"
        );
    }
}

#[test]
fn production_policy_actions_and_direct_legacy_append_are_side_effect_free() {
    let root = orch_host::util::test_scratch_dir("b323-retired-policy-writers");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("coordination/rounds/r323")).unwrap();
    fs::create_dir_all(root.join("coordination/runtime/ledger-wal")).unwrap();
    let ledger_path = root.join("coordination/rounds/r323/events.jsonl");
    let wal_path = root.join("coordination/runtime/ledger-wal/r323.jsonl");
    fs::write(&ledger_path, b"").unwrap();
    fs::write(&wal_path, b"").unwrap();

    assert!(format!(
        "{:#}",
        plan::activate_runtime_policy(&root, "review-pool-v1").unwrap_err()
    )
    .contains("production transition 已退役"));
    assert!(format!(
        "{:#}",
        plan::deactivate_runtime_policy(&root, "review-pool-v1", "retired").unwrap_err()
    )
    .contains("production transition 已退役"));

    for kind in [
        "RuntimePolicyActivated",
        "RuntimePolicyDeactivated",
        "ReviewSpoolPromoted",
        "ReviewPanelSelected",
        "ReviewSeatRouted",
        "ReviewSeatTerminated",
        "ReviewPanelClosed",
        "ReviewFallbackSelected",
        "ReviewSeatSubstituted",
        "NongateReviewDelivered",
    ] {
        let retired = ledger::event(
            kind,
            "runtime:orch",
            Some("B323"),
            Some("r323"),
            serde_json::json!({}),
        );
        let error = ledger::append(&root, "r323", &[retired]).unwrap_err();
        assert!(
            format!("{error:#}").contains("只读 legacy 历史事实"),
            "unexpected {kind} rejection: {error:#}"
        );
        assert_eq!(fs::read(&ledger_path).unwrap(), b"", "kind={kind}");
        assert_eq!(fs::read(&wal_path).unwrap(), b"", "kind={kind}");
    }

    for kind in ["ReviewRequested", "ReviewDelivered"] {
        for role in [None, Some("primary"), Some("secondary"), Some("nongate")] {
            let payload = role.map_or_else(
                || serde_json::json!({}),
                |role| serde_json::json!({"role": role}),
            );
            let retired = ledger::event(
                kind,
                "runtime:orch",
                Some("B323"),
                Some("r323"),
                payload,
            );
            let error = ledger::append(&root, "r323", &[retired]).unwrap_err();
            assert!(
                format!("{error:#}").contains("role=review"),
                "unexpected {kind}/{role:?} rejection: {error:#}"
            );
            assert_eq!(fs::read(&ledger_path).unwrap(), b"", "{kind}/{role:?}");
            assert_eq!(fs::read(&wal_path).unwrap(), b"", "{kind}/{role:?}");
        }
    }

    let checked_retired = ledger::event(
        "ReviewFallbackSelected",
        "runtime:orch",
        Some("B323"),
        Some("r323"),
        serde_json::json!({}),
    );
    let error = ledger::append_checked(&root, "r323", |_| Ok(vec![checked_retired])).unwrap_err();
    assert!(format!("{error:#}").contains("只读 legacy 历史事实"));
    assert_eq!(fs::read(&ledger_path).unwrap(), b"");
    assert_eq!(fs::read(&wal_path).unwrap(), b"");

    let generic = ["ReviewRequested", "ReviewDelivered"].map(|kind| {
        ledger::event(
            kind,
            "runtime:orch",
            Some("B323"),
            Some("r323"),
            serde_json::json!({
                "attemptId": "B323-A0004",
                "role": "review",
                "agent": "alpha",
                "harness": "alpha",
                "wakeId": "wake-alpha"
            }),
        )
    });
    let markerless_error = ledger::append(&root, "r323", &generic).unwrap_err();
    assert!(
        format!("{markerless_error:#}").contains("open schema 3"),
        "unexpected markerless generic review rejection: {markerless_error:#}"
    );
    assert_eq!(fs::read(&ledger_path).unwrap(), b"");
    assert_eq!(fs::read(&wal_path).unwrap(), b"");
    ledger::append(
        &root,
        "r323",
        &[ledger::event(
            "RoundOpened",
            "runtime:orch",
            None,
            Some("r323"),
            serde_json::json!({"contractSchemaVersion": 3}),
        )],
    )
    .unwrap();
    ledger::append(&root, "r323", &generic).unwrap();
    let ledger_bytes = fs::read(&ledger_path).unwrap();
    assert!(!ledger_bytes.is_empty());
    assert_eq!(fs::read(&wal_path).unwrap(), ledger_bytes);
    ledger::append(
        &root,
        "r323",
        &[ledger::event(
            "RoundClosed",
            "runtime:orch",
            None,
            Some("r323"),
            serde_json::json!({}),
        )],
    )
    .unwrap();
    let closed_ledger = fs::read(&ledger_path).unwrap();
    let closed_wal = fs::read(&wal_path).unwrap();
    let closed_error = ledger::append(&root, "r323", &generic).unwrap_err();
    assert!(
        format!("{closed_error:#}").contains("open schema 3"),
        "unexpected closed generic review rejection: {closed_error:#}"
    );
    assert_eq!(fs::read(&ledger_path).unwrap(), closed_ledger);
    assert_eq!(fs::read(&wal_path).unwrap(), closed_wal);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn typed_continuation_rejects_legacy_roles_before_no_wake_side_effects() {
    let root = orch_host::util::test_scratch_dir("b323-retired-continuation-role");
    let _ = fs::remove_dir_all(&root);
    for role in ["primary", "secondary", "nongate"] {
        let continuation = format!("review:r323:B323:B323-A0004:{role}:alpha");
        let error = wake::dispatch_wake_for_continuation(
            &root,
            "alpha",
            "r323",
            &continuation,
            "must not poke",
            true,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("legacy formal/nongate continuation wake 已退役"),
            "unexpected {role} rejection: {error:#}"
        );
        assert!(!root.exists(), "legacy {role} touched the filesystem");
    }

    for continuation in [
        "review:r323:B323:B323-A0004:review:alpha",
        "implementation:r323:B323:B323-A0004:alpha",
    ] {
        let error = wake::dispatch_wake_for_continuation(
            &root,
            "alpha",
            "r323",
            continuation,
            "allowed identity reaches ordinary preflight",
            true,
        )
        .unwrap_err();
        assert!(
            !format!("{error:#}").contains("legacy formal/nongate continuation wake 已退役"),
            "live identity was misclassified: {continuation}: {error:#}"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
