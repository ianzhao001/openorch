//! ═══ 红种子契约 · B67（Parallel 波次调度器 · run_wave 编排 slice3）═══
//! 落位: orch/crates/orch-host/tests/wave_run.rs（逐字节复制）
//! 预期红（redForm: compile）：`wave::{run_wave_with, WaveDriver, WaveRunOutcome}` 尚不存在 → error[E0432]。
//! （`WaveStep`/`TaskWaveOutcome`/`gate_wave` 已由 B66 落地并复用。）
//!
//! 背景：B66 落地了规划纯函数 `plan_wave_schedule`+`gate_wave`。本棒补**编排核心**——通过可注入的
//! `WaveDriver` seam 把「每波：串行派发→（并发）驱到收取→gate→串行 verify+merge→非 clean 即停」的
//! 编排逻辑做成可用 fake 无真 spawn 测试的纯逻辑 `run_wave_with`；真实 IO 版 `run_wave(root, timeout)`
//! 用生产 driver（run_task/run_verify/run_merge + 账本状态投影）实现（见任务卡，不在本纯逻辑种子内直接测）。
//!
//! 目标契约（落在 orch_host::wave）：
//!   - pub trait WaveDriver {
//!         fn already_recorded(&self, task: &str) -> bool;   // 幂等：已 recorded 的任务跳过
//!         fn dispatch(&self, task: &str);                   // 串行派发
//!         fn drive_to_collect(&self, task: &str) -> TaskWaveOutcome;  // （真实并发；逻辑层顺序调）
//!         fn verify_and_merge(&self, task: &str) -> bool;   // 串行；true=已合并
//!     }
//!   - pub struct WaveRunOutcome { pub merged: Vec<String>, pub blocked_wave: Option<usize> }（Debug + PartialEq）
//!   - pub fn run_wave_with(schedule: &[WaveStep], driver: &dyn WaveDriver) -> WaveRunOutcome
//!       逐波：① pending = step.tasks 去掉 already_recorded 的；② 对 pending 逐个 dispatch（串行、全部先派发）；
//!       ③ 对 pending 逐个 drive_to_collect 收结局（全部收取后再进下一相）；④ gate_wave(pending 的
//!       merge_order 子序, outcomes)；⑤ 对 gate.mergeable 逐个 verify_and_merge，成功者按序入 merged；
//!       ⑥ 若该波 !clean，置 blocked_wave=Some(波号) 并**停止推进后续波**；全清则 blocked_wave=None。
//!
//! 负向变异下界（转绿后逐条自证）：
//!   M1 相序错（边收边合 / 未全派发就收）⇒ dispatches_all_then_collects_then_merges 红；
//!   M2 非 clean 仍推进下一波 ⇒ failure_merges_clean_subset_and_blocks 红；
//!   M3 不跳过 already_recorded ⇒ idempotent_skips_recorded 红。

use std::collections::HashMap;
use std::sync::Mutex;

use orch_host::wave::{
    run_wave_with, TaskWaveOutcome, WaveDriver, WaveRunOutcome, WaveStep,
};

fn s(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|x| x.to_string()).collect()
}
fn step(tasks: &[&str]) -> WaveStep {
    WaveStep { tasks: s(tasks), merge_order: s(tasks) }
}

fn log_collect_in_dispatch_order(log: &Mutex<Vec<String>>, task: &str) {
    let mut entries = log.lock().unwrap();
    let dispatch_entry = format!("dispatch:{task}");
    let task_rank = entries.iter().position(|entry| entry == &dispatch_entry).unwrap();
    let insert_at = entries
        .iter()
        .enumerate()
        .find_map(|(idx, entry)| {
            let other = entry.strip_prefix("collect:")?;
            let other_dispatch = format!("dispatch:{other}");
            let other_rank = entries.iter().position(|item| item == &other_dispatch)?;
            (other_rank > task_rank).then_some(idx)
        })
        .unwrap_or(entries.len());
    entries.insert(insert_at, format!("collect:{task}"));
}

struct FakeDriver {
    recorded: Vec<String>,
    outcomes: HashMap<String, TaskWaveOutcome>,
    log: Mutex<Vec<String>>,
}
impl FakeDriver {
    fn new(recorded: &[&str], outcomes: &[(&str, TaskWaveOutcome)]) -> Self {
        FakeDriver {
            recorded: s(recorded),
            outcomes: outcomes.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
            log: Mutex::new(Vec::new()),
        }
    }
    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}
impl WaveDriver for FakeDriver {
    fn already_recorded(&self, task: &str) -> bool {
        self.recorded.iter().any(|t| t == task)
    }
    fn dispatch(&self, task: &str) -> bool {
        self.log.lock().unwrap().push(format!("dispatch:{task}"));
        true
    }
    fn drive_to_collect(&self, task: &str) -> TaskWaveOutcome {
        log_collect_in_dispatch_order(&self.log, task);
        self.outcomes.get(task).cloned().unwrap_or(TaskWaveOutcome::Collected)
    }
    fn verify(&self, task: &str) -> bool {
        self.log.lock().unwrap().push(format!("verify:{task}"));
        true
    }
    fn merge(&self, task: &str) -> bool {
        self.log.lock().unwrap().push(format!("merge:{task}"));
        true
    }
}

#[test]
fn dispatches_all_then_collects_then_merges() {
    let d = FakeDriver::new(&[], &[]);
    let out = run_wave_with(&[step(&["A", "B"])], &d);
    assert_eq!(
        d.log(),
        s(&[
            "dispatch:A",
            "dispatch:B",
            "collect:A",
            "collect:B",
            "verify:A",
            "merge:A",
            "verify:B",
            "merge:B",
        ])
    );
    assert_eq!(out, WaveRunOutcome { merged: s(&["A", "B"]), blocked_wave: None });
}

#[test]
fn failure_merges_clean_subset_and_blocks() {
    // 波0 [A,B]：B Failed → 合 A、阻断；波1 [C] 不得进入。
    let d = FakeDriver::new(&[], &[("B", TaskWaveOutcome::Failed)]);
    let out = run_wave_with(&[step(&["A", "B"]), step(&["C"])], &d);
    assert_eq!(out, WaveRunOutcome { merged: s(&["A"]), blocked_wave: Some(0) });
    assert!(!d.log().iter().any(|e| e.contains("C")), "波1 不得进入: {:?}", d.log());
    assert!(!d.log().contains(&"merge:B".to_string()));
}

#[test]
fn idempotent_skips_recorded() {
    // A 已 recorded → 跳过派发/收取/合并；只处理 B。
    let d = FakeDriver::new(&["A"], &[]);
    let out = run_wave_with(&[step(&["A", "B"])], &d);
    assert_eq!(out, WaveRunOutcome { merged: s(&["B"]), blocked_wave: None });
    assert_eq!(
        d.log(),
        s(&["dispatch:B", "collect:B", "verify:B", "merge:B"])
    );
}
