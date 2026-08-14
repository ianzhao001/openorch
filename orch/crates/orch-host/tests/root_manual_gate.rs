use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

use fd_lock::RwLock;
use orch_host::{ledger, plan, round, verify};
use sha2::{Digest, Sha256};

struct Site {
    root: PathBuf,
    task_head: String,
    main_head: String,
}

#[derive(Clone, Copy)]
enum AuthorizationSetup {
    Bootstrap,
    Normal,
    MissingLegacy,
    LegacyAfterDispatch,
    BootstrapWithoutPermit,
}

const REVIEW_REL: &str =
    "coordination/rounds/r48/reviews/B130T-A0001-primary-executor-claw.md";
const EVIDENCE_REL: &str = "coordination/rounds/r48/evidence/B130T-fixed-head.json";

impl Site {
    fn new(tag: &str) -> Self {
        Self::new_with_setup(tag, AuthorizationSetup::Bootstrap)
    }

    fn new_with_setup(tag: &str, setup: AuthorizationSetup) -> Self {
        Self::new_with_setup_and_gate(tag, setup, "exit 0")
    }

    fn new_with_gate(tag: &str, gate_command: &str) -> Self {
        Self::new_with_setup_and_gate(tag, AuthorizationSetup::Bootstrap, gate_command)
    }

    fn new_with_setup_and_gate(
        tag: &str,
        setup: AuthorizationSetup,
        gate_command: &str,
    ) -> Self {
        let root = orch_host::util::test_scratch_dir(&format!("b130-root-gate-{tag}"));
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.name", "orch test"]);
        git(
            &root,
            &["config", "user.email", "orch-test@example.invalid"],
        );
        fs::write(root.join("README.md"), "base\n").unwrap();
        git(&root, &["add", "README.md"]);
        git(&root, &["commit", "-q", "-m", "base"]);
        git(&root, &["branch", "-M", "main"]);
        git(&root, &["checkout", "-q", "-b", "task/B130T"]);
        fs::write(root.join("feature.txt"), "feature\n").unwrap();
        git(&root, &["add", "feature.txt"]);
        git(&root, &["commit", "-q", "-m", "feature"]);
        git(&root, &["checkout", "-q", "main"]);
        fs::create_dir_all(root.join(".worktrees")).unwrap();
        git(
            &root,
            &["worktree", "add", "-q", ".worktrees/B130T", "task/B130T"],
        );
        let task_head = git_output(&root, &["rev-parse", "task/B130T"]);
        let mut main_head = git_output(&root, &["rev-parse", "main"]);

        for path in [
            "coordination/runtime",
            "coordination/modes",
            "coordination/rounds/r48/tasks",
            "coordination/rounds/r48/reviews",
            "coordination/rounds/r48/evidence",
            "coordination/rounds/r48/seeds/B130T",
        ] {
            fs::create_dir_all(root.join(path)).unwrap();
        }
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r48\n").unwrap();
        fs::write(
            root.join("coordination/modes/test.yaml"),
            r#"agents:
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
budgets: {round: {maxUsd: 1, wallMinutes: 60, maxModelWakes: 2}}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
"#,
        )
        .unwrap();
        let gate_command = serde_json::to_string(gate_command).unwrap();
        fs::write(
            root.join("coordination/PROJECT-BINDING.yaml"),
            format!(
                "project: {{ecosystems: [test]}}\nscope: {{protectedPaths: [\"coordination/**\"]}}\ngit: {{pushPolicy: forbidden}}\ncommands:\n  testFast: {{argv: [\"sh\", \"-c\", {gate_command}], timeoutSeconds: 30}}\n  check: {{argv: [\"sh\", \"-c\", {gate_command}], timeoutSeconds: 30}}\n"
            ),
        )
        .unwrap();
        let seed_rel = "coordination/rounds/r48/seeds/B130T/contract.rs";
        let seed_bytes = b"seed contract\n";
        fs::write(root.join(seed_rel), seed_bytes).unwrap();
        let seed_sha = hex::encode(Sha256::digest(seed_bytes));
        let bootstrap_field = matches!(
            setup,
            AuthorizationSetup::Bootstrap
                | AuthorizationSetup::MissingLegacy
                | AuthorizationSetup::LegacyAfterDispatch
        )
        .then_some("bootstrapPreSignoffAttempt: B130T-A0001\n")
        .unwrap_or_default();
        fs::write(
            root.join("coordination/rounds/r48/tasks/B130T.md"),
            format!("---\ntaskId: B130T\nround: r48\nagent: executor-desktop\nseedProtocol: seeded-red\nseeds:\n  - {{src: {seed_rel}, target: tests/contract.rs, sha256: {seed_sha}}}\nwriteSet: [feature.txt, tests/contract.rs]\nfrozenPaths: [coordination/**]\ngates: {{fast: [testFast, check]}}\nbudgets: {{wallMinutes: 30}}\nrequiredReviews:\n  - {{role: primary, agent: executor-claw}}\nrequiredEvidence: [fixed-head]\n{bootstrap_field}---\n# fixture\n"),
        )
        .unwrap();
        let legacy = ledger::event(
            "TaskValidated",
            "runtime:orch",
            None,
            Some("r48"),
            plan::task_validated_payload(1, "0123456789abcdef"),
        );
        let dispatch = ledger::event(
            "DispatchIssued",
            "runtime:orch",
            Some("B130T"),
            Some("r48"),
            serde_json::json!({
                "taskId": "B130T",
                "agent": "executor-desktop",
                "attemptId": "B130T-A0001",
                "attemptNo": 1,
                "goPath": "coordination/rounds/r48/dispatch/executor-desktop/GO-B130T-A0001.md",
                "baseSha": main_head,
            }),
        );
        let receipt = ledger::event(
            "CollectGateSuccessReceipt",
            "runtime:orch",
            Some("B130T"),
            Some("r48"),
            serde_json::json!({
                "actionId": "collect-B130T-A0001",
                "attemptId": "B130T-A0001",
                "attemptNo": 1,
                "agent": "executor-desktop",
                "baseSha": main_head,
                "goPath": "coordination/rounds/r48/dispatch/executor-desktop/GO-B130T-A0001.md",
                "branchSha": task_head,
            }),
        );
        let collect = ledger::event(
            "ReportCollectCompleted",
            "runtime:orch",
            Some("B130T"),
            Some("r48"),
            serde_json::json!({
                "actionId": "collect-B130T-A0001",
                "attemptId": "B130T-A0001",
                "attemptNo": 1,
                "agent": "executor-desktop",
                "baseSha": main_head,
                "goPath": "coordination/rounds/r48/dispatch/executor-desktop/GO-B130T-A0001.md",
                "branchSha": task_head,
                "gateReceipt": receipt.event_id,
            }),
        );
        match setup {
            AuthorizationSetup::Normal => {
                plan::run_plan(&root).unwrap();
                round::run_sign_off(&root, Some("root gate fixture")).unwrap();
                ledger::append(&root, "r48", &[dispatch, receipt, collect]).unwrap();
            }
            AuthorizationSetup::Bootstrap | AuthorizationSetup::BootstrapWithoutPermit => {
                ledger::append(&root, "r48", &[legacy, dispatch, receipt, collect]).unwrap();
                plan::run_plan(&root).unwrap();
                round::run_sign_off(&root, Some("root gate fixture")).unwrap();
            }
            AuthorizationSetup::MissingLegacy => {
                ledger::append(&root, "r48", &[dispatch, receipt, collect]).unwrap();
                plan::run_plan(&root).unwrap();
                round::run_sign_off(&root, Some("root gate fixture")).unwrap();
            }
            AuthorizationSetup::LegacyAfterDispatch => {
                ledger::append(&root, "r48", &[dispatch, legacy, receipt, collect]).unwrap();
                plan::run_plan(&root).unwrap();
                round::run_sign_off(&root, Some("root gate fixture")).unwrap();
            }
        }
        fs::write(
            root.join(REVIEW_REL),
            format!(
                "---\ntaskId: B130T\nround: r48\nattemptId: B130T-A0001\nrole: primary\nreviewer: executor-claw\nverdict: PASS\nreviewedHead: {task_head}\n---\nreview\n"
            ),
        )
        .unwrap();
        fs::write(root.join(EVIDENCE_REL), "{\"ok\":true}\n").unwrap();
        git(&root, &["add", "coordination"]);
        git(&root, &["commit", "-q", "-m", "fixture signed contract"]);
        main_head = git_output(&root, &["rev-parse", "main"]);
        Self {
            root,
            task_head,
            main_head,
        }
    }

