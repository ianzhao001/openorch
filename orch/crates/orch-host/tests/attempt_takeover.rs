//! ═══ 红种子契约 · B97 ═══
//! 预期红（redForm: compile）：attempt/takeover API 尚不存在。
//! 变异清单：
//! M1 attempt 固定 1；M2 active attempt 可被接管；M3 takeover 刷新当前 main；
//! M4 ack/terminal 只按 task 判重；M5 stale evidence 用 >= 或无 marker；
//! M6 GO 不带 attempt 或复用旧路径；M7 已有 branch/worktree 仍要求 -b；
//! M8 snapshot 漏 staged/unstaged/untracked；M9 snapshot 改动真实 index/worktree。

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use orch_core::EventRecord;
use orch_host::attempt::{
    attempt_event_already_recorded, attempt_paths, current_attempt, evidence_is_current,
    plan_next_attempt, snapshot_worktree_wip, AttemptRef,
};

static TAKEOVER_SEQ: AtomicU64 = AtomicU64::new(0);

fn event(
    id: &str,
    kind: &str,
    task: &str,
    payload: serde_json::Value,
) -> EventRecord {
    EventRecord {
        event_id: id.into(),
        ts: "2026-07-25T00:00:00Z".into(),
        actor: "runtime:test".into(),
        kind: kind.into(),
        task_id: Some(task.into()),
        round: Some("r44".into()),
        payload: Some(payload),
        extra: serde_json::Map::new(),
    }
}

fn dispatch(
    id: &str,
    task: &str,
    agent: &str,
    base: &str,
    attempt_id: Option<&str>,
) -> EventRecord {
    let mut payload = serde_json::json!({
        "agent": agent,
        "baseSha": base,
    });
    if let Some(attempt_id) = attempt_id {
        payload["attemptId"] = serde_json::json!(attempt_id);
    }
    event(id, "DispatchIssued", task, payload)
}

#[test]
fn attempts_are_monotonic_per_task_and_legacy_dispatches_are_counted() {
    let events = vec![
        dispatch("d1", "B97", "executor-claw", "base0", None),
        event("x1", "AttemptCrashed", "B97", serde_json::json!({})),
        dispatch("d2", "B97", "executor-opencode", "base0", None),
        dispatch("o1", "OTHER", "executor-claw", "other", None),
    ];
    assert_eq!(
        current_attempt(&events, "B97").unwrap(),
        Some(AttemptRef {
            task_id: "B97".into(),
            ordinal: 2,
            attempt_id: "B97-A0002".into(),
        })
    );
    assert_eq!(
        current_attempt(&events, "OTHER").unwrap().unwrap().ordinal,
        1
    );
}

#[test]
fn active_attempt_rejects_takeover_but_terminal_attempt_preserves_original_base() {
    let mut events = vec![dispatch(
        "d1",
        "B97",
        "executor-claw",
        "original-base",
        Some("B97-A0001"),
    )];
    assert!(plan_next_attempt(
        &events,
        "B97",
        "executor-opencode",
        "new-main",
        "r44"
    )
    .is_err());

    events.push(event(
        "f1",
        "AttemptCrashed",
        "B97",
        serde_json::json!({"attemptId":"B97-A0001"}),
    ));
    let plan = plan_next_attempt(
        &events,
        "B97",
        "executor-opencode",
        "new-main",
        "r44",
    )
    .unwrap();
    assert_eq!(plan.attempt.ordinal, 2);
    assert_eq!(plan.attempt.attempt_id, "B97-A0002");
    assert_eq!(plan.base_sha, "original-base");
    assert_eq!(plan.branch, "task/B97");
    assert_eq!(plan.worktree_rel, ".worktrees/B97");
    assert_eq!(
        plan.previous_attempt.unwrap().attempt_id,
        "B97-A0001"
    );
}

