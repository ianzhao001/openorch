//! ═══ B97 recovery revision 7 · durable action / archive contract ═══
//! 预期红（redForm: compile）：durable action fold API 尚不存在。
//! 负向变异：
//! R1 Released 不校验 owner；R2 Released 不校验 leaseGeneration；
//! R3 Delivered/Executed/Completed 不校验合法前驱；
//! R4 REPORT terminal 不绑定 evidence 与 pinned branch；
//! R5 legacy resolver 只看 live source，不折叠 archived destination；
//! R6 same-inode ack 授权降级为 bool，GO mutation 前不重验对象身份；
//! R7 tierf 三条生产路径不调用共同的 durable fold。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use orch_core::EventRecord;
use orch_host::attempt::{
    archive_superseded_dispatch_with_hook, fold_durable_action, resolve_superseded_go, AttemptRef,
    DispatchRecord, DurableActionExpectation, DurableActionKind, DurableActionPhase,
};
use orch_host::liveness::{
    attempt_current_elapsed, judge, probe_with_durable_elapsed, Judgement, LivenessOpts,
};

fn event(kind: &str, payload: serde_json::Value) -> EventRecord {
    EventRecord {
        event_id: format!("event-{kind}"),
        ts: "2026-07-25T00:00:00Z".into(),
        actor: "runtime:test".into(),
        kind: kind.into(),
        task_id: Some("B97".into()),
        round: Some("r44".into()),
        payload: Some(payload),
        extra: serde_json::Map::new(),
    }
}

fn expectation(branch_sha: Option<&str>) -> DurableActionExpectation {
    DurableActionExpectation {
        round: "r44".into(),
        task_id: "B97".into(),
        attempt_id: "B97-A0004".into(),
        attempt_no: 4,
        agent: "executor-opencode".into(),
        base_sha: "base-concrete".into(),
        go_path: "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0004.md".into(),
        action_id: "action-4".into(),
        evidence_sha256: Some("report-digest".into()),
        evidence_len: Some(42),
        control_epoch: Some("epoch-4".into()),
        branch_sha: branch_sha.map(str::to_owned),
    }
}

fn state_payload(
    owner: Option<&str>,
    generation: Option<&str>,
    branch_sha: Option<&str>,
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "attemptId": "B97-A0004",
        "attemptNo": 4,
        "agent": "executor-opencode",
        "baseSha": "base-concrete",
        "goPath": "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0004.md",
        "actionId": "action-4",
        "evidenceSha256": "report-digest",
        "evidenceLen": 42,
        "controlEpoch": "epoch-4",
    });
    if let Some(owner) = owner {
        payload["owner"] = serde_json::json!(owner);
    }
    if let Some(generation) = generation {
        payload["leaseGeneration"] = serde_json::json!(generation);
    }
    if let Some(branch_sha) = branch_sha {
        payload["branchSha"] = serde_json::json!(branch_sha);
    }
    payload
}

#[test]
fn dispatch_release_must_match_current_owner_and_generation() {
    let base = vec![
        event(
            "DispatchWakeClaimed",
            state_payload(Some("owner-1"), Some("generation-1"), None),
        ),
        event(
            "DispatchWakeLaunching",
            state_payload(Some("owner-1"), Some("generation-1"), None),
        ),
    ];

    let mut wrong_owner = base.clone();
    wrong_owner.push(event(
        "DispatchWakeReleased",
        state_payload(Some("owner-2"), Some("generation-1"), None),
    ));
    assert!(fold_durable_action(
        &wrong_owner,
        DurableActionKind::DispatchWake,
        &expectation(None)
    )
    .is_err());

    let mut wrong_generation = base.clone();
    wrong_generation.push(event(
        "DispatchWakeReleased",
        state_payload(Some("owner-1"), Some("generation-0"), None),
    ));
    assert!(fold_durable_action(
        &wrong_generation,
        DurableActionKind::DispatchWake,
        &expectation(None)
    )
    .is_err());

    let mut missing_owner = base;
    missing_owner.push(event(
        "DispatchWakeReleased",
        state_payload(None, Some("generation-1"), None),
    ));
    assert!(fold_durable_action(
        &missing_owner,
        DurableActionKind::DispatchWake,
        &expectation(None)
    )
    .is_err());
}

