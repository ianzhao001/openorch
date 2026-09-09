//! ═══ 红种子契约 · B13 ═══════════════════════════════════════════════
//! 落位路径: orch/crates/orch-host/tests/budget_gate.rs （逐字节复制，不得改动）
//!
//! 预期红（redForm: compile，planner oracle 预验 2026-07-22）:
//!   编译红——`orch_host::budget` 模块尚不存在（E0432/E0433 类），4 用例全红。
//!
//! 负向变异自证清单（下界语义，errata E9）:
//!   M1 threshold 边界改严格大于（等于阈值不触发）→ ② 红
//!   M2 ModeConfig budgets.round 解析丢 maxModelWakes → ③ 红
//!   M3 无预算(None 值)也触发阈值                    → ④ 红
//! ══════════════════════════════════════════════════════════════════

use orch_host::budget::{self, RoundBudget};

const MODE_YAML: &str = r#"
preset: relay
budgets:
  round: {maxUsd: 4.0, wallMinutes: 90, maxModelWakes: 12}
"#;

#[test]
fn below_half_is_silent() {
    // ① 40% < 50%：无阈值触发
    assert_eq!(budget::threshold(0.4, 1.0), None);
}

#[test]
fn thresholds_are_inclusive_and_tiered() {
    // ② 阈值含等号，取最高档：50/80/100
    assert_eq!(budget::threshold(0.5, 1.0), Some(50));
    assert_eq!(budget::threshold(0.85, 1.0), Some(80));
    assert_eq!(budget::threshold(1.0, 1.0), Some(100));
    assert_eq!(budget::threshold(1.3, 1.0), Some(100));
}

#[test]
fn parses_mode_config_round_budget() {
    // ③ ModeConfig budgets.round 三字段解析
    let b: RoundBudget = budget::parse_mode_config(MODE_YAML).expect("mode yaml parses");
    assert_eq!(b.max_usd, Some(4.0));
    assert_eq!(b.wall_minutes, Some(90));
    assert_eq!(b.max_model_wakes, Some(12));
}

#[test]
fn absent_budget_never_triggers() {
    // ④ 未配置的维度（None）永不触发（usage 不可知不可填 0——design/07 §2）
    let b = RoundBudget { max_usd: None, wall_minutes: None, max_model_wakes: None };
    assert_eq!(budget::worst_threshold(&b, 999.0, 9999, 9999), None);
}
