//! B53 假 planner 集成测试：只 spawn 本机 `sh`，绝不调用真实模型。

mod support_legacy_plan;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use orch_host::{budget, ledger, round, tierf, wake};
use serde_json::json;

// B137：仅 pid+ULID 不足（卡面要求 pid+seq+nanos），沿
// src/binding.rs::b106_scratch_dir 范式叠加模块级单调计数器。
static TEMP_ROOT_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_root(name: &str) -> PathBuf {
    let orch_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("CARGO_MANIFEST_DIR 上溯两级应为 orch 工作区根");
    let seq = TEMP_ROOT_SEQ.fetch_add(1, Ordering::Relaxed);
    orch_root
        .join("target/test-tmp")
        .join(format!("orch-b53-{name}-{}-{}-{seq}", std::process::id(), ulid::Ulid::new()))
}

fn setup_round(root: &Path, round: &str) {
    fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    fs::write(
        root.join("coordination/runtime/CURRENT-ROUND"),
        format!("{round}\n"),
    )
    .unwrap();
    ledger::append(
        root,
        round,
        &[ledger::event(
            "RoundOpened",
            "runtime:orch",
            None,
            Some(round),
            json!({}),
        )],
    )
    .unwrap();
}

fn write_root_manual_contract(root: &Path, round: &str, task: &str) {
    fs::create_dir_all(root.join("coordination/modes")).unwrap();
    fs::create_dir_all(root.join(format!("coordination/rounds/{round}/tasks"))).unwrap();
    fs::write(
        root.join(".gitignore"),
        ".worktrees/\ncoordination/runtime/logs/\n",
    )
    .unwrap();
    fs::write(
        root.join("coordination/modes/test.yaml"),
        r#"agents:
  executor: {adapter: test, tier: none}
  verifier: {adapter: root-manual, tier: none}
hitl: {mergeGate: auto}
verification: {mode: root-manual-fixed-head}
liveness: {monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}
scheduling:
  allowedAgents: [executor-desktop, executor-claw, executor-opencode]
  capacities:
    executor-desktop: {agent: 2, quota: 2, roles: [implement]}
    executor-claw: {agent: 1, quota: 1, roles: [primary-review]}
    executor-opencode: {agent: 3, quota: 3, roles: [implement, secondary-review]}
budgets: {round: {wallMinutes: 60, maxModelWakes: 20}}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
"#,
    )
    .unwrap();
    fs::write(
        root.join("coordination/PROJECT-BINDING.yaml"),
        "scope: {protectedPaths: [\"coordination/**\"]}\ngit: {pushPolicy: forbidden}\ncommands:\n  testFast: {argv: [\"sh\", \"-c\", \"exit 0\"], timeoutSeconds: 30}\n",
    )
    .unwrap();
    fs::write(
        root.join(format!("coordination/rounds/{round}/tasks/{task}.md")),
        format!(
            "---\ntaskId: {task}\nround: {round}\nagent: executor-opencode\nseedProtocol: pure-spec\nwriteSet: [fixture.txt]\nfrozenPaths: [coordination/**]\ngates: {{fast: [testFast]}}\nbudgets: {{wallMinutes: 10}}\nbootstrapPreSignoffAttempt: {task}-A0001\nrequiredReviews:\n  - {{role: primary, agent: executor-claw}}\nrequiredEvidence: [auto-close]\n---\nauto-close fixture\n"
        ),
    )
    .unwrap();
}

fn activate_root_manual_round(root: &Path, round: &str, task: &str) -> (u32, String) {
    write_root_manual_contract(root, round, task);
    let planned = support_legacy_plan::materialize(root).unwrap();
    orch_host::round::run_sign_off(root, Some("auto-close fixture")).unwrap();
    (planned.revision, planned.digest)
}





fn write_wake_agent(root: &Path, argv: Vec<String>) {
    fs::create_dir_all(root.join("coordination")).unwrap();
    let value = json!({
        "agents": {
            "worker": {
                "injectable": true,
                "sessionId": "worker-session",
                "wake": {
                    "argv": argv,
                    "freshArgv": []
                }
            }
        }
    });
    fs::write(
        root.join("coordination/agents.yaml"),
        serde_json::to_string_pretty(&value).unwrap(),
    )
    .unwrap();
}

