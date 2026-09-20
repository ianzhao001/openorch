//! B330 evolves the recorded B285 target: writers are retired; historical
//! digest continuity, signed-plan reads and captured wake pins remain checked.
mod support_legacy_plan;
mod audited_agent_pin_amendment_support;
use std::{fs, path::{Path, PathBuf}};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().parent().unwrap().to_path_buf()
}

#[test]
fn the_readonly_validation_folds_amendments() {
    let source=fs::read_to_string(repo().join("orch/crates/orch-host/src/plan.rs")).unwrap();
    assert!(source.contains("expected_registry_digest_with_amendments"));
    let root=audited_agent_pin_amendment_support::fixture_root("b330-historical-reader");
    let ledger=root.join("coordination/rounds/r9999/events.jsonl");
    let before=fs::read(&ledger).unwrap();let registry=fs::read(root.join("coordination/agents.yaml")).unwrap();
    let events=orch_core::read_ledger(&ledger).unwrap();
    orch_host::plan::require_active_round_ir(&root,"r9999",&events.events).unwrap();
    assert_eq!(fs::read(ledger).unwrap(),before);
    assert_eq!(fs::read(root.join("coordination/agents.yaml")).unwrap(),registry);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn an_amendment_never_signs_the_plan_itself() {
    let source=fs::read_to_string(repo().join("orch/crates/orch-host/src/registry.rs")).unwrap();
    assert!(!source.contains("PlanSignedOff"));
}

#[test]
fn backend_reconciliation_still_reads_the_recorded_wake_pin() {
    use orch_host::wake::{backend_receipt_expectation_for_wake, backend_receipt_from_log};
    let root = audited_agent_pin_amendment_support::fixture_root("b357-durable-pin");
    let round = "r9999";
    let wake_id = "b357-pin";
    let event = orch_host::ledger::event("WakeIssued", "runtime:orch", Some("B357T"), Some(round),
        serde_json::json!({
            "wakeId": wake_id, "agent": "executor-pi", "attemptId": "B357T-A0001",
            "continuationId": "implementation:r9999:B357T:B357T-A0001:executor-pi",
            "providerKind": "pi", "requestSessionId": null,
            "probeOffset": 0, "backendState": "pending", "logPath": "fixture-only.log",
            "requestMessageSha256": "a".repeat(64), "renderedMessageSha256": "b".repeat(64),
            "requestedProvider": "old-provider", "requestedModel": "old-model", "requestedEffort": "high"
        }));
    let ledger = root.join("coordination/rounds/r9999/events.jsonl");
    let bytes = format!("{}\n", serde_json::to_string(&event).unwrap());
    fs::write(&ledger, &bytes).unwrap();
    let registry = root.join("coordination/agents.yaml");
    let current = fs::read_to_string(&registry).unwrap()
        .replace("synthetic-provider", "new-provider")
        .replace("synthetic-model-old", "new-model").replace("effort: xhigh", "effort: low");
    fs::write(&registry, &current).unwrap();
    let expected = backend_receipt_expectation_for_wake(&root, round, wake_id).unwrap();
    let frame = serde_json::json!({"type":"pi.session", "sessionId":"session-old",
        "provider":"old-provider", "model":"old-model", "effort":"high"});
    assert!(backend_receipt_from_log(&expected, frame.to_string().as_bytes(), true).unwrap().is_some());
    for (field, value) in [("provider", "new-provider"), ("model", "new-model"), ("effort", "low")] {
        let mut wrong = frame.clone(); wrong[field] = value.into();
        assert!(backend_receipt_from_log(&expected, wrong.to_string().as_bytes(), true).is_err(), "accepted changed {field}");
    }
    fs::write(&registry, "unreadable-as-registry: [").unwrap();
    assert_eq!(backend_receipt_expectation_for_wake(&root, round, wake_id).unwrap(), expected);
    assert_eq!(fs::read_to_string(&ledger).unwrap(), bytes);
    fs::remove_dir_all(root).unwrap();
}
