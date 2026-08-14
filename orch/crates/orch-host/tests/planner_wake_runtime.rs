//! B53 假 planner 集成测试：只 spawn 本机 `sh`，绝不调用真实模型。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use orch_host::{budget, inbox, ledger, round, serve, tierf, wake};
use serde_json::json;
use sha2::{Digest, Sha256};

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
    let planned = orch_host::plan::run_plan(root).unwrap();
    orch_host::round::run_sign_off(root, Some("auto-close fixture")).unwrap();
    (planned.revision, planned.digest)
}

fn fake_template(script: &str) -> Vec<String> {
    vec![
        "sh".to_string(),
        "-c".to_string(),
        script.to_string(),
        "{session}".to_string(),
        "--session-id".to_string(),
        "{session}".to_string(),
        "{message}".to_string(),
    ]
}

fn write_agents(root: &Path, template: Vec<String>) {
    fs::create_dir_all(root.join("coordination")).unwrap();
    let value = json!({
        "agents": {
            "planner": {
                "injectable": true,
                "sessionId": "",
                "wake": {
                    "argv": ["claude", "--resume", "{session}", "-p", "{message}"],
                    "freshArgv": template
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

fn lease(
    round: &str,
    wake_id: &str,
    session_id: &str,
    trigger: &str,
    attempt: u8,
) -> serve::PlannerLease {
    serve::PlannerLease {
        round: round.to_string(),
        wake_id: wake_id.to_string(),
        session_id: session_id.to_string(),
        trigger_key: trigger.to_string(),
        attempt,
        owner_pid: std::process::id(),
        child_pid: None,
        started_at: humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string(),
        reasons: vec!["new_instruction".to_string()],
        inbox_files: Vec::new(),
        log_path: None,
        model_wake_reservation_id: None,
    }
}

fn read_events(root: &Path, round: &str) -> Vec<orch_core::EventRecord> {
    orch_core::read_ledger(&root.join(format!("coordination/rounds/{round}/events.jsonl")))
        .unwrap()
        .events
}

fn wait_for_lease_release(root: &Path) {
    for _ in 0..100 {
        if serve::read_planner_lease(root).unwrap().is_none() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("planner lease did not release before test deadline");
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

fn commit_contract_and_task_head(root: &Path, task: &str) -> (String, String) {
    run_git(root, &["add", "."]);
    run_git(
        root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch-test@example.invalid",
            "commit",
            "-m",
            "signed contract",
        ],
    );
    let main_head = git_text(root, &["rev-parse", "main"]);
    run_git(root, &["checkout", "-b", &format!("task/{task}")]);
    fs::write(root.join("fixture.txt"), "task head\n").unwrap();
    run_git(root, &["add", "fixture.txt"]);
    run_git(
        root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch-test@example.invalid",
            "commit",
            "-m",
            "task head",
        ],
    );
    let task_head = git_text(root, &["rev-parse", "HEAD"]);
    run_git(root, &["checkout", "main"]);
    (main_head, task_head)
}

fn merge_task_no_ff(root: &Path, task: &str) -> String {
    run_git(
        root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch-test@example.invalid",
            "merge",
            "--no-ff",
            &format!("task/{task}"),
            "-m",
            "merge task",
        ],
    );
    git_text(root, &["rev-parse", "HEAD"])
}

fn append_real_archived_record_chain(
    root: &Path,
    round: &str,
    task: &str,
    dispatch_base: &str,
    task_head: &str,
) {
    let attempt_id = format!("{task}-A0001");
    let agent = "executor-opencode";
    let go_path = format!("coordination/rounds/{round}/dispatch/{agent}/GO-{attempt_id}.md");
    let action_id = "collect-action-1";
    let legacy_validation = ledger::event(
        "TaskValidated",
        "runtime:orch",
        None,
        Some(round),
        json!({"irRevision": 1, "validationDigest": "0123456789abcdef"}),
    );
    let dispatch = ledger::event(
        "DispatchIssued",
        "runtime:orch",
        Some(task),
        Some(round),
        json!({
            "attemptId": attempt_id,
            "attemptNo": 1,
            "agent": agent,
            "baseSha": dispatch_base,
            "goPath": go_path,
        }),
    );
    let receipt = ledger::event(
        "CollectGateSuccessReceipt",
        "runtime:orch",
        Some(task),
        Some(round),
        json!({
            "actionId": action_id,
            "attemptId": attempt_id,
            "attemptNo": 1,
            "agent": agent,
            "baseSha": dispatch_base,
            "goPath": go_path,
            "branchSha": task_head,
        }),
    );
    let collect = ledger::event(
        "ReportCollectCompleted",
        "runtime:orch",
        Some(task),
        Some(round),
        json!({
            "actionId": action_id,
            "attemptId": attempt_id,
            "attemptNo": 1,
            "agent": agent,
            "baseSha": dispatch_base,
            "goPath": go_path,
            "branchSha": task_head,
            "gateReceipt": receipt.event_id,
        }),
    );
    ledger::append(
        root,
        round,
        &[legacy_validation, dispatch, receipt, collect.clone()],
    )
    .unwrap();

    // This deliberately models the real bootstrap order: an old binary has
    // already dispatched and collected the implementation before the new
    // binary emits the canonical TaskValidated + exact user sign-off which
    // reauthorizes the later fixed-HEAD root PASS.
    let planned = orch_host::plan::run_plan(root).unwrap();
    orch_host::round::run_sign_off(root, Some("post-collect reauthorization")).unwrap();
    let review_rel =
        format!("coordination/rounds/{round}/reviews/{attempt_id}-primary-executor-claw.md");
    let evidence_rel = format!("coordination/rounds/{round}/evidence/{task}-auto-close.json");
    fs::create_dir_all(root.join(&review_rel).parent().unwrap()).unwrap();
    fs::create_dir_all(root.join(&evidence_rel).parent().unwrap()).unwrap();
    let review_bytes = format!(
        "---\ntaskId: {task}\nround: {round}\nattemptId: {attempt_id}\nrole: primary\nreviewer: executor-claw\nverdict: PASS\nreviewedHead: {task_head}\n---\nfixture review\n"
    )
    .into_bytes();
    let evidence_bytes = b"{\"fixture\":true}\n".to_vec();
    fs::write(root.join(&review_rel), &review_bytes).unwrap();
    fs::write(root.join(&evidence_rel), &evidence_bytes).unwrap();
    run_git(root, &["add", "."]);
    run_git(
        root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch-test@example.invalid",
            "commit",
            "-m",
            "authorize root pass",
        ],
    );
    let signed_main = git_text(root, &["rev-parse", "main"]);
    let verdict_log = root.join(format!(
        "coordination/runtime/logs/{task}-{attempt_id}-root-verdict-gate-testFast.log"
    ));
    fs::create_dir_all(verdict_log.parent().unwrap()).unwrap();
    fs::write(&verdict_log, []).unwrap();
    let root_pass = ledger::event(
        "VerdictIssued",
        "verifier:root",
        Some(task),
        Some(round),
        json!({
            "verdict": "PASS",
            "irRevision": planned.revision,
            "validationDigest": planned.digest,
            "attemptId": attempt_id,
            "attemptNo": 1,
            "implementerAgent": agent,
            "headSha": task_head,
            "mainHeadSha": signed_main,
            "collectCompletedEventId": collect.event_id,
            "bootstrapPreSignoffAttempt": attempt_id,
            "reviews": [{
                "path": format!(
                    "coordination/rounds/{round}/reviews/{attempt_id}-primary-executor-claw.md"
                ),
                "role": "primary",
                "reviewer": "executor-claw",
                "verdict": "PASS",
                "sha256": hex::encode(Sha256::digest(&review_bytes)),
                "bytes": review_bytes.len(),
            }],
            "evidence": [{
                "id": "auto-close",
                "path": format!(
                    "coordination/rounds/{round}/evidence/{task}-auto-close.json"
                ),
                "sha256": hex::encode(Sha256::digest(&evidence_bytes)),
                "bytes": evidence_bytes.len(),
            }],
            "gates": [{
                "name": "testFast",
                "exitCode": 0,
                "logSha256": hex::encode(Sha256::digest([])),
                "logBytes": 0,
            }],
        }),
    );
    ledger::append(root, round, &[root_pass]).unwrap();
    // Exercise the production lifecycle entry so MergeStarted, MergeExecuted
    // and TaskRecorded are minted only while the exclusive merge capability is
    // active.  The ledger capability guard intentionally rejects direct
    // fixture fabrication of these terminal facts.
    let task_worktree = root.join(".worktrees").join(task);
    fs::create_dir_all(task_worktree.parent().unwrap()).unwrap();
    run_git(
        root,
        &[
            "worktree",
            "add",
            task_worktree.to_str().unwrap(),
            &format!("task/{task}"),
        ],
    );
    orch_host::close::run_merge(root, task).unwrap();
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
fn fake_fresh_launch_captures_env_cwd_and_unique_logs() {
    let root = temp_root("fresh");
    let script =
        "printf 'wake=%s\\ncwd=%s\\nsession=%s\\nmessage=%s\\n' \"$ORCH_PLANNER_WAKE_ID\" \"$PWD\" \"$0\" \"$3\"";
    write_agents(&root, fake_template(script));

    let mut first = wake::run_fresh_planner_wake(&root, "decision one").unwrap();
    let first_wake = first.wake_id.clone();
    let first_session = first.session_id.clone();
    assert!(first
        .child
        .wait_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap()
        .success());
    let first_log = fs::read_to_string(&first.log_path).unwrap();
    assert!(first_log.contains(&format!("wake={first_wake}")));
    let child_cwd = first_log
        .lines()
        .find_map(|line| line.strip_prefix("cwd="))
        .unwrap();
    assert_eq!(
        fs::canonicalize(child_cwd).unwrap(),
        fs::canonicalize(&root).unwrap()
    );
    assert!(first_log.contains(&format!("session={first_session}")));
    assert!(first_log.contains("message=decision one"));

    let mut second = wake::run_fresh_planner_wake(&root, "decision two").unwrap();
    assert!(second
        .child
        .wait_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap()
        .success());
    assert_ne!(first_wake, second.wake_id);
    assert_ne!(first_session, second.session_id);
    assert_ne!(first.log_path, second.log_path);
    assert!(first.log_path.is_file() && second.log_path.is_file());
    fs::remove_dir_all(root).ok();
}

#[test]
fn atomic_lease_allows_one_daemon_and_stale_release_cannot_delete_new() {
    let root = temp_root("lease");
    let barrier = Arc::new(Barrier::new(3));
    let mut joins = Vec::new();
    for suffix in ["a", "b"] {
        let root = root.clone();
        let barrier = barrier.clone();
        joins.push(std::thread::spawn(move || {
            let candidate = lease("rT", &format!("wake-{suffix}"), "session", "trigger", 1);
            barrier.wait();
            serve::try_acquire_planner_lease(&root, &candidate).unwrap()
        }));
    }
    barrier.wait();
    let wins = joins
        .into_iter()
        .map(|join| join.join().unwrap())
        .filter(|won| *won)
        .count();
    assert_eq!(wins, 1);
    let first = serve::read_planner_lease(&root).unwrap().unwrap();
    assert!(serve::release_planner_lease(&root, &first.wake_id).unwrap());

    let newer = lease("rT", "wake-new", "session-new", "trigger", 2);
    assert!(serve::try_acquire_planner_lease(&root, &newer).unwrap());
    assert!(!serve::release_planner_lease(&root, &first.wake_id).unwrap());
    assert_eq!(
        serve::read_planner_lease(&root).unwrap().unwrap().wake_id,
        "wake-new"
    );
    assert!(serve::release_planner_lease(&root, "wake-new").unwrap());
    fs::remove_dir_all(root).ok();
}

#[test]
fn malformed_final_lease_is_quarantined_then_valid_atomic_publish_succeeds() {
    let root = temp_root("malformed-lease");
    let path = root.join("coordination/runtime/locks/planner-turn.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "{\"wakeId\":").unwrap();
    assert!(serve::read_planner_lease(&root).is_err());
    assert!(path.is_file());

    let status = Command::new("touch")
        .args(["-t", "200001010000", path.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success());
    assert!(serve::read_planner_lease(&root).unwrap().is_none());
    assert!(!path.exists());
    assert!(fs::read_dir(path.parent().unwrap()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("planner-turn.corrupt-")
    }));

    let valid = lease("rT", "wake-valid", "session-valid", "trigger-valid", 1);
    assert!(serve::try_acquire_planner_lease(&root, &valid).unwrap());
    assert_eq!(serve::read_planner_lease(&root).unwrap(), Some(valid));
    fs::remove_dir_all(root).ok();
}

#[test]
fn runtime_actor_tagged_progress_completes_turn() {
    let root = temp_root("progress");
    setup_round(&root, "rT");
    write_agents(&root, fake_template("exit 0"));
    let prepared = wake::prepare_fresh_planner_wake(&root, "progress").unwrap();
    let turn = lease(
        "rT",
        &prepared.wake_id,
        &prepared.session_id,
        "trigger-progress",
        1,
    );
    assert!(serve::try_acquire_planner_lease(&root, &turn).unwrap());
    let launch = wake::spawn_prepared_planner_wake(&root, prepared).unwrap();

    let mut progress = ledger::event("TaskValidated", "runtime:orch", None, Some("rT"), json!({}));
    progress
        .extra
        .insert("plannerWakeId".to_string(), json!(turn.wake_id.clone()));
    ledger::append(&root, "rT", &[progress]).unwrap();

    assert_eq!(
        serve::watch_planner_launch(&root, &turn, launch, Duration::from_secs(2)).unwrap(),
        serve::PlannerLivenessDecision::Completed
    );
    assert!(serve::read_planner_lease(&root).unwrap().is_none());
    assert!(read_events(&root, "rT")
        .iter()
        .any(|event| event.kind == "PlannerTurnCompleted"));
    fs::remove_dir_all(root).ok();
}

#[test]
fn successful_exit_without_tagged_progress_completes_turn() {
    let root = temp_root("exit-zero");
    setup_round(&root, "rT");
    write_agents(&root, fake_template("exit 0"));
    let prepared = wake::prepare_fresh_planner_wake(&root, "facts checked").unwrap();
    let turn = lease(
        "rT",
        &prepared.wake_id,
        &prepared.session_id,
        "trigger-exit-zero",
        1,
    );
    assert!(serve::try_acquire_planner_lease(&root, &turn).unwrap());
    let launch = wake::spawn_prepared_planner_wake(&root, prepared).unwrap();
    assert_eq!(
        serve::watch_planner_launch(&root, &turn, launch, Duration::from_secs(2)).unwrap(),
        serve::PlannerLivenessDecision::Completed
    );
    assert!(read_events(&root, "rT")
        .iter()
        .any(|event| event.kind == "PlannerTurnCompleted"));
    fs::remove_dir_all(root).ok();
}

#[test]
fn no_progress_retries_once_then_escalates() {
    let root = temp_root("retry");
    setup_round(&root, "rT");
    write_agents(&root, fake_template("exit 7"));

    let first_prepared = wake::prepare_fresh_planner_wake(&root, "first").unwrap();
    let first = lease(
        "rT",
        &first_prepared.wake_id,
        &first_prepared.session_id,
        "same-trigger",
        1,
    );
    assert!(serve::try_acquire_planner_lease(&root, &first).unwrap());
    let first_launch = wake::spawn_prepared_planner_wake(&root, first_prepared).unwrap();
    assert_eq!(
        serve::watch_planner_launch(&root, &first, first_launch, Duration::from_secs(2)).unwrap(),
        serve::PlannerLivenessDecision::Retry { attempt: 2 }
    );

    let second_prepared = wake::prepare_fresh_planner_wake(&root, "second").unwrap();
    let second = lease(
        "rT",
        &second_prepared.wake_id,
        &second_prepared.session_id,
        "same-trigger",
        2,
    );
    assert!(serve::try_acquire_planner_lease(&root, &second).unwrap());
    let second_launch = wake::spawn_prepared_planner_wake(&root, second_prepared).unwrap();
    assert_eq!(
        serve::watch_planner_launch(&root, &second, second_launch, Duration::from_secs(2)).unwrap(),
        serve::PlannerLivenessDecision::Escalate
    );
    let events = read_events(&root, "rT");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "PlannerTurnLost")
            .count(),
        2
    );
    assert!(events.iter().any(|event| {
        event.kind == "EscalationRaised"
            && event.payload.as_ref().and_then(|p| p.get("stage"))
                == Some(&json!("planner-liveness"))
    }));
    fs::remove_dir_all(root).ok();
}

#[test]
fn serve_retry_restores_processing_inbox_then_escalates_after_attempt_two() {
    let root = temp_root("serve-retry-inbox");
    setup_round(&root, "rT");
    write_budget(&root, 10);
    write_agents(&root, fake_template("exit 7"));
    let filename = inbox::add(&root, "retry this instruction", 1000).unwrap();

    let (first, _) = serve::serve_tick(&root).unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].reason, serve::InjectReason::NewInstruction);
    assert!(root
        .join(inbox::relocate_target(
            &filename,
            inbox::InboxStage::Processing
        ))
        .is_file());
    wait_for_lease_release(&root);
    assert!(root
        .join(inbox::relocate_target(
            &filename,
            inbox::InboxStage::Pending
        ))
        .is_file());

    let (second, _) = serve::serve_tick(&root).unwrap();
    assert_eq!(second.len(), 1);
    wait_for_lease_release(&root);

    let events = read_events(&root, "rT");
    let issued = events
        .iter()
        .filter(|event| event.kind == "InjectionIssued")
        .collect::<Vec<_>>();
    assert_eq!(issued.len(), 2);
    assert_eq!(issued[0].payload.as_ref().unwrap()["attempt"], 1);
    assert_eq!(issued[1].payload.as_ref().unwrap()["attempt"], 2);
    assert_eq!(
        issued[0].payload.as_ref().unwrap()["triggerKey"],
        issued[1].payload.as_ref().unwrap()["triggerKey"]
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "PlannerTurnLost")
            .count(),
        2
    );
    assert!(events.iter().any(|event| {
        event.kind == "EscalationRaised"
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("stage"))
                == Some(&json!("planner-liveness"))
    }));

    let (third, _) = serve::serve_tick(&root).unwrap();
    assert!(third.is_empty());
    assert_eq!(
        read_events(&root, "rT")
            .iter()
            .filter(|event| event.kind == "InjectionIssued")
            .count(),
        2
    );
    fs::remove_dir_all(root).ok();
}

