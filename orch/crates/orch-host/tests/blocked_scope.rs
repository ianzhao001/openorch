//! ═══ 红种子契约 · B83（D5 陈旧 BLOCKED 作用域守卫 · R-b）═══
//! 落位: orch/crates/orch-host/tests/blocked_scope.rs（逐字节复制）
//! 预期红（redForm: compile）：`blocked_is_current` 与 `latest_attempt_signal` 尚不存在
//! → error[E0432]。
//!
//! 背景：await-report 现只按 `<task>-BLOCKED.md`.is_file() 判阻塞。阻塞后 planner nudge/resume，
//! 旧文件仍在时，新 attempt 会在首 tick 被误报 Blocked。本棒以最近 DispatchIssued/NudgeIssued/
//! ResumeIssued 为 attempt marker：只有 BLOCKED 的 mtime 严格晚于 marker 才属于本 attempt。
//!
//! 负向变异下界：
//! M1 退化为只看 is_file → stale_blocked_is_ignored_after_new_attempt_signal 红；
//! M2 使用 >= 接受同刻文件 → equal_timestamp_is_not_current 红；
//! M3 marker 只看首次或混入其他 task → latest_signal_is_task_scoped_and_uses_latest_control 红；
//! M4 run_await 只在启动时读 marker、不在发现 BLOCKED 时重读账本 → verifier 代码审查 FAIL。

use std::time::{Duration, UNIX_EPOCH};

use orch_core::EventRecord;
use orch_host::tierf::{blocked_is_current, latest_attempt_signal};

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

#[test]
fn stale_blocked_is_ignored_after_new_attempt_signal() {
    let marker = UNIX_EPOCH + Duration::from_secs(200);
    assert!(!blocked_is_current(
        Some(UNIX_EPOCH + Duration::from_secs(199)),
        Some(marker)
    ));
    assert!(blocked_is_current(
        Some(UNIX_EPOCH + Duration::from_secs(201)),
        Some(marker)
    ));
}

#[test]
fn equal_timestamp_is_not_current() {
    let same = UNIX_EPOCH + Duration::from_secs(200);
    assert!(!blocked_is_current(Some(same), Some(same)));
}

#[test]
fn missing_marker_is_conservative_and_missing_file_is_false() {
    let blocked = UNIX_EPOCH + Duration::from_secs(200);
    assert!(blocked_is_current(Some(blocked), None));
    assert!(!blocked_is_current(None, Some(blocked)));
    assert!(!blocked_is_current(None, None));
}

#[test]
fn latest_signal_is_task_scoped_and_uses_latest_control() {
    let events = vec![
        event("DispatchIssued", "B83", "2026-07-25T01:00:00Z"),
        event("NudgeIssued", "OTHER", "2026-07-25T05:00:00Z"),
        event("ReportObserved", "B83", "2026-07-25T06:00:00Z"),
        event("NudgeIssued", "B83", "2026-07-25T02:00:00Z"),
        event("ResumeIssued", "B83", "2026-07-25T03:00:00Z"),
    ];

    assert_eq!(
        latest_attempt_signal(&events, "B83"),
        Some(UNIX_EPOCH + Duration::from_secs(1784948400))
    );
    assert_eq!(latest_attempt_signal(&events, "MISSING"), None);
}