fn marker_wake_argv(marker: &Path) -> Vec<String> {
    vec![
        "sh".to_string(),
        "-c".to_string(),
        format!("printf 'wake\\n' >> '{}'", marker.display()),
        "{session}".to_string(),
        "{message}".to_string(),
    ]
}

fn wait_for_file(path: &Path) {
    for _ in 0..100 {
        if path.is_file() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "file did not appear before test deadline: {}",
        path.display()
    );
}

fn write_budget(root: &Path, max_wakes: u64) {
    fs::create_dir_all(root.join("coordination/modes")).unwrap();
    fs::write(
        root.join("coordination/modes/test.yaml"),
        serde_json::to_string_pretty(&json!({
            "budgets": {"round": {"maxModelWakes": max_wakes}}
        }))
        .unwrap(),
    )
    .unwrap();
}



fn read_events(root: &Path, round: &str) -> Vec<orch_core::EventRecord> {
    orch_core::read_ledger(&root.join(format!("coordination/rounds/{round}/events.jsonl")))
        .unwrap()
        .events
}



fn run_git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .unwrap();
    assert!(status.success(), "git {:?} failed", args);
}

fn init_git_main(root: &Path) {
    fs::create_dir_all(root).unwrap();
    run_git(root, &["init"]);
    run_git(root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    fs::write(root.join("README.md"), "baseline\n").unwrap();
    run_git(root, &["add", "README.md"]);
    run_git(
        root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch-test@example.invalid",
            "commit",
            "-m",
            "baseline",
        ],
    );
}

fn git_text(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}







fn setup_force_close_round(name: &str) -> PathBuf {
    let root = temp_root(name);
    init_git_main(&root);
    setup_round(&root, "rT");
    fs::write(root.join("coordination/BOARD.md"), "# Board\n").unwrap();
    activate_root_manual_round(&root, "rT", "B53");
    let main_head = git_text(&root, &["rev-parse", "main"]);
    ledger::append(
        &root,
        "rT",
        &[
            ledger::event(
                "DispatchIssued",
                "runtime:orch",
                Some("B53"),
                Some("rT"),
                json!({
                    "attemptId": "B53-A0001",
                    "attemptNo": 1,
                    "agent": "executor-opencode",
                    "baseSha": main_head,
                    "goPath": "coordination/rounds/rT/dispatch/executor-opencode/GO-B53-A0001.md",
                }),
            ),
            ledger::event(
                "AttemptBlocked",
                "runtime:orch",
                Some("B53"),
                Some("rT"),
                json!({"reason": "recovery path exhausted"}),
            ),
        ],
    )
    .unwrap();
    root
}





















#[test]
fn model_wake_reservation_serializes_max_minus_one_and_is_cancellable() {
    let root = temp_root("budget-reservation");
    setup_round(&root, "rT");
    write_budget(&root, 1);

    let mut first = budget::check_before_model_wake(&root, "rT").unwrap();
    assert!(first.reservation_id().is_some());
    assert!(budget::check_before_model_wake(&root, "rT").is_err());
    budget::cancel_model_wake_reservation(&root, &mut first).unwrap();

    let mut after_cancel = budget::check_before_model_wake(&root, "rT").unwrap();
    assert!(after_cancel.reservation_id().is_some());
    budget::cancel_model_wake_reservation(&root, &mut after_cancel).unwrap();
    fs::remove_dir_all(root).ok();
}

#[test]
fn manual_wake_uses_last_slot_records_fact_and_then_blocks_without_spawn() {
    let root = temp_root("manual-wake-budget");
    setup_round(&root, "rT");
    write_budget(&root, 1);
    let marker = root.join("manual-wake.marker");
    write_wake_agent(&root, marker_wake_argv(&marker));

    wake::run_wake(&root, "worker", "first").unwrap();
    wait_for_file(&marker);
    let events = read_events(&root, "rT");
    let issued = events
        .iter()
        .filter(|event| event.kind == "WakeIssued")
        .collect::<Vec<_>>();
    assert_eq!(issued.len(), 1);
    assert_eq!(budget::count_model_wakes(&events), 1);
    assert_eq!(
        issued[0]
            .payload
            .as_ref()
            .and_then(|payload| payload.get("agent"))
            .and_then(|value| value.as_str()),
        Some("worker")
    );
    assert!(issued[0].extra.contains_key("modelWakeReservationId"));

    assert!(wake::run_wake(&root, "worker", "blocked").is_err());
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(fs::read_to_string(&marker).unwrap().lines().count(), 1);
    assert_eq!(
        read_events(&root, "rT")
            .iter()
            .filter(|event| event.kind == "WakeIssued")
            .count(),
        1
    );
    fs::remove_dir_all(root).ok();
}