#[test]
fn planner_prompt_resolves_processing_path_and_reads_instruction() {
    let root = temp_root("prompt-inbox-path");
    setup_round(&root, "rT");
    write_budget(&root, 10);
    let script = r#"sleep 0.1
processing=$(printf '%s\n' "$3" | sed -n 's/^- processing: //p' | head -n 1)
pending=$(printf '%s\n' "$3" | sed -n 's/^  pending fallback: //p' | head -n 1)
if [ -f "$processing" ]; then cat "$processing"; exit 0; fi
if [ -f "$pending" ]; then cat "$pending"; exit 0; fi
exit 44"#;
    write_agents(&root, fake_template(script));
    let filename = inbox::add(&root, "planner must read this exact instruction", 1003).unwrap();

    let (logical, _) = serve::serve_tick(&root).unwrap();
    assert_eq!(logical[0].reason, serve::InjectReason::NewInstruction);
    let turn = serve::read_planner_lease(&root).unwrap().unwrap();
    let log_path = root.join(turn.log_path.as_ref().unwrap());
    wait_for_lease_release(&root);
    let log = fs::read_to_string(log_path).unwrap();
    assert!(log.contains("planner must read this exact instruction"));
    assert!(root
        .join(inbox::relocate_target(
            &filename,
            inbox::InboxStage::Processing
        ))
        .is_file());
    fs::remove_dir_all(root).ok();
}

