//! ═══ 红种子契约 · B31 ═══（落位: orch/crates/orch-host/tests/gate_resolution.rs，逐字节复制）
//! 预期红（redForm: compile）：binding::resolve_gates 尚不存在（E0425，文件级编译红）。
//! 背景：doc-mini r1 实证 run-task/收取对 testFast/check 命令名硬耦合（BOARD r16 战报）。
//!   本棒把门命令面改为卡驱动：卡 gates.fast 列表逐名到 binding.commands 解析，
//!   缺名显式报错（列出全部缺名），不许静默跳过。
//! 变异清单（E9 下界）: M1 缺名静默跳过 → ② 红；M2 输出排序破坏入参顺序 → ① 红；M3 重复名不去重 → ④ 红
use orch_host::binding;

fn v(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

#[test]
fn all_resolved_in_card_order() {
    // ① 全部命中且保卡内顺序（zeta 在 alpha 前——不许按字典序重排）
    let got = binding::resolve_gates(&v(&["zeta", "alpha"]), &v(&["alpha", "zeta", "extra"])).unwrap();
    assert_eq!(got, v(&["zeta", "alpha"]));
}

#[test]
fn missing_names_error_lists_them() {
    // ② 缺名 → Err，错误文案含全部缺名
    let err = binding::resolve_gates(&v(&["testFast", "nope1", "nope2"]), &v(&["testFast"])).unwrap_err();
    assert!(err.contains("nope1"));
    assert!(err.contains("nope2"));
}

#[test]
fn empty_gate_list_is_ok_empty() {
    // ③ 空门列表合法（pure-spec 轻棒 gates: {fast: []} 场景）
    let got = binding::resolve_gates(&v(&[]), &v(&["testFast"])).unwrap();
    assert!(got.is_empty());
}

#[test]
fn duplicates_dedup_keep_first() {
    // ④ 重复名去重保首现
    let got = binding::resolve_gates(&v(&["a", "b", "a"]), &v(&["a", "b"])).unwrap();
    assert_eq!(got, v(&["a", "b"]));
}