#[test]
fn manual_wake_spawn_failure_has_no_fact_and_releases_reservation() {
    let root = temp_root("manual-wake-spawn-fail");
    setup_round(&root, "rT");
    write_budget(&root, 1);
    write_wake_agent(
        &root,
        vec![
            "/definitely/missing/orch-manual-wake".to_string(),
            "{session}".to_string(),
            "{message}".to_string(),
        ],
    );
    assert!(wake::run_wake(&root, "worker", "fail").is_err());
    assert_eq!(budget::count_model_wakes(&read_events(&root, "rT")), 0);

    let marker = root.join("manual-wake-after-failure.marker");
    write_wake_agent(&root, marker_wake_argv(&marker));
    wake::run_wake(&root, "worker", "recovered").unwrap();
    wait_for_file(&marker);
    assert_eq!(budget::count_model_wakes(&read_events(&root, "rT")), 1);
    fs::remove_dir_all(root).ok();
}



#[test]
fn dispatch_error_before_event_drops_raii_reservation() {
    let root = temp_root("dispatch-raii");
    setup_round(&root, "rT");
    write_budget(&root, 1);

    assert!(tierf::run_dispatch(&root, "missing-card", true).is_err());
    assert!(read_events(&root, "rT")
        .iter()
        .all(|event| event.kind != "DispatchIssued"));
    let mut permit = budget::check_before_model_wake(&root, "rT").unwrap();
    assert!(permit.reservation_id().is_some());
    budget::cancel_model_wake_reservation(&root, &mut permit).unwrap();
    fs::remove_dir_all(root).ok();
}

#[test]
fn same_second_round_close_event_id_starts_new_reservation_epoch() {
    let root = temp_root("same-second-epoch");
    setup_round(&root, "rT");
    write_budget(&root, 1);
    let old_permit = budget::check_before_model_wake(&root, "rT").unwrap();
    let opened_ts = read_events(&root, "rT")[0].ts.clone();
    let mut closed = ledger::event("RoundClosed", "runtime:orch", None, Some("rT"), json!({}));
    closed.ts = opened_ts;
    ledger::append(&root, "rT", &[closed]).unwrap();

    let mut new_permit = budget::check_before_model_wake(&root, "rT").unwrap();
    assert_ne!(old_permit.reservation_id(), new_permit.reservation_id());
    budget::cancel_model_wake_reservation(&root, &mut new_permit).unwrap();
    drop(old_permit);
    fs::remove_dir_all(root).ok();
}

#[test]
fn model_wake_reservation_child_helper() {
    let Ok(root) = std::env::var("ORCH_B53_CHILD_ROOT") else {
        return;
    };
    let child_id = std::env::var("ORCH_B53_CHILD_ID").unwrap();
    let root = PathBuf::from(root);
    fs::write(root.join(format!("ready-{child_id}")), b"ready").unwrap();
    wait_for_file(&root.join("go"));

    let outcome = match budget::check_before_model_wake(&root, "rT") {
        Ok(mut permit) => {
            let event = ledger::event(
                "DispatchIssued",
                "runtime:orch",
                Some(&format!("child-{child_id}")),
                Some("rT"),
                json!({}),
            );
            permit.commit();
            ledger::append(&root, "rT", &[event]).unwrap();
            "allowed"
        }
        Err(_) => "blocked",
    };
    fs::write(root.join(format!("result-{child_id}")), outcome).unwrap();
}