#[test]
fn completed_trigger_is_not_spawned_again_until_its_cause_changes() {
    let root = temp_root("serve-completed-dedupe");
    setup_round(&root, "rT");
    write_budget(&root, 10);
    let release_marker = root.join("planner-child-release");
    let child_script = format!(
        r#"marker='{}'
wait_step=0
while [ ! -f "$marker" ]; do
  wait_step=$((wait_step + 1))
  if [ "$wait_step" -ge 400 ]; then
    printf 'planner child release marker timeout: %s\n' "$marker" >&2
    exit 75
  fi
  sleep 0.01
done"#,
        release_marker.display()
    );
    write_agents(&root, fake_template(&child_script));
    ledger::append(
        &root,
        "rT",
        &[ledger::event(
            "MechCheckFailed",
            "runtime:orch",
            Some("B53"),
            Some("rT"),
            json!({"stage": "report"}),
        )],
    )
    .unwrap();

    let (first, _) = serve::serve_tick(&root).unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].reason, serve::InjectReason::TaskFailed);
    let turn = serve::read_planner_lease(&root).unwrap().unwrap();
    let mut progress = ledger::event(
        "TaskValidated",
        "runtime:orch",
        Some("B53"),
        Some("rT"),
        json!({}),
    );
    progress
        .extra
        .insert("plannerWakeId".to_string(), json!(turn.wake_id));
    ledger::append(&root, "rT", &[progress]).unwrap();
    fs::write(&release_marker, "release\n").unwrap();
    wait_for_lease_release(&root);
    assert!(read_events(&root, "rT")
        .iter()
        .any(|event| event.kind == "PlannerTurnCompleted"));

    let (duplicate, _) = serve::serve_tick(&root).unwrap();
    assert!(duplicate.is_empty());
    assert_eq!(
        read_events(&root, "rT")
            .iter()
            .filter(|event| event.kind == "InjectionIssued")
            .count(),
        1
    );

    fs::remove_file(&release_marker).unwrap();
    ledger::append(
        &root,
        "rT",
        &[ledger::event(
            "MechCheckFailed",
            "runtime:orch",
            Some("B53"),
            Some("rT"),
            json!({"stage": "new-report-failure"}),
        )],
    )
    .unwrap();
    let (new_cause, _) = serve::serve_tick(&root).unwrap();
    assert_eq!(new_cause.len(), 1);
    fs::write(&release_marker, "release\n").unwrap();
    wait_for_lease_release(&root);
    assert_eq!(
        read_events(&root, "rT")
            .iter()
            .filter(|event| event.kind == "InjectionIssued")
            .count(),
        2
    );
    fs::remove_dir_all(root).ok();
}

