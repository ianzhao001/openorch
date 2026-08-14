//! ═══ 红种子契约 · B79（Phase B/run-wave 真实并发 collect + exit 码 · 并行落地）═══
//! 落位: orch/crates/orch-host/tests/wave_concurrent.rs（逐字节复制）
//! 预期红（redForm: compile）：`wave::wave_exit_code` 不存在 + `WaveDriver` 尚无 `Sync` 上界
//!   （Mutex/Atomic fake 需 Sync 才能被 thread::scope 共享）→ error[E0432]/E0277。
//!
//! 背景：B78 已修 run-wave 编排正确性（verify-before-merge、失败→blocked），但 collect 仍**串行**
//! （`run_wave_with` `.iter().map(drive_to_collect)`）——每个 collect 阻塞数分钟等 REPORT，串行则
//! N 棒 = N×等待，**并行的意义丧失**。本棒把 collect 相改为 `std::thread::scope` **波内并发**（各 collect
//! 线程各自阻塞在 I/O 等待，真正重叠），并补 `wave_exit_code`（blocked→非0）供 CLI 返独立退出码。
//! 为让 `&dyn WaveDriver` 可跨线程共享，`WaveDriver` 加 `Sync` 上界（既有 RefCell fake 须改 Mutex/Atomic）。
//!
//! 目标契约（改 orch_host::wave；timeout 透传/`--model`/CLI 接线见任务卡）：
//!   - `pub trait WaveDriver: Sync { ... }`（方法签名同 B78：dispatch->bool / verify / merge / drive_to_collect / already_recorded）。
//!   - `run_wave_with` 的 collect 相用 `std::thread::scope` 对 pending 并发调用 `drive_to_collect`，
//!     **结果按输入序归集**（gate/verify/merge 相仍串行、语义同 B78 不变）。
//!   - `pub fn wave_exit_code(outcome: &WaveRunOutcome) -> u8`：`blocked_wave==None ⇒ 0`，`Some(_) ⇒ 1`。
//!
//! 负向变异下界（转绿后逐条自证）：
//!   M1 collect 仍串行（无 thread::scope）⇒ collect_runs_concurrently 红（max 观测=1）；
//!   M2 wave_exit_code 不区分 blocked（恒 0）⇒ exit_code_maps_blocked 红。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use orch_host::wave::{
    run_wave_with, wave_exit_code, TaskWaveOutcome, WaveDriver, WaveRunOutcome, WaveStep,
};

fn s(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|x| x.to_string()).collect()
}
fn step(tasks: &[&str]) -> WaveStep {
    WaveStep { tasks: s(tasks), merge_order: s(tasks) }
}

/// 线程安全 fake（Sync）：drive_to_collect 记录「同时在场」峰值以证并发。
struct ConcFake {
    cur: AtomicUsize,
    max: AtomicUsize,
    merged_log: Mutex<Vec<String>>,
}
impl ConcFake {
    fn new() -> Self {
        ConcFake { cur: AtomicUsize::new(0), max: AtomicUsize::new(0), merged_log: Mutex::new(Vec::new()) }
    }
}
impl WaveDriver for ConcFake {
    fn already_recorded(&self, _task: &str) -> bool {
        false
    }
    fn dispatch(&self, _task: &str) -> bool {
        true
    }
    fn drive_to_collect(&self, _task: &str) -> TaskWaveOutcome {
        let n = self.cur.fetch_add(1, Ordering::SeqCst) + 1;
        self.max.fetch_max(n, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(60)); // 并发窗口：并行则同时在场，串行则恒 1
        self.cur.fetch_sub(1, Ordering::SeqCst);
        TaskWaveOutcome::Collected
    }
    fn verify(&self, _task: &str) -> bool {
        true
    }
    fn merge(&self, task: &str) -> bool {
        self.merged_log.lock().unwrap().push(task.to_string());
        true
    }
}

// ── collect 相真实并发：3 棒应观测到「同时在场」峰值 >= 2 ──
#[test]
fn collect_runs_concurrently() {
    let d = ConcFake::new();
    let out = run_wave_with(&[step(&["A", "B", "C"])], &d);
    let peak = d.max.load(Ordering::SeqCst);
    assert!(peak >= 2, "collect 应波内并发，同时在场峰值应 >=2，实测 {peak}");
    assert_eq!(out, WaveRunOutcome { merged: s(&["A", "B", "C"]), blocked_wave: None });
}

// ── CLI 退出码映射：clean→0，blocked→1 ──
#[test]
fn exit_code_maps_blocked() {
    assert_eq!(wave_exit_code(&WaveRunOutcome { merged: s(&["A"]), blocked_wave: None }), 0);
    assert_eq!(wave_exit_code(&WaveRunOutcome { merged: vec![], blocked_wave: Some(0) }), 1);
}
