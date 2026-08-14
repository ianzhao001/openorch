use orch_host::card::RequiredReview;
use orch_host::plan::{compile_ir, TaskInput};

const MODE: &str = r#"
agents:
  executor: {adapter: test, tier: none}
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
budgets:
  round: {maxUsd: 1, wallMinutes: 90, maxModelWakes: 4}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
"#;

const BINDING: &str = r#"
scope: {protectedPaths: ["coordination/**"]}
git: {pushPolicy: forbidden}
"#;

fn task() -> TaskInput {
    TaskInput {
        id: "T1".into(),
        agent: "executor-desktop".into(),
        seed_protocol: "pure-spec".into(),
        has_seeds: false,
        write_set: vec!["src/lib.rs".into()],
        frozen_paths: vec![],
        gates_fast: vec!["testFast".into()],
        wall_minutes: Some(30),
        required_reviews: vec![RequiredReview {
            role: "primary".into(),
            agent: "executor-claw".into(),
        }],
        required_evidence: vec!["fixed-head".into()],
        bootstrap_pre_signoff_attempt: None,
        card_source_sha256: "0".repeat(64),
        seed_source_sha256: Default::default(),
    }
}

#[test]
fn exact_required_review_and_evidence_sets_are_persisted_in_ir() {
    let ir = compile_ir("r1", MODE, BINDING, &[task()]).unwrap();
    assert_eq!(ir.tasks[0].required_reviews, task().required_reviews);
    assert_eq!(ir.tasks[0].required_evidence, vec!["fixed-head"]);
    assert_eq!(ir.tasks[0].bootstrap_pre_signoff_attempt, None);
}

#[test]
fn primary_uniqueness_reviewer_independence_and_safe_ids_fail_closed() {
    let mut no_primary = task();
    no_primary.required_reviews[0].role = "secondary".into();
    assert!(compile_ir("r1", MODE, BINDING, &[no_primary]).is_err());

    let mut self_review = task();
    self_review.required_reviews[0].agent = "executor-desktop".into();
    assert!(compile_ir("r1", MODE, BINDING, &[self_review]).is_err());

    let mut traversal = task();
    traversal.required_evidence = vec!["../escape".into()];
    assert!(compile_ir("r1", MODE, BINDING, &[traversal]).is_err());

    for unsafe_prefix in ["-secondary", "_secondary"] {
        let mut bad_role = task();
        bad_role.required_reviews.push(RequiredReview {
            role: unsafe_prefix.into(),
            agent: "executor-opencode".into(),
        });
        assert!(compile_ir("r1", MODE, BINDING, &[bad_role]).is_err());

        let mut bad_evidence = task();
        bad_evidence.required_evidence = vec![unsafe_prefix.into()];
        assert!(compile_ir("r1", MODE, BINDING, &[bad_evidence]).is_err());
    }

    let mut unknown_reviewer = task();
    unknown_reviewer.required_reviews[0].agent = "executor-unknown".into();
    assert!(compile_ir("r1", MODE, BINDING, &[unknown_reviewer]).is_err());

    let reviewer_without_primary_capability = MODE.replace("primary-review", "secondary-review");
    assert!(compile_ir(
        "r1",
        &reviewer_without_primary_capability,
        BINDING,
        &[task()]
    )
    .is_err());

    let implementer_without_capability = MODE.replace("roles: [implement]", "roles: [observer]");
    assert!(compile_ir(
        "r1",
        &implementer_without_capability,
        BINDING,
        &[task()]
    )
    .is_err());
}

#[test]
fn bootstrap_permit_is_exact_safe_and_unique_across_the_round() {
    let mut permitted = task();
    permitted.bootstrap_pre_signoff_attempt = Some("T1-A0001".into());
    let ir = compile_ir("r1", MODE, BINDING, &[permitted.clone()]).unwrap();
    assert_eq!(
        ir.tasks[0].bootstrap_pre_signoff_attempt.as_deref(),
        Some("T1-A0001")
    );

    for value in ["_T1-A0001", "T1-A1", "T2-A0001", "T1-A0000"] {
        let mut bad = task();
        bad.bootstrap_pre_signoff_attempt = Some(value.into());
        assert!(compile_ir("r1", MODE, BINDING, &[bad]).is_err(), "{value}");
    }

    let mut second = task();
    second.id = "T2".into();
    second.bootstrap_pre_signoff_attempt = Some("T2-A0001".into());
    assert!(compile_ir("r1", MODE, BINDING, &[permitted, second]).is_err());
}
