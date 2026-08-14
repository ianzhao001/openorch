//! ═══ 红种子契约 · B64（成本报表 top-spenders 行）═══
//! 落位: orch/crates/orch-host/tests/cost_top_spenders.rs（逐字节复制）
//! 预期红（redForm: assertion）：`render_table` 现无 top-spenders 行 → 下列 contains 断言 FAIL。
//!
//! 背景：B59 已落地 `costliest_tasks`（按 verifier_usd 降序、id 升序 tiebreak），但未被
//! 任何输出消费。本棒把它接进 `render_table`——在既有「合计 verifier $…」行**之后追加**
//! 一行「top 支出」，让 `orch cost` 直接暴露花费集中度。**追加式**：不改表头、不改数据行、
//! 不改既有内联测试（`render_table_shows_model_column` 断言表头在第 2 行/数据行结尾，本行
//! 追加在末尾且以 `$金额` 结尾，均不受影响）。
//!
//! 目标契约（只改 cost.rs 的 render_table，无新符号）：
//!  - 在 `  合计 verifier $…` 行之后追加一行（2 空格缩进，与表体一致）：
//!      非空：`  top 支出: <id> $<usd:.2f>[, <id> $<usd:.2f>]…`（取 `costliest_tasks(rc, 3)`，
//!            降序、id 升序 tiebreak）；
//!      空 rc.tasks：`  top 支出: -`。
//!
//! 负向变异下界（转绿后逐条自证）：
//!  M1 不接 costliest_tasks / 顺序错（升序或不按 usd）⇒ lists_top_spenders_descending 红；
//!  M2 tiebreak 不按 id 升序 ⇒ ties_break_by_id_ascending 红；
//!  M3 空 rc 不给占位 ⇒ empty_tasks_shows_dash 红。

use orch_host::cost::{render_table, RoundCost, TaskCost};

fn rc(pairs: &[(&str, f64)]) -> RoundCost {
    let mut rc = RoundCost::default();
    for (id, usd) in pairs {
        rc.tasks.insert(
            id.to_string(),
            TaskCost { verifier_usd: *usd, ..Default::default() },
        );
    }
    rc
}

#[test]
fn lists_top_spenders_descending() {
    let out = render_table("rT", &rc(&[("A", 2.0), ("B", 5.0), ("C", 0.0)]));
    assert!(
        out.contains("  top 支出: B $5.00, A $2.00, C $0.00"),
        "rendered:\n{out}"
    );
}

#[test]
fn ties_break_by_id_ascending() {
    let out = render_table("rT", &rc(&[("B", 3.0), ("A", 3.0)]));
    assert!(out.contains("  top 支出: A $3.00, B $3.00"), "rendered:\n{out}");
}

#[test]
fn empty_tasks_shows_dash() {
    let out = render_table("rT", &rc(&[]));
    assert!(out.contains("  top 支出: -"), "rendered:\n{out}");
}
