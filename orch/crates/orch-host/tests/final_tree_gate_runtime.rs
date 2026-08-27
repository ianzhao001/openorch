//! B305 production-path fixture for one seal-time merged-tree full proof.
//!
//! Mutation map: M1 exact tree identity, M3 full-before-barrier, M4 durable
//! trial-to-postmerge reuse, M5 active collect preflight-only, M6 actual merge
//! comparison, M7 one lifecycle batch, M9 replay stability, and M11 raw-log
//! CAS reread are exercised here. Recovery-only mutations live beside this
//! fixture in `final_tree_gate_recovery.rs`.

#[path = "root_gate_reuse_runtime.rs"]
mod root_gate_reuse_runtime;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use orch_host::{close, ledger, plan, tierf};
use sha2::{Digest, Sha256};

pub(crate) const FINAL_ROUND: &str = "r95";
pub(crate) const FINAL_TASK: &str = "B100";
pub(crate) const FINAL_ATTEMPT: &str = "B100-A0001";

pub(crate) struct FinalTreeFixture {
    base: root_gate_reuse_runtime::Fixture,
    pub(crate) candidate_sha: String,
    #[allow(dead_code)]
    pub(crate) expected_main: String,
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("launch final-tree fixture git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn commit_all(root: &Path, message: &str) -> String {
    git(root, &["add", "-A"]);
    git(
        root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "-c",
            "commit.gpgSign=false",
            "commit",
            "-q",
            "-m",
            message,
        ],
    );
    git(root, &["rev-parse", "HEAD"])
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\"'\"'"))
}

