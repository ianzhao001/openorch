//! B113 · CLI/集成回归：action-scoped 拒绝事件、退出码与告警。
//!
//! 覆盖卡正文目标：wake/dispatch/run-task/nudge 的运行前拒绝与 provider launch
//! 拒绝统一落 `ActionRejected`（带 actionId/operation/reason/exitCode/attempt
//! identity）；snapshot 产生 action-scoped alert，重复同 action 拒绝幂等。
//! seed 两用例（`tests/action_rejection.rs`）保持 byte-identical 不改；本文件
//! 为 seed 外生产集成回归。

mod support_legacy_plan;

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use orch_core::{fold, EventRecord};
use orch_host::attempt::AttemptRef;
use orch_host::failure::{self, rejection_event, ActionRejection};
use orch_host::snapshot::{self, BudgetInput, SnapshotInputs};
use orch_host::{ledger, tierf, wake};

const ROUND: &str = "r47";

struct TestRepo {
    root: PathBuf,
}

impl TestRepo {
    fn new(tag: &str) -> Self {
        let root = orch_host::util::test_scratch_dir(&format!("b113-{tag}"));
        fs::create_dir_all(root.join("coordination/runtime")).unwrap();
        fs::write(
            root.join("coordination/runtime/CURRENT-ROUND"),
            format!("{ROUND}\n"),
        )
        .unwrap();
        ledger::append(
            &root,
            ROUND,
            &[ledger::event(
                "RoundOpened",
                "runtime:test",
                None,
                Some(ROUND),
                serde_json::json!({}),
            )],
        )
        .unwrap();
        Self { root }
    }

    fn init_git(&self) {
        git(&self.root, &["init", "-q"]);
        fs::write(self.root.join("README.md"), "base\n").unwrap();
        git(&self.root, &["add", "README.md"]);
        git(
            &self.root,
            &[
                "-c",
                "user.name=orch-test",
                "-c",
                "user.email=orch@test.invalid",
                "commit",
                "-q",
                "-m",
                "base",
            ],
        );
        git(&self.root, &["branch", "-M", "main"]);
    }

    fn write_agents(&self, agents: serde_json::Value) {
        fs::create_dir_all(self.root.join("coordination")).unwrap();
        fs::write(
            self.root.join("coordination/agents.yaml"),
            serde_json::to_vec_pretty(&serde_json::json!({"agents": agents})).unwrap(),
        )
        .unwrap();
    }



    fn activate_root_manual(&self, task: &str, agent: &str) {
        fs::create_dir_all(self.root.join("coordination/modes")).unwrap();
        fs::create_dir_all(self.root.join(".worktrees")).unwrap();
        fs::write(
            self.root.join(".gitignore"),
            "coordination/runtime/\ncoordination/rounds/*/dispatch/\n.worktrees/\n",
        )
        .unwrap();
        fs::create_dir_all(self.root.join(format!("coordination/rounds/{ROUND}/tasks"))).unwrap();
        fs::write(
            self.root.join("coordination/modes/test.yaml"),
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
            self.root.join("coordination/PROJECT-BINDING.yaml"),
            "scope: {protectedPaths: [\"coordination/**\"]}\ngit: {pushPolicy: forbidden}\ncommands:\n  testFast: {argv: [\"sh\", \"-c\", \"exit 0\"], timeoutSeconds: 30}\n",
        )
        .unwrap();
        fs::write(
            self.root.join(format!(
                "coordination/rounds/{ROUND}/tasks/{task}.md"
            )),
            format!("---\ntaskId: {task}\nround: {ROUND}\nagent: {agent}\nseedProtocol: pure-spec\nwriteSet: [fixture.txt]\nfrozenPaths: [coordination/**]\ngates: {{fast: [testFast]}}\nbudgets: {{wallMinutes: 10}}\nrequiredReviews:\n  - {{role: primary, agent: executor-claw}}\nrequiredEvidence: [cli]\n---\nfixture\n"),
        )
        .unwrap();
        support_legacy_plan::materialize(&self.root).unwrap();
        orch_host::round::run_sign_off(&self.root, Some("action fixture")).unwrap();
    }

