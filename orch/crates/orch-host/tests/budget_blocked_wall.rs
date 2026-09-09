//! ═══ 红种子契约 · B86（AttemptBlocked active-wall）═══
//! 落位: orch/crates/orch-host/tests/budget_blocked_wall.rs（逐字节复制）
//! 与 blocked_daemon_lifecycle.rs 同属一个 seed commit；编译红由后者缺符号触发。
//!
//! 负向变异下界：
//! M1 AttemptBlocked 不结清窗口 → blocked_closes_active_window 红；
//! M2 NudgeIssued 不重开 → nudge_and_resume_reopen_after_block 红；
//! M3 ResumeIssued 不重开 → nudge_and_resume_reopen_after_block 红；
//! M4 重开时错误累计冻结间隙 → frozen_gap_is_not_active_wall 红。

use orch_core::EventRecord;
use orch_host::budget::active_attempt_wall_mins;

fn ev(kind: &str, ts: &str) -> EventRecord {
    EventRecord {
        event_id: format!("{kind}-{ts}"),
        ts: ts.to_string(),
        actor: "runtime:test".to_string(),
        kind: kind.to_string(),
        task_id: Some("B86".to_string()),
        round: Some("r42".to_string()),
        payload: Some(serde_json::json!({})),
        extra: serde_json::Map::new(),
    }
}

#[test]
fn blocked_closes_active_window() {
    let events = vec![
        ev("DispatchIssued", "2026-07-25T10:00:00Z"),
        ev("AttemptBlocked", "2026-07-25T10:10:00Z"),
    ];
    assert_eq!(
        active_attempt_wall_mins(&events, "2026-07-25T11:00:00Z"),
        10
    );
}

#[test]
fn nudge_and_resume_reopen_after_block() {
    for signal in ["NudgeIssued", "ResumeIssued"] {
        let events = vec![
            ev("DispatchIssued", "2026-07-25T10:00:00Z"),
            ev("AttemptBlocked", "2026-07-25T10:10:00Z"),
            ev(signal, "2026-07-25T10:30:00Z"),
            ev("AttemptBlocked", "2026-07-25T10:35:00Z"),
        ];
        assert_eq!(
            active_attempt_wall_mins(&events, "2026-07-25T11:00:00Z"),
            15,
            "{signal} 应重开五分钟窗口"
        );
    }
}

#[test]
fn frozen_gap_is_not_active_wall() {
    let events = vec![
        ev("DispatchIssued", "2026-07-25T10:00:00Z"),
        ev("AttemptBlocked", "2026-07-25T10:10:00Z"),
        ev("NudgeIssued", "2026-07-25T10:50:00Z"),
        ev("AttemptBlocked", "2026-07-25T10:55:00Z"),
    ];
    assert_eq!(
        active_attempt_wall_mins(&events, "2026-07-25T12:00:00Z"),
        15,
        "10:10–10:50 的冻结等待不得计费"
    );
}
