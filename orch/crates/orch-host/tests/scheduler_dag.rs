//! ═══ 红种子契约 · B91（纯 scheduler DAG/action planner）═══
//! 落位: orch/crates/orch-host/tests/scheduler_dag.rs（逐字节复制）
//! 预期红（redForm: compile）：scheduler 契约符号尚不存在 → error[E0432]。
//!
//! 负向变异下界：
//! M1 忽略 writeSet 前驱 → conflicting_predecessor_blocks_dispatch 红；
//! M2 把 agent 当依赖而非容量 → same_agent_releases_after_current_slot 红；
//! M3 忽略 quotaDomain lane → quota_domain_capacity_is_one 红；
//! M4 不在规划中预占 lane → one_plan_never_dispatches_two_to_same_lane 红；
//! M5 改变输入序 → stable_input_order_and_recorded_skip 红；
//! M6 malformed task fail-open → malformed_tasks_fail_closed 红。

use std::collections::BTreeSet;

use orch_host::scheduler::{
    plan_actions, SchedulerAction, SchedulerSnapshot, SchedulerTask,
};

fn task(id: &str, agent: &str, quota: &str, write_set: &[&str]) -> SchedulerTask {
    SchedulerTask {
        id: id.into(),
        agent: agent.into(),
        quota_domain: quota.into(),
        write_set: write_set.iter().map(|path| (*path).to_string()).collect(),
    }
}

fn snapshot(recorded: &[&str], agents: &[&str], quotas: &[&str]) -> SchedulerSnapshot {
    SchedulerSnapshot {
        recorded: recorded.iter().map(|value| (*value).to_string()).collect(),
        active_agents: agents.iter().map(|value| (*value).to_string()).collect(),
        active_quota_domains: quotas.iter().map(|value| (*value).to_string()).collect(),
    }
}

#[test]
fn conflicting_predecessor_blocks_dispatch() {
    let tasks = vec![
        task("A", "codex", "openai", &["src/a.rs"]),
        task("B", "opencode", "zhipu", &["src/a.rs"]),
        task("C", "claw", "moonshot", &["src/c.rs"]),
    ];
    let actions = plan_actions(&tasks, &snapshot(&[], &[], &[])).unwrap();
    assert_eq!(
        actions,
        vec![
            SchedulerAction::Dispatch("A".into()),
            SchedulerAction::Wait {
                task: "B".into(),
                blockers: vec!["task:A".into()],
            },
            SchedulerAction::Dispatch("C".into()),
        ]
    );
}

#[test]
fn same_agent_releases_after_current_slot() {
    let tasks = vec![
        task("A", "codex", "openai", &["src/a.rs"]),
        task("B", "codex", "openai", &["src/b.rs"]),
    ];
    let first = plan_actions(&tasks, &snapshot(&[], &[], &[])).unwrap();
    assert_eq!(first[0], SchedulerAction::Dispatch("A".into()));
    assert!(matches!(first[1], SchedulerAction::Wait { .. }));

    let second = plan_actions(&tasks, &snapshot(&["A"], &[], &[])).unwrap();
    assert_eq!(second, vec![SchedulerAction::Dispatch("B".into())]);
}

#[test]
fn quota_domain_capacity_is_one() {
    let tasks = vec![
        task("A", "codex", "shared", &["src/a.rs"]),
        task("B", "opencode", "shared", &["src/b.rs"]),
    ];
    let actions = plan_actions(&tasks, &snapshot(&[], &[], &["shared"])).unwrap();
    assert!(actions.iter().all(|action| !matches!(action, SchedulerAction::Dispatch(_))));
}

#[test]
fn one_plan_never_dispatches_two_to_same_lane() {
    let tasks = vec![
        task("A", "codex", "openai", &["src/a.rs"]),
        task("B", "codex", "openai", &["src/b.rs"]),
        task("C", "opencode", "zhipu", &["src/c.rs"]),
    ];
    let actions = plan_actions(&tasks, &snapshot(&[], &[], &[])).unwrap();
    let dispatched = actions
        .iter()
        .filter_map(|action| match action {
            SchedulerAction::Dispatch(task) => Some(task.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(dispatched, vec!["A", "C"]);
}

#[test]
fn stable_input_order_and_recorded_skip() {
    let tasks = vec![
        task("C", "claw", "moonshot", &["src/c.rs"]),
        task("A", "codex", "openai", &["src/a.rs"]),
        task("B", "opencode", "zhipu", &["src/b.rs"]),
    ];
    let actions = plan_actions(&tasks, &snapshot(&["A"], &[], &[])).unwrap();
    assert_eq!(
        actions,
        vec![
            SchedulerAction::Dispatch("C".into()),
            SchedulerAction::Dispatch("B".into()),
        ]
    );
    let _: BTreeSet<String> = snapshot(&[], &[], &[]).recorded;
}

#[test]
fn malformed_tasks_fail_closed() {
    let duplicate = vec![
        task("A", "codex", "openai", &["src/a.rs"]),
        task("A", "opencode", "zhipu", &["src/b.rs"]),
    ];
    assert!(plan_actions(&duplicate, &snapshot(&[], &[], &[])).is_err());
    assert!(plan_actions(
        &[task("B", " ", "openai", &["src/b.rs"])],
        &snapshot(&[], &[], &[])
    )
    .is_err());
    assert!(plan_actions(
        &[task("C", "codex", " ", &["src/c.rs"])],
        &snapshot(&[], &[], &[])
    )
    .is_err());
}
