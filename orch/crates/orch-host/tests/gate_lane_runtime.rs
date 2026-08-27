use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use orch_host::binding::{
    parse_binding_bytes, validate_locked_rust_gates, validate_rust_check_all_targets, Binding,
};
use orch_host::plan::{
    self, resolved_command_argv_digest_at_policy_base, ResolvedCommandArgvV1, TaskInput,
};

const BINDING_SOURCE: &str = include_str!("../src/binding.rs");
const COLLECT_SOURCE: &str = include_str!("../src/collect.rs");
const GATE_SOURCE: &str = include_str!("../src/gate.rs");
const GUIDE_SOURCE: &str = include_str!("../../../docs/AI-MECHANICAL-GUIDE.md");
const PLAN_SOURCE: &str = include_str!("../src/plan.rs");
const SRCSHAPE_SOURCE: &str = include_str!("../src/srcshape.rs");
const TIERF_SOURCE: &str = include_str!("../src/tierf.rs");

const MODE_YAML: &str = r#"
preset: relay
objective: "gate lane runtime"
agents:
  executor: {adapter: codex-desktop, tier: F, waitStyle: self-wait, agentId: executor-desktop}
  verifier: {adapter: root-manual, tier: none}
hitl: {planSignoff: required, mergeGate: auto}
verification: {mode: root-manual-fixed-head}
liveness: {monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}
scheduling:
  allowedAgents: [executor-desktop, executor-claw]
  capacities:
    executor-desktop: {agent: 1, quota: 1, roles: [implement]}
    executor-claw: {agent: 1, quota: 1, roles: [primary-review]}
budgets:
  round: {maxUsd: 4.0, wallMinutes: 90, maxModelWakes: 12}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
"#;

const LANE_BINDING: &str = r#"
project: {ecosystems: [test]}
commands:
  seedTargets: {argv: [cargo, test, --locked]}
  sourceReaderClosure: {argv: [cargo, test, --locked]}
  testFast: {argv: [cargo, test, --workspace, --locked]}
  testExclusive: {argv: [sh, exclusive]}
  check: {argv: [cargo, check, --all-targets, --locked]}
gates:
  candidate: [seedTargets, sourceReaderClosure, check]
  merge: [testFast, testExclusive, check]
  fast: [testFast, testExclusive, check]
scope: {protectedPaths: ["coordination/**"]}
git: {pushPolicy: forbidden}
"#;

fn task() -> TaskInput {
    TaskInput {
        id: "T1".into(),
        agent: "executor-desktop".into(),
        seed_protocol: "seeded-red".into(),
        has_seeds: true,
        write_set: vec!["orch/crates/orch-host/src/x.rs".into()],
        frozen_paths: vec!["coordination/**".into()],
        gates_fast: vec!["testFast".into(), "testExclusive".into(), "check".into()],
        wall_minutes: Some(40),
        required_reviews: vec![orch_host::card::RequiredReview {
            role: "primary".into(),
            agent: "executor-claw".into(),
        }],
        required_evidence: vec!["lane-floor".into()],
        bootstrap_pre_signoff_attempt: None,
        card_source_sha256: "0".repeat(64),
        seed_source_sha256: BTreeMap::from([(
            "coordination/rounds/r99/seeds/T1/contract.rs".into(),
            "1".repeat(64),
        )]),
    }
}

#[test]
fn binding_parses_all_lanes_and_rejects_an_inert_typo() {
    let binding = parse_binding_bytes(LANE_BINDING.as_bytes()).unwrap();
    assert_eq!(
        binding.gates.candidate,
        ["seedTargets", "sourceReaderClosure", "check"]
    );
    assert_eq!(binding.gates.merge, ["testFast", "testExclusive", "check"]);
    assert_eq!(binding.gates.fast, ["testFast", "testExclusive", "check"]);
    assert!(binding.gates.candidate_declared());
    assert!(binding.gates.merge_declared());
    assert!(binding.gates.fast_declared());

    let typo = LANE_BINDING.replace(
        "  candidate: [seedTargets, sourceReaderClosure, check]",
        "  candidates: [seedTargets, sourceReaderClosure, check]",
    );
    assert!(parse_binding_bytes(typo.as_bytes()).is_err());
    for null_arm in ["candidate", "merge", "fast"] {
        let null = format!("gates:\n  {null_arm}: null\n");
        assert!(
            parse_binding_bytes(null.as_bytes()).is_err(),
            "explicit null {null_arm} must not masquerade as an absent legacy lane"
        );
    }
}

