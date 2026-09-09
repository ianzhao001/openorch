use orch_host::card::RequiredReview;
use orch_host::plan::{compile_ir, TaskInput};

const BINDING: &str = "scope: {protectedPaths: []}\ngit: {pushPolicy: forbidden}\n";

fn mode() -> String {
    r#"agents:
  verifier: {adapter: root-manual, tier: none}
hitl: {mergeGate: auto}
verification: {mode: root-manual-fixed-head}
liveness: {monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}
scheduling:
  allowedAgents: [executor-desktop, executor-claw, executor-opencode]
  capacities:
    executor-desktop: {agent: 2, quota: 2, roles: [implement]}
    executor-claw: {agent: 1, quota: 1, roles: [primary-review]}
    executor-opencode: {agent: 3, quota: 3, roles: [secondary-review]}
budgets: {round: {wallMinutes: 60}}
git: {pushPolicy: forbidden}
"#
    .into()
}

fn task() -> TaskInput {
    TaskInput {
        id: "T".into(),
        agent: "executor-desktop".into(),
        seed_protocol: "pure-spec".into(),
        has_seeds: false,
        write_set: vec!["src/x.rs".into()],
        frozen_paths: vec![],
        gates_fast: vec!["check".into()],
        wall_minutes: Some(10),
        required_reviews: vec![RequiredReview {
            role: "primary".into(),
            agent: "executor-claw".into(),
        }],
        required_evidence: vec!["e1".into()],
        bootstrap_pre_signoff_attempt: None,
        card_source_sha256: "0".repeat(64),
        seed_source_sha256: Default::default(),
    }
}

#[test]
fn root_mode_liveness_and_capacities_are_typed_ir_truth() {
    let ir = compile_ir("r48", &mode(), BINDING, &[task()]).unwrap();
    assert_eq!(ir.verification.mode, "root-manual-fixed-head");
    assert_eq!(ir.verification.adapter, "root-manual");
    assert_eq!(ir.liveness.monitor_seconds, 15);
    assert_eq!(ir.liveness.working_stall_minutes, 10);
    assert_eq!(ir.liveness.confirm_samples, 2);
    assert_eq!(ir.scheduling.capacities["executor-desktop"].agent, 2);
}

#[test]
fn missing_unknown_or_zero_capacity_and_liveness_drift_are_rejected() {
    let missing = mode().replace(
        "    executor-desktop: {agent: 2, quota: 2, roles: [implement]}\n",
        "",
    );
    assert!(compile_ir("r48", &missing, BINDING, &[task()]).is_err());

    let zero = mode().replace("agent: 2, quota: 2", "agent: 0, quota: 2");
    assert!(compile_ir("r48", &zero, BINDING, &[task()]).is_err());

    let drift = mode().replace("workingStallMinutes: 10", "workingStallMinutes: 20");
    assert!(compile_ir("r48", &drift, BINDING, &[task()]).is_err());
}