#[test]
fn first_attempt_uses_concrete_main_and_paths_are_attempt_scoped_only_for_go_ack() {
    let plan = plan_next_attempt(
        &[],
        "B97",
        "executor-claw",
        "concrete-main",
        "r44",
    )
    .unwrap();
    assert_eq!(plan.attempt.attempt_id, "B97-A0001");
    assert_eq!(plan.base_sha, "concrete-main");
    let paths = attempt_paths("r44", "executor-claw", &plan.attempt);
    assert_eq!(
        paths.go_rel,
        "coordination/rounds/r44/dispatch/executor-claw/GO-B97-A0001.md"
    );
    assert_eq!(paths.ack_rel, format!("{}.ack", paths.go_rel));
    assert_eq!(
        paths.report_rel,
        "coordination/rounds/r44/reports/B97-REPORT.md"
    );
    assert_eq!(
        paths.blocked_rel,
        "coordination/rounds/r44/reports/B97-BLOCKED.md"
    );
}

#[test]
fn evidence_marker_is_strict_and_attempt_event_idempotency_is_scoped() {
    let marker = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
    assert!(!evidence_is_current(Some(marker), Some(marker)));
    assert!(!evidence_is_current(
        Some(marker - Duration::from_secs(1)),
        Some(marker)
    ));
    assert!(evidence_is_current(
        Some(marker + Duration::from_secs(1)),
        Some(marker)
    ));
    assert!(!evidence_is_current(Some(marker), None));

    let events = vec![
        event(
            "a1",
            "DispatchAcked",
            "B97",
            serde_json::json!({"attemptId":"B97-A0001"}),
        ),
        event(
            "a2",
            "DispatchAcked",
            "B97",
            serde_json::json!({"attemptId":"B97-A0002"}),
        ),
    ];
    assert!(attempt_event_already_recorded(
        &events,
        "B97-A0002",
        "DispatchAcked"
    ));
    assert!(!attempt_event_already_recorded(
        &events,
        "B97-A0003",
        "DispatchAcked"
    ));
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_takeover_repo() -> std::path::PathBuf {
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seq = TAKEOVER_SEQ.fetch_add(1, Ordering::Relaxed);
    let orch_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("CARGO_MANIFEST_DIR 上溯两级应为 orch 工作区根");
    let root = orch_root
        .join("target/test-tmp")
        .join(format!("orch-b97-takeover-{}-{unique}-{seq}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q"]);
    fs::write(root.join("tracked.txt"), "base\n").unwrap();
    git(&root, &["add", "tracked.txt"]);
    git(
        &root,
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
    fs::create_dir_all(root.join(".worktrees")).unwrap();
    let wt = root.join(".worktrees/B97");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "task/B97",
            wt.to_str().unwrap(),
            "HEAD",
        ],
    );
    root
}

#[test]
fn wip_snapshot_captures_all_dirty_kinds_without_mutating_live_site() {
    let root = init_takeover_repo();
    let wt = root.join(".worktrees/B97");
    fs::write(wt.join("tracked.txt"), "staged\n").unwrap();
    git(&wt, &["add", "tracked.txt"]);
    fs::write(wt.join("tracked.txt"), "unstaged\n").unwrap();
    fs::write(wt.join("untracked.txt"), "untracked\n").unwrap();
    let before_status = git(&wt, &["status", "--porcelain=v2"]);
    let before_head = git(&wt, &["rev-parse", "HEAD"]);
    let before_index = fs::read(git(&wt, &["rev-parse", "--git-path", "index"])).unwrap();

    let snapshot = snapshot_worktree_wip(
        &root,
        "r44",
        &AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        },
    )
    .unwrap()
    .unwrap();
    assert!(snapshot.dirty);
    assert_eq!(snapshot.archive_ref, "archive/r44-B97-A0001-wip");
    let tree = git(
        &root,
        &["ls-tree", "-r", "--name-only", &snapshot.snapshot_sha],
    );
    assert!(tree.lines().any(|line| line == "tracked.txt"));
    assert!(tree.lines().any(|line| line == "untracked.txt"));
    assert_eq!(git(&wt, &["status", "--porcelain=v2"]), before_status);
    assert_eq!(git(&wt, &["rev-parse", "HEAD"]), before_head);
    assert_eq!(
        fs::read(git(&wt, &["rev-parse", "--git-path", "index"])).unwrap(),
        before_index
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn snapshot_rejects_worktree_on_wrong_branch() {
    let root = init_takeover_repo();
    let wt = root.join(".worktrees/B97");
    git(&wt, &["checkout", "-q", "--detach"]);
    let result = snapshot_worktree_wip(
        &root,
        "r44",
        &AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        },
    );
    assert!(result.is_err());
    let _ = fs::remove_dir_all(root);
}