#[test]
fn plan_rejects_merge_or_fast_that_weakens_the_card() {
    for (lane, replacement) in [
        ("merge", "  merge: [testFast, check]"),
        ("fast", "  fast: [testFast, check]"),
        ("merge", "  merge: []"),
    ] {
        let needle = if lane == "merge" {
            "  merge: [testFast, testExclusive, check]"
        } else {
            "  fast: [testFast, testExclusive, check]"
        };
        let binding = LANE_BINDING.replace(needle, replacement);
        let error = plan::compile_ir("r99", MODE_YAML, &binding, &[task()])
            .expect_err("a signed card floor may not be weakened");
        assert!(
            error.to_string().contains(&format!("gates.{lane}"))
                && error.to_string().contains("testExclusive"),
            "{error:#}"
        );
    }
}

#[test]
fn a_declared_candidate_requires_all_three_nonempty_lane_arms() {
    for binding in [
        LANE_BINDING.replace(
            "  candidate: [seedTargets, sourceReaderClosure, check]",
            "  candidate: []",
        ),
        LANE_BINDING.replace("  merge: [testFast, testExclusive, check]\n", ""),
        LANE_BINDING.replace("  fast: [testFast, testExclusive, check]\n", ""),
    ] {
        let error = plan::compile_ir("r99", MODE_YAML, &binding, &[task()])
            .expect_err("a declared candidate table must have merge and fast destinations");
        assert!(error.to_string().contains("gates."), "{error:#}");
    }
}

#[test]
fn successor_authorization_uses_the_current_signed_card_not_the_old_dispatch_card() {
    assert!(!COLLECT_SOURCE.contains("load_policy_base_card("));
    let check = COLLECT_SOURCE
        .split("pub fn check_and_gate(")
        .nth(1)
        .expect("collect production entry")
        .split("fn infer_mech_stage(")
        .next()
        .unwrap();
    assert!(check.contains("mech::check(root, c,"));
    assert!(check.contains("resolve_collect_lane_plan_at_candidate("));
}

#[test]
fn dormant_modern_policy_keeps_collect_fast_but_trial_merge_strength() {
    let dormant = COLLECT_SOURCE
        .split("if !policy_active {")
        .nth(1)
        .expect("dormant lane branch")
        .split("// Escalation is monotonic")
        .next()
        .unwrap();
    assert!(dormant.contains("resolved_merge_gate_refs(&committed, card, &available)"));
    assert!(dormant.contains("let commands = static_collect_commands(&committed, &refs)"));

    let stronger = LANE_BINDING
        .replace(
            "  check: {argv: [cargo, check, --all-targets, --locked]}",
            "  check: {argv: [cargo, check, --all-targets, --locked]}\n  mergeAudit: {argv: [sh, merge-audit]}",
        )
        .replace(
            "  merge: [testFast, testExclusive, check]",
            "  merge: [testFast, testExclusive, check, mergeAudit]",
        );
    plan::compile_ir("r99", MODE_YAML, &stronger, &[task()])
        .expect("merge may be a strict superset of signed card fast");
}

fn assert_public_fields_documented(source: &str, struct_name: &str) {
    let tail = source
        .split(&format!("pub struct {struct_name}"))
        .nth(1)
        .unwrap_or_else(|| panic!("missing public struct {struct_name}"));
    let body = tail
        .split_once('{')
        .and_then(|(_, tail)| tail.split_once('}'))
        .map(|(body, _)| body)
        .unwrap();
    let lines = body.lines().collect::<Vec<_>>();
    for (index, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with("pub ") {
            assert!(
                lines[..index]
                    .iter()
                    .rev()
                    .find(|line| !line.trim().is_empty())
                    .is_some_and(|line| line.trim_start().starts_with("///")),
                "{struct_name} field lacks its own ///: {}",
                line.trim()
            );
        }
    }
}

