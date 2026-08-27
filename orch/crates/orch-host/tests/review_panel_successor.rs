use orch_core::EventRecord;
use orch_host::{attempt::plan_next_attempt, ledger};

fn panel_exhaustion_events() -> Vec<EventRecord> {
    let dispatch = ledger::event(
        "DispatchIssued",
        "runtime:orch",
        Some("B306"),
        Some("r81"),
        serde_json::json!({
            "attemptId": "B306-A0001",
            "agent": "executor-desktop",
            "baseSha": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        }),
    );
    let closed = ledger::runtime_event_v1(
        "r81",
        Some("B306"),
        ledger::RuntimeEventPayloadV1::ReviewPanelClosed(ledger::ReviewPanelClosedPayloadV1 {
            schema_version: 1,
            panel_id: "panel-one".to_string(),
            attempt_id: "B306-A0001".to_string(),
            attempt_no: 1,
            reviewed_head: "a".repeat(40),
            policy_base_sha: "b".repeat(40),
            outcome: "pool-exhausted".to_string(),
            reason: "signed pool cannot reach quorum or primary lineage".to_string(),
            terminal_seat_count: 3,
            pass_count: 0,
            primary_pass: false,
        }),
    )
    .unwrap();
    let blocked = ledger::event(
        "AttemptBlocked",
        "runtime:orch",
        Some("B306"),
        Some("r81"),
        serde_json::json!({
            "attemptId": "B306-A0001",
            "attemptNo": 1,
            "agent": "runtime-review-panel",
            "stage": "review-panel-exhausted",
            "reason": "signed pool cannot reach quorum or primary lineage",
            "panelId": "panel-one",
        }),
    );
    vec![dispatch, closed, blocked]
}

#[test]
fn panel_exhaustion_preserves_implementer_for_successor_and_rejects_near_misses() {
    let events = panel_exhaustion_events();
    let main = "c".repeat(40);
    let next = plan_next_attempt(&events, "B306", "executor-desktop", &main, "r81").unwrap();
    assert_eq!(
        next.previous_agent,
        Some("executor-desktop".to_string()),
    );
    assert_eq!(next.attempt.attempt_id, "B306-A0002");
    assert_eq!(next.base_sha, "b".repeat(40));
    assert!(!next.is_reassignment);

    let mut wrong_stage = events.clone();
    wrong_stage[2].payload.as_mut().unwrap()["stage"] = serde_json::json!("other");
    assert!(plan_next_attempt(&wrong_stage, "B306", "executor-desktop", &main, "r81").is_err());

    let mut wrong_panel = events.clone();
    wrong_panel[2].payload.as_mut().unwrap()["panelId"] = serde_json::json!("panel-two");
    assert!(plan_next_attempt(&wrong_panel, "B306", "executor-desktop", &main, "r81").is_err());

    let missing_closed = vec![events[0].clone(), events[2].clone()];
    assert!(
        plan_next_attempt(
            &missing_closed,
            "B306",
            "executor-desktop",
            &main,
            "r81",
        )
        .is_err()
    );
}