fn write_json(path: &Path, value: &serde_json::Value) {
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn extend_signed_fixture(root: &Path) {
    let seed = b"#[test]\nfn seed_contract_b100() { assert!(true); }\n";
    fs::create_dir_all(root.join("coordination/rounds/r95/seeds/B100")).unwrap();
    fs::write(
        root.join("coordination/rounds/r95/seeds/B100/seed_contract_b100.rs"),
        seed,
    )
    .unwrap();
    fs::write(root.join("product2.txt"), "red\n").unwrap();
    fs::write(root.join("final-owner.txt"), "owner-base\n").unwrap();
    fs::write(
        root.join("coordination/rounds/r95/tasks/B99.md"),
        "---\ntaskId: B99\nround: r95\nagent: executor-desktop\nseedProtocol: pure-spec\nentryPoints: [final-owner.txt]\nwriteSet: [final-owner.txt]\nfrozenPaths: []\ngates: {fast: [testFast, testExclusive, check]}\nrequiredReviews:\n  - {role: primary, agent: executor-review}\nrequiredEvidence: [final-owner]\nbudgets: {wallMinutes: 20}\n---\n# final policy owner\n",
    )
    .unwrap();
    fs::write(
        root.join("coordination/rounds/r95/tasks/B100.md"),
        format!(
            "---\ntaskId: B100\nround: r95\nagent: executor-desktop\nseedProtocol: seeded-red\nredForm: assertion\ndependsOn: [B99]\nentryPoints: [product2.txt]\nseeds:\n  - {{src: coordination/rounds/r95/seeds/B100/seed_contract_b100.rs, target: orch/crates/fixture-host/tests/seed_contract_b100.rs, sha256: \"{}\"}}\nwriteSet:\n  - product2.txt\n  - orch/crates/fixture-host/tests/seed_contract_b100.rs\nfrozenPaths: []\ngates: {{fast: [testFast, testExclusive, check]}}\nrequiredReviews:\n  - {{role: primary, agent: executor-review}}\nrequiredEvidence: [final-tree]\nbudgets: {{wallMinutes: 30}}\n---\n# final tree fixture\n",
            sha256(seed)
        ),
    )
    .unwrap();

    let binding_path = root.join("coordination/PROJECT-BINDING.yaml");
    let mut binding: serde_json::Value =
        serde_json::from_slice(&fs::read(&binding_path).unwrap()).unwrap();
    let policies = binding["runtimePolicies"]["policies"]
        .as_object_mut()
        .unwrap();
    policies["candidate-lanes-v1"]["ownerTask"] = serde_json::json!("B99");
    policies["candidate-lanes-v1"]["initialState"] = serde_json::json!("dormant");
    policies["root-reuse-v1"]["ownerTask"] = serde_json::json!("B99");
    policies["root-reuse-v1"]["initialState"] = serde_json::json!("dormant");
    policies.insert(
        "final-tree-v1".to_string(),
        serde_json::json!({
            "schemaVersion": 1,
            "ownerTask": "B99",
            "initialState": "dormant",
            "scope": "round"
        }),
    );
    let marker = shell_quote(&root.join("coordination/runtime/test-observed/full-gates.log"));
    let red_marker = shell_quote(&root.join("coordination/runtime/test-observed/final-red"));
    let test_fast = format!(
        r#"printf 'testFast\n' >> {marker}
if [ -f orch/crates/fixture-host/tests/seed_contract_b100.rs ] && [ "$(/bin/cat product2.txt)" != green ]; then
printf 'running 1 test\ntest seed_contract_b100 ... FAILED\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n'
exit 1
fi
printf 'running 1 test\ntest seed_contract_b100 ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n'
exit 0"#
    );
    binding["commands"]["testFast"]["argv"] = serde_json::json!(["/bin/sh", "-c", test_fast]);
    binding["commands"]["testExclusive"]["argv"] = serde_json::json!([
        "/bin/sh",
        "-c",
        format!(
            "printf 'testExclusive\\n' >> {marker}; if [ -f {red_marker} ]; then printf 'exclusive red\\n'; exit 23; fi; printf 'exclusive ok\\n'; exit 0"
        )
    ]);
    write_json(&binding_path, &binding);
}

fn sign_current_plan(root: &Path, note: &str) {
    plan::run_plan(root).unwrap();
    let validation = plan::validate_round_ir_readonly(root, FINAL_ROUND).unwrap();
    let signoff = plan::plan_signed_off_payload(
        note,
        validation.persisted_revision,
        &validation.persisted_digest,
    )
    .unwrap();
    ledger::append(
        root,
        FINAL_ROUND,
        &[ledger::event(
            "PlanSignedOff",
            "user",
            None,
            Some(FINAL_ROUND),
            signoff,
        )],
    )
    .unwrap();
    commit_all(root, note);
}

fn record_final_policy_owner(root: &Path) {
    let task_id = "B99";
    let attempt_id = "B99-A0001";
    let base_sha = git(root, &["rev-parse", "main"]);
    let worktree = root.join(".worktrees/B99");
    git(
        root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "task/B99",
            worktree.to_str().unwrap(),
            &base_sha,
        ],
    );
    fs::write(worktree.join("final-owner.txt"), "recorded final owner\n").unwrap();
    git(&worktree, &["add", "final-owner.txt"]);
    git(
        &worktree,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "commit",
            "-q",
            "-m",
            "feat: record final policy owner",
        ],
    );
    let head_sha = git(root, &["rev-parse", "task/B99"]);
    let go_path = "coordination/rounds/r95/dispatch/executor-desktop/GO-B99-A0001.md";
    let dispatch = ledger::event(
        "DispatchIssued",
        "runtime:orch",
        Some(task_id),
        Some(FINAL_ROUND),
        serde_json::json!({
            "agent": "executor-desktop",
            "baseSha": base_sha,
            "goPath": go_path,
            "attemptId": attempt_id,
            "attemptNo": 1
        }),
    );
    let receipt = ledger::event(
        "CollectGateSuccessReceipt",
        "runtime:orch",
        Some(task_id),
        Some(FINAL_ROUND),
        serde_json::json!({
            "actionId": "collect-B99-A0001",
            "attemptId": attempt_id,
            "attemptNo": 1,
            "agent": "executor-desktop",
            "baseSha": base_sha,
            "goPath": go_path,
            "branchSha": head_sha
        }),
    );
    let completed = ledger::event(
        "ReportCollectCompleted",
        "runtime:orch",
        Some(task_id),
        Some(FINAL_ROUND),
        serde_json::json!({
            "actionId": "collect-B99-A0001",
            "attemptId": attempt_id,
            "attemptNo": 1,
            "agent": "executor-desktop",
            "baseSha": base_sha,
            "goPath": go_path,
            "branchSha": head_sha,
            "gateReceipt": receipt.event_id
        }),
    );
    ledger::append(root, FINAL_ROUND, &[dispatch, receipt, completed]).unwrap();
    let reviews = root.join("coordination/rounds/r95/reviews");
    let evidence = root.join("coordination/rounds/r95/evidence");
    fs::write(
        reviews.join("B99-A0001-primary-executor-review.md"),
        format!(
            "---\ntaskId: B99\nround: r95\nattemptId: B99-A0001\nrole: primary\nreviewer: executor-review\nverdict: PASS\nreviewedHead: {head_sha}\n---\nfinal policy owner review\n"
        ),
    )
    .unwrap();
    fs::write(evidence.join("B99-final-owner.json"), "{\"owner\":true}\n").unwrap();
    let expected_main = commit_all(root, "bind final policy owner evidence");
    orch_host::verify::run_root_verdict(
        root,
        task_id,
        attempt_id,
        &head_sha,
        &expected_main,
        orch_host::verify::RootVerdict::Pass,
        None,
        false,
    )
    .unwrap();
    close::run_seal(root, task_id, attempt_id, &head_sha).unwrap();
    commit_all(root, "commit recorded final policy owner lifecycle");
}

