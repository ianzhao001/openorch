//! ═══ 红种子契约 · B82（run-wave phase resume + in-flight 防双花）═══
//! 落位: orch/crates/orch-host/tests/wave_phase_resume.rs（逐字节复制）
//! 预期红（redForm: compile）：`WaveTaskPhase`、`classify_wave_phase` 与
//! `WaveDriver::phase` 尚不存在 → error[E0432]/E0407。
//!
//! 背景：r40 已证明 New→dispatch→collect→verify→merge 的健康并行路径，但真实账本没有
//! VerifyStarted/MergeStarted，且 run-wave 重启只跳过 Recorded。若进程在 collect/verify/merge
//! 之间崩溃，重跑会再次 dispatch 或重复 verifier/merge。本棒把账本 phase 与既有 runloop TTL
//! 接入 wave，要求从 Dispatched/Ready/Approved 精确续跑，fresh in-flight 一律停手。
//!
//! 负向变异下界：
//! M1 把所有非 Recorded 相位强制当 New → resumes_each_phase_without_replaying_prior_actions 红；
//! M2 忽略 fresh_inflight → classifier_maps_projection_and_fresh_inflight_fail_closed 红；
//! M3 NeedsOperator 仍继续或继续后续波 → needs_operator_blocks_next_wave_but_clean_peer_can_finish 红；
//! M4 RealDriver 不写 VerifyStarted/MergeStarted、动作前不重读账本或坏账本仍派发 → verifier 代码审查 FAIL。

use std::collections::HashMap;
use std::sync::Mutex;

use orch_core::TaskState;
use orch_host::wave::{
    classify_wave_phase, run_wave_with, TaskWaveOutcome, WaveDriver, WaveRunOutcome, WaveStep,
    WaveTaskPhase,
};

fn s(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| id.to_string()).collect()
}

fn step(tasks: &[&str]) -> WaveStep {
    WaveStep {
        tasks: s(tasks),
        merge_order: s(tasks),
    }
}

struct PhaseFake {
    phases: HashMap<String, WaveTaskPhase>,
    log: Mutex<Vec<String>>,
}

impl PhaseFake {
    fn new(phases: &[(&str, WaveTaskPhase)]) -> Self {
        Self {
            phases: phases
                .iter()
                .map(|(task, phase)| (task.to_string(), phase.clone()))
                .collect(),
            log: Mutex::new(Vec::new()),
        }
    }

    fn entries(&self, prefix: &str) -> Vec<String> {
        let mut out = self
            .log
            .lock()
            .unwrap()
            .iter()
            .filter_map(|entry| entry.strip_prefix(prefix).map(str::to_string))
            .collect::<Vec<_>>();
        out.sort();
        out
    }
}

impl WaveDriver for PhaseFake {
    fn already_recorded(&self, task: &str) -> bool {
        self.phases.get(task) == Some(&WaveTaskPhase::Recorded)
    }

    fn phase(&self, task: &str) -> WaveTaskPhase {
        self.phases
            .get(task)
            .cloned()
            .unwrap_or(WaveTaskPhase::New)
    }

    fn dispatch(&self, task: &str) -> bool {
        self.log.lock().unwrap().push(format!("dispatch:{task}"));
        true
    }

    fn drive_to_collect(&self, task: &str) -> TaskWaveOutcome {
        self.log.lock().unwrap().push(format!("collect:{task}"));
        TaskWaveOutcome::Collected
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
fn resumes_each_phase_without_replaying_prior_actions() {
    let driver = PhaseFake::new(&[
        ("N", WaveTaskPhase::New),
        ("D", WaveTaskPhase::Dispatched),
        ("R", WaveTaskPhase::ReadyForVerification),
        ("A", WaveTaskPhase::Approved),
        ("Z", WaveTaskPhase::Recorded),
    ]);

    let out = run_wave_with(&[step(&["N", "D", "R", "A", "Z"])], &driver);

    assert_eq!(driver.entries("dispatch:"), s(&["N"]));
    assert_eq!(driver.entries("collect:"), s(&["D", "N"]));
    assert_eq!(driver.entries("verify:"), s(&["D", "N", "R"]));
    assert_eq!(driver.entries("merge:"), s(&["A", "D", "N", "R"]));
    assert_eq!(
        out,
        WaveRunOutcome {
            merged: s(&["N", "D", "R", "A"]),
            blocked_wave: None,
        }
    );
}

#[test]
fn needs_operator_blocks_next_wave_but_clean_peer_can_finish() {
    let driver = PhaseFake::new(&[
        ("READY", WaveTaskPhase::ReadyForVerification),
        ("HOLD", WaveTaskPhase::NeedsOperator),
        ("NEXT", WaveTaskPhase::New),
    ]);

    let out = run_wave_with(
        &[step(&["READY", "HOLD"]), step(&["NEXT"])],
        &driver,
    );

    assert_eq!(driver.entries("verify:"), s(&["READY"]));
    assert_eq!(driver.entries("merge:"), s(&["READY"]));
    assert!(driver.entries("dispatch:").is_empty());
    assert_eq!(
        out,
        WaveRunOutcome {
            merged: s(&["READY"]),
            blocked_wave: Some(0),
        }
    );
}

#[test]
fn classifier_maps_projection_and_fresh_inflight_fail_closed() {
    assert_eq!(classify_wave_phase(None, false), WaveTaskPhase::New);
    assert_eq!(
        classify_wave_phase(Some(TaskState::Dispatched), false),
        WaveTaskPhase::Dispatched
    );
    assert_eq!(
        classify_wave_phase(Some(TaskState::ReadyForVerification), false),
        WaveTaskPhase::ReadyForVerification
    );
    assert_eq!(
        classify_wave_phase(Some(TaskState::Approved), false),
        WaveTaskPhase::Approved
    );
    assert_eq!(
        classify_wave_phase(Some(TaskState::Recorded), false),
        WaveTaskPhase::Recorded
    );

    assert_eq!(
        classify_wave_phase(Some(TaskState::ReadyForVerification), true),
        WaveTaskPhase::NeedsOperator
    );
    assert_eq!(
        classify_wave_phase(Some(TaskState::Approved), true),
        WaveTaskPhase::NeedsOperator
    );
    for state in [
        TaskState::ChangesRequested,
        TaskState::Blocked,
        TaskState::Merged,
        TaskState::Reopened,
    ] {
        assert_eq!(
            classify_wave_phase(Some(state), false),
            WaveTaskPhase::NeedsOperator
        );
    }
}
