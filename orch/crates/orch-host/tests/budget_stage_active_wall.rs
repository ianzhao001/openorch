//! ═══ 红种子契约 · B182（stage-aware active wall）═══
//! 落位: orch/crates/orch-host/tests/budget_stage_active_wall.rs（逐字节复制）
//! 预期红（redForm: compile）：`active_stage_wall_mins` / `ActiveWallBreakdown`
//! 尚不存在，rustc 必须报 `error[E0432]: unresolved imports ...`。
//!
//! 事故：旧 `active_attempt_wall_mins` 只在 dispatch/nudge/resume 开窗，并一直等到
//! TaskRecorded/attempt failure 才关窗。于是 canonical ReportObserved 已把任务投影推进
//! ready_for_verification 后，REPORT→review request、review delivery→verdict/merge/record 的
//! planner 空等仍全被冒充 implementation wall；r58 因此显示 2587/1500 假耗尽。
//!
//! 本 seed 要求纯折叠把 implementation 与 review 分开：
//! - implementation identity = (taskId, payload.attemptId)；
//! - review identity = (taskId, payload.attemptId, payload.role, payload.agent)；
//! - canonical ReportObserved 只关闭同 attempt 的 implementation；
//! - ReviewRequested/ReviewDelivered 独立开关精确 reviewer 窗；
//! - 阶段之间空闲不计，仍 in-flight 的窗计到显式 now；
//! - 同身份重复 open 不得重置起点，review 已 delivered 后不得被重复 request 重开；
//! - identity 缺失或错配的 ReportObserved/ReviewDelivered 不得越权关别人的窗。

use orch_core::EventRecord;
use orch_host::budget::{active_stage_wall_mins, ActiveWallBreakdown};

fn ev(id: &str, kind: &str, ts: &str, task: &str, payload: serde_json::Value) -> EventRecord {
    EventRecord {
        event_id: id.to_string(),
        ts: ts.to_string(),
        actor: "runtime:test".to_string(),
        kind: kind.to_string(),
        task_id: Some(task.to_string()),
        round: Some("r58".to_string()),
        payload: Some(payload),
        extra: serde_json::Map::new(),
    }
}

fn attempt(attempt_id: &str) -> serde_json::Value {
    serde_json::json!({"attemptId": attempt_id})
}

fn review(attempt_id: &str, role: &str, agent: &str) -> serde_json::Value {
    serde_json::json!({
        "attemptId": attempt_id,
        "role": role,
        "agent": agent,
    })
}

#[test]
fn report_and_post_review_idle_gaps_are_not_active_wall() {
    let events = vec![
        ev(
            "d",
            "DispatchIssued",
            "2026-07-30T10:00:00Z",
            "B182",
            attempt("B182-A0001"),
        ),
        ev(
            "r",
            "ReportObserved",
            "2026-07-30T10:10:00Z",
            "B182",
            attempt("B182-A0001"),
        ),
        // 10:10→11:00 是 planner 等待，不属于任何 active window。
        ev(
            "q",
            "ReviewRequested",
            "2026-07-30T11:00:00Z",
            "B182",
            review("B182-A0001", "primary", "executor-claw"),
        ),
        ev(
            "x",
            "ReviewDelivered",
            "2026-07-30T11:20:00Z",
            "B182",
            review("B182-A0001", "primary", "executor-claw"),
        ),
        // 11:20→13:00 的 verdict/merge/record 等待同样不计。
        ev(
            "t",
            "TaskRecorded",
            "2026-07-30T13:00:00Z",
            "B182",
            serde_json::json!({}),
        ),
    ];

    let got = active_stage_wall_mins(&events, "2026-07-30T14:00:00Z");
    assert_eq!(
        got,
        ActiveWallBreakdown {
            implementation_mins: 10,
            review_mins: 20,
        }
    );
    assert_eq!(got.total_mins(), 30);
}

