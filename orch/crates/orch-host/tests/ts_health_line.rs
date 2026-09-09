//! ═══ 红种子契约 · B29 ═══（落位: orch/crates/orch-host/tests/ts_health_line.rs，逐字节复制）
//! 预期红（redForm: compile）：ledger::ts_health_line 尚不存在（E0425，文件级编译红）。
//! 背景：B25 落了 ts_health 探针但曝光接线是尽力项（doctor 在 core 冻结）——本棒完成曝光：
//!   格式化纯函数 + orch status 接线（本卡 writeSet 含 orch-cli/main.rs，接线是硬要求）。
//!   健康时 None（不打扰），异常时 Some 一行含固定前缀「ts 健康」+计数+行号。
//! 变异清单（E9 下界）: M1 恒 None → ② 红；M2 文案不含行号 → ③ 红；M3 bad_ts=0 也 Some → ① 红
use orch_host::ledger::{self, TsHealth};

#[test]
fn healthy_is_silent() {
    // ① 无坏 ts → None（健康不打扰输出）
    let h = TsHealth { total: 5, bad_ts: 0, bad_lines: vec![] };
    assert!(ledger::ts_health_line(&h).is_none());
}

#[test]
fn unhealthy_reports_count() {
    // ② 有坏 ts → Some，含固定前缀「ts 健康」与计数
    let h = TsHealth { total: 9, bad_ts: 2, bad_lines: vec![3, 7] };
    let line = ledger::ts_health_line(&h).expect("must warn");
    assert!(line.contains("ts 健康"));
    assert!(line.contains('2'));
}

#[test]
fn line_numbers_are_listed_in_order() {
    // ③ 行号逐个在列且保序（3 在 7 前）
    let h = TsHealth { total: 9, bad_ts: 2, bad_lines: vec![3, 7] };
    let line = ledger::ts_health_line(&h).unwrap();
    let p3 = line.find('3').expect("line 3 missing");
    let p7 = line.rfind('7').expect("line 7 missing");
    assert!(p3 < p7);
}

#[test]
fn single_bad_line_reported() {
    // ④ 单坏行边界
    let h = TsHealth { total: 4, bad_ts: 1, bad_lines: vec![4] };
    let line = ledger::ts_health_line(&h).unwrap();
    assert!(line.contains('1'));
    assert!(line.contains('4'));
}
