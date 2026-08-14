//! ═══ 红种子契约 · B26 ═══（落位: orch/crates/orch-host/tests/ttl_throttle.rs，逐字节复制）
//! 预期红（redForm: compile）：runloop::throttle_expired 尚不存在（E0425，文件级编译红）。
//! 背景：B23 REPORT §6 自省①——expired 任务若一直不可再调度，每 tick 重复落
//!   EscalationRaised 刷屏。节流语义：同一 task 的 inflight-ttl-expired 升级只报一次，
//!   直到其后出现**新的 Started**（新一轮尝试再过期才允许再报）。账本即节流记忆，无新状态。
//! 变异清单（E9 下界）: M1 节流失效恒全报 → ② 红；M2 恒不报 → ① 红；M3 新 Started 不重置节流（只看有无 prior） → ③ 红
use orch_host::runloop;

fn s(x: &str) -> String {
    x.to_string()
}

#[test]
fn first_expiry_escalates() {
    // ① 无升级前科 → 报
    let out = runloop::throttle_expired(&[s("B1")], &[], &[(s("B1"), 100)]);
    assert_eq!(out, vec![s("B1")]);
}

#[test]
fn already_escalated_is_throttled() {
    // ② 已报过（prior ts=150 晚于其 Started ts=100）且无新 Started → 不再报
    let out = runloop::throttle_expired(&[s("B1")], &[(s("B1"), 150)], &[(s("B1"), 100)]);
    assert!(out.is_empty());
}

#[test]
fn new_started_after_escalation_reescalates() {
    // ③ 升级后出现新 Started（ts=200 > prior 150）又过期 → 允许再报
    let out = runloop::throttle_expired(&[s("B1")], &[(s("B1"), 150)], &[(s("B1"), 200)]);
    assert_eq!(out, vec![s("B1")]);
}

#[test]
fn mixed_tasks_partition() {
    // ④ 多任务分区：B1 被节流、B2 首报——互不影响
    let out = runloop::throttle_expired(
        &[s("B1"), s("B2")],
        &[(s("B1"), 150)],
        &[(s("B1"), 100), (s("B2"), 90)],
    );
    assert_eq!(out, vec![s("B2")]);
}