#[test]
fn every_b306_public_receipt_and_reader_item_has_its_own_docs() {
    for marker in [
        "/// Signed command-reference lists for the three V1 gate-strength lanes.",
        "/// Closed V1 gate lane selected by a runtime boundary.",
        "/// Complete, already-derived inputs to the pure V1 lane resolver.",
        "/// Fail-closed result of resolving one V1 gate lane.",
        "/// Resolve a V1 gate lane without filesystem or ledger side effects.",
    ] {
        assert!(BINDING_SOURCE.contains(marker), "missing documentation marker: {marker}");
    }
    for marker in [
        "/// Internal execution plan shared verbatim by collect and durable receipt replay.",
        "/// Resolve the exact ordered collect invocations from the current signed card and immutable base.",
        "/// Recompute a replay-stable collect sequence directly from dispatch base to candidate commit.",
        "/// Report whether this plan depends on the attempt's monotonic typed escalation fact.",
    ] {
        assert!(COLLECT_SOURCE.contains(marker), "missing documentation marker: {marker}");
    }
    for marker in [
        "/// Version of the candidate/merge gate-lane and workspace-full-permit contract.",
        "/// Repository-wide capability which serializes a full gate across all linked worktrees.",
        "/// Borrowed same-process authorization for nested full-gate runners.",
    ] {
        assert!(GATE_SOURCE.contains(marker), "missing documentation marker: {marker}");
    }
    for marker in [
        "/// One closed V1 command invocation whose immutable argv prefix comes from a",
        "/// Recompute the digest of ordered resolved argv from one immutable policy",
    ] {
        assert!(PLAN_SOURCE.contains(marker), "missing documentation marker: {marker}");
    }
    assert!(SRCSHAPE_SOURCE.contains(
        "/// Replay the complete reader-test closure against an immutable candidate commit."
    ));
    for marker in [
        "/// Result of consuming one exact attempt-scoped NUDGE without guessing another task or round.",
        "/// One fully validated collect gate that a later root-reuse consumer may adopt.",
        "/// Validated V1 bridge from Tier-F collect receipts to B304 root-gate reuse.",
        "/// Load and independently replay the latest completed collect receipt for one exact attempt.",
    ] {
        assert!(TIERF_SOURCE.contains(marker), "missing documentation marker: {marker}");
    }
    assert_public_fields_documented(TIERF_SOURCE, "ValidatedCollectGateV1");
    assert_public_fields_documented(TIERF_SOURCE, "ValidatedCollectGateBundleV1");
    for marker in [
        "该事件必须唯一且位于首条 collect `GateExecuted` 之前",
        "在每个 gate 前后重抓工作树 subject tree",
        "`root-reuse-v1` dormant 时 root 使用 merge lane",
        "`final-tree-v1` dormant 时 postmerge/recovery 使用 merge lane",
    ] {
        assert!(GUIDE_SOURCE.contains(marker), "missing guide marker: {marker}");
    }
}

#[test]
fn mixed_rust_uses_the_rust_named_commands_for_both_floors() {
    let yaml = r#"
project: {ecosystems: [rust, node]}
commands:
  rustTest: {argv: [cargo, test, --workspace, --locked]}
  rustCheck: {argv: [cargo, check, --workspace, --all-targets, --locked]}
  testFast: {argv: [npx, vitest, run]}
  check: {argv: [npx, tsc, --noEmit]}
"#;
    let good: Binding = serde_yaml::from_str(yaml).unwrap();
    validate_locked_rust_gates(&good).unwrap();
    validate_rust_check_all_targets(&good).unwrap();

    let missing = yaml.replace("--all-targets, ", "");
    let bad: Binding = serde_yaml::from_str(&missing).unwrap();
    let errors = validate_rust_check_all_targets(&bad).unwrap_err();
    assert!(errors.iter().any(|error| error.contains("rustCheck")));
}

