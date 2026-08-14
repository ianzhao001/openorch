use std::fs;

use orch_host::ledger;

fn root() -> std::path::PathBuf {
    let root = orch_host::util::test_scratch_dir("b130-barrier-lifecycle");
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
            "attemptId": "B1-A0001",
            "attemptNo": 1,
            "headSha": "1".repeat(40),
            "mainHeadSha": "2".repeat(40),
            "collectCompletedEventId": "collect",
            "verdictEventId": "verdict",
        }),
    )
}

#[test]
fn ordinary_append_cannot_arm_a_durable_merge_barrier() {
    let root = root();
    let before = fs::read(root.join("coordination/rounds/rT/events.jsonl")).unwrap_or_default();
    let error = ledger::append(&root, "rT", &[started()]).unwrap_err();
    assert!(format!("{error:#}").contains("无 capability"));
    let after = fs::read(root.join("coordination/rounds/rT/events.jsonl")).unwrap_or_default();
    assert_eq!(after, before);
}

#[test]
fn noncanonical_extra_never_counts_as_lifecycle_authority() {
    let root = root();
    let mut forged = started();
    forged
        .extra
        .insert("forged".into(), serde_json::json!(true));
    ledger::append(&root, "rT", &[forged]).unwrap();
    orch_host::close::with_protocol_transition(&root, "ordinary transition", || Ok(())).unwrap();
}