    fn events(&self) -> Vec<EventRecord> {
        orch_core::read_ledger(
            &self
                .root
                .join(format!("coordination/rounds/{ROUND}/events.jsonl")),
        )
        .unwrap()
        .events
    }
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn latest_rejection(events: &[EventRecord]) -> &serde_json::Value {
    events
        .iter()
        .rev()
        .find(|event| event.kind == "ActionRejected")
        .and_then(|event| event.payload.as_ref())
        .expect("production rejection must be durable")
}

fn assert_error_matches_event(error: &anyhow::Error, payload: &serde_json::Value) {
    let declared = payload["exitCode"].as_i64().unwrap() as i32;
    assert_eq!(failure::rejection_exit_code(error), Some(declared));
}

fn rejection_event_with(action_id: &str, op: &str, reason: &str, exit: i32) -> EventRecord {
    let r = ActionRejection::new(op, action_id, reason, exit).unwrap();
    rejection_event("r47", Some("B113"), &r)
}

#[test]
fn rejection_event_carries_attempt_identity_when_set() {
    // post-attempt 拒绝：with_attempt 注入 attempt identity。
    let rejection = ActionRejection::new("wake", "B113-A0001", "registry invalid", 2)
        .unwrap()
        .with_attempt(&AttemptRef {
            task_id: "B113".into(),
            ordinal: 1,
            attempt_id: "B113-A0001".into(),
        });
    let event = rejection_event("r47", Some("B113"), &rejection);
    let payload = event.payload.as_ref().unwrap();
    assert_eq!(payload["attemptId"], "B113-A0001");
    assert_eq!(payload["attemptNo"], 1);
    assert_eq!(payload["actionId"], "B113-A0001");
}

#[test]
fn rejection_event_omits_attempt_identity_when_none() {
    // pre-attempt 拒绝（run-task pre-attempt）：attempt identity 留 null。
    let rejection =
        ActionRejection::new("run-task", "run-task:r47:B113", "worktree exists", 2).unwrap();
    let event = rejection_event("r47", Some("B113"), &rejection);
    let payload = event.payload.as_ref().unwrap();
    assert!(payload["attemptId"].is_null());
    assert!(payload["attemptNo"].is_null());
    assert_eq!(payload["actionId"], "run-task:r47:B113");
}

#[test]
fn action_already_rejected_is_idempotent() {
    let event = rejection_event_with("wake-123", "wake", "registry invalid", 2);
    let events = vec![event];
    assert!(failure::action_already_rejected(&events, "wake-123"));
    assert!(!failure::action_already_rejected(&events, "wake-456"));
}

#[test]
fn run_task_pre_attempt_rejection_uses_deterministic_action_id() {
    // 卡正文 line 47-49：run-task pre-attempt actionId = run-task:<round>:<taskId>
    let rejection =
        failure::run_task_pre_attempt_rejection("r47", "B113", "worktree exists").unwrap();
    assert_eq!(rejection.action_id, "run-task:r47:B113");
    assert_eq!(rejection.operation, "run-task");
    assert!(rejection.attempt_id.is_none());
    assert!(rejection.attempt_no.is_none());
    assert_eq!(rejection.exit_code, 2);
}

#[test]
fn rejection_into_error_carries_exit_code() {
    // CLI 提取退出码：ActionRejection.into_error() → rejection_exit_code 提取 exit_code。
    let rejection = ActionRejection::new("wake", "wake-123", "registry invalid", 3).unwrap();
    let err = rejection.into_error();
    assert_eq!(failure::rejection_exit_code(&err), Some(3));
    let plain = anyhow::anyhow!("plain");
    assert!(failure::rejection_exit_code(&plain).is_none());
}

#[test]
fn snapshot_alerts_on_action_rejected_event() {
    // snapshot 产生 action-scoped alert。
    let events = vec![rejection_event_with(
        "wake-123",
        "wake",
        "registry invalid",
        2,
    )];
    let inputs = SnapshotInputs {
        round_id: "r47".into(),
        generated_at: "2026-07-26T00:00:00Z".into(),
        events,
        agents: vec![],
        budget: BudgetInput {
            max_usd: None,
            max_wall_minutes: None,
            max_model_wakes: None,
            spent_usd: 0.0,
            spent_wall_minutes: 0,
            spent_model_wakes: 0,
        },
        activity: vec![],
        liveness_opts: Default::default(),
    };
    let snap = snapshot::build_snapshot(&inputs);
    let action_alerts: Vec<_> = snap
        .alerts
        .iter()
        .filter(|a| a.kind == "action-rejected")
        .collect();
    assert_eq!(action_alerts.len(), 1);
    assert_eq!(action_alerts[0].subject, "wake-123");
    assert!(action_alerts[0].message.contains("wake"));
}

#[test]
fn snapshot_idempotent_on_same_action_rejection() {
    // 重复同 actionId 拒绝 → 只告警一次（幂等）。
    let ev = rejection_event_with("wake-123", "wake", "registry invalid", 2);
    let events = vec![ev.clone(), ev];
    let inputs = SnapshotInputs {
        round_id: "r47".into(),
        generated_at: "2026-07-26T00:00:00Z".into(),
        events,
        agents: vec![],
        budget: BudgetInput {
            max_usd: None,
            max_wall_minutes: None,
            max_model_wakes: None,
            spent_usd: 0.0,
            spent_wall_minutes: 0,
            spent_model_wakes: 0,
        },
        activity: vec![],
        liveness_opts: Default::default(),
    };
    let snap = snapshot::build_snapshot(&inputs);
    let count = snap
        .alerts
        .iter()
        .filter(|a| a.kind == "action-rejected")
        .count();
    assert_eq!(count, 1);
}

#[test]
fn snapshot_new_action_realerts() {
    // 新 actionId 的拒绝 → 再次告警。
    let events = vec![
        rejection_event_with("wake-123", "wake", "registry invalid", 2),
        rejection_event_with("wake-456", "wake", "not injectable", 2),
    ];
    let inputs = SnapshotInputs {
        round_id: "r47".into(),
        generated_at: "2026-07-26T00:00:00Z".into(),
        events,
        agents: vec![],
        budget: BudgetInput {
            max_usd: None,
            max_wall_minutes: None,
            max_model_wakes: None,
            spent_usd: 0.0,
            spent_wall_minutes: 0,
            spent_model_wakes: 0,
        },
        activity: vec![],
        liveness_opts: Default::default(),
    };
    let snap = snapshot::build_snapshot(&inputs);
    let count = snap
        .alerts
        .iter()
        .filter(|a| a.kind == "action-rejected")
        .count();
    assert_eq!(count, 2);
}

#[test]
fn action_rejected_is_known_to_fold() {
    // 生产路径产 ActionRejected 后 fold 不会推入 unknown_kinds（catalog 登记）。
    let events = vec![rejection_event_with(
        "wake-123",
        "wake",
        "registry invalid",
        2,
    )];
    let proj = fold(&events);
    assert!(
        proj.unknown_kinds.is_empty(),
        "ActionRejected should be known, got: {:?}",
        proj.unknown_kinds
    );
}

#[test]
fn rejection_with_current_attempt_falls_back_when_no_attempt() {
    // 无 current attempt 时回退到 fallback action_id（卡目标：显式 wake 用 wakeId）。
    let events: Vec<EventRecord> = vec![];
    let rejection = failure::rejection_with_current_attempt(
        &events,
        "B113",
        "wake",
        "wake:executor-opencode",
        "registry invalid",
        2,
    )
    .unwrap();
    assert_eq!(rejection.action_id, "wake:executor-opencode");
    assert!(rejection.attempt_id.is_none());
}

#[test]
fn explicit_wake_registry_rejection_uses_a_real_preallocated_wake_id() {
    let site = TestRepo::new("wake-registry");
    site.write_agents(serde_json::json!({}));

    let error = wake::run_wake(&site.root, "missing-agent", "wake up").unwrap_err();
    let events = site.events();
    let payload = latest_rejection(&events);
    let action_id = payload["actionId"].as_str().unwrap();
    assert_eq!(payload["operation"], "wake");
    assert_eq!(payload["exitCode"], 2);
    assert_eq!(action_id.len(), 36, "wakeId must be a rendered UUID");
    assert_ne!(action_id, "wake:missing-agent");
    assert!(payload["reason"].as_str().unwrap().contains("未注册"));
    assert_error_matches_event(&error, payload);
}

#[test]
fn dispatch_wake_rejects_unsafe_or_unknown_agent_without_poke_escape() {
    let site = TestRepo::new("wake-agent-path");
    site.write_agents(serde_json::json!({}));
    let victim = site.root.join("ESCAPED.txt");
    fs::write(&victim, "unchanged\n").unwrap();
    let before = fs::read(&victim).unwrap();

    for agent in ["../ESCAPED", "x/../../../ESCAPED", "missing-agent"] {
        assert!(wake::dispatch_wake(&site.root, agent, ROUND, "message", true).is_err());
    }
    assert_eq!(fs::read(&victim).unwrap(), before);
    assert!(!site
        .root
        .join("coordination/runtime/POKE-missing-agent.txt")
        .exists());
}

#[test]
fn explicit_wake_provider_spawn_fault_is_durable() {
    let site = TestRepo::new("wake-spawn");
    site.write_agents(serde_json::json!({
        "executor-opencode": {
            "injectable": true,
            "sessionId": "session-1",
            "wake": {
                "argv": ["/definitely/missing/b113-wake-provider", "{session}", "{message}"]
            }
        }
    }));
    site.activate_root_manual("B113", "executor-opencode");

    let error = wake::run_wake(&site.root, "executor-opencode", "wake up").unwrap_err();
    let events = site.events();
    let payload = latest_rejection(&events);
    assert_eq!(payload["operation"], "wake");
    assert_eq!(payload["exitCode"], 2);
    assert_eq!(payload["actionId"].as_str().unwrap().len(), 36);
    assert!(payload["reason"].as_str().unwrap().contains("spawn wake"));
    assert_error_matches_event(&error, payload);
}

#[test]
fn retired_dispatch_preserves_bad_ledger_without_appending_a_rejection() {
    let site = TestRepo::new("dispatch-bad-ledger");
    site.init_git();
    site.activate_root_manual("B113", "executor-opencode");
    let ledger_path = site
        .root
        .join(format!("coordination/rounds/{ROUND}/events.jsonl"));
    writeln!(
        fs::OpenOptions::new()
            .append(true)
            .open(&ledger_path)
            .unwrap(),
        "not-json"
    )
    .unwrap();

    fn collect(path: &Path, files: &mut std::collections::BTreeMap<PathBuf, Option<Vec<u8>>>) {
        let metadata = fs::symlink_metadata(path).unwrap();
        assert!(!metadata.file_type().is_symlink());
        files.insert(path.to_path_buf(), metadata.is_file().then(|| fs::read(path).unwrap()));
        if metadata.is_dir() {
            for entry in fs::read_dir(path).unwrap() { collect(&entry.unwrap().path(), files); }
        }
    }
    let snapshot = || {
        let mut runtime = std::collections::BTreeMap::new();
        collect(&site.root.join("coordination/runtime"), &mut runtime);
        (fs::read(&ledger_path).unwrap(), runtime,
            git(&site.root, &["rev-parse", "HEAD"]),
            git(&site.root, &["status", "--porcelain", "--untracked-files=all"]),
            git(&site.root, &["worktree", "list", "--porcelain"]))
    };
    let before = snapshot();
    let event_count = site.events().len();
    let error = tierf::run_dispatch(&site.root, "B113", true).unwrap_err();
    let read = orch_core::read_ledger(&ledger_path).unwrap();
    assert_eq!(read.bad_lines.len(), 1);
    assert_eq!(read.events.len(), event_count);
    assert!(format!("{error:#}").contains("坏行"));
    assert_eq!(snapshot(), before);
}

#[test]
fn dispatch_provider_spawn_fault_keeps_exact_action_and_attempt_identity() {
    let site = TestRepo::new("dispatch-spawn");
    site.init_git();
    site.activate_root_manual("B113", "executor-opencode");
    site.write_agents(serde_json::json!({
        "executor-opencode": {
            "injectable": true,
            "sessionId": "session-1",
            "wake": {
                "argv": [
                    "/definitely/missing/b113-dispatch-provider",
                    "{session}",
                    "{message}"
                ]
            }
        }
    }));
    let base_sha = git(&site.root, &["rev-parse", "main"]);
    let attempt = AttemptRef {
        task_id: "B113".into(),
        ordinal: 1,
        attempt_id: "B113-A0001".into(),
    };
    let go_rel = "coordination/rounds/r47/dispatch/executor-opencode/GO-B113-A0001.md";
    let go = site.root.join(go_rel);
    fs::create_dir_all(go.parent().unwrap()).unwrap();
    fs::write(&go, "# GO B113\n").unwrap();
    fs::write(format!("{}.ack", go.display()), "# ACK B113\n").unwrap();
    ledger::append(
        &site.root,
        ROUND,
        &[ledger::event(
            "DispatchIssued",
            "runtime:test",
            Some("B113"),
            Some(ROUND),
            serde_json::json!({
                "agent": "executor-opencode",
                "baseSha": base_sha,
                "goPath": go_rel,
                "attemptId": attempt.attempt_id,
                "attemptNo": attempt.ordinal,
                "wakePending": true
            }),
        )],
    )
    .unwrap();

    let error = tierf::finish_dispatch_wake_with_hook(
        &site.root,
        ROUND,
        "B113",
        "executor-opencode",
        &attempt,
        go_rel,
        false,
        true,
        &mut |_| Ok(()),
    )
    .unwrap_err();
    let events = site.events();
    let payload = latest_rejection(&events);
    let expected_action = orch_host::attempt::dispatch_wake_action_id(
        ROUND,
        "B113",
        &attempt,
        "executor-opencode",
        &base_sha,
        go_rel,
    );
    assert_eq!(payload["operation"], "dispatch");
    assert_eq!(payload["exitCode"], 2);
    assert_eq!(payload["actionId"], expected_action);
    assert_eq!(payload["attemptId"], "B113-A0001");
    assert_eq!(payload["attemptNo"], 1);
    assert!(payload["reason"]
        .as_str()
        .unwrap()
        .contains("provider wake launch rejected"));
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "DispatchWakeReleased")
            .count(),
        1
    );
    assert_error_matches_event(&error, payload);
}