fn prepare_final_attempt(root: &Path, policy_base_sha: &str) -> String {
    let worktree = root.join(".worktrees/B100");
    git(
        root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "task/B100",
            worktree.to_str().unwrap(),
            policy_base_sha,
        ],
    );
    let go_rel = "coordination/rounds/r95/dispatch/executor-desktop/GO-B100-A0001.md";
    let go = root.join(go_rel);
    fs::create_dir_all(go.parent().unwrap()).unwrap();
    fs::write(&go, "# GO B100\n").unwrap();
    fs::write(format!("{}.ack", go.display()), "# ACK B100\n").unwrap();
    ledger::append(
        root,
        FINAL_ROUND,
        &[ledger::event(
            "DispatchIssued",
            "runtime:orch",
            Some(FINAL_TASK),
            Some(FINAL_ROUND),
            serde_json::json!({
                "agent": "executor-desktop",
                "baseSha": policy_base_sha,
                "goPath": go_rel,
                "attemptId": FINAL_ATTEMPT,
                "attemptNo": 1,
                "wakePending": true
            }),
        )],
    )
    .unwrap();
    fs::copy(
        root.join("coordination/rounds/r95/seeds/B100/seed_contract_b100.rs"),
        worktree.join("orch/crates/fixture-host/tests/seed_contract_b100.rs"),
    )
    .unwrap();
    git(
        &worktree,
        &[
            "add",
            "orch/crates/fixture-host/tests/seed_contract_b100.rs",
        ],
    );
    git(
        &worktree,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "commit",
            "-q",
            "-m",
            "seed(B100): relocate contract test",
        ],
    );
    fs::write(worktree.join("product2.txt"), "green\n").unwrap();
    git(&worktree, &["add", "product2.txt"]);
    git(
        &worktree,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "commit",
            "-q",
            "-m",
            "fix: make final tree candidate green",
        ],
    );
    let implementation_sha = git(&worktree, &["rev-parse", "HEAD"]);
    let report_rel = "coordination/rounds/r95/reports/B100-REPORT.md";
    let report = worktree.join(report_rel);
    fs::create_dir_all(report.parent().unwrap()).unwrap();
    fs::write(
        &report,
        format!(
            "---\ntaskId: B100\nagent: executor-desktop\nbranch: task/B100\nheadSha: {implementation_sha}\nwroteAt: 2026-08-27T00:00:00Z\n---\n## 0 执行环境自报\nMODEL=test\nDEPTH=test\nCAPTURE=fixture\n## 1 变更文件清单\nproduct2 and seed\n## 2 提交序列\nseed then implementation\n## 3 种子搬运证据\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n## 4 快门实测\nfixture green\n## 5 负向变异自证\nfixture mutation\n## 6 我可能做错的地方\nfixture simplification\nshell semantics\n"
        ),
    )
    .unwrap();
    git(&worktree, &["add", report_rel]);
    git(
        &worktree,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "commit",
            "-q",
            "-m",
            "report(B100): final tree fixture",
        ],
    );
    git(root, &["rev-parse", "task/B100"])
}

