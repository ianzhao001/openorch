//! B323 read-only coverage for the legacy complete-seal cleanup suffix.
//!
//! The old target drove a schema-1/2 seal writer after `TaskRecorded`. That
//! writer is retired. Historical suffix recognition remains a pure audit, and
//! the public seal seam must reject the legacy generation byte-for-byte.

#[allow(dead_code)]
mod support_boundary;
#[allow(dead_code)]
mod support_legacy_plan;

use orch_host::{ledger, verify};
use support_boundary::LegacyWriterFixture;

fn cleanup_terminal(round: &str, task: &str, agent: &str, wake_id: &str) -> orch_core::EventRecord {
    ledger::event(
        "ManagedWakeTerminated",
        "runtime:orch",
        Some(task),
        Some(round),
        serde_json::json!({
            "wakeId": wake_id,
            "agent": agent,
            "completionReason": "natural-exit",
            "terminalSeen": false,
            "exitedNaturally": true,
            "hardDeadlineReached": false,
            "cancelRequestId": null,
            "signals": [],
            "managedScopeTerminated": true,
            "logBytesRead": 1,
            "outcomeClass": "TruncatedNoTerminal",
        }),
    )
}

#[test]
fn legacy_seal_replay_is_rejected_without_effects() {
    let fixture = LegacyWriterFixture::new("complete-seal");
    let before = fixture.snapshot();
    let head = fixture.head();
    let error = orch_host::close::run_seal(fixture.path(), "B313", "B313-A0001", &head)
        .unwrap_err()
        .to_string();
    assert!(error.contains("schema 3"), "{error}");
    fixture.assert_unchanged(&before);
}

#[test]
fn committed_cleanup_suffix_remains_a_pure_multi_batch_audit() {
    let round = "r82";
    let task = "B313";
    let agents = ["executor-claw", "executor-opencode"];
    let wakes = [
        "01a0b313-1111-4222-8333-444455556666",
        "01a0b313-7777-4888-8999-aaaabbbbcccc",
    ];
    let mut prior = agents
        .iter()
        .zip(wakes)
        .map(|(agent, wake_id)| {
            ledger::event(
                "WakeIssued",
                "runtime:orch",
                Some(task),
                Some(round),
                serde_json::json!({"wakeId": wake_id, "agent": agent}),
            )
        })
        .collect::<Vec<_>>();
    prior.push(ledger::event(
        "TaskRecorded",
        "runtime:orch",
        Some(task),
        Some(round),
        serde_json::json!({"postMergeGates": "all-green"}),
    ));
    let suffix = agents
        .iter()
        .zip(wakes)
        .map(|(agent, wake_id)| cleanup_terminal(round, task, agent, wake_id))
        .collect::<Vec<_>>();

    assert!(
        !verify::canonical_complete_seal_managed_terminal_suffix_v1(&prior, &suffix, round, task,)
            .unwrap(),
        "two terminal batches cannot masquerade as one adjacent batch"
    );
    assert!(
        verify::canonical_complete_seal_managed_terminal_suffixes_v1(&prior, &suffix, round, task,)
            .unwrap(),
        "the read-only multi-batch classifier must retain committed history"
    );
}

#[test]
fn cleanup_suffix_with_an_unrelated_event_is_not_authorized() {
    let round = "r82";
    let task = "B313";
    let wake_id = "01a0b313-1111-4222-8333-444455556666";
    let prior = vec![
        ledger::event(
            "WakeIssued",
            "runtime:orch",
            Some(task),
            Some(round),
            serde_json::json!({"wakeId": wake_id, "agent": "executor-claw"}),
        ),
        ledger::event(
            "TaskRecorded",
            "runtime:orch",
            Some(task),
            Some(round),
            serde_json::json!({"postMergeGates": "all-green"}),
        ),
    ];
    let suffix = vec![
        cleanup_terminal(round, task, "executor-claw", wake_id),
        ledger::event(
            "EscalationRaised",
            "runtime:orch",
            Some(task),
            Some(round),
            serde_json::json!({"stage": "unrelated"}),
        ),
    ];
    assert!(!verify::canonical_complete_seal_managed_terminal_suffix_v1(
        &prior, &suffix, round, task,
    )
    .unwrap());
}
