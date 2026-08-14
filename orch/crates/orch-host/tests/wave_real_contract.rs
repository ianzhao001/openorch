//! ═══ 红种子契约 · B78（Phase B/run-wave 编排正确性根修 · verify-before-merge + 失败→blocked）═══
//! 落位: orch/crates/orch-host/tests/wave_real_contract.rs（逐字节复制）
//! 预期红（redForm: compile）：`WaveDriver` 尚无 `verify`/`merge` 拆分方法、`dispatch` 仍返 `()`
//!   → error[E0407]/E0050/E0432（trait 方法签名不匹配 / 缺方法）。
//!
//! 背景（run-wave 首跑暴出的真 bug，r36 实证）：`RealDriver.verify_and_merge` 合并前**漏跑 verify**，
//! 直接 `run_merge`（要求账本 Approved）→ 5 棒全被拒、`merged 0`；且 `run_wave_with` ⑤ 对
//! `verify_and_merge` 返 false **静默**不入 merged、**不置 blocked**（wave.rs:128-138），CLI 仍打印
//! "all waves clean" exit0。B67 只用 fake（verify_and_merge 恒 true）测编排，故这些真实-IO 缺陷逃过。
//!
//! 本棒把 seam 拆细以令「verify 先于 merge」「dispatch/verify/merge 失败→阻断」成为**可测编排**：
//!   - `dispatch(task) -> bool`（返回是否派发成功，取代吞错的 `()`）；
//!   - `verify(task) -> bool` 与 `merge(task) -> bool`（拆分原 `verify_and_merge`）。
//! `run_wave_with` 新语义：每波 ① pending=去 recorded；② 逐个 dispatch，**失败者不进收取**；
//! ③ 对**派发成功者**逐个 drive_to_collect；④ gate_wave（派发失败者不在 outcomes → 天然入 blockers）；
//! ⑤ 对 gate.mergeable 逐个：先 `verify`，**失败→置 blocked_wave 并停**（不 merge）；verify 通过再
//! `merge`，**失败→置 blocked_wave 并停**；两者皆过才入 merged；⑥ 非 clean 或出现 verify/merge 失败 →
//! `blocked_wave=Some(波号)` 并停止后续波。
//!
//! 负向变异下界（转绿后逐条自证）：
//!   M1 merge 前不调 verify（或 verify 后不看结果）⇒ verify_precedes_merge / verify_fail_blocks 红；
//!   M2 verify/merge 返 false 仍静默不阻断 ⇒ verify_fail_blocks / merge_fail_blocks 红；
//!   M3 dispatch 失败仍照收照合（不阻断）⇒ dispatch_fail_blocks 红。
//!
//! 注：真实并发 collect（thread::scope）、timeout 透传、CLI exit1、phase resume 归 r39 后续棒 B79；
//! 本棒只锁「编排正确性」（lib-only 可测）。RealDriver 须实现新 trait：verify→run_verify(sonnet)、
//! merge→run_merge、dispatch→run_dispatch Ok?true:false（见任务卡）。

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use orch_host::wave::{run_wave_with, TaskWaveOutcome, WaveDriver, WaveRunOutcome, WaveStep};

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

/// 可配 dispatch/verify/merge 成败 + 调用序日志的 fake。未配置默认成功、collect 默认 Collected。
struct Fake {
    recorded: HashSet<String>,
    collect: HashMap<String, TaskWaveOutcome>,
    dispatch_fail: HashSet<String>,
    verify_fail: HashSet<String>,
    merge_fail: HashSet<String>,
    log: Mutex<Vec<String>>,
}
impl Fake {
    fn new() -> Self {
        Fake {
            recorded: HashSet::new(),
            collect: HashMap::new(),
            dispatch_fail: HashSet::new(),
            verify_fail: HashSet::new(),
            merge_fail: HashSet::new(),
            log: Mutex::new(Vec::new()),
        }
    }
    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}
impl WaveDriver for Fake {
    fn already_recorded(&self, task: &str) -> bool {
        self.recorded.contains(task)
    }
    fn dispatch(&self, task: &str) -> bool {
        self.log.lock().unwrap().push(format!("dispatch:{task}"));
        !self.dispatch_fail.contains(task)
    }
    fn drive_to_collect(&self, task: &str) -> TaskWaveOutcome {
        log_collect_in_dispatch_order(&self.log, task);
        self.collect.get(task).cloned().unwrap_or(TaskWaveOutcome::Collected)
    }
    fn verify(&self, task: &str) -> bool {
        self.log.lock().unwrap().push(format!("verify:{task}"));
        !self.verify_fail.contains(task)
    }
    fn merge(&self, task: &str) -> bool {
        self.log.lock().unwrap().push(format!("merge:{task}"));
        !self.merge_fail.contains(task)
    }
}

// ── verify 先于 merge，全过则合入 ──
#[test]
fn verify_precedes_merge() {
    let d = Fake::new();
    let out = run_wave_with(&[step(&["A"])], &d);
    assert_eq!(
        d.log(),
        s(&["dispatch:A", "collect:A", "verify:A", "merge:A"]),
        "verify 必须在 merge 之前"
    );
    assert_eq!(out, WaveRunOutcome { merged: s(&["A"]), blocked_wave: None });
}

// ── verify 失败 → 不 merge、置 blocked ──
#[test]
fn verify_fail_blocks() {
    let mut d = Fake::new();
    d.verify_fail.insert("A".to_string());
    let out = run_wave_with(&[step(&["A"])], &d);
    assert!(d.log().contains(&"verify:A".to_string()));
    assert!(!d.log().contains(&"merge:A".to_string()), "verify 失败不得 merge: {:?}", d.log());
    assert_eq!(out, WaveRunOutcome { merged: s(&[]), blocked_wave: Some(0) });
}

// ── merge 失败 → 置 blocked（不静默）──
#[test]
fn merge_fail_blocks() {
    let mut d = Fake::new();
    d.merge_fail.insert("A".to_string());
    let out = run_wave_with(&[step(&["A"])], &d);
    assert!(d.log().contains(&"verify:A".to_string()) && d.log().contains(&"merge:A".to_string()));
    assert_eq!(out, WaveRunOutcome { merged: s(&[]), blocked_wave: Some(0) });
}

// ── dispatch 失败 → 不收取该任务、置 blocked ──
#[test]
fn dispatch_fail_blocks() {
    let mut d = Fake::new();
    d.dispatch_fail.insert("A".to_string());
    let out = run_wave_with(&[step(&["A"])], &d);
    assert!(d.log().contains(&"dispatch:A".to_string()));
    assert!(!d.log().contains(&"collect:A".to_string()), "dispatch 失败不得收取: {:?}", d.log());
    assert_eq!(out, WaveRunOutcome { merged: s(&[]), blocked_wave: Some(0) });
}

// ── 既有健康路径：多任务全过，相序 = 全 dispatch→全 collect→逐个 verify+merge ──
#[test]
fn healthy_multi_task_phase_order() {
    let d = Fake::new();
    let out = run_wave_with(&[step(&["A", "B"])], &d);
    assert_eq!(
        d.log(),
        s(&[
            "dispatch:A", "dispatch:B", "collect:A", "collect:B",
            "verify:A", "merge:A", "verify:B", "merge:B",
        ])
    );
    assert_eq!(out, WaveRunOutcome { merged: s(&["A", "B"]), blocked_wave: None });
}