impl FinalTreeFixture {
    pub(crate) fn ready(tag: &str) -> Self {
        let base = root_gate_reuse_runtime::Fixture::ready(&format!("final-base-{tag}"));
        base.verdict(orch_host::verify::RootVerdict::Pass, None, false)
            .unwrap();
        close::run_seal(
            &base.root,
            root_gate_reuse_runtime::TASK,
            root_gate_reuse_runtime::ATTEMPT,
            &base.candidate_sha,
        )
        .unwrap();
        commit_all(&base.root, "record B97 before final-tree fixture extension");
        fs::write(base.root.join("coordination/BOARD.md"), "# fixture board\n").unwrap();
        commit_all(&base.root, "add fixture board before r95");
        orch_host::round::run_open(
            &base.root,
            FINAL_ROUND,
            "final-tree active production fixture",
            false,
            None,
            true,
        )
        .unwrap();
        extend_signed_fixture(&base.root);
        commit_all(&base.root, "open r95 with B99/B100 and final-tree policy");
        sign_current_plan(&base.root, "sign final-tree fixture revision");
        record_final_policy_owner(&base.root);
        plan::activate_runtime_policy(&base.root, "candidate-lanes-v1").unwrap();
        plan::activate_runtime_policy(&base.root, "root-reuse-v1").unwrap();
        plan::activate_runtime_policy(&base.root, "final-tree-v1").unwrap();
        let policy_base_sha = git(&base.root, &["rev-parse", "main"]);
        let candidate_sha = prepare_final_attempt(&base.root, &policy_base_sha);
        tierf::run_await(&base.root, FINAL_TASK, 10, None).unwrap();
        assert!(tierf::load_validated_collect_gate_bundle_v1(
            &base.root,
            FINAL_ROUND,
            FINAL_TASK,
            FINAL_ATTEMPT,
        )
        .unwrap()
        .is_some());
        let reviews = base.root.join("coordination/rounds/r95/reviews");
        let evidence = base.root.join("coordination/rounds/r95/evidence");
        fs::write(
            reviews.join("B100-A0001-primary-executor-review.md"),
            format!(
                "---\ntaskId: B100\nround: r95\nattemptId: B100-A0001\nrole: primary\nreviewer: executor-review\nverdict: PASS\nreviewedHead: {candidate_sha}\n---\nsubstantive final-tree fixture review\n"
            ),
        )
        .unwrap();
        fs::write(
            evidence.join("B100-final-tree.json"),
            "{\"finalTree\":true}\n",
        )
        .unwrap();
        let expected_main = commit_all(&base.root, "bind B100 final-tree review and evidence");
        Self {
            base,
            candidate_sha,
            expected_main,
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.base.root
    }

    pub(crate) fn events(&self) -> Vec<orch_core::EventRecord> {
        orch_core::read_ledger(&self.base.root.join("coordination/rounds/r95/events.jsonl"))
            .unwrap()
            .events
    }

    pub(crate) fn seal(&self) -> anyhow::Result<close::SealOutcome> {
        close::run_seal(self.root(), FINAL_TASK, FINAL_ATTEMPT, &self.candidate_sha)
    }

    pub(crate) fn cas_path(&self, sha256: &str) -> PathBuf {
        orch_host::cas::Store::new(&self.root().join("coordination/runtime/cas"))
            .object_path(sha256)
    }
}

fn standalone_final_tree_target() -> bool {
    module_path!() == "final_tree_gate_runtime"
}

fn phase(event: &orch_core::EventRecord) -> Option<&str> {
    event.payload.as_ref()?.get("phase")?.as_str()
}

#[test]
fn active_collect_keeps_trial_as_preflight_only() {
    if !standalone_final_tree_target() {
        return;
    }
    let fixture = FinalTreeFixture::ready("collect-preflight");
    assert!(!fixture.events().iter().any(|event| {
        event.kind == "GateExecuted"
            && event.task_id.as_deref() == Some(FINAL_TASK)
            && phase(event) == Some("trial")
    }));
}

#[test]
fn exact_actual_tree_reuses_one_premerge_full_and_replays_without_children() {
    if !standalone_final_tree_target() {
        return;
    }
    let fixture = FinalTreeFixture::ready("exact-reuse");
    fixture.seal().unwrap();
    let events = fixture.events();
    let trial = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "GateExecuted"
                && event.task_id.as_deref() == Some(FINAL_TASK)
                && phase(event) == Some("trial")
        })
        .collect::<Vec<_>>();
    let postmerge = events
        .iter()
        .filter(|event| {
            event.kind == "GateExecuted"
                && event.task_id.as_deref() == Some(FINAL_TASK)
                && phase(event) == Some("postmerge")
        })
        .count();
    assert_eq!(trial.len(), 3);
    assert_eq!(postmerge, 0);
    let started = events
        .iter()
        .position(|event| {
            event.kind == "MergeStarted" && event.task_id.as_deref() == Some(FINAL_TASK)
        })
        .unwrap();
    assert!(trial.last().unwrap().0 < started);
    let recorded = events
        .iter()
        .position(|event| {
            event.kind == "TaskRecorded" && event.task_id.as_deref() == Some(FINAL_TASK)
        })
        .unwrap();
    let reused = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            matches!(
                ledger::decode_runtime_event_v1(event),
                Ok(Some(ledger::RuntimeEventPayloadV1::GateReused(ref payload)))
                    if event.task_id.as_deref() == Some(FINAL_TASK)
                        && payload.target_phase == "postmerge"
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(reused.len(), 3);
    assert!(reused[0].0 > recorded);
    assert!(reused
        .windows(2)
        .all(|window| window[0].0 + 1 == window[1].0));
    let before = events.len();
    fixture.seal().unwrap();
    assert_eq!(fixture.events().len(), before);
}

#[test]
fn completed_replay_treats_raw_log_cas_corruption_as_hard_failure() {
    if !standalone_final_tree_target() {
        return;
    }
    let fixture = FinalTreeFixture::ready("cas-hard-failure");
    fixture.seal().unwrap();
    let before = fixture.events();
    let trial = before
        .iter()
        .find(|event| {
            event.kind == "GateExecuted"
                && event.task_id.as_deref() == Some(FINAL_TASK)
                && phase(event) == Some("trial")
        })
        .unwrap();
    let sha = trial.payload.as_ref().unwrap()["logSha256"]
        .as_str()
        .unwrap();
    fs::write(fixture.cas_path(sha), b"corrupt").unwrap();
    let error = fixture.seal().unwrap_err().to_string();
    assert!(error.contains("CAS") || error.contains("损坏"), "{error}");
    let after = fixture.events();
    assert_eq!(after.len(), before.len());
    assert_eq!(
        after
            .iter()
            .filter(|event| event.kind == "GateReuseMiss"
                && event.task_id.as_deref() == Some(FINAL_TASK))
            .count(),
        before
            .iter()
            .filter(|event| event.kind == "GateReuseMiss"
                && event.task_id.as_deref() == Some(FINAL_TASK))
            .count()
    );
}
