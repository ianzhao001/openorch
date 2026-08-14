//! B179 seed — backend receipt + continuation identity fence.
//!
//! Expected red: compile. The public receipt/fence API does not exist yet.
//!
//! M1: treat wrapper spawn/EOF as backend acceptance
//!     => `empty_eof_is_not_a_backend_receipt` red.
//! M2: accept any SmartClaw sessionId instead of the runtime-owned expected id
//!     => `smartclaw_receipt_is_bound_to_the_requested_session` red.
//! M3: let an unscoped ordinary wake bypass an active continuation
//!     => `active_continuations_reject_unscoped_wakes_even_after_wrapper_exit` red.
//! M4: release the fence because the wrapper exited, or because receipt is absent
//!     => the same case red for both accepted and unconfirmed active wakes.
//! M5: spawn again for an exact duplicate continuation/message
//!     => `exact_duplicates_are_idempotent_but_changed_requests_are_rejected` red.
//! M6: inherit the legacy probe file length as the per-attempt offset
//!     => `backend_receipt_window_always_starts_at_zero` red.
//! M7: keep the old generic `thread.started` / `turn.started` parser on the
//!     OpenCode production path
//!     => `live_opencode_frames_are_provider_bound_receipts` red.
//! M8: treat a timeout `ActionRejected` as proof that the backend never accepted,
//!     or retry the same review by appending another lifecycle
//!     => `late_receipt_reconcile_is_action_scoped_and_append_only` red.

use orch_host::wake::{
    backend_receipt_from_log, backend_receipt_reconcile_decision, identity_fence_decision,
    ActiveWake, BackendReceiptExpectation, BackendReceiptKind, BackendReceiptLedgerState,
    BackendReceiptReconcileDecision, WakeFenceDecision, WakeIntent,
};

const AGENT: &str = "executor-claw";
const CONTINUATION: &str = "review:r58:B174:B174-A0002:primary:executor-claw";
const WAKE: &str = "019faf68-6699-46e3-8d7c-9e6c88ab6299";
const SESSION: &str = "orch-wake-019faf68-6699-46e3-8d7c-9e6c88ab6299";
const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER_DIGEST: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn expectation() -> BackendReceiptExpectation {
    BackendReceiptExpectation::new(
        BackendReceiptKind::SmartClaw,
        WAKE,
        CONTINUATION,
        DIGEST,
        Some(SESSION),
    )
    .unwrap()
}

fn active(backend_accepted: bool, wrapper_exited: bool) -> ActiveWake {
    ActiveWake::new(
        AGENT,
        CONTINUATION,
        WAKE,
        DIGEST,
        backend_accepted,
        wrapper_exited,
    )
    .unwrap()
}

#[test]
fn smartclaw_receipt_is_bound_to_the_requested_session() {
    let log = format!("{{\"type\":\"text\",\"sessionId\":\"{SESSION}\",\"text\":\"received\"}}\n");
    let receipt = backend_receipt_from_log(&expectation(), log.as_bytes(), false)
        .unwrap()
        .expect("matching backend response must produce a receipt");
    assert_eq!(receipt.kind, BackendReceiptKind::SmartClaw);
    assert_eq!(receipt.wake_id, WAKE);
    assert_eq!(receipt.continuation_id, CONTINUATION);
    assert_eq!(receipt.message_sha256, DIGEST);
    assert_eq!(receipt.request_session_id.as_deref(), Some(SESSION));
    assert_eq!(receipt.observed_session_id.as_deref(), Some(SESSION));

    let wrong = b"{\"type\":\"text\",\"sessionId\":\"orch-wake-other\",\"text\":\"received\"}\n";
    assert!(
        backend_receipt_from_log(&expectation(), wrong, true).is_err(),
        "a response for another backend session must fail closed"
    );
}

#[test]
fn empty_eof_is_not_a_backend_receipt() {
    assert!(
        backend_receipt_from_log(&expectation(), b"", false)
            .unwrap()
            .is_none(),
        "an open empty stream is pending, never accepted"
    );
    assert!(
        backend_receipt_from_log(&expectation(), b"", true).is_err(),
        "0-byte EOF is the r58 phantom-wake shape and must be loud"
    );
    let provider_error = format!(
        "{{\"type\":\"error\",\"sessionId\":\"{SESSION}\",\"message\":\"Session is already running\"}}\n"
    );
    assert!(
        backend_receipt_from_log(&expectation(), provider_error.as_bytes(), true).is_err(),
        "an explicit provider error is not a receipt"
    );
}

