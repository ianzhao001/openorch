//! ═══ 红种子契约 · B20 ═══（落位: orch/crates/orch-host/tests/inflight.rs，逐字节复制）
//! 预期红（redForm: compile）：runloop::next_actions_with_inflight / infer_inflight 尚不存在。
//! 变异清单（E9 下界）: M1 in-flight 不跳过 → ① 红；M2 infer 忽略 Started 事件 → ③ 红；M3 完成事件不清除 in-flight → ④ 红
use orch_host::runloop::{self, Action, TaskSnapshot};

fn t(id: &str, state: &str) -> TaskSnapshot {
    TaskSnapshot { id: id.into(), state: state.into() }
}

#[test]
fn inflight_task_is_skipped() {
    // ① ready 但 verify 已在飞 → 不重复产 Verify
    let world = [t("B1", "ready_for_verification"), t("B2", "approved")];
    let inflight = vec!["B1".to_string()];
    let a = runloop::next_actions_with_inflight(&world, &inflight);
    assert_eq!(a, vec![Action::Merge { task: "B2".into() }]);
}

#[test]
fn empty_inflight_equals_plain() {
    // ② 空 in-flight 集合＝原语义
    let world = [t("B1", "ready_for_verification")];
    assert_eq!(
        runloop::next_actions_with_inflight(&world, &[]),
        runloop::next_actions(&world)
    );
}

#[test]
fn infer_inflight_from_started_events() {
    // ③ 账本流推断：VerifyStarted 无后继 VerdictIssued → in-flight
    let kinds = vec![
        ("B1".to_string(), "VerifyStarted".to_string()),
        ("B2".to_string(), "VerifyStarted".to_string()),
        ("B2".to_string(), "VerdictIssued".to_string()),
    ];
    let inflight = runloop::infer_inflight(&kinds);
    assert!(inflight.contains(&"B1".to_string()));
    assert!(!inflight.contains(&"B2".to_string()));
}

#[test]
fn merge_started_cleared_by_merge_executed() {
    // ④ MergeStarted 被 MergeExecuted 清除
    let kinds = vec![
        ("B3".to_string(), "MergeStarted".to_string()),
        ("B3".to_string(), "MergeExecuted".to_string()),
    ];
    assert!(runloop::infer_inflight(&kinds).is_empty());
}
