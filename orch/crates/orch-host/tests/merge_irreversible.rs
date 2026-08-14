//! ═══ 红种子契约 · B88（merge 不可逆动作记账）═══
//! 落位: orch/crates/orch-host/tests/merge_irreversible.rs（逐字节复制）
//! 预期红（redForm: compile）：`close::{merge_executed_event, post_merge_gate_failed_event,
//! task_recorded_event, cleanup_disposition, CleanupDisposition}` 尚不存在 → error[E0432]。
//!
//! 负向变异下界：
//! M1 合后门前不落 MergeExecuted → failing_post_gate_keeps_merge_truth_without_recording 红；
//! M2 合后门红仍落 TaskRecorded → 同测试红；
//! M3 BOARD 缺失使成功合并返回 Err → board_failure_is_best_effort_and_tier_f_cleanup_is_deferred 红；
//! M4 Tier F 立即删 task branch/worktree → 同测试红；
//! M5 升级事件不含 gate/exit/mergeSha → accounting_event_shapes_are_stable 红。

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use fd_lock::RwLock;
use orch_core::read_ledger;
use orch_host::{
    close::{
        cleanup_disposition, merge_executed_event, post_merge_gate_failed_event,
        task_recorded_event, CleanupDisposition, run_record,
    },
    ledger, plan, round, run_merge, verify,
};

// B137：pid+ULID 仍可能在同进程并发下撞名，沿
// src/binding.rs::b106_scratch_dir 范式叠加模块级单调计数器。
static TESTROOT_SEQ: AtomicU64 = AtomicU64::new(0);

struct TestRoot(PathBuf);