    fn pass(&self) -> anyhow::Result<verify::RootVerdictOutcome> {
        self.verdict(verify::RootVerdict::Pass, None)
    }

    fn verdict(
        &self,
        verdict: verify::RootVerdict,
        reason: Option<&str>,
    ) -> anyhow::Result<verify::RootVerdictOutcome> {
        verify::run_root_verdict(
            &self.root,
            "B130T",
            "B130T-A0001",
            &self.task_head,
            &self.main_head,
            verdict,
            reason,
            false,
        )
    }

    fn review_path(&self) -> PathBuf {
        self.root.join(REVIEW_REL)
    }

    fn evidence_path(&self) -> PathBuf {
        self.root.join(EVIDENCE_REL)
    }

    fn write_review_contract(
        &self,
        attempt_id: &str,
        role: &str,
        reviewer: &str,
        verdict: &str,
        reviewed_head: &str,
        body: &str,
    ) {
        fs::write(
            self.review_path(),
            format!(
                "---\ntaskId: B130T\nround: r48\nattemptId: {attempt_id}\nrole: {role}\nreviewer: {reviewer}\nverdict: {verdict}\nreviewedHead: {reviewed_head}\n---\n{body}"
            ),
        )
        .unwrap();
    }

    fn write_review(&self, verdict: &str, body: &str) {
        self.write_review_contract(
            "B130T-A0001",
            "primary",
            "executor-claw",
            verdict,
            &self.task_head,
            body,
        );
    }

