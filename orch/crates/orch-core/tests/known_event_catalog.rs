//! 红种子契约 · B105 · durable event 目录闭合。
//! 预期红：compile，缺少 known_event_kinds/is_known_event_kind。
//! M1：删除任一 DispatchWake 类型，durable_action_catalog_is_complete 红。
//! M2：删除任一 ResumeWake 类型，同一用例红。
//! M3：fold 未使用同一目录、仍把目录内事件记 unknown，fold_accepts_catalog 红。

use orch_core::{fold, is_known_event_kind, known_event_kinds, EventRecord};

const DURABLE_ACTION_KINDS: &[&str] = &[
    "DispatchWakeClaimed",
    "DispatchWakeLaunching",
    "DispatchWakeDelivered",
    "DispatchWakeCompleted",
    "DispatchWakeReleased",
    "ResumeWakeClaimed",
    "ResumeWakeLaunching",
    "ResumeWakeDelivered",
    "ResumeWakeCompleted",
    "ResumeWakeReleased",
    "ReportCollectClaimed",
    "ReportCollectExecuting",
    "ReportCollectExecuted",
    "ReportCollectCompleted",
    "ReportCollectReleased",
    "CollectGateSuccessReceipt",
];

const SESSION_OVERLAY_KINDS: &[&str] = &["SessionOverlayApplied", "SessionOverlayCleared"];

fn event(kind: &str) -> EventRecord {
    EventRecord {
        event_id: format!("e-{kind}"),
        ts: "2026-07-26T00:00:00Z".into(),
        actor: "test".into(),
        kind: kind.into(),
        task_id: Some("B105".into()),
        round: Some("r46".into()),
        payload: None,
        extra: Default::default(),
    }
}

#[test]
fn durable_action_catalog_is_complete() {
    let catalog = known_event_kinds();
    for kind in DURABLE_ACTION_KINDS {
        assert!(catalog.contains(kind), "catalog missing {kind}");
        assert!(is_known_event_kind(kind), "predicate missing {kind}");
    }
}

#[test]
fn fold_accepts_catalog() {
    let events = DURABLE_ACTION_KINDS
        .iter()
        .map(|kind| event(kind))
        .collect::<Vec<_>>();
    let projection = fold(&events);
    assert!(
        projection.unknown_kinds.is_empty(),
        "{:?}",
        projection.unknown_kinds
    );
}

#[test]
fn session_overlay_catalog_is_complete() {
    let catalog = known_event_kinds();
    for kind in SESSION_OVERLAY_KINDS {
        assert!(catalog.contains(kind), "catalog missing {kind}");
        assert!(is_known_event_kind(kind), "predicate missing {kind}");
    }
}

#[test]
fn fold_accepts_session_overlay_kinds() {
    let events = SESSION_OVERLAY_KINDS
        .iter()
        .map(|kind| event(kind))
        .collect::<Vec<_>>();
    let projection = fold(&events);
    assert!(
        projection.unknown_kinds.is_empty(),
        "{:?}",
        projection.unknown_kinds
    );
}

// ── B113：ActionRejected catalog 登记 + 回归 ──
// 卡正文 line 51-53 硬门：「ActionRejected 必须同步登记到 orch-core 的单一已知
// 目录，并扩展 known_event_catalog 回归，确保生产事件不会落入 unknown」。

#[test]
fn action_rejected_is_in_catalog() {
    assert!(known_event_kinds().contains(&"ActionRejected"));
    assert!(is_known_event_kind("ActionRejected"));
}

#[test]
fn fold_accepts_action_rejected() {
    let events = vec![event("ActionRejected")];
    let projection = fold(&events);
    assert!(
        projection.unknown_kinds.is_empty(),
        "ActionRejected should be a known kind, got: {:?}",
        projection.unknown_kinds
    );
}
