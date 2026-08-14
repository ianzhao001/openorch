//! B135 seeded-red contract.
//!
//! Negative mutations that must turn the named case red:
//! M1. Reorder the escalation chain by idleness so an idle later agent
//!     skips ahead of the configured hierarchy.
//! M2. Let the implementer (or executor-desktop in any role) appear in its
//!     own review chain.
//! M3. Give an unknown agent a default chain instead of failing closed.

use orch_host::scheduler::{
    escalation_chain, escalation_chain_from_round_ir, next_candidate, review_chain,
};
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

static IR_TEST_SEQ: AtomicU64 = AtomicU64::new(0);

fn s(v: &str) -> String {
    v.to_string()
}

#[test]
fn escalation_chain_follows_user_hierarchy() {
    assert_eq!(
        escalation_chain("executor-desktop").unwrap(),
        vec![s("executor-desktop"), s("executor-claw"), s("executor-opencode")]
    );
    assert_eq!(
        escalation_chain("executor-claw").unwrap(),
        vec![s("executor-claw"), s("executor-opencode")]
    );
    assert_eq!(
        escalation_chain("executor-opencode").unwrap(),
        vec![s("executor-opencode")]
    );
    // M3: unmodeled agents have no chain.
    assert!(escalation_chain("agy").is_err());
}

#[test]
fn next_candidate_never_skips_ahead_of_order() {
    let chain = vec![s("executor-desktop"), s("executor-claw"), s("executor-opencode")];
    // M1: the front of the chain wins while it is eligible, no matter who is idle.
    assert_eq!(next_candidate(&chain, &[], &[]).unwrap(), "executor-desktop");
    // Two-strike failure moves to the next rung; busy agents are stepped over.
    assert_eq!(
        next_candidate(&chain, &[s("executor-desktop")], &[]).unwrap(),
        "executor-claw"
    );
    assert_eq!(
        next_candidate(&chain, &[], &[s("executor-desktop")]).unwrap(),
        "executor-claw"
    );
    // Chain exhausted -> Err: root takeover, never a silent wraparound.
    assert!(next_candidate(
        &chain,
        &[s("executor-desktop"), s("executor-claw")],
        &[s("executor-opencode")]
    )
    .is_err());
}

#[test]
fn review_chain_excludes_implementer_and_desktop_review() {
    let codex = review_chain("executor-desktop").unwrap();
    assert_eq!(
        codex,
        vec![
            (s("primary"), s("executor-claw")),
            (s("secondary"), s("executor-opencode"))
        ]
    );
    let claw = review_chain("executor-claw").unwrap();
    assert_eq!(claw, vec![(s("primary"), s("executor-opencode"))]);
    let opencode = review_chain("executor-opencode").unwrap();
    assert_eq!(opencode, vec![(s("primary"), s("executor-claw"))]);
    // M2: nobody reviews their own delivery and executor-desktop never reviews.
    for (implementer, chain) in [
        ("executor-desktop", &codex),
        ("executor-claw", &claw),
        ("executor-opencode", &opencode),
    ] {
        assert!(chain.iter().all(|(_, agent)| agent != implementer));
        assert!(chain.iter().all(|(_, agent)| agent != "executor-desktop"));
    }
    assert!(review_chain("agy").is_err());
}

#[test]
fn signed_round_ir_order_drives_the_runtime_chain() {
    let seq = IR_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .unwrap()
        .join("target")
        .join("test-tmp")
        .join(format!("b142-ir-hierarchy-{}-{seq}", std::process::id()));
    fs::create_dir_all(root.join("coordination/rounds/r50")).unwrap();
    fs::write(
        root.join("coordination/rounds/r50/ROUND-IR.yaml"),
        "round: r50\nrevision: 1\npolicy:\n  pushPolicy: forbidden\n  mergePolicy: ff-only-else-no-ff\n  autoMergeOnPass: false\nbudgets: {}\nscheduling:\n  allowedAgents: [executor-opencode, executor-desktop]\ntasks: []\nskipped: []\n",
    )
    .unwrap();

    assert_eq!(
        escalation_chain_from_round_ir(&root, "r50", "executor-opencode").unwrap(),
        vec![s("executor-opencode"), s("executor-desktop")]
    );
    assert!(
        escalation_chain_from_round_ir(&root, "r50", "executor-claw").is_err(),
        "legacy hardcoded agents must not leak into a signed hierarchy"
    );
    fs::remove_dir_all(root).unwrap();
}

