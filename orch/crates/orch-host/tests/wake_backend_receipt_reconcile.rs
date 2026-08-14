use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use orch_host::{ledger, wake};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn root(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "orch-b179-{tag}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(path.join("coordination/rounds/r58")).unwrap();
    path
}

fn append_incident(root: &PathBuf) -> String {
    let wake_id = "019fb019-f94f-4896-8b85-ac0597fbfdfa";
    let continuation = "review:r58:B176:B176-A0002:secondary:executor-opencode";
    let digest = wake::wake_request_message_sha256("review B176", continuation).unwrap();
    let rendered = "b".repeat(64);
    let log_path = root.join("coordination/runtime/logs/wake-opencode.jsonl");
    fs::create_dir_all(log_path.parent().unwrap()).unwrap();
    fs::write(
        &log_path,
        b"noise\n{\"type\":\"step_start\",\"sessionID\":\"ses_late\",\"part\":{\"type\":\"step-start\"}}\n",
    )
    .unwrap();
    let wake = ledger::event(
        "WakeIssued",
        "runtime:orch",
        Some("B176"),
        Some("r58"),
        serde_json::json!({
            "wakeId": wake_id,
            "controlWakeId": wake_id,
            "runtimeLimit": null,
            "continuationId": continuation,
            "attemptId": "B176-A0002",
            "agent": "executor-opencode",
            "providerKind": "opencode",
            "requestMessageSha256": digest,
            "renderedMessageSha256": rendered,
            "requestSessionId": null,
            "backendState": "pending",
            "pid": 42,
            "logPath": log_path.display().to_string(),
            "probeOffset": 0,
            "probeEnd": 0,
            "legacyProbeOffset": 455075,
            "legacyProbe": "linked",
            "method": "typed-runtime",
        }),
    );
    let review = ledger::event(
        "ReviewRequested",
        "runtime:orch",
        Some("B176"),
        Some("r58"),
        serde_json::json!({
            "attemptId": "B176-A0002",
            "role": "secondary",
            "agent": "executor-opencode",
            "wakeId": wake_id,
            "continuationId": continuation,
            "requestMessageSha256": digest,
            "renderedMessageSha256": rendered,
            "providerKind": "opencode",
            "requestSessionId": null,
            "logPath": log_path.display().to_string(),
        }),
    );
    let rejected = ledger::event(
        "ActionRejected",
        "runtime:orch",
        Some("B176"),
        Some("r58"),
        serde_json::json!({
            "actionId": wake_id,
            "operation": "wake-backend-receipt",
            "reason": "obsolete timeout parser",
            "exitCode": 3,
            "alert": true,
            "attemptId": "B176-A0002",
            "attemptNo": 2,
        }),
    );
    ledger::append(root, "r58", &[wake, review, rejected]).unwrap();
    wake_id.to_string()
}

#[test]
fn late_frame_appends_only_one_receipt_and_keeps_the_rejection() {
    let root = root("late");
    let wake_id = append_incident(&root);
    assert!(wake::reconcile_backend_receipt(&root, "r58", &wake_id).unwrap());
    assert!(wake::reconcile_backend_receipt(&root, "r58", &wake_id).unwrap());
    let events = orch_core::read_ledger(&root.join("coordination/rounds/r58/events.jsonl"))
        .unwrap()
        .events;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "WakeIssued")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "ReviewRequested")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "ActionRejected")
            .count(),
        1
    );
    let receipts = events
        .iter()
        .filter(|event| {
            event.kind == "AgentEventReceived"
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("agentEvent"))
                    .and_then(serde_json::Value::as_str)
                    == Some("wake-backend-receipt")
        })
        .collect::<Vec<_>>();
    assert_eq!(receipts.len(), 1);
    let payload = receipts[0].payload.as_ref().unwrap();
    assert_eq!(payload["probeOffset"], 0);
    assert_eq!(payload["wakeId"], wake_id);
    assert_eq!(payload["observedSessionId"], "ses_late");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn concurrent_reconcile_is_append_once() {
    let root = Arc::new(root("concurrent"));
    let wake_id = Arc::new(append_incident(&root));
    let handles = (0..8)
        .map(|_| {
            let root = Arc::clone(&root);
            let wake_id = Arc::clone(&wake_id);
            std::thread::spawn(move || {
                wake::reconcile_backend_receipt(&root, "r58", &wake_id).unwrap()
            })
        })
        .collect::<Vec<_>>();
    for handle in handles {
        assert!(handle.join().unwrap());
    }
    let events = orch_core::read_ledger(&root.join("coordination/rounds/r58/events.jsonl"))
        .unwrap()
        .events;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "AgentEventReceived")
            .count(),
        1
    );
    fs::remove_dir_all(root.as_ref()).unwrap();
}
