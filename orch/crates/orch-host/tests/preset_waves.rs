//! ═══ 红种子契约 · B49 ═══（落位: orch/crates/orch-host/tests/preset_waves.rs，逐字节复制）
//! 预期红（redForm: compile）：`preset::plan_waves` 尚不存在（planner 已预置空占位 preset.rs + lib.rs
//!   声明）→ error[E0425] cannot find function，文件级编译红。
//! 背景：Parallel 预设首切片（design/09 M4+ 择机项）。v1 relay 串行；r26 实战=planner 手工核
//!   writeSet 两两互斥后并行派发。本棒把「分波」机器化：
//!   `plan_waves(tasks: &[(String, Vec<String>)]) -> Vec<Vec<String>>`
//!   —— 入参 (taskId, writeSet)，返回波次列表；同波任务 writeSet 两两无交集；
//!   贪心保序：按输入序逐个尝试放入最早可容纳的波（与该波所有已放任务均无交集），放不进则开新波。
//!   首切片交集判定=字符串精确相等（glob 语义留后续切片，实现头注释须披露此边界）。
//! 变异清单（E9 下界，负向自证逐条注入应各自变红）：
//!   M1 交集判定失效(全放一波) → `overlap_forces_later_wave` 红；
//!   M2 保序破坏(重排输入) → `wave_preserves_input_order` 红；
//!   M3 贪心失效(每任务独占一波) → `disjoint_tasks_share_first_wave` 红；
//!   M4 空输入 panic/返非空 → `empty_input_yields_no_waves` 红。
use orch_host::preset;

fn t(id: &str, ws: &[&str]) -> (String, Vec<String>) {
    (id.to_string(), ws.iter().map(|s| s.to_string()).collect())
}

#[test]
fn disjoint_tasks_share_first_wave() {
    // M3：互斥任务全部进第一波
    let waves = preset::plan_waves(&[t("B1", &["a.rs"]), t("B2", &["b.rs"]), t("B3", &["c.rs"])]);
    assert_eq!(waves, vec![vec!["B1".to_string(), "B2".to_string(), "B3".to_string()]]);
}

#[test]
fn overlap_forces_later_wave() {
    // M1：B2 与 B1 同写 a.rs → B2 落第二波；B3 与第一波无交集 → 回填第一波
    let waves = preset::plan_waves(&[t("B1", &["a.rs", "x.rs"]), t("B2", &["a.rs"]), t("B3", &["c.rs"])]);
    assert_eq!(
        waves,
        vec![vec!["B1".to_string(), "B3".to_string()], vec!["B2".to_string()]]
    );
}

#[test]
fn wave_preserves_input_order() {
    // M2：同波内保持输入相对顺序
    let waves = preset::plan_waves(&[t("B9", &["z.rs"]), t("B1", &["y.rs"])]);
    assert_eq!(waves, vec![vec!["B9".to_string(), "B1".to_string()]]);
}

#[test]
fn chained_overlaps_serialize_in_order() {
    // 链式冲突 B1→B2→B3 全串行,三波保序
    let waves = preset::plan_waves(&[t("B1", &["a.rs"]), t("B2", &["a.rs", "b.rs"]), t("B3", &["b.rs"])]);
    assert_eq!(
        waves,
        vec![vec!["B1".to_string()], vec!["B2".to_string()], vec!["B3".to_string()]]
    );
}

#[test]
fn empty_input_yields_no_waves() {
    // M4：空入参 → 空波次,不 panic
    assert!(preset::plan_waves(&[]).is_empty());
}