#[test]
fn budget_block_and_spawn_failure_keep_inbox_pending_without_issue() {
    let budget_root = temp_root("budget-block");
    setup_round(&budget_root, "rT");
    write_budget(&budget_root, 1);
    write_agents(&budget_root, fake_template("exit 0"));
    let budget_file = inbox::add(&budget_root, "budget blocked", 1000).unwrap();
    ledger::append(
        &budget_root,
        "rT",
        &[ledger::event(
            "DispatchIssued",
            "runtime:orch",
            Some("B0"),
            Some("rT"),
            json!({}),
        )],
    )
    .unwrap();
    assert!(serve::serve_tick(&budget_root).is_err());
    assert!(budget_root
        .join(inbox::relocate_target(
            &budget_file,
            inbox::InboxStage::Pending
        ))
        .is_file());
    assert_eq!(
        budget::count_model_wakes(&read_events(&budget_root, "rT")),
        1
    );
    assert!(serve::read_planner_lease(&budget_root).unwrap().is_none());

    let spawn_root = temp_root("spawn-fail");
    setup_round(&spawn_root, "rT");
    write_budget(&spawn_root, 10);
    write_agents(
        &spawn_root,
        vec![
            "/definitely/missing/orch-planner".to_string(),
            "--session-id".to_string(),
            "{session}".to_string(),
            "{message}".to_string(),
        ],
    );
    let spawn_file = inbox::add(&spawn_root, "spawn fails", 1001).unwrap();
    assert!(serve::serve_tick(&spawn_root).is_err());
    assert!(spawn_root
        .join(inbox::relocate_target(
            &spawn_file,
            inbox::InboxStage::Pending
        ))
        .is_file());
    assert_eq!(
        budget::count_model_wakes(&read_events(&spawn_root, "rT")),
        0
    );
    assert!(serve::read_planner_lease(&spawn_root).unwrap().is_none());

    fs::remove_dir_all(budget_root).ok();
    fs::remove_dir_all(spawn_root).ok();
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
fn max_budget_blocks_nudge_before_file_event_or_wake_spawn() {
    let root = temp_root("nudge-budget-block");
    setup_round(&root, "rT");
    write_budget(&root, 1);
    let marker = root.join("nudge-wake.marker");
    write_wake_agent(&root, marker_wake_argv(&marker));
    ledger::append(
        &root,
        "rT",
        &[ledger::event(
            "DispatchIssued",
            "runtime:orch",
            Some("B0"),
            Some("rT"),
            json!({}),
        )],
    )
    .unwrap();

    assert!(tierf::run_nudge(&root, "worker", None, "blocked", false, false).is_err());
    std::thread::sleep(Duration::from_millis(100));
    assert!(!marker.exists());
    assert!(!root
        .join("coordination/rounds/rT/dispatch/worker/NUDGE.md")
        .exists());
    assert_eq!(
        read_events(&root, "rT")
            .iter()
            .filter(|event| event.kind == "NudgeIssued")
            .count(),
        0
    );
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
fn reconcile_persists_spawn_fact_before_retry_and_budget_counts_it() {
    let root = temp_root("pid-before-issue");
    setup_round(&root, "rT");
    write_budget(&root, 1);
    write_agents(&root, fake_template("sleep 5; exit 7"));
    let permit = budget::check_before_model_wake(&root, "rT").unwrap();
    let prepared = wake::prepare_fresh_planner_wake(&root, "crash window").unwrap();
    let mut launch = wake::spawn_prepared_planner_wake(&root, prepared).unwrap();
    let mut turn = lease(
        "rT",
        &launch.wake_id,
        &launch.session_id,
        "trigger-crash-window",
        1,
    );
    turn.child_pid = Some(launch.pid);
    turn.log_path = Some(
        launch
            .log_path
            .strip_prefix(&root)
            .unwrap()
            .display()
            .to_string(),
    );
    turn.model_wake_reservation_id = permit.reservation_id().map(str::to_string);
    assert!(serve::try_acquire_planner_lease(&root, &turn).unwrap());

    assert!(serve::reconcile_planner_turn(&root).unwrap());
    let events = read_events(&root, "rT");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "InjectionIssued")
            .count(),
        1
    );
    assert_eq!(budget::count_model_wakes(&events), 1);
    assert!(budget::check_before_model_wake(&root, "rT").is_err());

    launch.child.kill().unwrap();
    launch.child.wait().unwrap();
    assert!(!serve::reconcile_planner_turn(&root).unwrap());
    assert!(serve::read_planner_lease(&root).unwrap().is_none());
    fs::remove_dir_all(root).ok();
}

