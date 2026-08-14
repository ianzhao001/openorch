//! ═══ 红种子契约 · B16 ═══（落位: orch/crates/orch-host/tests/runloop_kernel.rs，逐字节复制）
//! 预期红（redForm: compile）：orch_host::runloop 为占位空模块。
//! 变异清单（E9 下界）: M1 ready 态不产 Verify → ② 红；M2 全 recorded 不产 CloseRound → ④ 红；M3 决策掺随机 → ⑤ 红
use orch_host::runloop::{self, Action, TaskSnapshot};

fn t(id: &str, state: &str) -> TaskSnapshot {
    TaskSnapshot { id: id.into(), state: state.into() }
}

#[test]
fn empty_world_no_actions() {
    // ① 无任务 → 空动作序列
    assert!(runloop::next_actions(&[]).is_empty());
}

#[test]
fn ready_for_verification_yields_verify() {
    // ② ready_for_verification → Verify
    let a = runloop::next_actions(&[t("B1", "ready_for_verification")]);
    assert_eq!(a, vec![Action::Verify { task: "B1".into() }]);
}

#[test]
fn approved_yields_merge() {
    // ③ approved → Merge
    let a = runloop::next_actions(&[t("B1", "approved")]);
    assert_eq!(a, vec![Action::Merge { task: "B1".into() }]);
}

#[test]
fn all_recorded_yields_close_round() {
    // ④ 全 recorded → CloseRound（且不再有其他动作）
    let a = runloop::next_actions(&[t("B1", "recorded"), t("B2", "recorded")]);
    assert_eq!(a, vec![Action::CloseRound]);
}

#[test]
fn decision_is_pure_and_deterministic() {
    // ⑤ 纯函数：同输入两次调用同输出；dispatched 态＝等待（无动作，不重复派发）
    let world = [t("B1", "dispatched"), t("B2", "approved")];
    let a1 = runloop::next_actions(&world);
    let a2 = runloop::next_actions(&world);
    assert_eq!(a1, a2);
    assert_eq!(a1, vec![Action::Merge { task: "B2".into() }]);
}