impl TestRoot {
    fn new(label: &str, gate_exit: i32, board: bool) -> Self {
        let orch_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("CARGO_MANIFEST_DIR 上溯两级应为 orch 工作区根");
        let seq = TESTROOT_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = orch_root
            .join("target/test-tmp")
            .join(format!(
                "orch-r42-merge-{label}-{}-{}-{seq}",
                std::process::id(),
                ulid::Ulid::new()
            ));
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(
            &root,
            &["config", "user.email", "orch-test@example.invalid"],
        );
        git(&root, &["config", "user.name", "orch test"]);
        fs::write(root.join("README.md"), "base\n").unwrap();
        fs::write(
            root.join(".gitignore"),
            ".worktrees/\n.cowork-temp/\ncoordination/runtime/\n",
        )
        .unwrap();
        git(&root, &["add", "README.md", ".gitignore"]);
        git(&root, &["commit", "-q", "-m", "base"]);
        git(&root, &["branch", "-M", "main"]);
        git(&root, &["checkout", "-q", "-b", "task/B88"]);
        fs::write(root.join("feature.txt"), "merged\n").unwrap();
        git(&root, &["add", "feature.txt"]);
        git(&root, &["commit", "-q", "-m", "feature"]);
        git(&root, &["checkout", "-q", "main"]);
        fs::create_dir_all(root.join(".worktrees")).unwrap();
        git(
            &root,
            &["worktree", "add", "-q", ".worktrees/B88", "task/B88"],
        );

        fs::create_dir_all(root.join("coordination/runtime")).unwrap();
        fs::create_dir_all(root.join("coordination/rounds/r42/tasks")).unwrap();
        fs::create_dir_all(root.join("coordination/rounds/r42/reviews")).unwrap();
        fs::create_dir_all(root.join("coordination/rounds/r42/evidence")).unwrap();
        fs::create_dir_all(root.join("coordination/modes")).unwrap();
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r42\n").unwrap();
        fs::write(
            root.join("coordination/rounds/r42/tasks/B88.md"),
            "---\ntaskId: B88\nround: r42\nagent: executor-claw\nseedProtocol: pure-spec\nwriteSet: [feature.txt]\nfrozenPaths: [coordination/**]\ngates: {fast: [postGate]}\nbudgets: {wallMinutes: 30}\nrequiredReviews:\n  - {role: primary, agent: executor-desktop}\nrequiredEvidence: [merge-boundary]\n---\n# test\n",
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
    executor-desktop: {agent: 2, quota: 2, roles: [implement, primary-review]}
    executor-claw: {agent: 1, quota: 1, roles: [implement, primary-review]}
    executor-opencode: {agent: 3, quota: 3, roles: [secondary-review]}
budgets:
  round: {maxUsd: 1, wallMinutes: 60, maxModelWakes: 2}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
"#,
        )
        .unwrap();
        let gate_command = if gate_exit == 0 {
            "exit 0".to_string()
        } else {
            "test \"$(git branch --show-current)\" = task/B88".to_string()
        };
        fs::write(
            root.join("coordination/PROJECT-BINDING.yaml"),
            format!(
                "project: {{ecosystems: [test]}}\nworkspace: {{worktreeRoot: .worktrees}}\nscope: {{protectedPaths: [\"coordination/**\"]}}\ngit: {{pushPolicy: forbidden}}\ncommands:\n  postGate:\n    argv: [\"sh\", \"-c\", {gate_command:?}]\n    timeoutSeconds: 30\n"
            ),
        )
        .unwrap();
        fs::write(
            root.join("coordination/agents.yaml"),
            "agents:\n  executor-claw:\n    injectable: true\n    sessionId: test\n    wake: {argv: [\"true\"]}\n",
        )
        .unwrap();
        if board {
            fs::write(root.join("coordination/BOARD.md"), "# board\n").unwrap();
        }

        let planned = plan::run_plan(&root).unwrap();
        assert!(planned.event_appended);
        round::run_sign_off(&root, Some("fixture root signoff")).unwrap();
        let task_head = git_output(&root, &["rev-parse", "task/B88"]);
        let mut main_head = git_output(&root, &["rev-parse", "main"]);
        let dispatch = ledger::event(
            "DispatchIssued",
            "runtime:orch",
            Some("B88"),
            Some("r42"),
            serde_json::json!({
                "taskId": "B88",
                "agent": "executor-claw",
                "attemptId": "B88-A0001",
                "attemptNo": 1,
                "goPath": "coordination/rounds/r42/dispatch/executor-claw/GO-B88-A0001.md",
                "baseSha": main_head,
            }),
        );
        let receipt = ledger::event(
            "CollectGateSuccessReceipt",
            "runtime:orch",
            Some("B88"),
            Some("r42"),
            serde_json::json!({
                "actionId": "collect-B88-A0001",
                "attemptId": "B88-A0001",
                "attemptNo": 1,
                "agent": "executor-claw",
                "baseSha": main_head,
                "goPath": "coordination/rounds/r42/dispatch/executor-claw/GO-B88-A0001.md",
                "branchSha": task_head,
            }),
        );
        let collect = ledger::event(
            "ReportCollectCompleted",
            "runtime:orch",
            Some("B88"),
            Some("r42"),
            serde_json::json!({
                "actionId": "collect-B88-A0001",
                "attemptId": "B88-A0001",
                "attemptNo": 1,
                "agent": "executor-claw",
                "baseSha": main_head,
                "goPath": "coordination/rounds/r42/dispatch/executor-claw/GO-B88-A0001.md",
                "branchSha": task_head,
                "gateReceipt": receipt.event_id,
            }),
        );
        ledger::append(
            &root,
            "r42",
            &[dispatch, receipt, collect],
        )
        .unwrap();
        fs::write(
            root.join("coordination/rounds/r42/reviews/B88-A0001-primary-executor-desktop.md"),
            format!(
                "---\ntaskId: B88\nround: r42\nattemptId: B88-A0001\nrole: primary\nreviewer: executor-desktop\nverdict: PASS\nreviewedHead: {task_head}\n---\nfixture review\n"
            ),
        )
        .unwrap();
        fs::write(
            root.join("coordination/rounds/r42/evidence/B88-merge-boundary.json"),
            "{\"fixture\":true}\n",
        )
        .unwrap();
        git(&root, &["add", "coordination"]);
        git(&root, &["commit", "-q", "-m", "fixture signed contract"]);
        main_head = git_output(&root, &["rev-parse", "main"]);
        verify::run_root_verdict(
            &root,
            "B88",
            "B88-A0001",
            &task_head,
            &main_head,
            verify::RootVerdict::Pass,
            None,
            false,
        )
        .unwrap();
        Self(root)
    }

