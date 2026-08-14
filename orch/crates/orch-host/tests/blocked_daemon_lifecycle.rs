//! ═══ 红种子契约 · B86（BLOCKED daemon reason + 去抖清除）═══
//! 落位: orch/crates/orch-host/tests/blocked_daemon_lifecycle.rs（逐字节复制）
//! 预期红（redForm: compile）：`serve::executor_blocked_tasks` 尚不存在 → error[E0432]。
//!
//! 负向变异下界：
//! M1 忽略 AttemptBlocked → attempt_blocked_routes_existing_agent_down_reason 红；
//! M2 Nudge/Resume/Report 不清 executor BLOCKED → corrective_progress_clears_blocked_cause 红；
//! M3 InjectionIssued(agent_down) 不被 Nudge/Resume/Report 清去抖 → corrective_progress_clears_pending_debounce 红；
//! M4 提示仍只写“判死/判滞”、不披露 BLOCKED → attempt_blocked_routes_existing_agent_down_reason 红。

use orch_core::EventRecord;
use orch_host::{
    ledger,
    serve::{
        dead_or_stalled_tasks, decide_injections, executor_blocked_tasks, rebuild_pending_reasons,
        InjectReason, TickInputs,
    },
};

fn ev(kind: &str, task: &str) -> EventRecord {
    ledger::event(
        kind,
        "runtime:test",
        Some(task),
        Some("r42"),
        serde_json::json!({}),
    )
}

#[test]
fn attempt_blocked_routes_existing_agent_down_reason() {
    let blocked = ev("AttemptBlocked", "B86");
    assert_eq!(executor_blocked_tasks(&[blocked.clone()]), vec!["B86"]);
    assert_eq!(dead_or_stalled_tasks(&[blocked]), vec!["B86"]);

    let inputs = TickInputs {
        new_inbox: vec![],
        fail_tasks: vec![],
        all_recorded: false,
        dead_or_stalled: vec!["B86".to_string()],
        pending: vec![],
    };
    let injections = decide_injections(&inputs);
    assert_eq!(injections.len(), 1);
    assert_eq!(injections[0].reason, InjectReason::AgentDown);
    assert!(
        injections[0].message.contains("BLOCKED"),
        "planner 必须知道是诚实 BLOCKED 判断边界：{}",
        injections[0].message
    );
}

#[test]
fn corrective_progress_clears_blocked_cause() {
    for clear in ["NudgeIssued", "ResumeIssued", "ReportObserved"] {
        let events = vec![ev("AttemptBlocked", "B86"), ev(clear, "B86")];
        assert!(
            executor_blocked_tasks(&events).is_empty(),
            "{clear} 必须清 executor BLOCKED cause"
        );
        assert!(
            dead_or_stalled_tasks(&events).is_empty(),
            "{clear} 后不得继续触发 planner"
        );
    }
}

#[test]
fn corrective_progress_clears_pending_debounce() {
    for clear in ["NudgeIssued", "ResumeIssued", "ReportObserved"] {
        let issued = ledger::event(
            "InjectionIssued",
            "runtime:orch",
            None,
            Some("r42"),
            serde_json::json!({"reason": "agent_down"}),
        );
        let progress = ev(clear, "B86");
        assert!(
            rebuild_pending_reasons(&[issued, progress]).is_empty(),
            "{clear} 必须清 agent_down 去抖，后续新 BLOCKED 才能再唤 planner"
        );
    }
}

#[test]
fn a_new_block_after_progress_can_trigger_again() {
    let events = vec![
        ev("AttemptBlocked", "B86"),
        ev("NudgeIssued", "B86"),
        ev("AttemptBlocked", "B86"),
    ];
    assert_eq!(executor_blocked_tasks(&events), vec!["B86"]);
    assert_eq!(dead_or_stalled_tasks(&events), vec!["B86"]);
}
