//! ═══ 红种子契约 · B90（agent-aware wave + prerequisites + single-flight）═══
//! 落位: orch/crates/orch-host/tests/wave_agent_guard.rs（逐字节复制）
//! 预期红（redForm: compile）：wave 中契约符号尚不存在 → error[E0432]。
//!
//! 负向变异下界：
//! M1 不按 agent 拆波 → same_agent_tasks_are_serialized 红；
//! M2 跨原波回填 → original_wave_boundary_is_preserved 红；
//! M3 缺 agent 仍派发 → missing_agent_fails_before_dispatch 红；
//! M4 collect join panic 继续 expect → collect_panic_maps_to_failed_in_input_order 红；
//! M5 任一前置闸 fail-open → prerequisites_fail_closed 红；
//! M6 不持 single-flight lease → second_wave_lease_is_rejected 红。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use orch_host::wave::{
    collect_tasks_with, split_wave_by_agent, validate_wave_prerequisites, TaskWaveOutcome,
    WaveLease, WavePrerequisites, WaveStep,
};

// B137：pid+ULID 不足（卡面要求 pid+seq+nanos），沿
// src/binding.rs::b106_scratch_dir 范式叠加模块级单调计数器。
static WAVE_LEASE_SEQ: AtomicU64 = AtomicU64::new(0);

fn step(tasks: &[&str]) -> WaveStep {
    let tasks = tasks.iter().map(|task| (*task).to_string()).collect::<Vec<_>>();
    WaveStep {
        merge_order: tasks.clone(),
        tasks,
    }
}

#[test]
fn same_agent_tasks_are_serialized() {
    let schedule = vec![step(&["A", "B", "C", "D"])];
    let agents = HashMap::from([
        ("A".to_string(), "codex".to_string()),
        ("B".to_string(), "codex".to_string()),
        ("C".to_string(), "opencode".to_string()),
        ("D".to_string(), "claw".to_string()),
    ]);
    let split = split_wave_by_agent(&schedule, &agents).unwrap();
    assert_eq!(split, vec![step(&["A", "C", "D"]), step(&["B"])]);
}

#[test]
fn original_wave_boundary_is_preserved() {
    let schedule = vec![step(&["A", "B"]), step(&["C"])];
    let agents = HashMap::from([
        ("A".to_string(), "codex".to_string()),
        ("B".to_string(), "codex".to_string()),
        ("C".to_string(), "opencode".to_string()),
    ]);
    let split = split_wave_by_agent(&schedule, &agents).unwrap();
    assert_eq!(split, vec![step(&["A"]), step(&["B"]), step(&["C"])]);
}

#[test]
fn missing_agent_fails_before_dispatch() {
    let schedule = vec![step(&["A", "B"])];
    let agents = HashMap::from([
        ("A".to_string(), "codex".to_string()),
        ("B".to_string(), "   ".to_string()),
    ]);
    let error = split_wave_by_agent(&schedule, &agents).unwrap_err();
    assert!(error.contains("B"));
}

#[test]
fn collect_panic_maps_to_failed_in_input_order() {
    let tasks = vec!["A".to_string(), "B".to_string(), "C".to_string()];
    let outcomes = collect_tasks_with(&tasks, &|task| {
        if task == "B" {
            panic!("synthetic worker crash");
        }
        TaskWaveOutcome::Collected
    });
    assert_eq!(
        outcomes,
        vec![
            ("A".to_string(), TaskWaveOutcome::Collected),
            ("B".to_string(), TaskWaveOutcome::Failed),
            ("C".to_string(), TaskWaveOutcome::Collected),
        ]
    );
}

#[test]
fn prerequisites_fail_closed() {
    let base = WavePrerequisites {
        plan_signed_off: true,
        bad_ledger_lines: 0,
        ir_digest_matches: true,
        task_ids: vec!["A".into(), "B".into()],
        seeded_red_tasks: vec!["A".into()],
        seed_verified_tasks: HashSet::from(["A".to_string()]),
    };
    assert!(validate_wave_prerequisites(&base).is_ok());

    let mut unsigned = base.clone();
    unsigned.plan_signed_off = false;
    assert!(validate_wave_prerequisites(&unsigned).is_err());

    let mut bad = base.clone();
    bad.bad_ledger_lines = 1;
    assert!(validate_wave_prerequisites(&bad).is_err());

    let mut drifted = base.clone();
    drifted.ir_digest_matches = false;
    assert!(validate_wave_prerequisites(&drifted).is_err());

    let mut missing_seed = base;
    missing_seed.seed_verified_tasks.clear();
    assert!(validate_wave_prerequisites(&missing_seed).is_err());
}

#[test]
fn second_wave_lease_is_rejected() {
    let orch_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("CARGO_MANIFEST_DIR 上溯两级应为 orch 工作区根");
    let seq = WAVE_LEASE_SEQ.fetch_add(1, Ordering::Relaxed);
    let root = orch_root
        .join("target/test-tmp")
        .join(format!(
            "orch-r43-wave-lease-{}-{}-{seq}",
            std::process::id(),
            ulid::Ulid::new()
        ));
    std::fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    let first = WaveLease::acquire(&root).unwrap();
    assert!(WaveLease::acquire(&root).is_err());
    drop(first);
    assert!(WaveLease::acquire(&root).is_ok());
    let _ = std::fs::remove_dir_all(root);
}