#[test]
fn resume_delivery_and_completion_require_the_same_legal_lineage() {
    let delivered_without_launch = vec![
        event(
            "ResumeWakeClaimed",
            state_payload(Some("owner-1"), Some("generation-1"), None),
        ),
        event(
            "ResumeWakeDelivered",
            state_payload(Some("owner-1"), Some("generation-1"), None),
        ),
    ];
    assert!(fold_durable_action(
        &delivered_without_launch,
        DurableActionKind::ResumeWake,
        &expectation(None)
    )
    .is_err());

    let legal = vec![
        event(
            "ResumeWakeClaimed",
            state_payload(Some("owner-1"), Some("generation-1"), None),
        ),
        event(
            "ResumeWakeLaunching",
            state_payload(Some("owner-1"), Some("generation-1"), None),
        ),
        event(
            "ResumeWakeDelivered",
            state_payload(Some("owner-1"), Some("generation-1"), None),
        ),
        event(
            "ResumeWakeCompleted",
            state_payload(Some("owner-1"), Some("generation-1"), None),
        ),
    ];
    assert_eq!(
        fold_durable_action(&legal, DurableActionKind::ResumeWake, &expectation(None)).unwrap(),
        DurableActionPhase::Completed
    );
}

#[test]
fn report_terminal_requires_executing_and_exact_pinned_facts() {
    let executed_without_executing = vec![
        event(
            "ReportCollectClaimed",
            state_payload(Some("owner-1"), Some("generation-1"), None),
        ),
        event(
            "ReportCollectExecuted",
            state_payload(Some("owner-1"), Some("generation-1"), Some("wrong-sha")),
        ),
    ];
    assert!(fold_durable_action(
        &executed_without_executing,
        DurableActionKind::ReportCollect,
        &expectation(Some("pinned-sha"))
    )
    .is_err());

    let wrong_pinned_sha = vec![
        event(
            "ReportCollectClaimed",
            state_payload(Some("owner-1"), Some("generation-1"), None),
        ),
        event(
            "ReportCollectExecuting",
            state_payload(Some("owner-1"), Some("generation-1"), None),
        ),
        event(
            "ReportCollectExecuted",
            state_payload(Some("owner-1"), Some("generation-1"), Some("wrong-sha")),
        ),
        event(
            "ReportCollectCompleted",
            state_payload(Some("owner-1"), Some("generation-1"), Some("wrong-sha")),
        ),
    ];
    assert!(fold_durable_action(
        &wrong_pinned_sha,
        DurableActionKind::ReportCollect,
        &expectation(Some("pinned-sha"))
    )
    .is_err());

    let legal = vec![
        event(
            "ReportCollectClaimed",
            state_payload(Some("owner-1"), Some("generation-1"), None),
        ),
        event(
            "ReportCollectExecuting",
            state_payload(Some("owner-1"), Some("generation-1"), None),
        ),
        event(
            "ReportCollectExecuted",
            state_payload(Some("owner-1"), Some("generation-1"), Some("pinned-sha")),
        ),
        event(
            "ReportCollectCompleted",
            state_payload(Some("owner-1"), Some("generation-1"), Some("pinned-sha")),
        ),
    ];
    assert_eq!(
        fold_durable_action(
            &legal,
            DurableActionKind::ReportCollect,
            &expectation(Some("pinned-sha"))
        )
        .unwrap(),
        DurableActionPhase::Completed
    );
}

#[test]
fn report_release_cannot_clear_another_collect_owner() {
    let events = vec![
        event(
            "ReportCollectClaimed",
            state_payload(Some("owner-1"), Some("generation-1"), None),
        ),
        event(
            "ReportCollectExecuting",
            state_payload(Some("owner-1"), Some("generation-1"), None),
        ),
        event(
            "ReportCollectReleased",
            state_payload(Some("owner-2"), Some("generation-1"), None),
        ),
    ];
    assert!(fold_durable_action(
        &events,
        DurableActionKind::ReportCollect,
        &expectation(Some("pinned-sha"))
    )
    .is_err());
}

