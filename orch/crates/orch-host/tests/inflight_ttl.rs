//! ═══ 红种子契约 · B23 ═══（落位: orch/crates/orch-host/tests/inflight_ttl.rs，逐字节复制）
//! 预期红（redForm: compile）：runloop::infer_inflight_with_ttl 尚不存在（E0425，文件级编译红）。
//! 背景：B20 REPORT §6 自省①——VerifyStarted 落账后进程崩溃则任务永挂 in-flight，
//!   后续 tick 永远跳过它。修复：Started 携带时间戳，超 TTL 无完成事件即踢出（expired），
//!   重新可调度；expired 列表供 run_tick 落账 EscalationRaised（不新增 core kind）。
//! 变异清单（E9 下界）: M1 TTL 比较劣化为恒新鲜 → ② 红；M2 完成事件不清除 in-flight → ③ 红；M3 重复 Started 不刷新时间戳（取首个） → ④ 红
use orch_host::runloop;

fn ev(task: &str, kind: &str, ts: i64) -> (String, String, i64) {
    (task.to_string(), kind.to_string(), ts)
}

#[test]
fn fresh_started_is_inflight() {
    // ① TTL 内的 Started → in-flight，expired 空
    let evs = [ev("B1", "VerifyStarted", 100)];
    let (inflight, expired) = runloop::infer_inflight_with_ttl(&evs, 110, 60);
    assert_eq!(inflight, vec!["B1".to_string()]);
    assert!(expired.is_empty());
}

#[test]
fn stale_started_expires() {
    // ② 超 TTL 无完成 → 踢出 in-flight、列入 expired（崩溃恢复的核心语义）
    let evs = [ev("B1", "VerifyStarted", 100)];
    let (inflight, expired) = runloop::infer_inflight_with_ttl(&evs, 200, 60);
    assert!(inflight.is_empty());
    assert_eq!(expired, vec!["B1".to_string()]);
}

#[test]
fn completed_is_neither_inflight_nor_expired() {
    // ③ 已有完成事件对 → 既不在飞也不算过期（正常收口不触发踢出）
    let evs = [ev("B1", "VerifyStarted", 100), ev("B1", "VerdictIssued", 120)];
    let (inflight, expired) = runloop::infer_inflight_with_ttl(&evs, 300, 60);
    assert!(inflight.is_empty());
    assert!(expired.is_empty());
}

#[test]
fn restart_refreshes_ttl_window() {
    // ④ 踢出后重新 Started → 以最新 Started 计时，重新在飞（重试路径不被旧时间戳拖死）
    let evs = [ev("B1", "VerifyStarted", 100), ev("B1", "VerifyStarted", 250)];
    let (inflight, expired) = runloop::infer_inflight_with_ttl(&evs, 280, 60);
    assert_eq!(inflight, vec!["B1".to_string()]);
    assert!(expired.is_empty());
}