// ─────────────── B147：seam 级判定 pin（非 seed，卡面交付边界 5 的支撑单测） ───────────────
// 主审 B147-A0001 FAIL 整改说明：卡面强制生产路径回归已升级为真实账本事实断言，
// 见 serve.rs mod tests——`b147_wave_dispatches_dependent_only_after_prerequisite_
// taskrecorded`（真实 run_wave/run_dispatch + 账本断言）与 `b147_critical_two_
// strikes_escalate_exhausted_without_downgrade`（真实 liveness_monitor_tick 接替
// 路径 + 账本断言）。以下两条保留为 seam 级 pin：纯函数判定与 digest 覆盖的
// 细粒度锁定，不再自称生产路径回归。

/// seam pin 1：A/B writeSet 完全不相交 + B dependsOn A → 静态分波把 B 锁在
/// 严格更晚波次，blocker 纯函数判定 Recorded 前阻塞、Recorded 后放行。
#[test]
fn explicit_dependencies_hold_dependents_until_prerequisites_record() {
    use orch_host::wave::{
        plan_wave_schedule_with_dependencies, run_wave_with, TaskWaveOutcome, WaveDriver,
    };

    // 记录派发顺序的 fake driver：全部成功，无需真实 spawn。
    #[derive(Default)]
    struct RecordingDriver {
        dispatched: std::sync::Mutex<Vec<String>>,
    }
    impl WaveDriver for RecordingDriver {
        fn already_recorded(&self, _task: &str) -> bool {
            false
        }
        fn dispatch(&self, task: &str) -> bool {
            self.dispatched.lock().unwrap().push(task.to_string());
            true
        }
        fn drive_to_collect(&self, _task: &str) -> TaskWaveOutcome {
            TaskWaveOutcome::Collected
        }
        fn verify(&self, _task: &str) -> bool {
            true
        }
        fn merge(&self, _task: &str) -> bool {
            true
        }
    }

    // A/B writeSet 完全不相交；B 显式 dependsOn A（输入为拓扑序，与 ROUND-IR 一致）。
    let tasks = vec![
        ("A".to_string(), vec!["src/a.rs".to_string()]),
        ("B".to_string(), vec!["src/b.rs".to_string()]),
    ];
    let dependencies = std::collections::BTreeMap::from([
        ("A".to_string(), Vec::<String>::new()),
        ("B".to_string(), vec!["A".to_string()]),
    ]);
    let schedule = plan_wave_schedule_with_dependencies(&tasks, &dependencies);
    // 首波只派 A：显式前驱未 Recorded，B 落在严格更晚的波次（writeSet 不相交也拆波）。
    assert_eq!(schedule[0].tasks, vec!["A".to_string()]);
    assert_eq!(schedule[1].tasks, vec!["B".to_string()]);

    // 可派发判定的依赖分量：A 未 Recorded → B 阻塞；A Recorded → B 放行。
    let none: Vec<String> = Vec::new();
    assert_eq!(
        orch_host::plan::task_dependency_blockers(&dependencies["B"], &none),
        vec!["A".to_string()]
    );
    assert!(orch_host::plan::task_dependency_blockers(
        &dependencies["B"],
        &["A".to_string()]
    )
    .is_empty());

    // 生产编排（run_wave_with = run_wave 的波次执行体）：A 先派先收口，B 后派。
    let driver = RecordingDriver::default();
    let outcome = run_wave_with(&schedule, &driver);
    assert_eq!(outcome.blocked_wave, None);
    assert_eq!(outcome.merged, vec!["A".to_string(), "B".to_string()]);
    assert_eq!(
        *driver.dispatched.lock().unwrap(),
        vec!["A".to_string(), "B".to_string()]
    );
}