#[test]
fn expired_no_child_lease_with_live_owner_is_never_stolen() {
    let root = temp_root("live-owner-no-child");
    setup_round(&root, "rT");
    let mut turn = lease("rT", "wake-owner", "session-owner", "trigger-owner", 1);
    turn.started_at = "2000-01-01T00:00:00Z".to_string();
    assert!(serve::try_acquire_planner_lease(&root, &turn).unwrap());
    assert!(serve::reconcile_planner_turn(&root).unwrap());
    assert_eq!(
        serve::read_planner_lease(&root).unwrap().unwrap().wake_id,
        "wake-owner"
    );
    serve::release_planner_lease(&root, "wake-owner").unwrap();
    fs::remove_dir_all(root).ok();
}

#[test]
fn expired_child_that_ignores_term_keeps_lease_and_escalates() {
    let root = temp_root("term-resistant");
    setup_round(&root, "rT");
    write_agents(&root, fake_template("trap '' TERM; sleep 10"));
    let mut launch = wake::run_fresh_planner_wake(&root, "do not duplicate").unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let mut turn = lease(
        "rT",
        &launch.wake_id,
        &launch.session_id,
        "trigger-term-resistant",
        1,
    );
    turn.child_pid = Some(launch.pid);
    turn.started_at = "2000-01-01T00:00:00Z".to_string();
    assert!(serve::try_acquire_planner_lease(&root, &turn).unwrap());

    assert!(serve::reconcile_planner_turn(&root).unwrap());
    assert!(serve::read_planner_lease(&root).unwrap().is_some());
    assert!(read_events(&root, "rT").iter().any(|event| {
        event.kind == "EscalationRaised"
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("reason"))
                == Some(&json!("planner-timeout-process-still-alive"))
    }));

    launch.child.kill().unwrap();
    launch.child.wait().unwrap();
    assert!(!serve::reconcile_planner_turn(&root).unwrap());
    assert!(serve::read_planner_lease(&root).unwrap().is_none());
    fs::remove_dir_all(root).ok();
}

