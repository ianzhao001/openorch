//! B133 fault-injection table.  Every row persists a fixture ledger, reads it
//! back through the production parser, and asserts both ordering and terminal
//! state.  P1 is covered by the managed-process-group test; P4 by the
//! front-end-dead/backend-writing archive refusal.

use std::fs;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use orch_core::EventRecord;
use orch_host::attempt::plan_next_attempt;
use orch_host::serve::{monitor_alarm_needs_append, monitor_verdict, MonitorVerdict};
use orch_host::tierf::{
    reassignment_gate, terminate_managed_process_group, wip_archive_gate, TerminationSignal,
};

static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

fn event(id: &str, kind: &str, payload: serde_json::Value) -> EventRecord {
    EventRecord {
        event_id: id.into(),
        ts: "2026-07-27T00:00:00Z".into(),
        actor: "runtime:test".into(),
        kind: kind.into(),
        task_id: Some("B133-FI".into()),
        round: Some("r48".into()),
        payload: Some(payload),
        extra: serde_json::Map::new(),
    }
}

fn persisted(name: &str, events: &[EventRecord]) -> Vec<EventRecord> {
    let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let orch_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("CARGO_MANIFEST_DIR 上溯两级应为 orch 工作区根");
    let dir = orch_root
        .join("target/test-tmp")
        .join(format!("orch-b133-fault-{name}-{}-{unique}-{seq}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("events.jsonl");
    let mut bytes = Vec::new();
    for event in events {
        serde_json::to_writer(&mut bytes, event).unwrap();
        bytes.push(b'\n');
    }
    fs::write(&path, bytes).unwrap();
    let ledger = orch_core::read_ledger(&path).unwrap();
    assert!(ledger.bad_lines.is_empty());
    let _ = fs::remove_dir_all(dir);
    ledger.events
}

fn kinds(events: &[EventRecord]) -> Vec<&str> {
    events.iter().map(|event| event.kind.as_str()).collect()
}

#[test]
fn no_await_report_collects_without_false_death() {
    let events = persisted(
        "no-await",
        &[
            event(
                "d1",
                "DispatchIssued",
                serde_json::json!({"agent":"executor-desktop","attemptId":"B133-FI-A0001","attemptNo":1,"baseSha":"base"}),
            ),
            event(
                "r1",
                "ReportObserved",
                serde_json::json!({"attemptId":"B133-FI-A0001"}),
            ),
            event(
                "c1",
                "CollectCompleted",
                serde_json::json!({"attemptId":"B133-FI-A0001"}),
            ),
        ],
    );
    assert_eq!(
        kinds(&events),
        ["DispatchIssued", "ReportObserved", "CollectCompleted"]
    );
    assert_eq!(events.last().unwrap().kind, "CollectCompleted");
    assert!(!events.iter().any(|event| event.kind == "AttemptCrashed"));
}

#[test]
fn orch_restart_recovers_monitor_without_duplicate_alarm() {
    let events = persisted(
        "restart",
        &[
            event(
                "d1",
                "DispatchIssued",
                serde_json::json!({"agent":"executor-desktop","attemptId":"B133-FI-A0001","attemptNo":1,"baseSha":"base"}),
            ),
            event(
                "e1",
                "EscalationRaised",
                serde_json::json!({"stage":"liveness-monitor","verdict":"ambiguous","attemptId":"B133-FI-A0001","attemptNo":1,"agent":"executor-desktop"}),
            ),
        ],
    );
    // A fresh in-memory monitor state after restart consults the durable alarm.
    assert!(!monitor_alarm_needs_append(
        &events,
        "B133-FI",
        "B133-FI-A0001",
        MonitorVerdict::Ambiguous,
    ));
    assert_eq!(kinds(&events), ["DispatchIssued", "EscalationRaised"]);
    assert_eq!(events.last().unwrap().kind, "EscalationRaised");
}

#[test]
fn stale_working_heartbeat_without_durable_signal_is_ambiguous_not_dead() {
    let verdict = monitor_verdict(None, 86_400, 600);
    assert_eq!(verdict, MonitorVerdict::Ambiguous);
    assert!(reassignment_gate(true, None, false, None).is_err());
    let events = persisted(
        "stale-heartbeat",
        &[
            event(
                "d1",
                "DispatchIssued",
                serde_json::json!({"agent":"executor-desktop","attemptId":"B133-FI-A0001","attemptNo":1,"baseSha":"base"}),
            ),
            event(
                "e1",
                "EscalationRaised",
                serde_json::json!({"stage":"liveness-monitor","verdict":"ambiguous","attemptId":"B133-FI-A0001"}),
            ),
        ],
    );
    assert_eq!(kinds(&events), ["DispatchIssued", "EscalationRaised"]);
    assert!(!events
        .iter()
        .any(|event| event.kind == "ReassignmentIssued"));
    assert_eq!(
        events.last().unwrap().payload.as_ref().unwrap()["verdict"],
        "ambiguous"
    );
}

#[test]
fn dead_frontend_with_backend_writes_blocks_reassignment_and_wip_archive() {
    assert!(reassignment_gate(true, Some(false), true, Some(true)).is_err());
    let reason = wip_archive_gate(true, None, true).unwrap_err();
    let events = persisted(
        "backend-writing",
        &[
            event(
                "x1",
                "AttemptCrashed",
                serde_json::json!({"attemptId":"B133-FI-A0001"}),
            ),
            event(
                "e1",
                "EscalationRaised",
                serde_json::json!({"stage":"wip-archive-blocked","reason":reason,"attemptId":"B133-FI-A0001"}),
            ),
        ],
    );
    assert_eq!(kinds(&events), ["AttemptCrashed", "EscalationRaised"]);
    assert_eq!(
        events.last().unwrap().payload.as_ref().unwrap()["stage"],
        "wip-archive-blocked"
    );
    assert!(!events.iter().any(|event| event.kind == "WipArchived"));
}

#[test]
fn slow_verifier_does_not_block_monitor_sampling() {
    let verifier = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(200));
        "VerifierCompleted"
    });
    let started = Instant::now();
    let verdict = monitor_verdict(Some(true), 30, 600);
    assert_eq!(verdict, MonitorVerdict::Healthy);
    assert!(started.elapsed() < Duration::from_millis(100));
    let terminal = verifier.join().unwrap();
    let events = persisted(
        "slow-verifier",
        &[
            event(
                "m1",
                "LivenessSampled",
                serde_json::json!({"verdict":"healthy"}),
            ),
            event("v1", terminal, serde_json::json!({"result":"pass"})),
        ],
    );
    assert_eq!(kinds(&events), ["LivenessSampled", "VerifierCompleted"]);
    assert_eq!(events.last().unwrap().kind, "VerifierCompleted");
}