#[test]
fn early_report_cannot_hide_a_real_in_flight_reviewer() {
    let events = vec![
        ev(
            "d",
            "DispatchIssued",
            "2026-07-30T10:00:00Z",
            "B182",
            attempt("B182-A0001"),
        ),
        ev(
            "r",
            "ReportObserved",
            "2026-07-30T10:01:00Z",
            "B182",
            attempt("B182-A0001"),
        ),
        ev(
            "q",
            "ReviewRequested",
            "2026-07-30T10:02:00Z",
            "B182",
            review("B182-A0001", "primary", "executor-claw"),
        ),
    ];

    let got = active_stage_wall_mins(&events, "2026-07-30T10:32:00Z");
    assert_eq!(got.implementation_mins, 1);
    assert_eq!(
        got.review_mins, 30,
        "ReportObserved 不能吞掉真实 reviewer in-flight wall"
    );
    assert_eq!(got.total_mins(), 31);
}

#[test]
fn review_delivery_requires_the_full_exact_identity() {
    let events = vec![
        ev(
            "q",
            "ReviewRequested",
            "2026-07-30T10:00:00Z",
            "B182",
            review("B182-A0001", "primary", "executor-claw"),
        ),
        ev(
            "wrong-attempt",
            "ReviewDelivered",
            "2026-07-30T10:05:00Z",
            "B182",
            review("B182-A0002", "primary", "executor-claw"),
        ),
        ev(
            "wrong-role",
            "ReviewDelivered",
            "2026-07-30T10:06:00Z",
            "B182",
            review("B182-A0001", "secondary", "executor-claw"),
        ),
        ev(
            "wrong-agent",
            "ReviewDelivered",
            "2026-07-30T10:07:00Z",
            "B182",
            review("B182-A0001", "primary", "executor-opencode"),
        ),
        ev(
            "x",
            "ReviewDelivered",
            "2026-07-30T10:10:00Z",
            "B182",
            review("B182-A0001", "primary", "executor-claw"),
        ),
    ];

    let got = active_stage_wall_mins(&events, "2026-07-30T11:00:00Z");
    assert_eq!(got.review_mins, 10);
    assert_eq!(got.implementation_mins, 0);
}

#[test]
fn duplicate_lifecycle_events_are_identity_idempotent() {
    let events = vec![
        ev(
            "d1",
            "DispatchIssued",
            "2026-07-30T10:00:00Z",
            "B182",
            attempt("B182-A0001"),
        ),
        // 同 implementation identity 的重复 open 不得把起点推迟到 10:05。
        ev(
            "d2",
            "NudgeIssued",
            "2026-07-30T10:05:00Z",
            "B182",
            attempt("B182-A0001"),
        ),
        ev(
            "r",
            "ReportObserved",
            "2026-07-30T10:10:00Z",
            "B182",
            attempt("B182-A0001"),
        ),
        ev(
            "q1",
            "ReviewRequested",
            "2026-07-30T10:20:00Z",
            "B182",
            review("B182-A0001", "primary", "executor-claw"),
        ),
        // 同 review identity 的重发也不得把起点推迟到 10:25。
        ev(
            "q2",
            "ReviewRequested",
            "2026-07-30T10:25:00Z",
            "B182",
            review("B182-A0001", "primary", "executor-claw"),
        ),
        ev(
            "x1",
            "ReviewDelivered",
            "2026-07-30T10:30:00Z",
            "B182",
            review("B182-A0001", "primary", "executor-claw"),
        ),
        ev(
            "x2",
            "ReviewDelivered",
            "2026-07-30T10:35:00Z",
            "B182",
            review("B182-A0001", "primary", "executor-claw"),
        ),
        // delivery 已终结该四元组，迟到/重放 request 不得重新制造 in-flight。
        ev(
            "q3",
            "ReviewRequested",
            "2026-07-30T10:40:00Z",
            "B182",
            review("B182-A0001", "primary", "executor-claw"),
        ),
    ];

    let got = active_stage_wall_mins(&events, "2026-07-30T11:40:00Z");
    assert_eq!(got.implementation_mins, 10);
    assert_eq!(got.review_mins, 10);
    assert_eq!(got.total_mins(), 20);
}