#[test]
fn serve_batches_adjacent_reasons_and_live_lease_blocks_duplicate_spawn() {
    let root = temp_root("serve-batch");
    setup_round(&root, "rT");
    write_budget(&root, 1);
    let hold = root.join("hold-planner");
    fs::write(&hold, b"hold").unwrap();
    write_agents(
        &root,
        fake_template(&format!(
            "while [ -e '{}' ]; do sleep 0.05; done",
            hold.display()
        )),
    );
    let filename = inbox::add(&root, "one fresh instruction", 1002).unwrap();
    ledger::append(
        &root,
        "rT",
        &[
            ledger::event(
                "DispatchIssued",
                "runtime:orch",
                Some("old"),
                Some("rT"),
                json!({}),
            ),
            ledger::event(
                "MechCheckFailed",
                "runtime:orch",
                Some("B53"),
                Some("rT"),
                json!({"stage": "report"}),
            ),
            ledger::event(
                "EscalationRaised",
                "runtime:orch",
                Some("B54"),
                Some("rT"),
                json!({"stage": "liveness-dead"}),
            ),
            ledger::event("RoundClosed", "runtime:orch", None, Some("rT"), json!({})),
        ],
    )
    .unwrap();

    let (logical, pending) = serve::serve_tick(&root).unwrap();
    assert_eq!(pending, 1);
    assert_eq!(
        logical.iter().map(|item| item.reason).collect::<Vec<_>>(),
        vec![
            serve::InjectReason::NewInstruction,
            serve::InjectReason::TaskFailed,
            serve::InjectReason::AgentDown,
        ]
    );
    let issued = read_events(&root, "rT")
        .into_iter()
        .filter(|event| event.kind == "InjectionIssued")
        .collect::<Vec<_>>();
    assert_eq!(issued.len(), 1);
    let payload = issued[0].payload.as_ref().unwrap();
    assert_eq!(payload["reasons"].as_array().unwrap().len(), 3);
    assert_eq!(payload["attempt"], 1);
    assert!(payload["wakeId"].as_str().is_some());
    assert!(payload["sessionId"].as_str().is_some());
    assert!(!payload["sessionId"].as_str().unwrap().contains("d08add7f"));
    assert!(root
        .join(inbox::relocate_target(
            &filename,
            inbox::InboxStage::Processing
        ))
        .is_file());

    let (second, _) = serve::serve_tick(&root).unwrap();
    assert!(second.is_empty());
    assert_eq!(
        read_events(&root, "rT")
            .iter()
            .filter(|event| event.kind == "InjectionIssued")
            .count(),
        1
    );

    fs::remove_file(&hold).unwrap();
    for _ in 0..30 {
        if serve::read_planner_lease(&root).unwrap().is_none() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(serve::read_planner_lease(&root).unwrap().is_none());
    fs::remove_dir_all(root).ok();
}

#[test]
fn utf8_prompt_is_bounded_and_new_source_changes_key() {
    let injections = vec![serve::Injection {
        reason: serve::InjectReason::TaskFailed,
        message: "失败".repeat(200),
    }];
    let first =
        serve::plan_planner_wake("rT", &injections, &[], &["event-1".to_string()], 127).unwrap();
    let second =
        serve::plan_planner_wake("rT", &injections, &[], &["event-2".to_string()], 127).unwrap();
    assert!(first.prompt.len() <= 127);
    assert!(first.prompt.is_char_boundary(first.prompt.len()));
    assert_ne!(first.trigger_key, second.trigger_key);
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
fn daemon_close_waits_for_all_recorded_judgment_then_closes_mechanically() {
    let root = temp_root("auto-close-gate");
    init_git_main(&root);
    setup_round(&root, "rT");
    fs::write(root.join("coordination/BOARD.md"), "# Board\n").unwrap();
    write_root_manual_contract(&root, "rT", "B53");
    let (dispatch_base, task_head) = commit_contract_and_task_head(&root, "B53");
    append_real_archived_record_chain(&root, "rT", "B53", &dispatch_base, &task_head);
    let actions = vec![orch_host::runloop::Action::CloseRound];
    assert!(!serve::auto_close_round_if_ready(&root, &actions).unwrap());
    assert!(!read_events(&root, "rT")
        .iter()
        .any(|event| event.kind == "RoundClosed"));

    ledger::append(
        &root,
        "rT",
        &[ledger::event(
            "PlannerTurnCompleted",
            "runtime:orch",
            None,
            Some("rT"),
            json!({"reasons": ["all_recorded"], "triggerKey": "all-recorded-key"}),
        )],
    )
    .unwrap();
    assert!(serve::auto_close_round_if_ready(&root, &actions).unwrap());
    assert!(read_events(&root, "rT")
        .iter()
        .any(|event| event.kind == "RoundClosed"));
    fs::remove_dir_all(root).ok();
}

#[test]
fn daemon_close_rejects_forged_merge_and_record_suffix_without_authorization_chain() {
    let root = temp_root("auto-close-forged-suffix");
    init_git_main(&root);
    setup_round(&root, "rT");
    fs::write(root.join("coordination/BOARD.md"), "# Board\n").unwrap();
    activate_root_manual_round(&root, "rT", "B53");
    let (_main_head, _task_head) = commit_contract_and_task_head(&root, "B53");
    let merge_sha = merge_task_no_ff(&root, "B53");
    ledger::append(
        &root,
        "rT",
        &[
            ledger::event(
                "MergeExecuted",
                "reviewer:orch-runtime",
                Some("B53"),
                Some("rT"),
                json!({"mergeSha": merge_sha, "policy": "no-ff"}),
            ),
            ledger::event(
                "TaskRecorded",
                "runtime:orch",
                Some("B53"),
                Some("rT"),
                json!({"postMergeGates": "all-green"}),
            ),
            ledger::event(
                "PlannerTurnCompleted",
                "runtime:orch",
                None,
                Some("rT"),
                json!({"reasons": ["all_recorded"], "triggerKey": "forged-suffix"}),
            ),
        ],
    )
    .unwrap();

    let actions = vec![orch_host::runloop::Action::CloseRound];
    assert!(serve::auto_close_round_if_ready(&root, &actions).is_err());
    assert!(!read_events(&root, "rT")
        .iter()
        .any(|event| event.kind == "RoundClosed"));
    assert!(!root
        .join("coordination/rounds/rT/dispatch/DONE.md")
        .exists());
    fs::remove_dir_all(root).ok();
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