fn unique_root(tag: &str) -> PathBuf {
    orch_host::util::test_scratch_dir(&format!("b97-recovery-{tag}"))
}

fn legacy_record() -> DispatchRecord {
    DispatchRecord {
        attempt: AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        },
        agent: "executor-opencode".into(),
        base_sha: Some("legacy-base".into()),
        go_path: "coordination/rounds/r44/dispatch/executor-opencode/GO-B97.md".into(),
        is_legacy: true,
        previous_attempt_id: None,
        previous_agent: None,
        reassignment: false,
        has_companion_reassignment: false,
        wake_pending: false,
        wake_completed: false,
    }
}

#[test]
fn legacy_destination_only_replay_resolves_one_logical_go() {
    let root = unique_root("legacy-destination");
    let archived = root.join(
        "coordination/rounds/r44/dispatch/executor-opencode/\
         superseded/B97-A0001/GO-B97.md"
            .replace(' ', ""),
    );
    fs::create_dir_all(archived.parent().unwrap()).unwrap();
    fs::write(&archived, b"archived-go").unwrap();

    let source_rel = resolve_superseded_go(&root, "r44", &legacy_record()).unwrap();
    assert_eq!(
        source_rel,
        "coordination/rounds/r44/dispatch/executor-opencode/GO-B97.md"
    );
    archive_superseded_dispatch_with_hook(
        &root,
        &source_rel,
        "B97-A0001",
        "executor-opencode",
        &mut |_| Ok(()),
    )
    .unwrap();
    assert_eq!(fs::read(&archived).unwrap(), b"archived-go");
    fs::remove_dir_all(root).unwrap();
}

fn same_inode_paths(root: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf, String) {
    let go_rel = "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0001.md".to_string();
    let go_src = root.join(&go_rel);
    let ack_src = PathBuf::from(format!("{}.ack", go_src.display()));
    let go_dst = root.join(
        "coordination/rounds/r44/dispatch/executor-opencode/\
         superseded/B97-A0001/GO-B97-A0001.md"
            .replace(' ', ""),
    );
    let ack_dst = PathBuf::from(format!("{}.ack", go_dst.display()));
    (go_src, ack_src, go_dst, ack_dst, go_rel)
}