#[test]
fn managed_cli_receipts_are_normalized_without_process_or_database_lookups() {
    let codex = b"{\"type\":\"thread.started\",\"thread_id\":\"thread-1\"}\n";
    let receipt = backend_receipt_from_log(
        &BackendReceiptExpectation::new(
            BackendReceiptKind::Codex,
            "wake-c",
            "implementation:r58:B174:B174-A0002:executor-desktop",
            DIGEST,
            None,
        )
        .unwrap(),
        codex,
        false,
    )
    .unwrap()
    .unwrap();
    assert_eq!(receipt.kind, BackendReceiptKind::Codex);
    assert_eq!(receipt.observed_session_id.as_deref(), Some("thread-1"));

    let opencode =
        b"{\"type\":\"step_start\",\"sessionID\":\"ses_1\",\"part\":{\"type\":\"step-start\"}}\n";
    let receipt = backend_receipt_from_log(
        &BackendReceiptExpectation::new(
            BackendReceiptKind::OpenCode,
            "wake-o",
            "review:r58:B174:B174-A0002:secondary:executor-opencode",
            DIGEST,
            None,
        )
        .unwrap(),
        opencode,
        false,
    )
    .unwrap()
    .unwrap();
    assert_eq!(receipt.kind, BackendReceiptKind::OpenCode);
    assert_eq!(receipt.observed_session_id.as_deref(), Some("ses_1"));
}

#[test]
fn live_opencode_frames_are_provider_bound_receipts() {
    let expectation = BackendReceiptExpectation::new(
        BackendReceiptKind::OpenCode,
        "019fb019-f94f-4896-8b85-ac0597fbfdfa",
        "review:r58:B176:B176-A0002:secondary:executor-opencode",
        DIGEST,
        None,
    )
    .unwrap();
    for frame in [
        r#"{"type":"step_start","sessionID":"ses_live","part":{"type":"step-start"}}"#,
        r#"{"type":"step_finish","sessionID":"ses_live","part":{"type":"step-finish","reason":"tool-calls"}}"#,
        r#"{"type":"tool_use","sessionID":"ses_live","part":{"type":"tool","tool":"bash"}}"#,
    ] {
        let bytes = format!("{frame}\n");
        let receipt = backend_receipt_from_log(&expectation, bytes.as_bytes(), false)
            .unwrap()
            .expect("every observed OpenCode backend frame proves acceptance");
        assert_eq!(receipt.kind, BackendReceiptKind::OpenCode);
        assert_eq!(receipt.observed_session_id.as_deref(), Some("ses_live"));
        assert_eq!(receipt.probe_offset, 0);
    }

    let codex_shape = b"{\"type\":\"thread.started\",\"thread_id\":\"wrong-provider\"}\n";
    assert!(
        backend_receipt_from_log(&expectation, codex_shape, true).is_err(),
        "a session-bearing frame from the wrong configured provider must not cross-accept"
    );
}

#[test]
fn backend_receipt_window_always_starts_at_zero() {
    let log = format!("{{\"type\":\"tool_use\",\"sessionId\":\"{SESSION}\",\"tool\":\"Bash\"}}\n");
    let receipt = backend_receipt_from_log(&expectation(), log.as_bytes(), false)
        .unwrap()
        .unwrap();
    assert_eq!(
        receipt.probe_offset, 0,
        "a create_new per-attempt log has no legacy prefix"
    );
    assert_eq!(receipt.probe_end, log.len() as u64);
    let stale_legacy_offset = receipt.probe_end + 1_048_576;
    assert!(stale_legacy_offset > receipt.probe_end);
    assert_ne!(receipt.probe_offset, stale_legacy_offset);
}

