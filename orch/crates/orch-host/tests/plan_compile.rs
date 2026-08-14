//! ═══ 红种子契约 · B14 ═══════════════════════════════════════════════
//! 落位路径: orch/crates/orch-host/tests/plan_compile.rs （逐字节复制，不得改动）
//!
//! 预期红（redForm: compile，planner oracle 预验 2026-07-22）:
//!   编译红——`orch_host::plan` 模块尚不存在（E0432 类），5 用例全红。
//!
//! 负向变异自证清单（下界语义，errata E9）:
//!   M1 校验④删 protectedPaths 相交拒绝 → ② 红
//!   M2 校验⑨删 Tier F 必须 wake 上限   → ③ 红
//!   M3 digest 掺入随机量（不再幂等）    → ⑤ 红
//! ══════════════════════════════════════════════════════════════════

use orch_host::plan::{self, TaskInput};

const MODE_YAML: &str = r#"
preset: relay
objective: "test round"
agents:
  executor: {adapter: codex-desktop, tier: F, waitStyle: self-wait, agentId: executor-desktop}
  verifier: {adapter: root-manual, tier: none}
hitl: {planSignoff: required, mergeGate: auto}
verification: {mode: root-manual-fixed-head}
liveness: {monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}
scheduling:
  allowedAgents: [executor-desktop, executor-claw, executor-opencode]
  capacities:
    executor-desktop: {agent: 2, quota: 2, roles: [implement]}
    executor-claw: {agent: 1, quota: 1, roles: [primary-review]}
    executor-opencode: {agent: 3, quota: 3, roles: [secondary-review]}
budgets:
  round: {maxUsd: 4.0, wallMinutes: 90, maxModelWakes: 12}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
"#;

const BINDING_YAML: &str = r#"
project: {ecosystems: [rust]}
commands:
  testFast: {argv: [cargo, test]}
  check: {argv: [cargo, check]}
gates: {fast: [testFast, check]}
scope:
  protectedPaths: ["coordination/**", "design/**"]
git: {pushPolicy: forbidden}
"#;

fn task_ok() -> TaskInput {
    TaskInput {
        id: "T1".into(),
        agent: "executor-desktop".into(),
        seed_protocol: "seeded-red".into(),
        has_seeds: true,
        write_set: vec!["orch/crates/orch-host/src/x.rs".into()],
        frozen_paths: vec!["coordination/**".into()],
        gates_fast: vec!["testFast".into(), "check".into()],
        wall_minutes: Some(40),
        required_reviews: vec![orch_host::card::RequiredReview {
            role: "primary".into(),
            agent: "executor-claw".into(),
        }],
        required_evidence: vec!["contract".into()],
        bootstrap_pre_signoff_attempt: None,
        card_source_sha256: "0".repeat(64),
        seed_source_sha256: std::collections::BTreeMap::from([(
            "coordination/rounds/r99/seeds/T1/contract.rs".into(),
            "1".repeat(64),
        )]),
    }
}

#[test]
fn compiles_relay_round_ir() {
    // ① 正常编译：IR 含 round/revision/policy/tasks
    let ir = plan::compile_ir("r99", MODE_YAML, BINDING_YAML, &[task_ok()]).expect("compiles");
    assert_eq!(ir.round, "r99");
    assert_eq!(ir.revision, 1);
    assert_eq!(ir.policy.push_policy, "forbidden");
    assert!(ir.policy.auto_merge_on_pass);
    assert_eq!(ir.tasks.len(), 1);
    assert_eq!(ir.tasks[0].id, "T1");
}

#[test]
fn validation4_rejects_protected_writeset() {
    // ② 校验④：writeSet ∩ protectedPaths ≠ ∅ → 拒绝
    let mut bad = task_ok();
    bad.write_set
        .push("design/00-overview-and-decisions.md".into());
    let err = plan::compile_ir("r99", MODE_YAML, BINDING_YAML, &[bad]).expect_err("must reject");
    assert!(
        err.to_string().contains("protected"),
        "错误应指明 protected：{err}"
    );
}

#[test]
fn validation9_requires_wake_cap_for_tier_f() {
    // ③ 校验⑨：Tier F（unknown-cost）在场时 maxModelWakes 必填、不可为 0
    let mode_no_wakes = MODE_YAML.replace(", maxModelWakes: 12", "");
    let err = plan::compile_ir("r99", &mode_no_wakes, BINDING_YAML, &[task_ok()])
        .expect_err("must reject");
    assert!(
        err.to_string().contains("wake"),
        "错误应指明 wake 上限：{err}"
    );
}

#[test]
fn validation1_merger_must_equal_reviewer() {
    // ④ 校验①：merger 恒=reviewer——mode 显式配 merger≠reviewer 即拒
    let mode_bad_merger = format!("{MODE_YAML}  merger: {{adapter: other-tool}}\n");
    // 注：merger 段缩进进 agents 之下
    let mode_bad_merger = mode_bad_merger.replace("git: {pushPolicy", "gitx: {pushPolicy");
    let _ = mode_bad_merger; // 上两行仅为构造非法样本的说明性代码——真正的非法样本如下
    let bad = MODE_YAML.replace(
        "  verifier: {adapter: root-manual, tier: none}",
        "  verifier: {adapter: root-manual, tier: none}\n  merger: {adapter: other-tool}",
    );
    let err = plan::compile_ir("r99", &bad, BINDING_YAML, &[task_ok()]).expect_err("must reject");
    assert!(
        err.to_string().contains("merger"),
        "错误应指明 merger：{err}"
    );
}

#[test]
fn validation_digest_is_deterministic() {
    // ⑤ TaskValidated digest 幂等：同输入同 digest
    let a = plan::compile_ir("r99", MODE_YAML, BINDING_YAML, &[task_ok()]).unwrap();
    let b = plan::compile_ir("r99", MODE_YAML, BINDING_YAML, &[task_ok()]).unwrap();
    assert_eq!(plan::validation_digest(&a), plan::validation_digest(&b));
    assert_eq!(plan::validation_digest(&a).len(), 64);
}

#[test]
fn unsafe_gate_refs_are_rejected_before_they_can_become_log_paths() {
    for gate in ["", "..", "../victim", "/tmp/victim", "a/b", "a\\b"] {
        let mut bad = task_ok();
        bad.gates_fast = vec![gate.to_string()];
        let error = plan::compile_ir("r99", MODE_YAML, BINDING_YAML, &[bad])
            .expect_err("unsafe gate ref must fail closed");
        assert!(error.to_string().contains("gate ref"), "{gate:?}: {error}");
    }
}