#[test]
fn ack_authorization_is_rechecked_after_same_inode_partial_recovery() {
    let root = unique_root("ack-object-identity");
    let (go_src, ack_src, _go_dst, ack_dst, go_rel) = same_inode_paths(&root);
    fs::create_dir_all(go_src.parent().unwrap()).unwrap();
    fs::create_dir_all(ack_dst.parent().unwrap()).unwrap();
    fs::write(&go_src, b"go-bytes").unwrap();
    fs::write(&ack_src, b"authorized-ack").unwrap();
    fs::hard_link(&ack_src, &ack_dst).unwrap();

    let result = archive_superseded_dispatch_with_hook(
        &root,
        &go_rel,
        "B97-A0001",
        "executor-opencode",
        &mut |phase| {
            if phase == "before-move" {
                fs::remove_file(&ack_dst)?;
                fs::write(&ack_dst, b"forged-ack")?;
            }
            Ok(())
        },
    );
    assert!(result.is_err());
    assert!(
        go_src.exists(),
        "GO must remain live after authorization loss"
    );
    assert_eq!(fs::read(&ack_dst).unwrap(), b"forged-ack");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn all_three_tierf_paths_use_the_common_durable_fold() {
    let source = include_str!("../src/tierf.rs");
    assert!(
        source.matches("fold_durable_action").count() >= 3,
        "dispatch, collect, and resume must each consume the common validated fold"
    );
}

// ═══ B112-A0004 · durable elapsed 不经下溢 Instant（P0-DURABLE-ELAPSED-CLAMP 修复回归）═══
// A0003 用 `Instant::now().checked_sub(elapsed).unwrap_or_else(Instant::now)` 恢复
// attempt 时钟：刚开机且 durable elapsed > monotonic uptime 时 checked_sub 下溢，
// grace 被静默清零（已持续很久的 attempt 被重新置入 boot/cold grace）。A0004 拆除
// 该载体：durable_base 是账本派生的 Duration 数值，当前 elapsed = durable_base +
// captured_at.elapsed()。以下三测试钉死该契约；M4 变异（probe 忽略 durable_elapsed
// 退化为 started.elapsed()）必须使前两个测试同时变红。

#[test]
fn probe_with_durable_elapsed_overrides_started() {
    // durable elapsed 不经过 Instant：进程内 started≈now（started.elapsed()≈0），
    // 显式 durable 3600s 必须原样进入快照。若实现退化回 started.elapsed()
    // （A0003 下溢路径的等价形态），本测试红（≈0 ≠ 3600）。
    // 密封：task_id 无对应分支/worktree（commit_unix_time Err → 零活动信号），
    // 本测试结果与 scratch 所在仓的分支 tip 年龄无关。
    let root = unique_root("probe-durable-override");
    let durable = Duration::from_secs(3600);
    let snap = probe_with_durable_elapsed(
        &root,
        "agent-x",
        "B112-NO-SUCH-TASK",
        None,
        &[],
        Instant::now(),
        Some(durable),
    );
    assert_eq!(snap.elapsed, durable);
    // None ⇒ legacy 进程内计时（started.elapsed()≈0，远小于 durable）
    let legacy = probe_with_durable_elapsed(
        &root,
        "agent-x",
        "B112-NO-SUCH-TASK",
        None,
        &[],
        Instant::now(),
        None,
    );
    assert!(legacy.elapsed < Duration::from_secs(60));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cold_boot_recovery_of_long_attempt_not_reset_to_grace() {
    // 刚开机生产场景：进程 started≈0（monotonic uptime 远小于 3600s），恢复一个
    // durable elapsed=3600s 的 attempt（无心跳、无 ack、无 worktree 活动——密封
    // task_id，分支/worktree 均不存在）。durable 必须直接进入判定：
    // 3600s > cold_boot_grace 300s ⇒ DeadCandidate；不得因 started.elapsed()≈0
    // 被误置 booting——那正是 A0003 下溢清零的退化。
    let root = unique_root("cold-boot-long-attempt");
    let snap = probe_with_durable_elapsed(
        &root,
        "agent-x",
        "B112-NO-SUCH-TASK",
        None,
        &[],
        Instant::now(),
        Some(Duration::from_secs(3600)),
    );
    let opts = LivenessOpts {
        cold_boot_grace: Duration::from_secs(300),
        ..LivenessOpts::default()
    };
    let verdict = judge(&snap, &opts);
    assert!(
        matches!(verdict, Judgement::DeadCandidate(_)),
        "cold boot 恢复 3600s attempt 不得落入 boot/cold grace: {verdict:?}"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn durable_elapsed_keeps_growing_after_recovery() {
    // 恢复后的当前 elapsed = durable_base + captured_at.elapsed()：必须随本进程
    // monotonic delta 继续增长，不得冻结为不增长快照；且 durable_base 可以远超
    // monotonic 可回溯范围而不下溢（durable 是 Duration 数值，从不进入 Instant）。
    let durable_base = Duration::from_secs(3600);
    let captured_at = Instant::now();
    let session_start = Instant::now();
    std::thread::sleep(Duration::from_millis(80));
    let first = attempt_current_elapsed(Some((durable_base, captured_at)), session_start);
    assert!(first >= durable_base + Duration::from_millis(80));
    assert!(first < durable_base + Duration::from_secs(30));
    // 增长性：再次采样严格大于首次（非冻结快照）
    std::thread::sleep(Duration::from_millis(40));
    let second = attempt_current_elapsed(Some((durable_base, captured_at)), session_start);
    assert!(second > first);
    // None（无 DispatchIssued）⇒ 会话进程内计时
    let none_elapsed = attempt_current_elapsed(None, session_start);
    assert!(none_elapsed >= Duration::from_millis(120));
    assert!(none_elapsed < Duration::from_secs(30));
    // durable_base 超越任何现实 uptime（一年）也不下溢、不饱和
    let huge = attempt_current_elapsed(
        Some((Duration::from_secs(365 * 24 * 3600), captured_at)),
        session_start,
    );
    assert!(huge >= Duration::from_secs(365 * 24 * 3600));
}