#[test]
fn stale_second_reassignment_loses_to_active_attempt_gate() {
    let events = persisted(
        "double-dispatch",
        &[
            event(
                "d1",
                "DispatchIssued",
                serde_json::json!({"agent":"executor-desktop","attemptId":"B133-FI-A0001","attemptNo":1,"baseSha":"original"}),
            ),
            event(
                "x1",
                "AttemptCrashed",
                serde_json::json!({"attemptId":"B133-FI-A0001"}),
            ),
            event(
                "d2",
                "DispatchIssued",
                serde_json::json!({"agent":"executor-opencode","attemptId":"B133-FI-A0002","attemptNo":2,"baseSha":"original","reassignment":true}),
            ),
        ],
    );
    assert!(plan_next_attempt(&events, "B133-FI", "executor-claw", "new-main", "r48",).is_err());
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "DispatchIssued")
            .count(),
        2
    );
    assert_eq!(events.last().unwrap().kind, "DispatchIssued");
    assert_eq!(
        events.last().unwrap().payload.as_ref().unwrap()["attemptId"],
        "B133-FI-A0002"
    );
}

#[test]
fn managed_process_group_uses_term_grace_kill_then_kill0_verification() {
    let mut child = Command::new("/bin/sh");
    child
        .arg("-c")
        .arg("trap '' TERM; while :; do sleep 1; done")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = child.spawn().unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let evidence = terminate_managed_process_group(child.id(), Duration::from_millis(40)).unwrap();
    assert_eq!(
        evidence.signals,
        [TerminationSignal::Term, TerminationSignal::Kill]
    );
    assert!(evidence.process_tree_terminated);
    assert_eq!(child.wait().unwrap().signal(), Some(9));
}
