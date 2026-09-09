//! B53 retained historical wake accounting; automatic planner launch/retry retired.
use orch_host::{budget, ledger};

#[test]
fn planner_wake_counts_once_and_stops_at_max() {
    let events = vec![
        ledger::event(
            "DispatchIssued",
            "runtime:orch",
            Some("B53"),
            Some("r28"),
            serde_json::json!({}),
        ),
        ledger::event(
            "NudgeIssued",
            "runtime:orch",
            Some("B53"),
            Some("r28"),
            serde_json::json!({}),
        ),
        ledger::event(
            "ResumeIssued",
            "runtime:orch",
            Some("B53"),
            Some("r28"),
            serde_json::json!({}),
        ),
        ledger::event(
            "InjectionIssued",
            "runtime:orch",
            None,
            Some("r28"),
            serde_json::json!({
                "reasons": ["new_instruction", "task_failed", "all_recorded", "agent_down"]
            }),
        ),
    ];
    assert_eq!(budget::count_model_wakes(&events), 4);
    assert!(budget::model_wake_permitted(3, Some(4)));
    assert!(!budget::model_wake_permitted(4, Some(4)));
    assert!(budget::model_wake_permitted(999, None));
}
