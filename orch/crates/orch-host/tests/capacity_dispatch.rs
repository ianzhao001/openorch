//! 红种子契约 · B104 · 容量计数调度。
//! 预期红：compile，缺少 CapacityLimits/CapacitySnapshot/plan_actions_with_capacity/
//! split_wave_by_capacity。
//! M1：把 OpenCode cap 从 3 退化成布尔占用/1，three_opencode_slots_and_one_claw_dispatch 红。
//! M2：规划中不预占槽位，fourth_opencode_waits 红。
//! M3：忽略已有 active count，existing_usage_consumes_capacity 红。

use std::collections::{BTreeMap, BTreeSet, HashMap};

use orch_host::scheduler::{
    plan_actions_with_capacity, CapacityLimits, CapacitySnapshot, SchedulerAction, SchedulerTask,
};
use orch_host::wave::{split_wave_by_capacity, WaveStep};

fn task(id: &str, agent: &str, quota: &str, path: &str) -> SchedulerTask {
    SchedulerTask {
        id: id.into(),
        agent: agent.into(),
        quota_domain: quota.into(),
        write_set: vec![path.into()],
    }
}

fn limits() -> CapacityLimits {
    CapacityLimits {
        agents: BTreeMap::from([
            ("executor-opencode".into(), 3),
            ("executor-claw".into(), 1),
        ]),
        quota_domains: BTreeMap::from([("zhipu".into(), 3), ("moonshot".into(), 1)]),
    }
}

fn empty_snapshot() -> CapacitySnapshot {
    CapacitySnapshot {
        recorded: BTreeSet::new(),
        active_agent_counts: BTreeMap::new(),
        active_quota_counts: BTreeMap::new(),
    }
}

#[test]
fn three_opencode_slots_and_one_claw_dispatch() {
    let tasks = vec![
        task("O1", "executor-opencode", "zhipu", "src/o1.rs"),
        task("O2", "executor-opencode", "zhipu", "src/o2.rs"),
        task("O3", "executor-opencode", "zhipu", "src/o3.rs"),
        task("C1", "executor-claw", "moonshot", "src/c1.rs"),
    ];
    let got = plan_actions_with_capacity(&tasks, &empty_snapshot(), &limits()).unwrap();
    assert_eq!(
        got,
        vec![
            SchedulerAction::Dispatch("O1".into()),
            SchedulerAction::Dispatch("O2".into()),
            SchedulerAction::Dispatch("O3".into()),
            SchedulerAction::Dispatch("C1".into()),
        ]
    );
}

#[test]
fn fourth_opencode_waits() {
    let tasks = (1..=4)
        .map(|i| task(&format!("O{i}"), "executor-opencode", "zhipu", &format!("src/o{i}.rs")))
        .collect::<Vec<_>>();
    let got = plan_actions_with_capacity(&tasks, &empty_snapshot(), &limits()).unwrap();
    assert_eq!(
        got.iter().filter(|a| matches!(a, SchedulerAction::Dispatch(_))).count(),
        3
    );
    assert!(matches!(got[3], SchedulerAction::Wait { ref task, .. } if task == "O4"));
}

#[test]
fn existing_usage_consumes_capacity() {
    let snapshot = CapacitySnapshot {
        recorded: BTreeSet::new(),
        active_agent_counts: BTreeMap::from([("executor-opencode".into(), 2)]),
        active_quota_counts: BTreeMap::from([("zhipu".into(), 2)]),
    };
    let tasks = vec![
        task("O1", "executor-opencode", "zhipu", "src/o1.rs"),
        task("O2", "executor-opencode", "zhipu", "src/o2.rs"),
    ];
    let got = plan_actions_with_capacity(&tasks, &snapshot, &limits()).unwrap();
    assert!(matches!(got[0], SchedulerAction::Dispatch(_)));
    assert!(matches!(got[1], SchedulerAction::Wait { .. }));
}

#[test]
fn capacity_wave_keeps_three_same_agent_tasks_together() {
    let schedule = vec![WaveStep {
        tasks: vec!["O1".into(), "O2".into(), "O3".into(), "O4".into(), "C1".into()],
        merge_order: vec!["O1".into(), "O2".into(), "O3".into(), "O4".into(), "C1".into()],
    }];
    let agents = HashMap::from([
        ("O1".into(), "executor-opencode".into()),
        ("O2".into(), "executor-opencode".into()),
        ("O3".into(), "executor-opencode".into()),
        ("O4".into(), "executor-opencode".into()),
        ("C1".into(), "executor-claw".into()),
    ]);
    let caps = HashMap::from([
        ("executor-opencode".into(), 3usize),
        ("executor-claw".into(), 1usize),
    ]);
    let waves = split_wave_by_capacity(&schedule, &agents, &caps).unwrap();
    assert_eq!(waves.len(), 2);
    assert_eq!(waves[0].tasks, vec!["O1", "O2", "O3", "C1"]);
    assert_eq!(waves[1].tasks, vec!["O4"]);
}