#[test]
fn attempts_and_parallel_reviewer_roles_have_independent_keys() {
    let events = vec![
        ev(
            "d1",
            "DispatchIssued",
            "2026-07-30T10:00:00Z",
            "B182",
            attempt("B182-A0001"),
        ),
        ev(
            "r1",
            "ReportObserved",
            "2026-07-30T10:05:00Z",
            "B182",
            attempt("B182-A0001"),
        ),
        ev(
            "d2",
            "DispatchIssued",
            "2026-07-30T10:10:00Z",
            "B182",
            attempt("B182-A0002"),
        ),
        // A0001 的迟到 report 绝不能关闭 A0002。
        ev(
            "r1-late",
            "ReportObserved",
            "2026-07-30T10:15:00Z",
            "B182",
            attempt("B182-A0001"),
        ),
        ev(
            "qp",
            "ReviewRequested",
            "2026-07-30T10:20:00Z",
            "B182",
            review("B182-A0001", "primary", "executor-claw"),
        ),
        ev(
            "qs",
            "ReviewRequested",
            "2026-07-30T10:25:00Z",
            "B182",
            review("B182-A0001", "secondary", "executor-opencode"),
        ),
        ev(
            "xp",
            "ReviewDelivered",
            "2026-07-30T10:30:00Z",
            "B182",
            review("B182-A0001", "primary", "executor-claw"),
        ),
        ev(
            "xs",
            "ReviewDelivered",
            "2026-07-30T10:45:00Z",
            "B182",
            review("B182-A0001", "secondary", "executor-opencode"),
        ),
    ];

    let got = active_stage_wall_mins(&events, "2026-07-30T10:50:00Z");
    // implementation: A0001=5 + A0002 in-flight=40；review: primary=10 + secondary=20。
    assert_eq!(got.implementation_mins, 45);
    assert_eq!(got.review_mins, 30);
    assert_eq!(got.total_mins(), 75);
}

#[test]
fn blocked_then_resume_reopens_without_counting_the_idle_gap() {
    let events = vec![
        ev(
            "d",
            "DispatchIssued",
            "2026-07-30T10:00:00Z",
            "B182",
            attempt("B182-A0001"),
        ),
        ev(
            "b",
            "AttemptBlocked",
            "2026-07-30T10:10:00Z",
            "B182",
            attempt("B182-A0001"),
        ),
        ev(
            "n",
            "ResumeIssued",
            "2026-07-30T10:40:00Z",
            "B182",
            attempt("B182-A0001"),
        ),
        ev(
            "r",
            "ReportObserved",
            "2026-07-30T10:45:00Z",
            "B182",
            attempt("B182-A0001"),
        ),
    ];

    let got = active_stage_wall_mins(&events, "2026-07-30T12:00:00Z");
    assert_eq!(got.implementation_mins, 15);
    assert_eq!(got.review_mins, 0);
}

#[test]
fn malformed_or_wrong_report_identity_cannot_close_a_live_attempt() {
    let events = vec![
        ev(
            "d",
            "DispatchIssued",
            "2026-07-30T10:00:00Z",
            "B182",
            attempt("B182-A0001"),
        ),
        ev(
            "missing",
            "ReportObserved",
            "2026-07-30T10:03:00Z",
            "B182",
            serde_json::json!({}),
        ),
        ev(
            "wrong",
            "ReportObserved",
            "2026-07-30T10:05:00Z",
            "B182",
            attempt("B182-A0002"),
        ),
        ev(
            "right",
            "ReportObserved",
            "2026-07-30T10:10:00Z",
            "B182",
            attempt("B182-A0001"),
        ),
        ev(
            "bad-review",
            "ReviewRequested",
            "2026-07-30T10:11:00Z",
            "B182",
            serde_json::json!({"attemptId":"B182-A0001","role":"primary"}),
        ),
    ];

    let got = active_stage_wall_mins(&events, "2026-07-30T11:00:00Z");
    assert_eq!(got.implementation_mins, 10);
    assert_eq!(got.review_mins, 0);
}

#[test]
fn legacy_task_only_windows_keep_the_b73_b86_fallback() {
    let events = vec![
        ev(
            "d",
            "DispatchIssued",
            "2026-07-30T10:00:00Z",
            "B182",
            serde_json::json!({}),
        ),
        // Legacy/malformed ReportObserved remains non-terminal because it lacks exact attempt identity.
        ev(
            "r",
            "ReportObserved",
            "2026-07-30T10:05:00Z",
            "B182",
            serde_json::json!({}),
        ),
        ev(
            "t",
            "TaskRecorded",
            "2026-07-30T10:10:00Z",
            "B182",
            serde_json::json!({}),
        ),
    ];

    let got = active_stage_wall_mins(&events, "2026-07-30T11:00:00Z");
    assert_eq!(got.implementation_mins, 10);
    assert_eq!(got.review_mins, 0);
}