    fn events(&self) -> Vec<orch_core::EventRecord> {
        read_ledger(&self.0.join("coordination/rounds/r42/events.jsonl"))
            .unwrap()
            .events
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .unwrap();
    assert!(status.success(), "git {:?} failed", args);
}

fn git_output(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {:?} failed", args);
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn payload<'a>(event: &'a orch_core::EventRecord, key: &str) -> Option<&'a serde_json::Value> {
    event.payload.as_ref()?.get(key)
}

fn rewrite_events(root: &Path, events: &[orch_core::EventRecord]) {
    let mut text = events
        .iter()
        .map(|event| serde_json::to_string(event).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    text.push('\n');
    fs::write(
        root.join("coordination/rounds/r42/events.jsonl"),
        text,
    )
    .unwrap();
}

fn with_preheld_transition_lease<T>(root: &Path, action: impl FnOnce() -> T) -> T {
    let lock_dir = root.join("coordination/runtime/locks");
    fs::create_dir_all(&lock_dir).unwrap();
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(lock_dir.join("merge.lock"))
        .unwrap();
    let mut lock = RwLock::new(file);
    let _guard = lock.try_write().unwrap();
    action()
}

#[test]
fn accounting_event_shapes_are_stable() {
    let merged = merge_executed_event("B88", "r42", "abc1234");
    assert_eq!(merged.kind, "MergeExecuted");
    assert_eq!(merged.actor, "reviewer:orch-runtime");
    assert_eq!(
        payload(&merged, "mergeSha").and_then(|v| v.as_str()),
        Some("abc1234")
    );

    let failed = post_merge_gate_failed_event("B88", "r42", "abc1234", "postGate", 7);
    assert_eq!(failed.kind, "EscalationRaised");
    assert_eq!(
        payload(&failed, "stage").and_then(|v| v.as_str()),
        Some("post-merge-gate")
    );
    assert_eq!(
        payload(&failed, "gate").and_then(|v| v.as_str()),
        Some("postGate")
    );
    assert_eq!(payload(&failed, "exit").and_then(|v| v.as_i64()), Some(7));
    assert_eq!(
        payload(&failed, "mergeSha").and_then(|v| v.as_str()),
        Some("abc1234")
    );

    assert_eq!(task_recorded_event("B88", "r42").kind, "TaskRecorded");
    assert_eq!(cleanup_disposition(true), CleanupDisposition::DeferredTierF);
    assert_eq!(
        cleanup_disposition(false),
        CleanupDisposition::AttemptBestEffort
    );
}

#[test]
fn preheld_transition_lease_stops_merge_before_ledger_or_refs_move() {
    let root = TestRoot::new("preheld-transition", 0, true);
    let main_before = git_output(&root.0, &["rev-parse", "main"]);
    let ledger_path = root.0.join("coordination/rounds/r42/events.jsonl");
    let ledger_before = fs::read(&ledger_path).unwrap();

    let error = with_preheld_transition_lease(&root.0, || {
        run_merge(&root.0, "B88").unwrap_err()
    });
    let message = error.to_string();
    assert!(message.contains("protocol-transition lease busy"), "{message}");
    assert!(message.contains("fail-fast"), "{message}");
    assert_eq!(git_output(&root.0, &["rev-parse", "main"]), main_before);
    assert_eq!(fs::read(&ledger_path).unwrap(), ledger_before);
}

#[test]
fn failing_post_gate_keeps_merge_truth_without_recording() {
    let root = TestRoot::new("red", 7, true);
    let error = run_merge(&root.0, "B88").unwrap_err().to_string();
    assert!(error.contains("合并后门"), "{error}");
    assert!(root.0.join("feature.txt").is_file(), "git merge 已真实发生");

    let events = root.events();
    let merged = events
        .iter()
        .position(|event| event.kind == "MergeExecuted")
        .expect("merge 成功后必须立即落 MergeExecuted");
    let escalated = events
        .iter()
        .position(|event| {
            event.kind == "EscalationRaised"
                && payload(event, "stage").and_then(|v| v.as_str()) == Some("post-merge-gate")
        })
        .expect("合后门红必须落升级事实");
    assert!(merged < escalated);
    assert!(!events.iter().any(|event| event.kind == "TaskRecorded"));
}

#[test]
fn board_failure_is_best_effort_and_tier_f_cleanup_is_deferred() {
    let root = TestRoot::new("green-no-board", 0, false);
    let outcome = run_merge(&root.0, "B88").expect("BOARD 缺失不得推翻已成功的 merge+gate");
    assert!(!outcome.merge_sha_short.is_empty());
    let events = root.events();
    let merged = events
        .iter()
        .position(|event| event.kind == "MergeExecuted")
        .unwrap();
    let recorded = events
        .iter()
        .position(|event| event.kind == "TaskRecorded")
        .unwrap();
    assert!(merged < recorded);

    let branch = Command::new("git")
        .args(["rev-parse", "--verify", "task/B88"])
        .current_dir(&root.0)
        .status()
        .unwrap();
    assert!(
        branch.success(),
        "Tier F branch 必须延迟到确认会话退出后再清"
    );
    assert_eq!(
        git_output(&root.0, &["rev-parse", "HEAD"]),
        git_output(&root.0, &["rev-parse", "main"]),
        "成功 merge 后 HEAD 与 refs/heads/main 必须共同指向 merge SHA"
    );
}

#[test]
fn non_main_checkout_is_rejected_before_merge_started_or_git_merge() {
    let root = TestRoot::new("wrong-main-checkout", 0, true);
    git(&root.0, &["checkout", "-q", "-b", "detour"]);
    let error = run_merge(&root.0, "B88").unwrap_err().to_string();
    assert!(error.contains("主工作区") || error.contains("main"), "{error}");
    let events = root.events();
    assert!(!events.iter().any(|event| event.kind == "MergeStarted"));
    assert!(!events.iter().any(|event| event.kind == "MergeExecuted"));
    assert!(!root.0.join("feature.txt").is_file());
}

#[test]
fn staged_or_unrelated_dirty_root_is_rejected_before_merge_started() {
    for (label, staged) in [("staged", true), ("unstaged", false)] {
        let root = TestRoot::new(label, 0, true);
        fs::write(root.0.join("unrelated.txt"), "must not enter merge\n").unwrap();
        if staged {
            git(&root.0, &["add", "unrelated.txt"]);
        }
        let before = git_output(&root.0, &["rev-parse", "main"]);
        let error = run_merge(&root.0, "B88").unwrap_err().to_string();
        assert!(error.contains("dirty") || error.contains("staged"), "{error}");
        assert_eq!(git_output(&root.0, &["rev-parse", "main"]), before);
        let events = root.events();
        assert!(!events.iter().any(|event| event.kind == "MergeStarted"));
        assert!(!events.iter().any(|event| event.kind == "MergeExecuted"));
    }
}

#[test]
fn wrong_actor_exact_merge_started_cannot_cross_git_boundary() {
    let root = TestRoot::new("forged-start-actor", 0, true);
    let authorization = verify::validate_root_merge_authorization(
        &root.0,
        "r42",
        "B88",
        &root.events(),
    )
    .unwrap();
    ledger::append(
        &root.0,
        "r42",
        &[ledger::event(
            "MergeStarted",
            "planner",
            Some("B88"),
            Some("r42"),
            serde_json::json!({
                "attemptId": authorization.attempt_id,
                "attemptNo": authorization.attempt_no,
                "headSha": authorization.head_sha,
                "mainHeadSha": authorization.main_head_sha,
                "collectCompletedEventId": authorization.collect_completed_event_id,
                "verdictEventId": authorization.verdict_event_id,
            }),
        )],
    )
    .unwrap();
    let before = git_output(&root.0, &["rev-parse", "main"]);
    let error = run_merge(&root.0, "B88").unwrap_err().to_string();
    assert!(error.contains("MergeStarted") || error.contains("未授权事件"), "{error}");
    assert_eq!(git_output(&root.0, &["rev-parse", "main"]), before);
    assert!(!root.0.join("feature.txt").exists());
    assert!(!root.events().iter().any(|event| event.kind == "MergeExecuted"));
}

#[test]
fn archived_chain_binds_bootstrap_scope_and_entire_pre_root_ledger_prefix() {
    let root = TestRoot::new("archived-prefix", 0, true);
    run_merge(&root.0, "B88").unwrap();
    let events = root.events();
    verify::validate_archived_record_chain(&root.0, "r42", "B88", &events).unwrap();

    let mut forged_permit = events.clone();
    forged_permit
        .iter_mut()
        .find(|event| event.kind == "VerdictIssued" && event.actor == "verifier:root")
        .unwrap()
        .payload
        .as_mut()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert(
            "bootstrapPreSignoffAttempt".into(),
            serde_json::Value::String("B88-A0001".into()),
        );
    let error = verify::validate_archived_record_chain(
        &root.0,
        "r42",
        "B88",
        &forged_permit,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("committed ROUND-IR") || error.contains("prefix"), "{error}");

    let mut injected_prefix = events;
    let root_position = injected_prefix
        .iter()
        .position(|event| event.kind == "VerdictIssued" && event.actor == "verifier:root")
        .unwrap();
    injected_prefix.insert(
        root_position,
        ledger::event(
            "TaskValidated",
            "runtime:orch",
            None,
            Some("r42"),
            plan::task_validated_payload(2, &"f".repeat(64)),
        ),
    );
    let error = verify::validate_archived_record_chain(
        &root.0,
        "r42",
        "B88",
        &injected_prefix,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("prefix"), "{error}");
}

#[test]
fn forged_merge_executed_actor_cannot_use_record_recovery() {
    let root = TestRoot::new("forged-record", 7, true);
    assert!(run_merge(&root.0, "B88").is_err());
    let mut events = root.events();
    events
        .iter_mut()
        .find(|event| event.kind == "MergeExecuted")
        .unwrap()
        .actor = "planner".to_string();
    rewrite_events(&root.0, &events);

    let error = run_record(&root.0, "B88").unwrap_err().to_string();
    assert!(
        error.contains("MergeExecuted envelope") || error.contains("未授权事件 MergeExecuted"),
        "{error}"
    );
    assert!(!root.events().iter().any(|event| event.kind == "TaskRecorded"));
}

#[test]
fn concurrent_record_recovery_is_fail_fast_or_idempotent() {
    let root = TestRoot::new("record-race", 0, true);
    run_merge(&root.0, "B88").unwrap();
    let mut events = root.events();
    events.retain(|event| event.kind != "TaskRecorded");
    rewrite_events(&root.0, &events);

    let left_root = root.0.clone();
    let right_root = root.0.clone();
    let left = std::thread::spawn(move || run_record(&left_root, "B88"));
    let right = std::thread::spawn(move || run_record(&right_root, "B88"));
    let results = [left.join().unwrap(), right.join().unwrap()];
    assert_eq!(
        results
            .iter()
            .filter(|result| result.as_ref().is_ok_and(|outcome| !outcome.already_recorded))
            .count(),
        1
    );
    for error in results.iter().filter_map(|result| result.as_ref().err()) {
        assert!(
            error
                .to_string()
                .contains("protocol-transition lease busy"),
            "{error:#}"
        );
    }
    assert_eq!(
        root.events()
            .iter()
            .filter(|event| event.kind == "TaskRecorded")
            .count(),
        1
    );
}

#[test]
fn record_recovery_survives_a_later_unrelated_main_commit() {
    let root = TestRoot::new("record-after-main-advance", 0, true);
    run_merge(&root.0, "B88").unwrap();
    let mut events = root.events();
    events.retain(|event| event.kind != "TaskRecorded");
    rewrite_events(&root.0, &events);

    fs::write(root.0.join("later.txt"), "unrelated main advance\n").unwrap();
    git(&root.0, &["add", "later.txt"]);
    git(&root.0, &["commit", "-q", "-m", "later unrelated main commit"]);
    let outcome = run_record(&root.0, "B88").unwrap();
    assert!(!outcome.already_recorded);
    assert_eq!(
        root.events()
            .iter()
            .filter(|event| event.kind == "TaskRecorded")
            .count(),
        1
    );
}
