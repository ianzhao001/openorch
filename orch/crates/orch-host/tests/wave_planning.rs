//! ═══ 红种子契约 · B66（Parallel 波次调度器 · 规划纯函数 slice1+2）═══
//! 落位: orch/crates/orch-host/tests/wave_planning.rs（逐字节复制）
//! 预期红（redForm: compile）：`wave::{plan_wave_schedule, WaveStep, gate_wave, WaveGate,
//! TaskWaveOutcome}` 尚不存在 → error[E0432]。（`pub mod wave` 已由 planner 预置占位于 lib.rs；
//! 执行者只在 wave.rs 内实现，勿动 lib.rs——frozenPaths。）
//!
//! 背景：`preset::plan_waves`(B49) + `write_sets_overlap_glob`(B62) 已把「writeSet 互斥分波」机器化，
//! 但只被测试消费、零运行时接线。本棒补规划层两个纯函数，供 slice3 的 run_wave 编排器消费：
//!   - plan_wave_schedule：把 (taskId, writeSet) 规划成有序波次执行计划（复用 plan_waves 分波，
//!     每波派生确定性 merge_order——波内并发派发/收取，收取后按 merge_order 串行 verify+merge）；
//!   - gate_wave：给定一波各任务收取结局，返回可 verify+merge 的子集 + 该波是否干净 + blockers。
//! 不接 IO/并发（那是 slice3）；不改 plan_waves 及任何既有函数/测试；不新增依赖。
//!
//! 目标契约（全部落在 orch_host::wave）：
//!   - pub struct WaveStep { pub tasks: Vec<String>, pub merge_order: Vec<String> }（Debug + PartialEq）
//!   - pub fn plan_wave_schedule(tasks: &[(String, Vec<String>)]) -> Vec<WaveStep>
//!       · tasks 分波复用 preset::plan_waves（writeSet 互斥、保输入序）；
//!       · 每波 WaveStep.tasks = 该波任务（保序）；merge_order = tasks（首切片：= 输入序）。
//!   - pub enum TaskWaveOutcome { Collected, Failed, DeadOrStalled }（Debug + PartialEq）
//!   - pub struct WaveGate { pub mergeable: Vec<String>, pub clean: bool, pub blockers: Vec<String> }（Debug + PartialEq）
//!   - pub fn gate_wave(merge_order: &[String], outcomes: &[(String, TaskWaveOutcome)]) -> WaveGate
//!       · mergeable = merge_order 中结局为 Collected 的任务（**保 merge_order 序**）；
//!       · clean = 无 Failed/DeadOrStalled；blockers = 非 Collected 的任务（保 merge_order 序）。
//!
//! 负向变异下界（转绿后逐条自证）：
//!   M1 plan_wave_schedule 丢波/波数≠plan_waves ⇒ schedule_mirrors_plan_waves 红；
//!   M2 merge_order 不等于 tasks（乱序/空）⇒ schedule_merge_order_is_wave_task_order 红；
//!   M3 gate_wave 把 Failed 计入 mergeable 或 clean 不翻假 ⇒ gate_blocks_on_failure 红。

use orch_host::wave::{
    gate_wave, plan_wave_schedule, TaskWaveOutcome, WaveGate, WaveStep,
};

fn t(id: &str, ws: &[&str]) -> (String, Vec<String>) {
    (id.to_string(), ws.iter().map(|s| s.to_string()).collect())
}
fn s(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|x| x.to_string()).collect()
}

#[test]
fn schedule_mirrors_plan_waves() {
    // W1[src/a.rs] 波0；W2[src/b.rs] 与 W1 不交→回填波0；W3[src/a.rs] 与 W1 精确相交→波1。
    let sched = plan_wave_schedule(&[t("W1", &["src/a.rs"]), t("W2", &["src/b.rs"]), t("W3", &["src/a.rs"])]);
    assert_eq!(
        sched,
        vec![
            WaveStep { tasks: s(&["W1", "W2"]), merge_order: s(&["W1", "W2"]) },
            WaveStep { tasks: s(&["W3"]), merge_order: s(&["W3"]) },
        ]
    );
}

#[test]
fn schedule_merge_order_is_wave_task_order() {
    let sched = plan_wave_schedule(&[t("A", &["x"]), t("B", &["x"])]);
    // A 波0、B 与 A 相交→波1；每波 merge_order 恒等于该波 tasks。
    assert!(sched.iter().all(|step| step.merge_order == step.tasks));
    assert_eq!(sched.len(), 2);
}

#[test]
fn gate_all_collected_is_clean() {
    let g = gate_wave(
        &s(&["W1", "W2"]),
        &[("W1".into(), TaskWaveOutcome::Collected), ("W2".into(), TaskWaveOutcome::Collected)],
    );
    assert_eq!(
        g,
        WaveGate { mergeable: s(&["W1", "W2"]), clean: true, blockers: vec![] }
    );
}

#[test]
fn gate_blocks_on_failure() {
    // W2 Failed、W3 DeadOrStalled ⇒ 非 clean；mergeable 仅 Collected 且保 merge_order 序。
    let g = gate_wave(
        &s(&["W1", "W2", "W3"]),
        &[
            ("W1".into(), TaskWaveOutcome::Collected),
            ("W2".into(), TaskWaveOutcome::Failed),
            ("W3".into(), TaskWaveOutcome::DeadOrStalled),
        ],
    );
    assert_eq!(
        g,
        WaveGate { mergeable: s(&["W1"]), clean: false, blockers: s(&["W2", "W3"]) }
    );
}
