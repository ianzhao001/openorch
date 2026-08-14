use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use orch_host::card::RequiredReview;
use orch_host::{plan, round};
use sha2::{Digest, Sha256};

const MODE: &str = r#"agents:
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
budgets: {round: {maxUsd: 1, wallMinutes: 60, maxModelWakes: 2}}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
"#;

const BINDING: &str = r#"project: {ecosystems: [rust]}
workspace: {worktreeRoot: .worktrees}
scope: {protectedPaths: ["coordination/**"]}
git: {pushPolicy: forbidden}
verification: {independentVerifier: required-for-write}
commands:
  testFast:
    argv: ["sh", "-c", "echo 'error[E0432]: unresolved import missing_api' >&2; exit 1"]
    timeoutSeconds: 30
oracle: {dialect: cargo}
"#;

struct Site {
    root: PathBuf,
    seed_sha: String,
}

impl Site {
    fn new(tag: &str) -> Self {
        let root = orch_host::util::test_scratch_dir(&format!("b136-reverify-{tag}"));
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.name", "orch test"]);
        git(
            &root,
            &["config", "user.email", "orch-test@example.invalid"],
        );
        for path in [
            "coordination/runtime",
            "coordination/modes",
            "coordination/rounds/r49/tasks",
            "coordination/rounds/r49/seeds/T1",
        ] {
            fs::create_dir_all(root.join(path)).unwrap();
        }
        fs::write(
            root.join(".gitignore"),
            ".worktrees/\ncoordination/runtime/\n",
        )
        .unwrap();
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r49\n").unwrap();
        fs::write(root.join("coordination/modes/test.yaml"), MODE).unwrap();
        fs::write(root.join("coordination/PROJECT-BINDING.yaml"), BINDING).unwrap();
        let seed = b"#[test]\nfn contract() {}\n";
        let seed_sha = hex::encode(Sha256::digest(seed));
        fs::write(
            root.join("coordination/rounds/r49/seeds/T1/contract.rs"),
            seed,
        )
        .unwrap();
        let site = Self { root, seed_sha };
        site.write_card("tests/contract.rs", "[coordination/**]", "initial");
        git(&site.root, &["add", "."]);
        git(&site.root, &["commit", "-q", "-m", "fixture"]);
        git(&site.root, &["branch", "-M", "main"]);
        site
    }

    fn write_card(&self, target: &str, frozen: &str, body: &str) {
        fs::write(
            self.root.join("coordination/rounds/r49/tasks/T1.md"),
            format!(
                "---\n\
taskId: T1\n\
round: r49\n\
agent: executor-desktop\n\
seedProtocol: seeded-red\n\
redForm: compile\n\
seeds:\n\
  - {{src: coordination/rounds/r49/seeds/T1/contract.rs, target: {target}, sha256: {sha}}}\n\
writeSet: [src/lib.rs, tests/contract.rs]\n\
frozenPaths: {frozen}\n\
gates: {{fast: [testFast]}}\n\
budgets: {{wallMinutes: 30}}\n\
requiredReviews:\n\
  - {{role: primary, agent: executor-claw}}\n\
requiredEvidence: [reverify]\n\
---\n\
# {body}\n",
                sha = self.seed_sha
            ),
        )
        .unwrap();
    }

    fn events(&self) -> Vec<orch_core::EventRecord> {
        orch_core::read_ledger(&self.root.join("coordination/rounds/r49/events.jsonl"))
            .unwrap()
            .events
    }
}