#[test]
fn reservation_to_event_is_serialized_across_processes() {
    let root = temp_root("cross-process-reservation");
    setup_round(&root, "rT");
    write_budget(&root, 1);
    let test_binary = std::env::current_exe().unwrap();
    let mut children = Vec::new();
    for child_id in ["a", "b"] {
        children.push(
            Command::new(&test_binary)
                .args([
                    "--exact",
                    "model_wake_reservation_child_helper",
                    "--nocapture",
                ])
                .env("ORCH_B53_CHILD_ROOT", &root)
                .env("ORCH_B53_CHILD_ID", child_id)
                .spawn()
                .unwrap(),
        );
    }
    wait_for_file(&root.join("ready-a"));
    wait_for_file(&root.join("ready-b"));
    fs::write(root.join("go"), b"go").unwrap();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    let outcomes = [
        fs::read_to_string(root.join("result-a")).unwrap(),
        fs::read_to_string(root.join("result-b")).unwrap(),
    ];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| outcome.as_str() == "allowed")
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| outcome.as_str() == "blocked")
            .count(),
        1
    );
    assert_eq!(budget::count_model_wakes(&read_events(&root, "rT")), 1);
    fs::remove_dir_all(root).ok();
}











#[test]
fn planner_lifecycle_events_are_known_to_projection() {
    let events = [
        "InjectionIssued",
        "InjectionConsumed",
        "PlannerTurnCompleted",
        "PlannerTurnLost",
        "WakeIssued",
    ]
    .into_iter()
    .map(|kind| ledger::event(kind, "runtime:orch", None, Some("rT"), json!({})))
    .collect::<Vec<_>>();
    let projection = orch_core::fold(&events);
    assert!(projection.unknown_kinds.is_empty());
}





#[test]
fn scoped_force_close_records_unrecorded_tasks_without_weakening_signed_ir() {
    let root = setup_force_close_round("force-close-positive");
    let note = "1.state=blocked/runtime recovery exhausted; 2.record recovery attempted; 3.no post-merge gate; 4.main green; 5.next round will reconcile";
    let outcome = round::run_close(&root, true, Some(note)).unwrap();
    assert_eq!(outcome.recorded, 0);
    assert_eq!(outcome.total, 1);
    assert_eq!(outcome.unrecorded, vec!["B53(blocked)"]);
    let events = read_events(&root, "rT");
    let closed = events
        .iter()
        .find(|event| event.kind == "RoundClosed")
        .unwrap();
    assert_eq!(closed.actor, "runtime:orch");
    assert_eq!(closed.payload.as_ref().unwrap()["forced"], true);
    assert_eq!(
        closed.payload.as_ref().unwrap()["unrecorded"],
        json!(["B53(blocked)"])
    );
    assert_eq!(closed.payload.as_ref().unwrap()["note"], note);
    assert!(root
        .join("coordination/rounds/rT/dispatch/DONE.md")
        .is_file());
    let board = fs::read_to_string(root.join("coordination/BOARD.md")).unwrap();
    assert!(board.contains("forced=true"));
    assert!(board.contains(note));
    fs::remove_dir_all(root).ok();
}

#[test]
fn non_force_and_blank_force_reject_unrecorded_round_without_close_side_effects() {
    for (name, force, note) in [
        ("non-force-close-negative", false, None),
        ("blank-force-close-negative", true, Some("  \t")),
    ] {
        let root = setup_force_close_round(name);
        assert!(round::run_close(&root, force, note).is_err());
        assert!(!read_events(&root, "rT")
            .iter()
            .any(|event| event.kind == "RoundClosed"));
        assert!(!root
            .join("coordination/rounds/rT/dispatch/DONE.md")
            .exists());
        assert_eq!(
            fs::read_to_string(root.join("coordination/BOARD.md")).unwrap(),
            "# Board\n"
        );
        fs::remove_dir_all(root).ok();
    }
}

#[test]
fn force_does_not_accept_a_forged_recorded_projection() {
    let root = setup_force_close_round("force-forged-recorded");
    ledger::append(
        &root,
        "rT",
        &[ledger::event(
            "TaskRecorded",
            "runtime:orch",
            Some("B53"),
            Some("rT"),
            json!({"postMergeGates": "all-green"}),
        )],
    )
    .unwrap();
    assert!(round::run_close(&root, true, Some("force audit answers present")).is_err());
    assert!(!read_events(&root, "rT")
        .iter()
        .any(|event| event.kind == "RoundClosed"));
    assert!(!root
        .join("coordination/rounds/rT/dispatch/DONE.md")
        .exists());
    fs::remove_dir_all(root).ok();
}
