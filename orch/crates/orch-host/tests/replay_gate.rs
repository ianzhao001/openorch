//! ═══ 红种子契约 · B32 ═══（落位: orch/crates/orch-host/tests/replay_gate.rs，逐字节复制）
//! 预期红（redForm: compile）：oracle::red_replay_gate 尚不存在（E0425，文件级编译红）。
//! 背景：B31 把门命令面改为卡驱动，但先红复跑（④½/seed-verified）仍硬取 testFast
//!   （B31 REPORT §6 注明的残留约定）。本棒收尾：复跑命令=卡 gates.fast 首个门。
//! 变异清单（E9 下界）: M1 取末位而非首位 → ① 红；M2 空列表返回默认 "testFast" → ② 红；M3 按字典序取最小 → ④ 红
use orch_host::oracle;

fn v(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

#[test]
fn first_gate_is_replay_command() {
    // ① 多门取首（卡内顺序即优先级）
    assert_eq!(oracle::red_replay_gate(&v(&["docStructure", "docTodo"])).unwrap(), "docStructure");
}

#[test]
fn empty_gates_is_error_for_seeded() {
    // ② 空门列表对 seeded 任务是错误——不许静默回退默认名
    let err = oracle::red_replay_gate(&v(&[])).unwrap_err();
    assert!(err.contains("seeded"));
}

#[test]
fn single_gate_passthrough() {
    // ③ 单门直通（cargo 域惯例 testFast 不受影响）
    assert_eq!(oracle::red_replay_gate(&v(&["testFast"])).unwrap(), "testFast");
}

#[test]
fn card_order_wins_over_lexicographic() {
    // ④ 卡内顺序优先——zeta 在 alpha 前时取 zeta，禁字典序重排
    assert_eq!(oracle::red_replay_gate(&v(&["zeta", "alpha"])).unwrap(), "zeta");
}
