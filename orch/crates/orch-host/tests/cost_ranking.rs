//! ═══ 红种子契约 · B59（成本报表：最贵任务排名纯折叠）═══
//! 落位: orch/crates/orch-host/tests/cost_ranking.rs（逐字节复制）
//! 预期红（redForm: compile）：`costliest_tasks` 尚不存在 → error[E0432]。
//!
//! 背景：`cost::{RoundCost, TaskCost}` 已有（决策 12 成本报表）。缺一个「最贵 N 个任务」
//! 的纯排名折叠，供报表输出 top-spender 行。本棒 additive——不改 `RoundCost`/`TaskCost`、
//! 不改 `cost_report`/`render_table` 或任何既有测试、不新增依赖。
//!
//! 目标契约（落在 orch_host::cost，模块已 pub 导出，勿动 lib.rs）：
//!  - pub fn costliest_tasks(rc: &RoundCost, n: usize) -> Vec<(String, f64)>
//!      · 从 rc.tasks 取 (task_id, verifier_usd)，按 verifier_usd **降序**；
//!      · 相等时按 task_id **升序**（确定性全序：先 f64::total_cmp 降、再 id 升）；
//!      · 最多返回 n 个；n=0 ⇒ 空；n>=任务数 ⇒ 全部；
//!      · 零成本任务也计入（排在最后）。
//!
//! 负向变异下界（转绿后逐条自证）：
//!  M1 升序而非降序（或不按 verifier_usd）⇒ ranks_desc_by_verifier_usd 红；
//!  M2 不做 n 截断 / n=0 不返回空 ⇒ respects_n_bound 红；
//!  M3 相等成本不按 id 升序（不稳定/不确定）⇒ ties_break_by_task_id_ascending 红。

use orch_host::cost::{costliest_tasks, RoundCost, TaskCost};

fn round_cost(pairs: &[(&str, f64)]) -> RoundCost {
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
fn ranks_desc_by_verifier_usd() {
    let rc = round_cost(&[("A", 2.0), ("B", 5.0), ("C", 0.0)]);
    assert_eq!(
        costliest_tasks(&rc, 5),
        vec![
            ("B".to_string(), 5.0),
            ("A".to_string(), 2.0),
            ("C".to_string(), 0.0),
        ]
    );
}

#[test]
fn respects_n_bound() {
    let rc = round_cost(&[("A", 2.0), ("B", 5.0), ("C", 0.0)]);
    assert_eq!(costliest_tasks(&rc, 1), vec![("B".to_string(), 5.0)]);
    assert!(costliest_tasks(&rc, 0).is_empty());
}

#[test]
fn ties_break_by_task_id_ascending() {
    let rc = round_cost(&[("B", 3.0), ("A", 3.0)]);
    assert_eq!(
        costliest_tasks(&rc, 5),
        vec![("A".to_string(), 3.0), ("B".to_string(), 3.0)]
    );
}