    fn commit_coordination(&mut self, message: &str) {
        git(&self.root, &["add", "coordination"]);
        git(&self.root, &["commit", "-q", "-m", message]);
        self.main_head = git_output(&self.root, &["rev-parse", "main"]);
    }

    fn root_payloads(&self) -> Vec<verify::RootVerdictPayload> {
        self.events()
            .into_iter()
            .filter(|event| event.kind == "VerdictIssued" && event.actor == "verifier:root")
            .map(|event| serde_json::from_value(event.payload.unwrap()).unwrap())
            .collect()
    }

    fn events(&self) -> Vec<orch_core::EventRecord> {
        orch_core::read_ledger(&self.root.join("coordination/rounds/r48/events.jsonl"))
            .unwrap()
            .events
    }
}

impl Drop for Site {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn blank_signoff_note_is_rejected_before_any_round_side_effect() {
    let root = orch_host::util::test_scratch_dir("b130-blank-signoff-note");
    let error = round::run_sign_off(&root, Some(" \t\n"))
        .expect_err("blank PlanSignedOff note must fail closed");
    assert!(error.to_string().contains("note"));
    assert!(!root.join("coordination").exists());
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

fn git_output(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn with_preheld_transition_lease<T>(root: &Path, action: impl FnOnce() -> T) -> T {
    let lock_dir = root.join("coordination/runtime/locks");
    fs::create_dir_all(&lock_dir).unwrap();
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(lock_dir.join("merge.lock"))
        .unwrap();
    let mut lock = RwLock::new(file);
    let _guard = lock.try_write().unwrap();
    action()
}

fn assert_transition_busy(error: anyhow::Error) {
    let message = error.to_string();
    assert!(message.contains("protocol-transition lease busy"), "{message}");
    assert!(message.contains("fail-fast"), "{message}");
}

#[test]
fn preheld_transition_lease_blocks_plan_signoff_and_root_verdict_without_semantic_effects() {
    let plan_site = Site::new_with_setup("lease-plan", AuthorizationSetup::Normal);
    let binding_path = plan_site.root.join("coordination/PROJECT-BINDING.yaml");
    let changed = fs::read_to_string(&binding_path)
        .unwrap()
        .replace("exit 0", "true --locked");
    fs::write(&binding_path, changed).unwrap();
    let ir_path = plan_site
        .root
        .join("coordination/rounds/r48/ROUND-IR.yaml");
    let ledger_path = plan_site
        .root
        .join("coordination/rounds/r48/events.jsonl");
    let ir_before = fs::read(&ir_path).unwrap();
    let ledger_before = fs::read(&ledger_path).unwrap();
    let plan_error = with_preheld_transition_lease(&plan_site.root, || {
        plan::run_plan(&plan_site.root).unwrap_err()
    });
    assert_transition_busy(plan_error);
    assert_eq!(fs::read(&ir_path).unwrap(), ir_before);
    assert_eq!(fs::read(&ledger_path).unwrap(), ledger_before);

    let signoff_site = Site::new_with_setup("lease-signoff", AuthorizationSetup::Normal);
    let mut events = signoff_site.events();
    events.retain(|event| event.kind != "PlanSignedOff");
    let mut unsigned_ledger = events
        .iter()
        .map(|event| serde_json::to_string(event).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    unsigned_ledger.push('\n');
    let signoff_ledger_path = signoff_site
        .root
        .join("coordination/rounds/r48/events.jsonl");
    fs::write(&signoff_ledger_path, &unsigned_ledger).unwrap();
    let signoff_error = with_preheld_transition_lease(&signoff_site.root, || {
        round::run_sign_off(&signoff_site.root, Some("must not append")).unwrap_err()
    });
    assert_transition_busy(signoff_error);
    assert_eq!(
        fs::read(&signoff_ledger_path).unwrap(),
        unsigned_ledger.as_bytes()
    );

    let verdict_site = Site::new("lease-verdict");
    let verdict_ledger_path = verdict_site
        .root
        .join("coordination/rounds/r48/events.jsonl");
    let verdict_ledger_before = fs::read(&verdict_ledger_path).unwrap();
    let verdict_error = with_preheld_transition_lease(&verdict_site.root, || {
        verdict_site.pass().unwrap_err()
    });
    assert_transition_busy(verdict_error);
    assert_eq!(
        fs::read(&verdict_ledger_path).unwrap(),
        verdict_ledger_before
    );
    assert!(!verdict_site.root.join("coordination/runtime/logs").exists());
}

#[test]
fn pass_binds_fixed_heads_attempt_collect_reviews_evidence_and_gates_idempotently() {
    let site = Site::new("pass");
    assert!(site.pass().unwrap().appended);
    assert!(!site.pass().unwrap().appended);
    let events = site.events();
    let roots = events
        .iter()
        .filter(|event| event.kind == "VerdictIssued" && event.actor == "verifier:root")
        .collect::<Vec<_>>();
    assert_eq!(roots.len(), 1);
    let payload = roots[0].payload.as_ref().unwrap();
    assert_eq!(payload["attemptId"], "B130T-A0001");
    assert_eq!(payload["headSha"], site.task_head);
    assert_eq!(payload["mainHeadSha"], site.main_head);
    assert_eq!(payload["reviews"].as_array().unwrap().len(), 1);
    assert_eq!(payload["evidence"].as_array().unwrap().len(), 1);
    assert_eq!(payload["gates"].as_array().unwrap().len(), 2);
}

#[test]
fn fail_and_blocked_bind_reviews_gates_and_reason_without_touching_evidence() {
    for (tag, verdict, review_verdict, expected_verdict, reason) in [
        (
            "fail-no-evidence",
            verify::RootVerdict::Fail,
            "FAIL",
            "FAIL",
            "primary review found a regression",
        ),
        (
            "blocked-no-evidence",
            verify::RootVerdict::Blocked,
            "BLOCKED",
            "BLOCKED",
            "review established an external blocker",
        ),
    ] {
        let mut site = Site::new(tag);
        site.write_review(review_verdict, "substantive terminal review\n");
        fs::remove_file(site.evidence_path()).unwrap();
        site.commit_coordination("terminal review without pass evidence");

        let outcome = site.verdict(verdict, Some(reason)).unwrap();
        assert!(outcome.appended, "{tag}");
        assert_eq!(outcome.verdict, expected_verdict, "{tag}");

        let events = site.events();
        let roots = events
            .iter()
            .filter(|event| event.kind == "VerdictIssued" && event.actor == "verifier:root")
            .collect::<Vec<_>>();
        assert_eq!(roots.len(), 1, "{tag}");
        let raw = roots[0].payload.as_ref().unwrap();
        assert_eq!(raw["evidence"], serde_json::json!([]), "{tag}");
        let payload: verify::RootVerdictPayload = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(payload.verdict, expected_verdict, "{tag}");
        assert_eq!(payload.reason.as_deref(), Some(reason), "{tag}");
        assert!(payload.ir_revision > 0, "{tag}");
        assert_eq!(payload.validation_digest.len(), 64, "{tag}");
        assert_eq!(payload.attempt_id, "B130T-A0001", "{tag}");
        assert_eq!(payload.attempt_no, 1, "{tag}");
        assert_eq!(payload.implementer_agent, "executor-desktop", "{tag}");
        assert_eq!(payload.head_sha, site.task_head, "{tag}");
        assert_eq!(payload.main_head_sha, site.main_head, "{tag}");
        assert!(!payload.collect_completed_event_id.is_empty(), "{tag}");
        assert_eq!(payload.reviews.len(), 1, "{tag}");
        assert_eq!(payload.reviews[0].verdict, review_verdict, "{tag}");
        assert!(payload.evidence.is_empty(), "{tag}");
        assert_eq!(payload.gates.len(), 2, "{tag}");
        assert!(payload.gates.iter().all(|gate| gate.exit_code == 0), "{tag}");

        assert!(
            verify::validate_root_merge_authorization(&site.root, "r48", "B130T", &events)
                .is_err(),
            "{tag} must not authorize merge"
        );
        assert!(
            verify::validate_root_record_authorization(&site.root, "r48", "B130T", &events)
                .is_err(),
            "{tag} must not authorize record"
        );
        assert!(!events.iter().any(|event| matches!(
            event.kind.as_str(),
            "MergeStarted" | "MergeExecuted" | "TaskRecorded"
        )));
    }
}

#[test]
fn fail_ignores_even_committed_malformed_json_evidence() {
    let mut site = Site::new("fail-malformed-evidence");
    site.write_review("FAIL", "substantive failed review\n");
    fs::write(site.evidence_path(), "this is not json\n").unwrap();
    site.commit_coordination("commit deliberately malformed pass evidence");

    let outcome = site
        .verdict(
            verify::RootVerdict::Fail,
            Some("malformed pass evidence is irrelevant to an honest failure"),
        )
        .unwrap();
    assert!(outcome.appended);
    let payloads = site.root_payloads();
    assert_eq!(payloads.len(), 1);
    assert!(payloads[0].evidence.is_empty());
}

#[test]
fn pass_remains_strict_about_review_verdict_and_evidence_json() {
    let mut failed_review = Site::new("pass-failed-review");
    failed_review.write_review("FAIL", "substantive failed review\n");
    failed_review.commit_coordination("commit failed review");
    let review_error = failed_review.pass().unwrap_err().to_string();
    assert!(review_error.contains("所有 required review PASS"), "{review_error}");
    assert!(failed_review.root_payloads().is_empty());

    let mut malformed_evidence = Site::new("pass-malformed-evidence");
    fs::write(malformed_evidence.evidence_path(), "not json\n").unwrap();
    malformed_evidence.commit_coordination("commit malformed evidence");
    let evidence_error = malformed_evidence.pass().unwrap_err().to_string();
    assert!(evidence_error.contains("非合法 JSON"), "{evidence_error}");
    assert!(malformed_evidence.root_payloads().is_empty());
}

#[test]
fn terminal_verdicts_require_nonempty_reason_and_pass_rejects_reason() {
    let site = Site::new("reason-shape");
    for (verdict, reason) in [
        (verify::RootVerdict::Fail, None),
        (verify::RootVerdict::Fail, Some(" \t\n")),
        (verify::RootVerdict::Blocked, None),
        (verify::RootVerdict::Blocked, Some(" \n")),
        (verify::RootVerdict::Pass, Some("not allowed")),
    ] {
        assert!(site.verdict(verdict, reason).is_err());
    }
    assert!(site.root_payloads().is_empty());
}

#[test]
fn fail_still_requires_a_committed_exact_substantive_review() {
    let missing = Site::new("fail-missing-review");
    fs::remove_file(missing.review_path()).unwrap();
    let error = missing
        .verdict(verify::RootVerdict::Fail, Some("missing review"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("required review"), "{error}");
    assert!(missing.root_payloads().is_empty());

    let mut uncommitted = Site::new("fail-uncommitted-review");
    fs::remove_file(uncommitted.review_path()).unwrap();
    uncommitted.commit_coordination("remove committed review");
    uncommitted.write_review("FAIL", "uncommitted review\n");
    let error = uncommitted
        .verdict(verify::RootVerdict::Fail, Some("uncommitted review"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("expected main"), "{error}");
    assert!(uncommitted.root_payloads().is_empty());

    let drifted = Site::new("fail-review-byte-drift");
    drifted.write_review("FAIL", "uncommitted byte drift\n");
    let error = drifted
        .verdict(verify::RootVerdict::Fail, Some("review drift"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("bytes 与 expected main blob 不一致"), "{error}");
    assert!(drifted.root_payloads().is_empty());

    for field in ["attempt", "head", "role", "reviewer", "body"] {
        let mut site = Site::new(&format!("fail-review-wrong-{field}"));
        let attempt_id = if field == "attempt" {
            "B130T-A0002"
        } else {
            "B130T-A0001"
        };
        let role = if field == "role" { "secondary" } else { "primary" };
        let reviewer = if field == "reviewer" {
            "executor-opencode"
        } else {
            "executor-claw"
        };
        let reviewed_head = if field == "head" {
            "f".repeat(40)
        } else {
            site.task_head.clone()
        };
        let body = if field == "body" { "" } else { "review body\n" };
        site.write_review_contract(
            attempt_id,
            role,
            reviewer,
            "FAIL",
            &reviewed_head,
            body,
        );
        site.commit_coordination(&format!("commit wrong review {field}"));
        let error = site
            .verdict(
                verify::RootVerdict::Fail,
                Some("review tuple must remain exact"),
            )
            .unwrap_err()
            .to_string();
        let expected = if field == "body" {
            "substantive body"
        } else {
            "review artifact contract"
        };
        assert!(error.contains(expected), "field={field}: {error}");
        assert!(site.root_payloads().is_empty(), "field={field}");
    }
}

#[test]
fn fail_replay_is_idempotent_without_evidence_and_rejects_drift() {
    let mut site = Site::new("fail-replay");
    site.write_review("FAIL", "stable failed review\n");
    fs::remove_file(site.evidence_path()).unwrap();
    site.commit_coordination("commit failed review without evidence");
    let reason = "the reviewed implementation does not satisfy the contract";

    assert!(site
        .verdict(verify::RootVerdict::Fail, Some(reason))
        .unwrap()
        .appended);
    assert!(!site
        .verdict(verify::RootVerdict::Fail, Some(reason))
        .unwrap()
        .appended);
    assert_eq!(site.root_payloads().len(), 1);

    let reason_error = site
        .verdict(
            verify::RootVerdict::Fail,
            Some("a different replay reason"),
        )
        .unwrap_err()
        .to_string();
    assert!(reason_error.contains("tuple 冲突"), "{reason_error}");
    assert_eq!(site.root_payloads().len(), 1);

    site.write_review("FAIL", "drifted failed review\n");
    assert!(site
        .verdict(verify::RootVerdict::Fail, Some(reason))
        .is_err());
    assert_eq!(site.root_payloads().len(), 1);
}

#[test]
fn append_recomputes_review_and_evidence_bytes_after_gates() {
    let review_command = format!("printf '\\ngate drift\\n' >> ../../{REVIEW_REL}");
    let review_site = Site::new_with_gate("append-review-drift", &review_command);
    let review_error = review_site
        .verdict(
            verify::RootVerdict::Fail,
            Some("gate-time review drift must fail closed"),
        )
        .unwrap_err()
        .to_string();
    assert!(
        review_error.contains("required review 当前 bytes 与 expected main blob 不一致"),
        "{review_error}"
    );
    assert!(review_site.root_payloads().is_empty());

    let evidence_command = format!("printf ' ' >> ../../{EVIDENCE_REL}");
    let evidence_site = Site::new_with_gate("append-evidence-drift", &evidence_command);
    let evidence_error = evidence_site.pass().unwrap_err().to_string();
    assert!(
        evidence_error.contains("required evidence 当前 bytes 与 expected main blob 不一致"),
        "{evidence_error}"
    );
    assert!(evidence_site.root_payloads().is_empty());
}

#[test]
fn red_gates_are_bound_by_fail_but_still_block_pass() {
    let mut failed = Site::new_with_gate("red-gate-fail", "exit 7");
    failed.write_review("FAIL", "review explains the red gate\n");
    fs::remove_file(failed.evidence_path()).unwrap();
    failed.commit_coordination("commit failed review for red gate");
    let outcome = failed
        .verdict(
            verify::RootVerdict::Fail,
            Some("the signed gate is red and the verdict records that truth"),
        )
        .unwrap();
    assert!(outcome.appended);
    assert_eq!(outcome.gates.len(), 2);
    assert!(outcome.gates.iter().all(|gate| gate.exit_code == 7));
    let payloads = failed.root_payloads();
    assert_eq!(payloads.len(), 1);
    assert!(payloads[0].gates.iter().all(|gate| gate.exit_code == 7));
    assert!(payloads[0].evidence.is_empty());

    let passing = Site::new_with_gate("red-gate-pass", "exit 7");
    let error = passing.pass().unwrap_err().to_string();
    assert!(error.contains("root PASS gate"), "{error}");
    assert!(passing.root_payloads().is_empty());
}

#[test]
fn normal_signoff_before_dispatch_and_exact_bootstrap_chain_are_both_authorized() {
    let normal = Site::new_with_setup("normal-order", AuthorizationSetup::Normal);
    assert!(normal.pass().unwrap().appended);

    let bootstrap = Site::new("bootstrap-order");
    assert!(bootstrap.pass().unwrap().appended);
    let root = bootstrap
        .events()
        .into_iter()
        .find(|event| event.actor == "verifier:root")
        .unwrap();
    assert_eq!(
        root.payload.as_ref().unwrap()["bootstrapPreSignoffAttempt"],
        "B130T-A0001"
    );
}

#[test]
fn bootstrap_scope_and_legacy_order_fail_closed_before_gate_logs() {
    for (tag, setup, needle) in [
        (
            "missing-legacy",
            AuthorizationSetup::MissingLegacy,
            "legacy",
        ),
        (
            "late-legacy",
            AuthorizationSetup::LegacyAfterDispatch,
            "早于 dispatch",
        ),
        (
            "missing-permit",
            AuthorizationSetup::BootstrapWithoutPermit,
            "permit",
        ),
    ] {
        let site = Site::new_with_setup(tag, setup);
        let error = site.pass().unwrap_err().to_string();
        assert!(error.contains(needle), "{tag}: {error}");
        assert!(!site.root.join("coordination/runtime/logs").exists());
        assert!(!site
            .events()
            .iter()
            .any(|event| event.actor == "verifier:root"));
    }
}

#[test]
fn later_production_revision_cannot_reuse_bootstrap_collect() {
    let mut site = Site::new("revision-reuse");
    let binding = site.root.join("coordination/PROJECT-BINDING.yaml");
    let changed = fs::read_to_string(&binding)
        .unwrap()
        .replace("exit 0", "true --locked");
    fs::write(&binding, changed).unwrap();
    let replanned = plan::run_plan(&site.root).unwrap();
    assert_eq!(replanned.revision, 3);
    round::run_sign_off(&site.root, Some("revision three")).unwrap();
    git(&site.root, &["add", "coordination"]);
    git(&site.root, &["commit", "-q", "-m", "revision three contract"]);
    site.main_head = git_output(&site.root, &["rev-parse", "main"]);

    let error = site.pass().unwrap_err().to_string();
    assert!(error.contains("first production"), "{error}");
    assert!(!site.root.join("coordination/runtime/logs").exists());
}

#[test]
fn concurrent_signoff_appends_at_most_one_exact_event() {
    let site = Site::new_with_setup("signoff-race", AuthorizationSetup::Normal);
    let mut events = site.events();
    events.retain(|event| event.kind != "PlanSignedOff");
    let mut ledger_text = events
        .iter()
        .map(|event| serde_json::to_string(event).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    ledger_text.push('\n');
    fs::write(
        site.root.join("coordination/rounds/r48/events.jsonl"),
        ledger_text,
    )
    .unwrap();

    let left_root = site.root.clone();
    let right_root = site.root.clone();
    let left = std::thread::spawn(move || round::run_sign_off(&left_root, Some("left")));
    let right = std::thread::spawn(move || round::run_sign_off(&right_root, Some("right")));
    let results = [left.join().unwrap(), right.join().unwrap()];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        site.events()
            .iter()
            .filter(|event| event.kind == "PlanSignedOff")
            .count(),
        1
    );
}

#[test]
fn short_sha_wrong_attempt_or_main_and_missing_evidence_fail_before_event() {
    let site = Site::new("bad-tuple");
    assert!(verify::run_root_verdict(
        &site.root,
        "B130T",
        "B130T-A0001",
        "abc1234",
        &site.main_head,
        verify::RootVerdict::Pass,
        None,
        false,
    )
    .is_err());
    assert!(verify::run_root_verdict(
        &site.root,
        "B130T",
        "B130T-A0002",
        &site.task_head,
        &site.main_head,
        verify::RootVerdict::Pass,
        None,
        false,
    )
    .is_err());
    assert!(verify::run_root_verdict(
        &site.root,
        "B130T",
        "B130T-A0001",
        &site.task_head,
        "0000000000000000000000000000000000000000",
        verify::RootVerdict::Pass,
        None,
        false,
    )
    .is_err());
    fs::remove_file(
        site.root
            .join("coordination/rounds/r48/evidence/B130T-fixed-head.json"),
    )
    .unwrap();
    assert!(site.pass().is_err());
    assert!(!site
        .events()
        .iter()
        .any(|event| event.actor == "verifier:root"));
}

#[test]
fn review_byte_mutation_invalidates_an_existing_root_pass_at_merge_boundary() {
    let site = Site::new("review-mutation");
    site.pass().unwrap();
    fs::write(
        site.root
            .join("coordination/rounds/r48/reviews/B130T-A0001-primary-executor-claw.md"),
        "mutated\n",
    )
    .unwrap();
    let events = site.events();
    assert!(
        verify::validate_root_merge_authorization(&site.root, "r48", "B130T", &events,).is_err()
    );
}

#[test]
fn non_root_pass_never_authorizes_merge_and_fail_requires_reason() {
    let site = Site::new("actor-reason");
    ledger::append(
        &site.root,
        "r48",
        &[ledger::event(
            "VerdictIssued",
            "verifier:not-root",
            Some("B130T"),
            Some("r48"),
            serde_json::json!({"verdict": "PASS"}),
        )],
    )
    .unwrap();
    let events = site.events();
    assert!(
        verify::validate_root_merge_authorization(&site.root, "r48", "B130T", &events,).is_err()
    );
    assert!(verify::run_root_verdict(
        &site.root,
        "B130T",
        "B130T-A0001",
        &site.task_head,
        &site.main_head,
        verify::RootVerdict::Fail,
        None,
        false,
    )
    .is_err());
}

#[test]
fn direct_verify_validates_signed_ir_and_rejects_mode_downgrade_before_logs() {
    let site = Site::new("direct-verify-policy");
    let error = verify::run_verify(&site.root, "B130T", "must-not-spawn", 1)
        .err()
        .expect("root-manual direct verify must reject")
        .to_string();
    assert!(error.contains("root-manual-fixed-head"), "{error}");
    assert!(!site.root.join("coordination/runtime/logs").exists());

    let ir_path = site
        .root
        .join("coordination/rounds/r48/ROUND-IR.yaml");
    let ir = fs::read_to_string(&ir_path).unwrap();
    let downgraded = ir
        .lines()
        .filter(|line| {
            !line.starts_with("verification:")
                && !line.starts_with("  mode:")
                && !line.starts_with("  adapter:")
                && !line.starts_with("  model:")
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&ir_path, format!("{downgraded}\n")).unwrap();
    assert!(verify::run_verify(&site.root, "B130T", "must-not-spawn", 1).is_err());
    assert!(!site.root.join("coordination/runtime/logs").exists());
}

#[test]
fn source_binding_drift_in_gate_or_seed_rejects_before_gates_or_verdict() {
    let gate_site = Site::new("binding-source-drift");
    let binding = gate_site.root.join("coordination/PROJECT-BINDING.yaml");
    let weakened = fs::read_to_string(&binding)
        .unwrap()
        .replace("exit 0", "true --locked");
    fs::write(&binding, weakened).unwrap();
    assert!(gate_site.pass().is_err());
    assert!(!gate_site.root.join("coordination/runtime/logs").exists());
    assert!(!gate_site
        .events()
        .iter()
        .any(|event| event.actor == "verifier:root"));

    let seed_site = Site::new("seed-source-drift");
    fs::write(
        seed_site
            .root
            .join("coordination/rounds/r48/seeds/B130T/contract.rs"),
        "mutated seed\n",
    )
    .unwrap();
    assert!(seed_site.pass().is_err());
    assert!(!seed_site.root.join("coordination/runtime/logs").exists());
}

#[test]
fn wrong_round_root_event_and_non_root_pass_never_cross_runloop_or_merge_barrier() {
    let site = Site::new("wrong-round-root");
    site.pass().unwrap();
    let mut events = site.events();
    let root = events
        .iter_mut()
        .find(|event| event.actor == "verifier:root")
        .unwrap();
    root.round = Some("r47".into());
    assert!(verify::validate_root_merge_authorization(&site.root, "r48", "B130T", &events)
        .is_err());

    let non_root = Site::new("non-root-runloop");
    ledger::append(
        &non_root.root,
        "r48",
        &[ledger::event(
            "VerdictIssued",
            "verifier:not-root",
            Some("B130T"),
            Some("r48"),
            serde_json::json!({"verdict": "PASS"}),
        )],
    )
    .unwrap();
    let outcome = orch_host::runloop::run_loop(&non_root.root, true).unwrap();
    assert_eq!(outcome.awaiting_root, vec!["B130T"]);
    assert!(outcome.actions.is_empty());
    assert!(!non_root
        .events()
        .iter()
        .any(|event| matches!(event.kind.as_str(), "MergeStarted" | "EscalationRaised")));
}

#[test]
fn current_attempt_implementer_cannot_review_itself_and_dispatch_is_runtime_owned() {
    let site = Site::new("takeover-reviewer");
    let dispatch = ledger::event(
        "DispatchIssued",
        "runtime:orch",
        Some("B130T"),
        Some("r48"),
        serde_json::json!({
            "taskId": "B130T",
            "agent": "executor-claw",
            "attemptId": "B130T-A0002",
            "attemptNo": 2,
            "goPath": "coordination/rounds/r48/dispatch/executor-claw/GO-B130T-A0002.md",
            "baseSha": site.main_head,
        }),
    );
    let receipt = ledger::event(
        "CollectGateSuccessReceipt",
        "runtime:orch",
        Some("B130T"),
        Some("r48"),
        serde_json::json!({
            "actionId": "collect-B130T-A0002",
            "attemptId": "B130T-A0002",
            "attemptNo": 2,
            "agent": "executor-claw",
            "baseSha": site.main_head,
            "goPath": "coordination/rounds/r48/dispatch/executor-claw/GO-B130T-A0002.md",
            "branchSha": site.task_head,
        }),
    );
    let collect = ledger::event(
        "ReportCollectCompleted",
        "runtime:orch",
        Some("B130T"),
        Some("r48"),
        serde_json::json!({
            "actionId": "collect-B130T-A0002",
            "attemptId": "B130T-A0002",
            "attemptNo": 2,
            "agent": "executor-claw",
            "baseSha": site.main_head,
            "goPath": "coordination/rounds/r48/dispatch/executor-claw/GO-B130T-A0002.md",
            "branchSha": site.task_head,
            "gateReceipt": receipt.event_id,
        }),
    );
    ledger::append(&site.root, "r48", &[dispatch, receipt, collect]).unwrap();
    assert!(verify::run_root_verdict(
        &site.root,
        "B130T",
        "B130T-A0002",
        &site.task_head,
        &site.main_head,
        verify::RootVerdict::Pass,
        None,
        false,
    )
    .is_err());

    let wrong_actor = Site::new("dispatch-actor");
    let mut events = wrong_actor.events();
    events
        .iter_mut()
        .find(|event| event.kind == "DispatchIssued")
        .unwrap()
        .actor = "planner".into();
    // A root payload is unnecessary: resolving the current dispatch itself
    // must fail before any authorization could be assembled.
    let collect = events
        .iter_mut()
        .find(|event| event.kind == "ReportCollectCompleted")
        .unwrap();
    collect.actor = "planner".into();
    assert!(verify::validate_root_merge_authorization(
        &wrong_actor.root,
        "r48",
        "B130T",
        &events,
    )
    .is_err());
}

#[test]
fn already_integrated_task_and_newly_signed_ir_cannot_reuse_old_root_pass() {
    let integrated = Site::new("already-integrated");
    git(
        &integrated.root,
        &["merge", "--no-ff", "-q", "task/B130T", "-m", "premature"],
    );
    assert!(integrated.pass().is_err());
    assert!(!integrated
        .events()
        .iter()
        .any(|event| event.actor == "verifier:root"));

    let revised = Site::new("revised-after-pass");
    revised.pass().unwrap();
    let binding = revised.root.join("coordination/PROJECT-BINDING.yaml");
    let changed = fs::read_to_string(&binding)
        .unwrap()
        .replace("exit 0", "true --locked");
    fs::write(&binding, changed).unwrap();
    plan::run_plan(&revised.root).unwrap();
    round::run_sign_off(&revised.root, Some("new source contract")).unwrap();
    let events = revised.events();
    assert!(verify::validate_root_merge_authorization(
        &revised.root,
        "r48",
        "B130T",
        &events,
    )
    .is_err());
}

#[test]
fn validation_revision_high_water_blocks_rollback_and_replan_uses_max_plus_one() {
    let site = Site::new("ir-rollback");
    let binding_path = site.root.join("coordination/PROJECT-BINDING.yaml");
    let ir_path = site
        .root
        .join("coordination/rounds/r48/ROUND-IR.yaml");
    let rev1_binding = fs::read(&binding_path).unwrap();
    let rev1_ir = fs::read(&ir_path).unwrap();

    let changed = String::from_utf8(rev1_binding.clone())
        .unwrap()
        .replace("exit 0", "true --locked");
    fs::write(&binding_path, changed).unwrap();
    let rev3 = plan::run_plan(&site.root).unwrap();
    assert_eq!(rev3.revision, 3);
    round::run_sign_off(&site.root, Some("rev3")).unwrap();

    fs::write(&binding_path, &rev1_binding).unwrap();
    fs::write(&ir_path, &rev1_ir).unwrap();
    let events = site.events();
    assert!(plan::require_active_round_ir(&site.root, "r48", &events).is_err());

    let rev4 = plan::run_plan(&site.root).unwrap();
    assert_eq!(rev4.revision, 4);
}
