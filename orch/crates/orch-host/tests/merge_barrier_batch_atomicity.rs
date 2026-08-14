use std::fs;

use orch_host::ledger;

fn root() -> std::path::PathBuf {
    let root = orch_host::util::test_scratch_dir("b130-barrier-batch");
    fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "rT\n").unwrap();
    root
}

fn started() -> orch_core::EventRecord {
    ledger::event(
        "MergeStarted",
        "runtime:orch",
        Some("B1"),
        Some("rT"),
        serde_json::json!({
            "attemptId": "B1-A0001", "attemptNo": 1,
            "headSha": "1".repeat(40), "mainHeadSha": "2".repeat(40),
            "collectCompletedEventId": "collect", "verdictEventId": "verdict"
        }),
    )
}

#[test]
fn lifecycle_plus_unrelated_batch_is_rejected_atomically() {
    let root = root();
    let unrelated = ledger::event(
        "PlannerTurnCompleted",
        "runtime:orch",
        None,
        Some("rT"),
        serde_json::json!({}),
    );
    assert!(ledger::append(&root, "rT", &[started(), unrelated]).is_err());
    assert_eq!(
        fs::read(root.join("coordination/rounds/rT/events.jsonl")).unwrap_or_default(),
        Vec::<u8>::new()
    );
}

#[test]
fn append_checked_does_not_run_decide_under_an_active_barrier() {
    let source = include_str!("../src/ledger.rs");
    let guard = source
        .find("ordinary append_checked before decide")
        .expect("active barrier guard");
    let decide = source.find("let new_events = decide").expect("decide call");
    assert!(guard < decide);
}