static SCRATCH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TestRepo(PathBuf);

impl TestRepo {
    fn new() -> Self {
        let orch_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap();
        let root = orch_root.join("target/test-tmp").join(format!(
            "gate-lane-runtime-{}-{}",
            std::process::id(),
            SCRATCH_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("coordination")).unwrap();
        git(&root, &["init", "-b", "main"]);
        Self(root)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn commit_binding(root: &Path, binding: &str) -> String {
    fs::write(root.join("coordination/PROJECT-BINDING.yaml"), binding).unwrap();
    git(root, &["add", "coordination/PROJECT-BINDING.yaml"]);
    git(
        root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch-test@example.invalid",
            "commit",
            "-m",
            "binding",
        ],
    );
    git(root, &["rev-parse", "HEAD"])
}

#[test]
fn command_digest_rereads_committed_prefix_and_hashes_closed_derived_argv() {
    let repo = TestRepo::new();
    let binding = r#"
commands:
  seedTargets: {argv: [cargo, test, --locked]}
  check: {argv: [cargo, check, --all-targets, --locked]}
"#;
    let base = commit_binding(repo.path(), binding);
    let commands = vec![
        ResolvedCommandArgvV1 {
            command_ref: "sourceReaderClosure".into(),
            binding_command_ref: "seedTargets".into(),
            derived_argv: vec![
                "-p".into(),
                "orch-host".into(),
                "--test".into(),
                "reader".into(),
            ],
        },
        ResolvedCommandArgvV1 {
            command_ref: "check".into(),
            binding_command_ref: "check".into(),
            derived_argv: Vec::new(),
        },
    ];
    let digest =
        resolved_command_argv_digest_at_policy_base(repo.path(), &base, &commands).unwrap();
    assert_eq!(digest.len(), 64);

    fs::write(
        repo.path().join("coordination/PROJECT-BINDING.yaml"),
        binding.replace("cargo, test", "forged, test"),
    )
    .unwrap();
    assert_eq!(
        resolved_command_argv_digest_at_policy_base(repo.path(), &base, &commands).unwrap(),
        digest,
        "mutable binding bytes must not affect policy-base identity"
    );

    let changed_prefix = commit_binding(
        repo.path(),
        &binding.replace("cargo, test", "forged-cargo, test"),
    );
    assert_ne!(
        resolved_command_argv_digest_at_policy_base(repo.path(), &changed_prefix, &commands)
            .unwrap(),
        digest,
        "a different committed argv prefix must change the resolved digest"
    );

    let mut reordered = commands.clone();
    reordered.reverse();
    assert_ne!(
        resolved_command_argv_digest_at_policy_base(repo.path(), &base, &reordered).unwrap(),
        digest
    );

    let duplicated = vec![
        commands[0].clone(),
        commands[0].clone(),
        commands[1].clone(),
    ];
    let duplicate_digest =
        resolved_command_argv_digest_at_policy_base(repo.path(), &base, &duplicated).unwrap();
    assert_ne!(
        duplicate_digest, digest,
        "sequence multiplicity belongs to the digest"
    );

    let mut forged_base = commands;
    forged_base[0].binding_command_ref = "check".into();
    assert!(resolved_command_argv_digest_at_policy_base(repo.path(), &base, &forged_base).is_err());

    let terminated = TestRepo::new();
    let terminated_base = commit_binding(
        terminated.path(),
        "commands:\n  seedTargets: {argv: [cargo, test, --locked, --]}\n",
    );
    let derived = [ResolvedCommandArgvV1 {
        command_ref: "sourceReaderClosure".into(),
        binding_command_ref: "seedTargets".into(),
        derived_argv: vec![
            "-p".into(),
            "orch-host".into(),
            "--test".into(),
            "reader".into(),
        ],
    }];
    assert!(resolved_command_argv_digest_at_policy_base(
        terminated.path(),
        &terminated_base,
        &derived
    )
    .is_err());
}