/// seam pin 2：critical 任务的接替候选 seam——无 critical 能力的候选被
/// next_succession_candidate 过滤（Err 含 eligible-chain-exhausted），有能力
/// 候选在链上时跳过无能力者中签。能力数据消费签核 IR 的 IrTask.requirement，
/// 且 requirement 进入 validation digest（digest 覆盖）。
#[test]
fn critical_succession_skips_incapable_candidates_and_escalates() {
    fn ir_yaml(trailer_roles: &str, capabilities: &str) -> String {
        format!(
            "schemaVersion: 2\n\
             round: r51\n\
             revision: 1\n\
             policy: {{pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff, autoMergeOnPass: false}}\n\
             budgets: {{}}\n\
             scheduling:\n\
             \x20 allowedAgents: [executor-desktop, executor-claw, executor-opencode]\n\
             \x20 capacities:\n\
             \x20   executor-desktop: {{agent: 1, quota: 1, roles: [implement, critical-implement]}}\n\
             {trailer_roles}\n\
             tasks:\n\
             \x20 - id: CRIT\n\
             \x20   agent: executor-desktop\n\
             \x20   seedProtocol: seeded-red\n\
             \x20   writeSet: [src/crit.rs]\n\
             \x20   frozenPaths: [\"coordination/**\"]\n\
             \x20   gatesFast: [testFast]\n\
             \x20   requirement: {{capabilities: [{capabilities}]}}\n\
             skipped: []\n"
        )
    }
    let incapable = "    executor-claw: {agent: 1, quota: 1, roles: [implement]}\n    executor-opencode: {agent: 1, quota: 1, roles: [implement]}";
    let capable = "    executor-claw: {agent: 1, quota: 1, roles: [implement]}\n    executor-opencode: {agent: 1, quota: 1, roles: [implement, critical-implement]}";

    let ir = orch_host::plan::parse_signed_round_ir(&ir_yaml(incapable, "critical-implement"))
        .unwrap();
    assert_eq!(
        ir.tasks[0].requirement.capabilities,
        vec!["critical-implement".to_string()]
    );
    let chain = orch_host::scheduler::escalation_chain_from_hierarchy(
        &ir.scheduling.allowed_agents,
        "executor-desktop",
    )
    .unwrap();
    // critical 任务两振：首派 agent（executor-desktop）已出局。
    let failed = vec!["executor-desktop".to_string()];
    let none: Vec<String> = Vec::new();
    // 链上再无 critical-implement 能力候选 → 升级事件路径（Err），绝不降级派发。
    let err = orch_host::serve::next_succession_candidate(&ir, "CRIT", &chain, &failed, &none)
        .unwrap_err();
    assert!(err.contains("eligible-chain-exhausted"));

    // 对照：executor-opencode 持 critical-implement → 跳过无能力的 executor-claw 中签。
    let ir_capable =
        orch_host::plan::parse_signed_round_ir(&ir_yaml(capable, "critical-implement")).unwrap();
    let next = orch_host::serve::next_succession_candidate(
        &ir_capable,
        "CRIT",
        &chain,
        &failed,
        &none,
    )
    .unwrap();
    assert_eq!(next, "executor-opencode");

    // digest 覆盖：仅 requirement.capabilities 不同的两版 IR digest 必须不同。
    let ir_plain = orch_host::plan::parse_signed_round_ir(&ir_yaml(incapable, "")).unwrap();
    assert_ne!(
        orch_host::plan::validation_digest(&ir),
        orch_host::plan::validation_digest(&ir_plain)
    );
}