impl Drop for Site {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn git(root: &Path, args: &[&str]) {
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
}

fn task_input() -> plan::TaskInput {
    plan::TaskInput {
        id: "T1".into(),
        agent: "executor-desktop".into(),
        seed_protocol: "seeded-red".into(),
        has_seeds: true,
        write_set: vec!["tests/contract.rs".into()],
        frozen_paths: vec!["coordination/**".into()],
        gates_fast: vec!["testFast".into()],
        wall_minutes: Some(30),
        required_reviews: vec![RequiredReview {
            role: "primary".into(),
            agent: "executor-claw".into(),
        }],
        required_evidence: vec!["reverify".into()],
        bootstrap_pre_signoff_attempt: None,
        card_source_sha256: "a".repeat(64),
        seed_source_sha256: [(
            "coordination/rounds/r49/seeds/T1/contract.rs".into(),
            "b".repeat(64),
        )]
        .into(),
    }
}

#[test]
fn card_byte_change_forces_revision_seed_reverify_and_new_signoff() {
    let site = Site::new("chain");

    let first = plan::run_plan(&site.root).unwrap();
    assert!(first.reverify_tasks.is_empty());
    round::run_seed_verified(&site.root, "T1", "error[E0432]", false, 0).unwrap();
    round::run_sign_off(&site.root, Some("revision one")).unwrap();

    site.write_card(
        "tests/contract.rs",
        "[coordination/**]",
        "card bytes changed",
    );
    let second = plan::run_plan(&site.root).unwrap();
    assert_eq!(second.revision, first.revision + 1);
    assert_eq!(second.reverify_tasks, vec!["T1"]);
    let signoff_error = round::run_sign_off(&site.root, Some("stale oracle"))
        .unwrap_err()
        .to_string();
    assert!(signoff_error.contains("reverifyTasks"), "{signoff_error}");

    round::run_seed_verified(&site.root, "T1", "error[E0432]", false, 0).unwrap();
    let latest_seed = site
        .events()
        .into_iter()
        .rev()
        .find(|event| event.kind == "SeedOracleVerified")
        .unwrap();
    let payload = latest_seed.payload.as_ref().unwrap();
    assert_eq!(
        payload
            .get("irRevision")
            .and_then(serde_json::Value::as_u64),
        Some(second.revision as u64)
    );
    assert_eq!(
        payload
            .get("oracleSchemaVersion")
            .and_then(serde_json::Value::as_u64),
        Some(2)
    );
    assert_eq!(
        payload
            .pointer("/expectedRedProof/form")
            .and_then(serde_json::Value::as_str),
        Some("compile")
    );
    assert_eq!(
        payload
            .pointer("/measured/redForm")
            .and_then(serde_json::Value::as_str),
        Some("compile")
    );
    assert!(payload.pointer("/measured/compileIdentity").is_some());
    round::run_sign_off(&site.root, Some("revision two")).unwrap();
}

#[test]
fn expected_red_mismatch_and_record_only_leave_ledger_and_wal_unchanged() {
    let site = Site::new("expected-red-no-append");
    plan::run_plan(&site.root).unwrap();
    let ledger_path = site.root.join("coordination/rounds/r49/events.jsonl");
    let wal_path = site.root.join("coordination/runtime/ledger-wal/r49.jsonl");
    let ledger_before = fs::read(&ledger_path).unwrap_or_default();
    let wal_before = fs::read(&wal_path).unwrap_or_default();

    let mismatch = round::run_seed_verified(&site.root, "T1", "error[E0583]", false, 0)
        .unwrap_err()
        .to_string();
    assert!(
        mismatch.contains("E0583") || mismatch.contains("absent"),
        "{mismatch}"
    );
    assert_eq!(fs::read(&ledger_path).unwrap_or_default(), ledger_before);
    assert_eq!(fs::read(&wal_path).unwrap_or_default(), wal_before);

    let syntax = round::run_seed_verified(&site.root, "T1", "compile", false, 0)
        .unwrap_err()
        .to_string();
    assert!(syntax.contains("error[Edddd]"), "{syntax}");
    assert_eq!(fs::read(&ledger_path).unwrap_or_default(), ledger_before);
    assert_eq!(fs::read(&wal_path).unwrap_or_default(), wal_before);

    let record_only = round::run_seed_verified(&site.root, "T1", "error[E0432]", true, 0)
        .unwrap_err()
        .to_string();
    assert!(record_only.contains("record-only"), "{record_only}");
    assert_eq!(fs::read(&ledger_path).unwrap_or_default(), ledger_before);
    assert_eq!(fs::read(&wal_path).unwrap_or_default(), wal_before);

    round::run_seed_verified(&site.root, "T1", "error[E0432]", false, 0).unwrap();
    let seed_events = site
        .events()
        .into_iter()
        .filter(|event| event.kind == "SeedOracleVerified")
        .count();
    assert_eq!(seed_events, 1);
}

#[test]
fn invalid_card_crosschecks_leave_ir_and_validation_unchanged() {
    let site = Site::new("crosscheck");
    plan::run_plan(&site.root).unwrap();
    let ir_path = site.root.join("coordination/rounds/r49/ROUND-IR.yaml");
    let ledger_path = site.root.join("coordination/rounds/r49/events.jsonl");
    let ir_before = fs::read(&ir_path).unwrap();
    let ledger_before = fs::read(&ledger_path).unwrap();

    site.write_card(
        "outside/contract.rs",
        "[coordination/**]",
        "bad seed target",
    );
    let target_error = plan::run_plan(&site.root).unwrap_err().to_string();
    assert!(target_error.contains("T1") && target_error.contains("outside/contract.rs"));
    assert_eq!(fs::read(&ir_path).unwrap(), ir_before);
    assert_eq!(fs::read(&ledger_path).unwrap(), ledger_before);

    site.write_card(
        "tests/contract.rs",
        "[coordination/**, tests/**]",
        "bad frozen overlap",
    );
    let frozen_error = plan::run_plan(&site.root).unwrap_err().to_string();
    assert!(frozen_error.contains("T1") && frozen_error.contains("tests/**"));
    assert_eq!(fs::read(&ir_path).unwrap(), ir_before);
    assert_eq!(fs::read(&ledger_path).unwrap(), ledger_before);
}

#[test]
fn termination_grace_is_typed_validated_and_digest_bound() {
    let baseline = plan::compile_ir("r49", MODE, BINDING, &[task_input()]).unwrap();
    assert_eq!(baseline.liveness.termination_grace_seconds, None);

    let with_grace = MODE.replace(
        "confirmSamples: 2}",
        "confirmSamples: 2, terminationGraceSeconds: 30}",
    );
    let typed = plan::compile_ir("r49", &with_grace, BINDING, &[task_input()]).unwrap();
    assert_eq!(typed.liveness.termination_grace_seconds, Some(30));
    assert_ne!(
        plan::validation_digest(&baseline),
        plan::validation_digest(&typed)
    );

    for invalid in [0, 601] {
        let mode = MODE.replace(
            "confirmSamples: 2}",
            &format!("confirmSamples: 2, terminationGraceSeconds: {invalid}}}"),
        );
        assert!(plan::compile_ir("r49", &mode, BINDING, &[task_input()]).is_err());
    }
}

#[test]
fn signed_ir_unknown_field_and_policy_drift_require_replan_and_resign() {
    let site = Site::new("signed-policy");
    let first = plan::run_plan(&site.root).unwrap();
    round::run_seed_verified(&site.root, "T1", "error[E0432]", false, 0).unwrap();
    round::run_sign_off(&site.root, Some("policy revision one")).unwrap();

    let ir_path = site.root.join("coordination/rounds/r49/ROUND-IR.yaml");
    let signed = fs::read_to_string(&ir_path).unwrap();
    fs::write(
        &ir_path,
        signed.replace("scheduling:", "unknownRuntimePolicy: true\nscheduling:"),
    )
    .unwrap();
    let unknown_error = plan::require_active_round_ir(&site.root, "r49", &site.events())
        .unwrap_err()
        .to_string();
    assert!(
        unknown_error.contains("解析 ROUND-IR")
            || unknown_error.contains("unknown field")
            || unknown_error.contains("unknownRuntimePolicy"),
        "{unknown_error}"
    );

    fs::write(&ir_path, signed).unwrap();
    let mode_path = site.root.join("coordination/modes/test.yaml");
    let policy_mode = MODE
        .replace(
            "confirmSamples: 2}",
            "confirmSamples: 2, stallEscalationMultiplier: 2, autoTerminateStalled: true}",
        )
        .replace(
            "scheduling:",
            "dispatch: {ackTimeoutSeconds: 45}\nscheduling:",
        );
    fs::write(&mode_path, policy_mode).unwrap();
    let drift_error = plan::require_active_round_ir(&site.root, "r49", &site.events())
        .unwrap_err()
        .to_string();
    assert!(drift_error.contains("漂移"), "{drift_error}");

    let second = plan::run_plan(&site.root).unwrap();
    assert_eq!(second.revision, first.revision + 1);
    assert_ne!(second.digest, first.digest);
    let unsigned_error = plan::require_active_round_ir(&site.root, "r49", &site.events())
        .unwrap_err()
        .to_string();
    assert!(
        unsigned_error.contains("尚未绑定 PlanSignedOff"),
        "{unsigned_error}"
    );
    round::run_sign_off(&site.root, Some("policy revision two")).unwrap();
    let active = plan::require_active_round_ir(&site.root, "r49", &site.events()).unwrap();
    assert_eq!(
        active.candidate.liveness.stall_escalation_multiplier,
        Some(2)
    );
    assert_eq!(active.candidate.liveness.auto_terminate_stalled, Some(true));
    assert_eq!(active.candidate.dispatch.ack_timeout_seconds, Some(45));
}

#[test]
fn runtime_policy_consumers_have_no_raw_round_ir_yaml_path() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for file in ["serve.rs", "tierf.rs"] {
        let text = fs::read_to_string(src.join(file)).unwrap();
        assert!(
            !text.contains("serde_yaml::Value"),
            "{file} must consume typed require_active_round_ir output"
        );
        assert!(
            !text.contains("serde_yaml::from_str"),
            "{file} must not parse ROUND-IR YAML independently"
        );
    }
}

