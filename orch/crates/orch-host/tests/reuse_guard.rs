//! ═══ 红种子契约 · B28 ═══（落位: orch/crates/orch-host/tests/reuse_guard.rs，逐字节复制）
//! 预期红（redForm: compile）：gate::GatePlan / plan_gate_run 尚不存在（E0432/E0425，文件级编译红）。
//! 背景：B24 已落门证据存取（GateEvidence/record/lookup），「命中即披露」不跳过执行。
//!   本棒补合法复用判定（design/06 §7：缓存键任一变化即 miss，严禁新 HEAD 复用旧绿）：
//!   同键且绿 → 合法复用；键不同/无缓存/缓存为红 → 必须执行。
//! 变异清单（E9 下界）: M1 键比对劣化为恒等 → ③ 红；M2 忽略 exit_code → ④ 红；M3 恒 Execute → ② 红
use orch_host::gate::{self, GateEvidence, GatePlan};

fn ev(key: &str, exit_code: i32, log: &str) -> GateEvidence {
    GateEvidence { key: key.to_string(), exit_code, log_sha256: log.to_string() }
}

#[test]
fn no_cache_executes() {
    // ① 无缓存 → Execute
    assert_eq!(gate::plan_gate_run(None, "k1"), GatePlan::Execute);
}

#[test]
fn same_key_green_reuses() {
    // ② 同键且绿 → 合法复用，携带证据字段
    let cached = ev("k1", 0, "abc123");
    assert_eq!(
        gate::plan_gate_run(Some(&cached), "k1"),
        GatePlan::Reuse { exit_code: 0, log_sha256: "abc123".to_string() }
    );
}

#[test]
fn different_key_never_reuses() {
    // ③ 键不同 → Execute（「严禁在新 HEAD 复用旧绿」的机械位）
    let cached = ev("old-head-key", 0, "abc123");
    assert_eq!(gate::plan_gate_run(Some(&cached), "new-head-key"), GatePlan::Execute);
}

#[test]
fn cached_red_always_reruns() {
    // ④ 同键但缓存为红 → Execute（失败必须重跑取新证，红结果不享受缓存）
    let cached = ev("k1", 1, "abc123");
    assert_eq!(gate::plan_gate_run(Some(&cached), "k1"), GatePlan::Execute);
}