#[test]
fn late_receipt_reconcile_is_action_scoped_and_append_only() {
    let opencode = b"{\"type\":\"step_start\",\"sessionID\":\"ses_late\",\"part\":{\"type\":\"step-start\"}}\n";
    let expectation = BackendReceiptExpectation::new(
        BackendReceiptKind::OpenCode,
        "wake-late",
        "review:r58:B176:B176-A0002:secondary:executor-opencode",
        DIGEST,
        None,
    )
    .unwrap();
    let receipt = backend_receipt_from_log(&expectation, opencode, false)
        .unwrap()
        .unwrap();

    // This is the live incident shape: WakeIssued + ReviewRequested are already
    // durable, the obsolete 300s parser appended ActionRejected, but the exact
    // per-attempt log proves that this same backend accepted the request.
    let rejected = BackendReceiptLedgerState::new(
        "wake-late",
        "review:r58:B176:B176-A0002:secondary:executor-opencode",
        DIGEST,
        1,
        1,
        0,
        true,
    )
    .unwrap();
    assert_eq!(
        backend_receipt_reconcile_decision(&rejected, &receipt).unwrap(),
        BackendReceiptReconcileDecision::AppendReceiptOnly {
            wake_id: "wake-late".to_string(),
        },
        "a timeout rejection is an observation, not a release or permission to duplicate the review"
    );

    let committed = BackendReceiptLedgerState::new(
        "wake-late",
        "review:r58:B176:B176-A0002:secondary:executor-opencode",
        DIGEST,
        1,
        1,
        1,
        true,
    )
    .unwrap();
    assert_eq!(
        backend_receipt_reconcile_decision(&committed, &receipt).unwrap(),
        BackendReceiptReconcileDecision::AlreadyCommitted {
            wake_id: "wake-late".to_string(),
        }
    );

    for invalid in [
        BackendReceiptLedgerState::new(
            "wake-late",
            "review:r58:B176:B176-A0002:secondary:executor-opencode",
            DIGEST,
            2,
            1,
            0,
            true,
        ),
        BackendReceiptLedgerState::new(
            "wake-late",
            "review:r58:B176:B176-A0002:secondary:executor-opencode",
            DIGEST,
            1,
            2,
            0,
            true,
        ),
    ] {
        assert!(
            invalid.is_err(),
            "duplicate wake/review lifecycle must fail closed"
        );
    }
}

#[test]
fn active_continuations_reject_unscoped_wakes_even_after_wrapper_exit() {
    for item in [active(true, true), active(false, true)] {
        let error = identity_fence_decision(&[item], AGENT, &WakeIntent::unscoped(), OTHER_DIGEST)
            .unwrap_err();
        assert!(
            error.contains(CONTINUATION),
            "the blocker must be named: {error}"
        );
    }
}

#[test]
fn exact_duplicates_are_idempotent_but_changed_requests_are_rejected() {
    let decision = identity_fence_decision(
        &[active(true, false)],
        AGENT,
        &WakeIntent::continuation(CONTINUATION).unwrap(),
        DIGEST,
    )
    .unwrap();
    assert_eq!(
        decision,
        WakeFenceDecision::Idempotent {
            wake_id: WAKE.to_string(),
            backend_accepted: true,
        }
    );

    assert!(identity_fence_decision(
        &[active(true, false)],
        AGENT,
        &WakeIntent::continuation(CONTINUATION).unwrap(),
        OTHER_DIGEST,
    )
    .is_err());
    assert!(identity_fence_decision(
        &[active(true, false)],
        AGENT,
        &WakeIntent::continuation("review:r58:B175:B175-A0002:primary:executor-claw").unwrap(),
        DIGEST,
    )
    .is_err());
}

#[test]
fn an_idle_agent_may_spawn_but_ambiguous_multiple_owners_fail_closed() {
    assert_eq!(
        identity_fence_decision(&[], AGENT, &WakeIntent::unscoped(), DIGEST).unwrap(),
        WakeFenceDecision::Spawn
    );
    let mut other = active(true, false);
    other.wake_id = "wake-duplicate".to_string();
    assert!(identity_fence_decision(
        &[active(true, false), other],
        AGENT,
        &WakeIntent::continuation(CONTINUATION).unwrap(),
        DIGEST,
    )
    .is_err());
}
