//! B202 · Review 生命周期词表与状态中性合同。
//!
//! 首红必须是 E0432：生产侧尚未导出唯一目录 seam `REVIEW_LIFECYCLE_EVENT_KINDS`。
//!
//! M1/M2：删除任一 Review* 目录项必须红；
//! M3：让任一 Review* 改变 TaskState 必须红；
//! M4：移除 ReportObserved 的既有就绪投影必须红。

use orch_core::{
    fold, is_known_event_kind, known_event_kinds, EventRecord, TaskState,
    REVIEW_LIFECYCLE_EVENT_KINDS,
};
use serde_json::{json, Value};

fn event(kind: &str, n: usize, payload: Value) -> EventRecord {
    EventRecord {
        event_id: format!("e-{n}"),
        ts: format!("2026-08-02T00:00:{n:02}Z"),
        actor: "runtime:orch".into(),
        kind: kind.into(),
        task_id: Some("B202".into()),
        round: Some("r62".into()),
        payload: Some(payload),
        extra: Default::default(),
    }
}

#[test]
fn review_lifecycle_kinds_are_catalogued() {
    assert_eq!(
        REVIEW_LIFECYCLE_EVENT_KINDS,
        ["ReviewRequested", "ReviewDelivered"]
    );
    let catalog = known_event_kinds();
    for kind in REVIEW_LIFECYCLE_EVENT_KINDS {
        assert!(catalog.contains(&kind), "catalog missing {kind}");
        assert!(is_known_event_kind(kind), "predicate missing {kind}");
    }
}

#[test]
fn review_events_are_state_neutral_and_preserve_report_projection() {
    let before_report = fold(&[
        event(
            "DispatchIssued",
            1,
            json!({"attemptId": "B202-A0001"}),
        ),
        event(
            "ReviewRequested",
            2,
            json!({
                "attemptId": "B202-A0001",
                "role": "primary",
                "agent": "executor-opencode",
                "deadlineSecs": 1800,
                "requestedAt": "2026-08-02T00:00:02Z"
            }),
        ),
        event(
            "ReviewDelivered",
            3,
            json!({
                "attemptId": "B202-A0001",
                "role": "primary",
                "agent": "executor-opencode",
                "bodyLen": 4096
            }),
        ),
    ]);
    assert!(
        before_report.unknown_kinds.is_empty(),
        "Review* leaked into unknown_kinds: {:?}",
        before_report.unknown_kinds
    );
    assert_eq!(
        before_report.tasks["B202"].state,
        Some(TaskState::Dispatched),
        "reviews must not manufacture REPORT readiness"
    );

    let after_report = fold(&[
        event(
            "DispatchIssued",
            1,
            json!({"attemptId": "B202-A0001"}),
        ),
        event(
            "ReportObserved",
            2,
            json!({
                "attemptId": "B202-A0001",
                "reportPath": "coordination/rounds/r62/reports/B202-REPORT.md",
                "commitTitleForRegressionOnly": "docs(B202): arbitrary legal report title"
            }),
        ),
        event(
            "ReviewRequested",
            3,
            json!({
                "attemptId": "B202-A0001",
                "role": "secondary",
                "agent": "executor-antigravity"
            }),
        ),
        event(
            "ReviewDelivered",
            4,
            json!({
                "attemptId": "B202-A0001",
                "role": "secondary",
                "agent": "executor-antigravity",
                "bodyLen": 2048
            }),
        ),
    ]);
    assert!(after_report.unknown_kinds.is_empty());
    assert_eq!(
        after_report.tasks["B202"].state,
        Some(TaskState::ReadyForVerification),
        "reviews must not clobber durable REPORT projection"
    );
}