#[test]
fn await_loop_new_bad_line_is_durably_rejected() {
    let site = TestRepo::new("await-new-bad-line");
    site.activate_root_manual("B113", "executor-opencode");
    let go_rel = "coordination/rounds/r47/dispatch/executor-opencode/GO-B113-A0001.md";
    let go = site.root.join(go_rel);
    fs::create_dir_all(go.parent().unwrap()).unwrap();
    fs::write(&go, "# GO B113\n").unwrap();
    fs::write(format!("{}.ack", go.display()), "# ACK B113\n").unwrap();
    ledger::append(
        &site.root,
        ROUND,
        &[ledger::event(
            "DispatchIssued",
            "runtime:test",
            Some("B113"),
            Some(ROUND),
            serde_json::json!({
                "agent": "executor-opencode",
                "baseSha": "0123456789abcdef",
                "goPath": go_rel,
                "attemptId": "B113-A0001",
                "attemptNo": 1,
                "wakePending": true
            }),
        )],
    )
    .unwrap();
    let ledger_path = site
        .root
        .join(format!("coordination/rounds/{ROUND}/events.jsonl"));
    let mut injected = false;
    let error = tierf::run_await_with_hook(
        &site.root,
        "B113",
        // H48: only widen this fixture's scheduling budget. The hook still
        // injects on the first loop, so production await semantics are unchanged.
        30,
        Some(Default::default()),
        &mut |point| {
            if point == "before-ack-claim" && !injected {
                writeln!(
                    fs::OpenOptions::new().append(true).open(&ledger_path)?,
                    "new-loop-bad-line"
                )?;
                injected = true;
            }
            Ok(())
        },
    )
    .err()
    .expect("loop bad line must reject await-report");
    assert!(
        injected,
        "test must inject after the loop's fresh read; got {error:#}"
    );
    let read = orch_core::read_ledger(&ledger_path).unwrap();
    assert_eq!(read.bad_lines.len(), 1);
    let payload = latest_rejection(&read.events);
    assert_eq!(payload["operation"], "await-report");
    assert_eq!(payload["exitCode"], 4);
    assert_eq!(payload["attemptId"], "B113-A0001");
    assert!(payload["reason"].as_str().unwrap().contains("坏行"));
    assert_error_matches_event(&error, payload);
}



#[test]
fn rejection_construction_and_append_failures_are_not_swallowed() {
    let construct = failure::reject_action::<()>(
        Path::new("/unused"),
        ROUND,
        None,
        "wake",
        "",
        "registry invalid",
        2,
        None,
    )
    .unwrap_err();
    assert!(format!("{construct:#}").contains("构造 ActionRejected 失败"));

    let root = orch_host::util::test_scratch_dir("b113-append-failure");
    fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    fs::write(
        root.join("coordination/runtime/CURRENT-ROUND"),
        format!("{ROUND}\n"),
    )
    .unwrap();
    fs::create_dir_all(root.join("coordination")).unwrap();
    fs::write(root.join("coordination/agents.yaml"), "agents: {}\n").unwrap();
    fs::create_dir_all(root.join(format!("coordination/rounds/{ROUND}/events.jsonl"))).unwrap();
    let append = wake::run_wake(&root, "missing", "wake").unwrap_err();
    let text = format!("{append:#}");
    assert!(text.contains("追加 ActionRejected 失败"));
    assert!(
        text.contains("未注册"),
        "original rejection reason must survive: {text}"
    );
    fs::remove_dir_all(root).ok();
}
