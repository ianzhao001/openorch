//! ═══ 红种子契约 · B63（合后门折叠摘要行渲染）═══
//! 落位: orch/crates/orch-host/tests/gate_summary_line.rs（逐字节复制）
//! 预期红（redForm: compile）：`render_gate_summary_line` 尚不存在 → error[E0425]/E0433。
//!
//! 背景：B58 已落地 `summarize_gate_results` + `GateSummary`，但未被任何输出消费。
//! 本棒新增一个纯渲染函数把门结果折叠成一行人读摘要，并接进 `close.rs::run_merge`
//! 合后门循环之后（见任务卡；wiring 那一行不在本纯函数种子内直接测，属真实 git 流程）。
//!
//! 目标契约（落在 orch_host::gate，模块已 pub 导出，勿动 lib.rs）：
//!  - pub fn render_gate_summary_line(results: &[GateResult]) -> String
//!      · 内部用 `summarize_gate_results` 折叠；
//!      · 精确格式："合后门: {green} 绿 / {red} 红 / {total} 门 · {total_ms}ms"；
//!      · 空切片 ⇒ "合后门: 0 绿 / 0 红 / 0 门 · 0ms"。
//!
//! 负向变异下界（转绿后逐条自证）：
//!  M1 绿/红计数用错门限（exit==0 之外）⇒ renders_mixed 红；
//!  M2 total_ms 不求和 ⇒ renders_all_green 红；
//!  M3 空切片格式不符 ⇒ renders_empty 红。

use orch_host::gate::{render_gate_summary_line, GateResult};

fn gr(exit: i32, ms: u128) -> GateResult {
    GateResult {
        name: "g".to_string(),
        exit_code: exit,
        duration_ms: ms,
        log_path: "log".to_string(),
    }
}

#[test]
fn renders_all_green() {
    assert_eq!(
        render_gate_summary_line(&[gr(0, 100), gr(0, 25)]),
        "合后门: 2 绿 / 0 红 / 2 门 · 125ms"
    );
}

#[test]
fn renders_mixed() {
    assert_eq!(
        render_gate_summary_line(&[gr(0, 100), gr(1, 50)]),
        "合后门: 1 绿 / 1 红 / 2 门 · 150ms"
    );
}

#[test]
fn renders_empty() {
    assert_eq!(
        render_gate_summary_line(&[]),
        "合后门: 0 绿 / 0 红 / 0 门 · 0ms"
    );
}