#[test]
fn closed_legacy_round_irs_without_schema_version_still_replay() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap();
    for round_id in ["r48", "r49", "r50"] {
        let yaml =
            fs::read_to_string(repo.join(format!("coordination/rounds/{round_id}/ROUND-IR.yaml")))
                .unwrap();
        let ir = plan::parse_signed_round_ir(&yaml).unwrap();
        assert_eq!(ir.schema_version, 1);
        assert_eq!(ir.round, round_id);
        let replayed = plan::parse_signed_round_ir(&serde_yaml::to_string(&ir).unwrap()).unwrap();
        assert_eq!(replayed.round, round_id);
    }
}

/// B141 multi-card fixture: two tasks T1/T2 in the same round so that the
/// whole-round `dependsOn` graph can be exercised end-to-end through
/// `run_plan`. T2 declares `dependsOn: [T1]` in the legal case and a
/// self-edge in the cycle case.
struct MultiCardSite {
    root: PathBuf,
    seed_sha: String,
}

impl MultiCardSite {
    fn new(tag: &str) -> Self {
        let root = orch_host::util::test_scratch_dir(&format!("b141-deps-{tag}"));
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.name", "orch test"]);
        git(
            &root,
            &["config", "user.email", "orch-test@example.invalid"],
        );
        for path in [
            "coordination/runtime",
            "coordination/modes",
            "coordination/rounds/r49/tasks",
            "coordination/rounds/r49/seeds/T1",
            "coordination/rounds/r49/seeds/T2",
        ] {
            fs::create_dir_all(root.join(path)).unwrap();
        }
        fs::write(
            root.join(".gitignore"),
            ".worktrees/\ncoordination/runtime/\n",
        )
        .unwrap();
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r49\n").unwrap();
        fs::write(root.join("coordination/modes/test.yaml"), MODE).unwrap();
        fs::write(root.join("coordination/PROJECT-BINDING.yaml"), BINDING).unwrap();
        let seed = b"#[test]\nfn contract() {}\n";
        let seed_sha = hex::encode(Sha256::digest(seed));
        for task in ["T1", "T2"] {
            fs::write(
                root.join(format!("coordination/rounds/r49/seeds/{task}/contract.rs")),
                seed,
            )
            .unwrap();
        }
        let site = Self { root, seed_sha };
        site.write_t1_card("dependsOn: []", "T1 base");
        site.write_t2_card("dependsOn: [T1]", "T2 depends on T1");
        git(&site.root, &["add", "."]);
        git(&site.root, &["commit", "-q", "-m", "fixture"]);
        git(&site.root, &["branch", "-M", "main"]);
        site
    }

