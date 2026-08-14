//! ═══ 红种子契约 · B85（诚实 BLOCKED 落账 · L4/O20）═══
//! 落位: orch/crates/orch-host/tests/blocked_ledger_record.rs（逐字节复制）
//! 预期红（redForm: compile）：`blocked_ledger_events` 与 `blocked_already_recorded` 尚不存在
//! → error[E0432]。
//!
//! 背景：O18(D5/B77) 让 await-report 能**看见** `<task>-BLOCKED.md`，但 `run_await` 的 Blocked
//! 分支直接 return、无任何 ledger::append，CLI 只打印 + exit6。于是 fold 里该任务仍停在
//! Dispatched——`orch status`/daemon/planner 全都看不见「执行者在等授权」。信号到了人眼，
//! 没到账本。后果是 B82 的 `TaskState::Blocked → NeedsOperator` 这条相位**有消费者、没有生产者**：
//! 续跑只会把 BLOCKED 任务当 Dispatched 重新等满 timeout。
//! 本棒补上生产者：新增 `AttemptBlocked` 事件、fold 映射到 `TaskState::Blocked`，
//! 并让 Blocked 分支在**同一 attempt 内幂等**地落账（run-wave 重跑不堆事件）。
//!
//! 语义边界：阻塞**不是终态**——planner 授权后执行者续做并写 REPORT，晚到的 ReportObserved
//! 照常把投影推回 ReadyForVerification（见 blocked_is_not_terminal_later_report_resumes）。
//!
//! 负向变异下界：
//! M1 fold 不认 AttemptBlocked（落回 unknown_kinds）→ fold_maps_attempt_blocked_to_blocked_state 红；
//! M2 只落 EscalationRaised 或两条顺序颠倒或 stage 改名 → blocked_events_are_escalation_then_state_change 红；
//! M3 幂等判据忽略后续 Nudge/Resume（永远 true）或不分 task 作用域 → blocked_record_is_idempotent_within_attempt 红；
//! M4 run_await 的 Blocked 分支不落账／每 tick 重复落账／落账错误被吞 → verifier 代码审查 FAIL。

use orch_core::{fold, EventRecord, TaskState};
use orch_host::tierf::{blocked_already_recorded, blocked_ledger_events};

fn event(kind: &str, task: &str, ts: &str) -> EventRecord {
    EventRecord {
        event_id: format!("{kind}-{task}-{ts}"),
        ts: ts.to_string(),
        actor: "test".to_string(),
        kind: kind.to_string(),
        task_id: Some(task.to_string()),
        round: Some("r41".to_string()),
        payload: None,
        extra: serde_json::Map::new(),
    }
}

fn payload_str(ev: &EventRecord, key: &str) -> Option<String> {
    ev.payload
        .as_ref()?
        .get(key)?
        .as_str()
        .map(str::to_string)
}

#[test]
fn fold_maps_attempt_blocked_to_blocked_state() {
    let events = vec![
        event("DispatchIssued", "B85", "2026-07-25T01:00:00Z"),
        event("AttemptBlocked", "B85", "2026-07-25T02:00:00Z"),
    ];

    let projection = fold(&events);
    assert_eq!(projection.tasks["B85"].state, Some(TaskState::Blocked));
    // 已知事件：不得落进容错兜底桶（否则 daemon/planner 仍读不到语义）
    assert!(
        projection.unknown_kinds.is_empty(),
        "AttemptBlocked 必须进字典，实际 unknown={:?}",
        projection.unknown_kinds
    );
}

#[test]
fn blocked_is_not_terminal_later_report_resumes() {
    // 阻塞→planner 授权→执行者续做并写 REPORT：晚到的 ReportObserved 照常推进投影
    let events = vec![
        event("DispatchIssued", "B85", "2026-07-25T01:00:00Z"),
        event("AttemptBlocked", "B85", "2026-07-25T02:00:00Z"),
        event("NudgeIssued", "B85", "2026-07-25T03:00:00Z"),
        event("ReportObserved", "B85", "2026-07-25T04:00:00Z"),
    ];

    assert_eq!(
        fold(&events).tasks["B85"].state,
        Some(TaskState::ReadyForVerification)
    );
}

#[test]
fn blocked_events_are_escalation_then_state_change() {
    let blocked_rel = "coordination/rounds/r41/reports/B85-BLOCKED.md";
    let events = blocked_ledger_events("B85", "r41", Some("executor-desktop"), blocked_rel);

    assert_eq!(events.len(), 2, "阻塞落账固定两条：升级 + 改态");

    // 顺序是契约：先 EscalationRaised（人/daemon 的处置入口），后 AttemptBlocked（改态）
    assert_eq!(events[0].kind, "EscalationRaised");
    assert_eq!(payload_str(&events[0], "stage").as_deref(), Some("executor-blocked"));
    assert_eq!(payload_str(&events[0], "blockedPath").as_deref(), Some(blocked_rel));

    assert_eq!(events[1].kind, "AttemptBlocked");
    assert_eq!(payload_str(&events[1], "agent").as_deref(), Some("executor-desktop"));
    assert_eq!(payload_str(&events[1], "blockedPath").as_deref(), Some(blocked_rel));

    for ev in &events {
        assert_eq!(ev.task_id.as_deref(), Some("B85"));
        assert_eq!(ev.round.as_deref(), Some("r41"));
    }

    // 单独跑 fold 也必须得到 Blocked——落账内容与投影语义是同一件事
    assert_eq!(fold(&events).tasks["B85"].state, Some(TaskState::Blocked));
}

#[test]
fn blocked_ledger_events_tolerate_unknown_agent() {
    let events = blocked_ledger_events("B85", "r41", None, "reports/B85-BLOCKED.md");
    assert_eq!(events.len(), 2);
    // agent 未知不得让落账整体缺席（账本宁可少一个字段，不可少一条事实）
    assert_eq!(events[1].kind, "AttemptBlocked");
    assert!(payload_str(&events[1], "agent").is_none());
}

#[test]
fn blocked_record_is_idempotent_within_attempt() {
    let dispatched = event("DispatchIssued", "B85", "2026-07-25T01:00:00Z");
    let blocked = event("AttemptBlocked", "B85", "2026-07-25T02:00:00Z");

    // 本 attempt 已记 → 重跑 await/run-wave 不得再堆一条
    assert!(blocked_already_recorded(
        &[dispatched.clone(), blocked.clone()],
        "B85"
    ));

    // 只派发过、尚未记 → 必须落账
    assert!(!blocked_already_recorded(&[dispatched.clone()], "B85"));

    // 新控制信号（nudge/resume/重派）开启新 attempt → 旧记录不再抵账
    for signal in ["NudgeIssued", "ResumeIssued", "DispatchIssued"] {
        let events = vec![
            dispatched.clone(),
            blocked.clone(),
            event(signal, "B85", "2026-07-25T03:00:00Z"),
        ];
        assert!(
            !blocked_already_recorded(&events, "B85"),
            "{signal} 之后必须允许重新落账"
        );
    }

    // task 作用域：别人的阻塞不能抵自己的账
    let events = vec![
        dispatched.clone(),
        event("AttemptBlocked", "OTHER", "2026-07-25T02:00:00Z"),
    ];
    assert!(!blocked_already_recorded(&events, "B85"));
}
