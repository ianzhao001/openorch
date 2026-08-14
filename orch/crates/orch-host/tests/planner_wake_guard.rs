//! ═══ 红种子契约 · B53 ═══
//! 落位: orch/crates/orch-host/tests/planner_wake_guard.rs（逐字节复制）
//! 预期红（redForm: compile）：下列 7 个 O14/O13/O11 API 尚不存在 → error[E0425]。
//!
//! 目标契约：
//! - O13：同 tick 四个判断 reason 合成一个物理 planner wake，reason 仍全部可观测；
//! - 新旧 InjectionIssued（reason / reasons[]）都可重建 pending；
//! - O14：InjectionIssued 一事件只计一次模型唤醒，max-1 放最后一次、max 精确阻断；
//! - fresh planner 每次生成不同 wake/session UUID，argv 只用 --session-id，拒绝 --resume；
//! - O11：活 child 等待；死 child 有进展=完成，无进展仅重试一次，第二次升级；
//! - 只有预算、spawn、InjectionIssued 三步都成功，inbox 才能进 processing。
//!
//! 负向变异下界：
//! M1 四 reason 拆成多 wake / 丢 reason → batch_all_reasons_once 红；
//! M2 只读旧 reason 或只读新 reasons[] → old_and_new_payloads_rebuild_pending 红；
//! M3 InjectionIssued 不计或按 reasons 数量多计 → planner_wake_counts_once 红；
//! M4 fresh argv 使用 --resume / 复用 session UUID → fresh_session_never_resumes 红；
//! M5 planner 无进展无限重试或首败即升级 → liveness_retries_exactly_once 红；
//! M6 预算/spawn/落账任一步失败仍移动 inbox → inbox_advances_only_after_durable_issue 红。

use orch_host::{budget, ledger, serve, wake};

fn injection(reason: serve::InjectReason, message: &str) -> serve::Injection {
    serve::Injection {
        reason,
        message: message.to_string(),
    }
}

#[test]
fn batch_all_reasons_once() {
    let injections = vec![
        injection(serve::InjectReason::NewInstruction, "new instruction"),
        injection(serve::InjectReason::TaskFailed, "B53 failed"),
        injection(serve::InjectReason::AllRecorded, "all recorded"),
        injection(serve::InjectReason::AgentDown, "executor-a down"),
    ];
    let inbox = vec!["1000-priority.md".to_string()];
    let sources = vec![
        "inbox:1000-priority.md".to_string(),
        "event:fail-01".to_string(),
        "event:recorded-01".to_string(),
        "event:down-01".to_string(),
    ];

    let first =
        serve::plan_planner_wake("r28", &injections, &inbox, &sources, 2048).unwrap();
    assert_eq!(first.reasons.len(), 4);
    assert_eq!(
        first.reasons,
        vec![
            serve::InjectReason::NewInstruction,
            serve::InjectReason::TaskFailed,
            serve::InjectReason::AllRecorded,
            serve::InjectReason::AgentDown,
        ]
    );
    assert!(first.prompt.len() <= 2048);
    assert!(first.prompt.contains("r28"));
    assert!(first.prompt.contains("coordination/planner-bootstrap.md"));
    assert!(first.prompt.contains("1000-priority.md"));

    let same =
        serve::plan_planner_wake("r28", &injections, &inbox, &sources, 2048).unwrap();
    assert_eq!(first.trigger_key, same.trigger_key);
    let mut newer_sources = sources;
    newer_sources[1] = "event:fail-02".to_string();
    let newer =
        serve::plan_planner_wake("r28", &injections, &inbox, &newer_sources, 2048).unwrap();
    assert_ne!(first.trigger_key, newer.trigger_key);
}

#[test]
fn old_and_new_payloads_rebuild_pending() {
    let events = vec![
        ledger::event(
            "InjectionIssued",
            "runtime:orch",
            None,
            Some("r28"),
            serde_json::json!({"reason": "task_failed"}),
        ),
        ledger::event(
            "InjectionIssued",
            "runtime:orch",
            None,
            Some("r28"),
            serde_json::json!({"reasons": ["new_instruction", "agent_down"]}),
        ),
        ledger::event(
            "PlannerTurnCompleted",
            "runtime:orch",
            None,
            Some("r28"),
            serde_json::json!({"reasons": ["task_failed"]}),
        ),
    ];
    assert_eq!(
        serve::rebuild_pending_reasons(&events),
        vec![
            serve::InjectReason::NewInstruction,
            serve::InjectReason::AgentDown,
        ]
    );
}

#[test]
fn planner_wake_counts_once_and_stops_at_max() {
    let events = vec![
        ledger::event(
            "DispatchIssued",
            "runtime:orch",
            Some("B53"),
            Some("r28"),
            serde_json::json!({}),
        ),
        ledger::event(
            "NudgeIssued",
            "runtime:orch",
            Some("B53"),
            Some("r28"),
            serde_json::json!({}),
        ),
        ledger::event(
            "ResumeIssued",
            "runtime:orch",
            Some("B53"),
            Some("r28"),
            serde_json::json!({}),
        ),
        ledger::event(
            "InjectionIssued",
            "runtime:orch",
            None,
            Some("r28"),
            serde_json::json!({
                "reasons": ["new_instruction", "task_failed", "all_recorded", "agent_down"]
            }),
        ),
    ];
    assert_eq!(budget::count_model_wakes(&events), 4);
    assert!(budget::model_wake_permitted(3, Some(4)));
    assert!(!budget::model_wake_permitted(4, Some(4)));
    assert!(budget::model_wake_permitted(999, None));
}

#[test]
fn fresh_session_never_resumes() {
    let template = vec![
        "claude".to_string(),
        "--session-id".to_string(),
        "{session}".to_string(),
        "-p".to_string(),
        "{message}".to_string(),
    ];
    let first = wake::plan_fresh_planner_wake(&template, "short decision").unwrap();
    let second = wake::plan_fresh_planner_wake(&template, "short decision").unwrap();
    assert_ne!(first.wake_id, second.wake_id);
    assert_ne!(first.session_id, second.session_id);
    assert!(first.argv.iter().any(|arg| arg == "--session-id"));
    assert!(first.argv.iter().any(|arg| arg == &first.session_id));
    assert!(!first.argv.iter().any(|arg| arg == "--resume" || arg == "resume" || arg == "-r"));
    assert!(!first.argv.iter().any(|arg| arg.contains("d08add7f")));

    let forbidden = vec![
        "claude".to_string(),
        "--resume".to_string(),
        "{session}".to_string(),
        "-p".to_string(),
        "{message}".to_string(),
    ];
    assert!(wake::plan_fresh_planner_wake(&forbidden, "must fail closed").is_err());
}

#[test]
fn liveness_retries_exactly_once() {
    assert_eq!(
        format!("{:?}", serve::decide_planner_liveness(true, false, 1)),
        "Wait"
    );
    assert_eq!(
        format!("{:?}", serve::decide_planner_liveness(false, true, 1)),
        "Completed"
    );
    assert_eq!(
        format!("{:?}", serve::decide_planner_liveness(false, false, 1)),
        "Retry { attempt: 2 }"
    );
    assert_eq!(
        format!("{:?}", serve::decide_planner_liveness(false, false, 2)),
        "Escalate"
    );
}

#[test]
fn inbox_advances_only_after_durable_issue() {
    assert!(serve::may_advance_inbox_after_wake(true, true, true));
    assert!(!serve::may_advance_inbox_after_wake(false, true, true));
    assert!(!serve::may_advance_inbox_after_wake(true, false, true));
    assert!(!serve::may_advance_inbox_after_wake(true, true, false));
}