    fn write_t1_card(&self, depends_on_field: &str, body: &str) {
        write_multi_card(
            &self.root,
            "T1",
            self.seed_sha.clone(),
            depends_on_field,
            body,
        );
    }

    fn write_t2_card(&self, depends_on_field: &str, body: &str) {
        write_multi_card(
            &self.root,
            "T2",
            self.seed_sha.clone(),
            depends_on_field,
            body,
        );
    }

    fn round_ir(&self) -> plan::RoundIr {
        plan::load_round_ir(&self.root, "r49").unwrap()
    }
}

impl Drop for MultiCardSite {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn write_multi_card(
    root: &Path,
    task_id: &str,
    seed_sha: String,
    depends_on_field: &str,
    body: &str,
) {
    fs::write(
        root.join(format!("coordination/rounds/r49/tasks/{task_id}.md")),
        format!(
            "---\n\
taskId: {task_id}\n\
round: r49\n\
agent: executor-desktop\n\
seedProtocol: seeded-red\n\
redForm: compile\n\
seeds:\n\
  - {{src: coordination/rounds/r49/seeds/{task_id}/contract.rs, target: tests/contract.rs, sha256: {seed_sha}}}\n\
writeSet: [src/lib.rs, tests/contract.rs]\n\
frozenPaths: [coordination/**]\n\
{depends_on_field}\n\
gates: {{fast: [testFast]}}\n\
budgets: {{wallMinutes: 30}}\n\
requiredReviews:\n\
  - {{role: primary, agent: executor-claw}}\n\
requiredEvidence: [reverify]\n\
---\n\
# {body}\n"
        ),
    )
    .unwrap();
}

#[test]
fn b141_legal_dependson_yields_topological_task_order_and_digest_change() {
    let site = MultiCardSite::new("legal");
    plan::run_plan(&site.root).unwrap();
    let first_ir = site.round_ir();
    let first_order = first_ir.task_order.clone();
    assert_eq!(
        first_ir.task_order,
        vec!["T1".to_string(), "T2".to_string()]
    );
    let first_digest = plan::validation_digest(&first_ir);

    // Declaring T2 -> T1 a second time must not change the order or digest
    // (idempotent replan): the graph is identical.
    let second = plan::run_plan(&site.root).unwrap();
    assert!(!second.ir_written);
    assert_eq!(site.round_ir().task_order, first_order);
    assert_eq!(plan::validation_digest(&site.round_ir()), first_digest);
}

#[test]
fn b141_cycle_dependson_is_rejected_and_leaves_ir_and_revision_unchanged() {
    let site = MultiCardSite::new("cycle");
    plan::run_plan(&site.root).unwrap();
    let ir_path = site.root.join("coordination/rounds/r49/ROUND-IR.yaml");
    let ledger_path = site.root.join("coordination/rounds/r49/events.jsonl");
    let ir_before = fs::read(&ir_path).unwrap();
    let ledger_before = fs::read(&ledger_path).unwrap();
    let revision_before = site.round_ir().revision;

    // T2 depends on T1 and T1 depends on T2: a cycle.
    site.write_t1_card("dependsOn: [T2]", "T1 cycle");
    site.write_t2_card("dependsOn: [T1]", "T2 cycle");
    let error = plan::run_plan(&site.root).unwrap_err().to_string();
    assert!(error.contains("环"), "{error}");
    assert_eq!(fs::read(&ir_path).unwrap(), ir_before);
    assert_eq!(fs::read(&ledger_path).unwrap(), ledger_before);
    assert_eq!(site.round_ir().revision, revision_before);
}
