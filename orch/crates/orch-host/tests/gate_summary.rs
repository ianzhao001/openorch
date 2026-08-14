//! ═══ 红种子契约 · B58（门结果纯汇总）═══
//! 落位: orch/crates/orch-host/tests/gate_summary.rs（逐字节复制）
//! 预期红（redForm: compile）：`GateSummary` 与 `summarize_gate_results` 尚不存在 → error[E0432]。
//!
//! 背景：`gate::GateResult` 已有（name/exit_code/duration_ms/log_path），但缺一个把
//! 一组门结果折叠成「绿/红/总数/总耗时」的纯汇总原语，供 status/round close 报表用。
//! 本棒 additive——不改 `GateResult`、不改任何既有门逻辑或测试、不新增依赖。
//!
//! 目标契约（落在 orch_host::gate，模块已 pub 导出，勿动 lib.rs）：
//!  - pub struct GateSummary { pub total: usize, pub green: usize, pub red: usize, pub total_ms: u128 }
//!      派生 Debug + PartialEq。
//!  - pub fn summarize_gate_results(results: &[GateResult]) -> GateSummary
//!      · total = results.len()；
//!      · green = exit_code == 0 的条数；red = exit_code != 0 的条数（含 -1 spawn/超时哨兵计红）；
//!      · total_ms = 所有 duration_ms 之和；
//!      · 空切片 ⇒ 全 0。
//!
//! 负向变异下界（转绿后逐条自证）：
//!  M1 red 用 exit_code==0（绿红颠倒）或漏计 -1 哨兵 ⇒ counts_green_red_including_sentinel 红；
//!  M2 total_ms 不求和（取首个/取最大）⇒ sums_all_durations 红；
//!  M3 空切片不返回全 0 ⇒ empty_is_all_zero 红。

use orch_host::gate::{summarize_gate_results, GateResult, GateSummary};

fn gr(name: &str, exit: i32, ms: u128) -> GateResult {
    GateResult {
        name: name.to_string(),
        exit_code: exit,
        duration_ms: ms,
        log_path: "log".to_string(),
    }
}

#[test]
fn counts_green_red_including_sentinel() {
    let s = summarize_gate_results(&[gr("testFast", 0, 100), gr("check", 1, 50), gr("spawn", -1, 0)]);
    assert_eq!(
        s,
        GateSummary { total: 3, green: 1, red: 2, total_ms: 150 }
    );
}

#[test]
fn sums_all_durations() {
    let s = summarize_gate_results(&[gr("a", 0, 100), gr("b", 0, 25), gr("c", 0, 25)]);
    assert_eq!(
        s,
        GateSummary { total: 3, green: 3, red: 0, total_ms: 150 }
    );
}

#[test]
fn empty_is_all_zero() {
    assert_eq!(
        summarize_gate_results(&[]),
        GateSummary { total: 0, green: 0, red: 0, total_ms: 0 }
    );
}
