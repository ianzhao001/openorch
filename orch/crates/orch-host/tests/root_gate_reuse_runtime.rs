//! B304 production-path regression for collect proof reuse at root.
//!
//! Mutation map: M1 tree, M2 contract, M3 command, M4 toolchain/environment, M5 red source,
//! M6 receipt/raw-CAS reread, M7 unconditional root execution, M8 hollowed checked-batch append,
//! M9 accidental main-SHA identity, and M10 broken replay/archive references all have named
//! assertions here or in `root_gate_reuse_archive.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use orch_host::{close, ledger, plan, tierf, verify};
use sha2::{Digest, Sha256};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

pub(crate) const ROUND: &str = "r94";
pub(crate) const TASK: &str = "B97";
pub(crate) const ATTEMPT: &str = "B97-A0001";

/// Complete active-policy fixture with a real collect receipt and committed root evidence.
pub(crate) struct Fixture {
    pub(crate) root: PathBuf,
    pub(crate) candidate_sha: String,
    pub(crate) expected_main: String,
    pub(crate) policy_base_sha: String,
    full_gate_log: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).ok();
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("launch fixture git");
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

fn write_json(path: &Path, value: &serde_json::Value) -> Vec<u8> {
    let bytes = serde_json::to_vec_pretty(value).unwrap();
    fs::write(path, &bytes).unwrap();
    bytes
}

fn shell_quote(path: &Path) -> String {
    format!(
        "'{}'",
        path.display().to_string().replace('\'', "'\"'\"'")
    )
}

fn record_policy_owner(root: &Path, task_id: &str) {
    let base_sha = git(root, &["rev-parse", "main"]);
    let worktree = root.join(format!(".worktrees/{task_id}"));
    git(
        root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            &format!("task/{task_id}"),
            worktree.to_str().unwrap(),
            &base_sha,
        ],
    );
    fs::write(worktree.join("candidate-owner.txt"), "recorded owner\n").unwrap();
    git(&worktree, &["add", "candidate-owner.txt"]);
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
            "feat: record policy owner",
        ],
    );
    let head_sha = git(root, &["rev-parse", &format!("task/{task_id}")]);
    let attempt_id = format!("{task_id}-A0001");
    let go_path = format!(
        "coordination/rounds/{ROUND}/dispatch/executor-desktop/GO-{task_id}-A0001.md"
    );
    let dispatch = ledger::event(
        "DispatchIssued",
        "runtime:orch",
        Some(task_id),
        Some(ROUND),
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
        Some(ROUND),
        serde_json::json!({
            "actionId": format!("collect-{attempt_id}"),
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
        Some(ROUND),
        serde_json::json!({
            "actionId": format!("collect-{attempt_id}"),
            "attemptId": attempt_id,
            "attemptNo": 1,
            "agent": "executor-desktop",
            "baseSha": base_sha,
            "goPath": go_path,
            "branchSha": head_sha,
            "gateReceipt": receipt.event_id
        }),
    );
    ledger::append(
        root,
        ROUND,
        &[dispatch, receipt, completed],
    )
    .unwrap();
    fs::create_dir_all(root.join("coordination/rounds/r94/reviews")).unwrap();
    fs::create_dir_all(root.join("coordination/rounds/r94/evidence")).unwrap();
    fs::write(
        root.join(format!(
            "coordination/rounds/r94/reviews/{task_id}-A0001-primary-executor-review.md"
        )),
        format!(
            "---\ntaskId: {task_id}\nround: r94\nattemptId: {task_id}-A0001\nrole: primary\nreviewer: executor-review\nverdict: PASS\nreviewedHead: {head_sha}\n---\npolicy owner review\n"
        ),
    )
    .unwrap();
    fs::write(
        root.join(format!(
            "coordination/rounds/r94/evidence/{task_id}-candidate-owner.json"
        )),
        "{\"owner\":true}\n",
    )
    .unwrap();
    let expected_main = commit_all(root, &format!("bind policy owner {task_id} evidence"));
    verify::run_root_verdict(
        root,
        task_id,
        &attempt_id,
        &head_sha,
        &expected_main,
        verify::RootVerdict::Pass,
        None,
        false,
    )
    .unwrap();
    close::run_seal(root, task_id, &attempt_id, &head_sha).unwrap();
    commit_all(root, &format!("commit recorded lifecycle for {task_id}"));
}

fn setup_sources(tag: &str) -> (PathBuf, PathBuf) {
    let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("host crate below orch workspace")
        .join("target/test-tmp")
        .join(format!(
            "b304-root-reuse-{tag}-{}-{sequence}",
            std::process::id()
        ));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    for directory in [
        "coordination/runtime/test-observed",
        "coordination/modes",
        "coordination/rounds/r94/tasks",
        "coordination/rounds/r94/seeds/B97",
        "orch/crates/fixture-host/tests",
        ".worktrees",
    ] {
        fs::create_dir_all(root.join(directory)).unwrap();
    }
    fs::write(
        root.join(".gitignore"),
        "coordination/runtime/\ncoordination/rounds/*/dispatch/\n.worktrees/\n.cowork-temp/\n",
    )
    .unwrap();
    fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r94\n").unwrap();
    fs::write(root.join("product.txt"), "red\n").unwrap();
    fs::write(root.join("candidate-owner.txt"), "candidate-owner\n").unwrap();
    fs::write(root.join("reuse-owner.txt"), "reuse-owner\n").unwrap();

    let seed = b"#[test]\nfn seed_contract() { assert!(true); }\n";
    fs::write(
        root.join("coordination/rounds/r94/seeds/B97/seed_contract.rs"),
        seed,
    )
    .unwrap();
    fs::write(
        root.join("orch/crates/fixture-host/tests/reader_contract.rs"),
        "const PRODUCT: &str = include_str!(\"../../../../product.txt\");\n#[test]\nfn reader_contract() { assert!(!PRODUCT.is_empty()); }\n",
    )
    .unwrap();
    let baseline = write_json(
        &root.join("coordination/source-shape-baseline-v1.json"),
        &serde_json::json!({
            "schemaVersion": 1,
            "registeredReaders": {
                "orch/crates/fixture-host/tests/reader_contract.rs": [
                    {"target": "product.txt", "mechanism": "IncludeStr"}
                ]
            }
        }),
    );
    let descriptor = write_json(
        &root.join("coordination/source-reader-closure-v1.json"),
        &serde_json::json!({
            "schemaVersion": 1,
            "baseDescriptor": {
                "path": "coordination/source-shape-baseline-v1.json",
                "sha256": sha256(&baseline),
                "readerMapKey": "registeredReaders"
            },
            "overlays": [],
            "unknownEdgeDisposition": "upgrade-to-fast-and-audit"
        }),
    );

    let full_gate_log = root.join("coordination/runtime/test-observed/full-gates.log");
    let selector_log = root.join("coordination/runtime/test-observed/selectors.log");
    let full_log = shell_quote(&full_gate_log);
    let selector_log_quoted = shell_quote(&selector_log);
    let test_fast = format!(
        r#"printf 'testFast\n' >> {full_log}
if [ ! -f orch/crates/fixture-host/tests/seed_contract.rs ] || [ "$(/bin/cat product.txt)" = green ]; then
printf 'running 1 test\ntest seed_contract ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n'
exit 0
else
printf 'running 1 test\ntest seed_contract ... FAILED\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n'
exit 1
fi"#
    );
    let selector = format!(
        r#"if [ "$0" != -p ] || [ "$1" != fixture-host ] || [ "$2" != --test ]; then
printf 'invalid selector argv\n' >&2
exit 41
fi
case "$3" in
seed_contract|reader_contract) ;;
*) printf 'unexpected selector: %s\n' "$3" >&2; exit 42 ;;
esac
printf '%s\n' "$3" >> {selector_log_quoted}
printf 'selector %s ok\n' "$3""#
    );
    let exclusive = format!("printf 'testExclusive\\n' >> {full_log}; exit 0");
    let check = format!("printf 'check\\n' >> {full_log}; printf 'check ok\\n'; exit 0");
    write_json(
        &root.join("coordination/PROJECT-BINDING.yaml"),
        &serde_json::json!({
            "project": {"ecosystems": []},
            "workspace": {"worktreeRoot": ".worktrees"},
            "commands": {
                "testFast": {"argv": ["/bin/sh", "-c", test_fast], "timeoutSeconds": 30},
                "testExclusive": {"argv": ["/bin/sh", "-c", exclusive], "timeoutSeconds": 30},
                "check": {"argv": ["/bin/sh", "-c", check], "timeoutSeconds": 30},
                "seedTargets": {"argv": ["/bin/sh", "-c", selector], "timeoutSeconds": 30}
            },
            "gates": {
                "candidate": ["seedTargets", "sourceReaderClosure", "check"],
                "merge": ["testFast", "testExclusive", "check"],
                "fast": ["testFast", "testExclusive", "check"]
            },
            "runtimePolicies": {
                "schemaVersion": 1,
                "policies": {
                    "candidate-lanes-v1": {
                        "schemaVersion": 1,
                        "ownerTask": "B306",
                        "initialState": "dormant",
                        "scope": "round",
                        "sourceReaderClosure": {
                            "schemaVersion": 1,
                            "path": "coordination/source-reader-closure-v1.json",
                            "sha256": sha256(&descriptor)
                        }
                    },
                    "root-reuse-v1": {
                        "schemaVersion": 1,
                        "ownerTask": "B306",
                        "initialState": "dormant",
                        "scope": "round"
                    }
                }
            },
            "scope": {"protectedPaths": []},
            "git": {"pushPolicy": "forbidden", "mergePolicy": "ff-only-else-no-ff"},
            "oracle": {"dialect": "cargo"}
        }),
    );
    fs::write(
        root.join("coordination/agents.yaml"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "agents": {
                "executor-desktop": {
                    "injectable": true,
                    "sessionId": "fixture-session",
                    "wake": {"argv": ["/bin/sh", "-c", "exit 0", "{session}", "{message}"]}
                },
                "executor-review": {
                    "injectable": false,
                    "wake": {"argv": ["/bin/sh", "-c", "exit 0"]}
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        root.join("coordination/modes/test.yaml"),
        "preset: relay\n\
         agents:\n  \
           planner: {adapter: root, tier: none}\n  \
           executor: {adapter: codex-desktop, tier: F, agentId: executor-desktop}\n  \
           verifier: {adapter: root-manual, tier: none}\n\
         hitl: {planSignoff: required, mergeGate: auto}\n\
         verification: {mode: root-manual-fixed-head}\n\
         liveness: {monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}\n\
         scheduling:\n  \
           allowedAgents: [executor-desktop, executor-review]\n  \
           capacities:\n    \
             executor-desktop: {agent: 1, quota: 1, roles: [implement]}\n    \
             executor-review: {agent: 1, quota: 1, roles: [primary-review]}\n\
         budgets: {round: {maxUsd: 1, wallMinutes: 60, maxModelWakes: 4}}\n\
         git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}\n",
    )
    .unwrap();
    fs::write(
        root.join("coordination/rounds/r94/tasks/B306.md"),
        "---\ntaskId: B306\nround: r94\nagent: executor-desktop\nseedProtocol: pure-spec\nentryPoints: [candidate-owner.txt]\nwriteSet: [candidate-owner.txt]\nfrozenPaths: []\ngates: {fast: [testFast, testExclusive, check]}\nrequiredReviews:\n  - {role: primary, agent: executor-review}\nrequiredEvidence: [candidate-owner]\nbudgets: {wallMinutes: 10}\n---\n# candidate owner\n",
    )
    .unwrap();
    fs::write(
        root.join("coordination/rounds/r94/tasks/B97.md"),
        format!(
            "---\ntaskId: B97\nround: r94\nagent: executor-desktop\nseedProtocol: seeded-red\nredForm: assertion\ndependsOn: [B306]\nentryPoints: [product.txt]\nseeds:\n  - {{src: coordination/rounds/r94/seeds/B97/seed_contract.rs, target: orch/crates/fixture-host/tests/seed_contract.rs, sha256: \"{}\"}}\nwriteSet:\n  - product.txt\n  - orch/crates/fixture-host/tests/seed_contract.rs\nfrozenPaths: []\ngates: {{fast: [testFast, testExclusive, check]}}\nrequiredReviews:\n  - {{role: primary, agent: executor-review}}\nrequiredEvidence: [root-reuse]\nbudgets: {{wallMinutes: 30}}\n---\n# root reuse fixture\n",
            sha256(seed)
        ),
    )
    .unwrap();
    (root, full_gate_log)
}

fn prepare_attempt(root: &Path, policy_base_sha: &str) -> String {
    let worktree = root.join(".worktrees/B97");
    git(
        root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "task/B97",
            worktree.to_str().unwrap(),
            policy_base_sha,
        ],
    );
    let go_rel = "coordination/rounds/r94/dispatch/executor-desktop/GO-B97-A0001.md";
    let go = root.join(go_rel);
    fs::create_dir_all(go.parent().unwrap()).unwrap();
    fs::write(&go, "# GO B97\n").unwrap();
    fs::write(format!("{}.ack", go.display()), "# ACK B97\n").unwrap();
    ledger::append(
        root,
        ROUND,
        &[ledger::event(
            "DispatchIssued",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "agent": "executor-desktop",
                "baseSha": policy_base_sha,
                "goPath": go_rel,
                "attemptId": ATTEMPT,
                "attemptNo": 1,
                "wakePending": true
            }),
        )],
    )
    .unwrap();
    fs::copy(
        root.join("coordination/rounds/r94/seeds/B97/seed_contract.rs"),
        worktree.join("orch/crates/fixture-host/tests/seed_contract.rs"),
    )
    .unwrap();
    git(
        &worktree,
        &["add", "orch/crates/fixture-host/tests/seed_contract.rs"],
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
            "seed(B97): relocate contract test",
        ],
    );
    fs::write(worktree.join("product.txt"), "green\n").unwrap();
    git(&worktree, &["add", "product.txt"]);
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
            "fix: make candidate green",
        ],
    );
    let implementation_sha = git(&worktree, &["rev-parse", "HEAD"]);
    let report_rel = "coordination/rounds/r94/reports/B97-REPORT.md";
    let report = worktree.join(report_rel);
    fs::create_dir_all(report.parent().unwrap()).unwrap();
    fs::write(
        &report,
        format!(
            "---\ntaskId: B97\nagent: executor-desktop\nbranch: task/B97\nheadSha: {implementation_sha}\nwroteAt: 2026-08-26T00:00:00Z\n---\n## 0 执行环境自报\nMODEL=test\nDEPTH=test\nCAPTURE=fixture\n## 1 变更文件清单\nproduct and seed\n## 2 提交序列\nseed then implementation\n## 3 种子搬运证据\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n## 4 快门实测\nfixture green\n## 5 负向变异自证\nfixture mutation\n## 6 我可能做错的地方\nfixture simplification\nshell semantics\n"
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
            "report(B97): root reuse fixture",
        ],
    );
    git(root, &["rev-parse", "task/B97"])
}

impl Fixture {
    /// Build the entire real collect lifecycle before returning a root-ready fixture.
    pub(crate) fn ready(tag: &str) -> Self {
        let (root, full_gate_log) = setup_sources(tag);
        commit_all(&root, "root reuse fixture sources");
        plan::run_plan(&root).unwrap();
        let validation = plan::validate_round_ir_readonly(&root, ROUND).unwrap();
        let signoff = plan::plan_signed_off_payload(
            "root reuse fixture sign-off",
            validation.persisted_revision,
            &validation.persisted_digest,
        )
        .unwrap();
        ledger::append(
            &root,
            ROUND,
            &[ledger::event(
                "PlanSignedOff",
                "user",
                None,
                Some(ROUND),
                signoff,
            )],
        )
        .unwrap();
        commit_all(&root, "sign root reuse fixture");
        record_policy_owner(&root, "B306");
        plan::activate_runtime_policy(&root, "candidate-lanes-v1").unwrap();
        plan::activate_runtime_policy(&root, "root-reuse-v1").unwrap();
        let policy_base_sha = git(&root, &["rev-parse", "main"]);
        let candidate_sha = prepare_attempt(&root, &policy_base_sha);
        tierf::run_await(&root, TASK, 10, None).unwrap();
        let bundle = tierf::load_validated_collect_gate_bundle_v1(
            &root, ROUND, TASK, ATTEMPT,
        )
        .unwrap()
        .expect("real collect receipt must expose a reusable bundle");
        assert_eq!(bundle.candidate_sha, candidate_sha);
        assert_eq!(
            bundle
                .gates
                .iter()
                .map(|gate| gate.command_ref.as_str())
                .collect::<Vec<_>>(),
            ["seedTargets", "sourceReaderClosure", "check"]
        );
        fs::create_dir_all(root.join("coordination/rounds/r94/reviews")).unwrap();
        fs::create_dir_all(root.join("coordination/rounds/r94/evidence")).unwrap();
        fs::write(
            root.join(
                "coordination/rounds/r94/reviews/B97-A0001-primary-executor-review.md",
            ),
            format!(
                "---\ntaskId: B97\nround: r94\nattemptId: B97-A0001\nrole: primary\nreviewer: executor-review\nverdict: PASS\nreviewedHead: {candidate_sha}\n---\nsubstantive fixture review\n"
            ),
        )
        .unwrap();
        fs::write(
            root.join("coordination/rounds/r94/evidence/B97-root-reuse.json"),
            "{\"rootReuse\":true}\n",
        )
        .unwrap();
        let expected_main = commit_all(&root, "bind root review and evidence");
        Self {
            root,
            candidate_sha,
            expected_main,
            policy_base_sha,
            full_gate_log,
        }
    }

    /// Read the current durable round history.
    pub(crate) fn events(&self) -> Vec<orch_core::EventRecord> {
        orch_core::read_ledger(&self.root.join("coordination/rounds/r94/events.jsonl"))
            .unwrap()
            .events
    }

    /// Count every fixture command that belongs to the real merge/root lane.
    pub(crate) fn full_gate_count(&self) -> usize {
        fs::read_to_string(&self.full_gate_log)
            .unwrap_or_default()
            .lines()
            .count()
    }

    /// Run one exact root verdict against the fixture's pinned tuple.
    pub(crate) fn verdict(
        &self,
        verdict: verify::RootVerdict,
        reason: Option<&str>,
        dry_run: bool,
    ) -> anyhow::Result<verify::RootVerdictOutcome> {
        verify::run_root_verdict(
            &self.root,
            TASK,
            ATTEMPT,
            &self.candidate_sha,
            &self.expected_main,
            verdict,
            reason,
            dry_run,
        )
    }

    /// Replace the formal review verdict, remove PASS-only evidence, and pin a new expected main.
    pub(crate) fn prepare_terminal_review(&mut self, verdict: &str) {
        fs::write(
            self.root.join(
                "coordination/rounds/r94/reviews/B97-A0001-primary-executor-review.md",
            ),
            format!(
                "---\ntaskId: B97\nround: r94\nattemptId: B97-A0001\nrole: primary\nreviewer: executor-review\nverdict: {verdict}\nreviewedHead: {}\n---\nsubstantive terminal review\n",
                self.candidate_sha
            ),
        )
        .unwrap();
        fs::remove_file(
            self.root
                .join("coordination/rounds/r94/evidence/B97-root-reuse.json"),
        )
        .unwrap();
        self.expected_main = commit_all(&self.root, "bind terminal root review");
    }
}

fn standalone_runtime_target() -> bool {
    module_path!() == "root_gate_reuse_runtime"
}

#[test]
fn real_collect_to_root_to_ledger_reuses_without_a_second_proof() {
    if !standalone_runtime_target() {
        return;
    }
    let fixture = Fixture::ready("reuse-hit");
    assert_ne!(
        fixture.policy_base_sha, fixture.expected_main,
        "normal review/receipt commits must move main without poisoning reuse identity"
    );
    let before = fixture.full_gate_count();
    let precheck = fixture
        .verdict(verify::RootVerdict::Pass, None, true)
        .unwrap();
    assert!(precheck.dry_run && precheck.gates.is_empty());
    assert_eq!(fixture.full_gate_count(), before);
    assert!(!fixture
        .events()
        .iter()
        .any(|event| {
            event.task_id.as_deref() == Some(TASK)
                && matches!(event.kind.as_str(), "GateReused" | "VerdictIssued")
        }));

    let outcome = fixture
        .verdict(verify::RootVerdict::Pass, None, false)
        .unwrap();
    assert!(outcome.appended);
    assert_eq!(fixture.full_gate_count(), before, "reuse spawned a real root gate");
    let events = fixture.events();
    let verdict_position = events
        .iter()
        .position(|event| {
            event.kind == "VerdictIssued"
                && event.actor == "verifier:root"
                && event.task_id.as_deref() == Some(TASK)
        })
        .unwrap();
    let reused = events[..verdict_position]
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "GateReused" && event.task_id.as_deref() == Some(TASK)
        })
        .collect::<Vec<_>>();
    assert_eq!(reused.len(), 3);
    assert_eq!(reused[0].0 + 3, verdict_position);
    assert!(!events.iter().any(|event| {
        event.kind == "GateExecuted"
            && event.task_id.as_deref() == Some(TASK)
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("phase"))
                .and_then(serde_json::Value::as_str)
                == Some("root")
    }));
    let verdict = events[verdict_position].payload.as_ref().unwrap();
    let bindings = verdict["gates"].as_array().unwrap();
    assert_eq!(bindings.len(), reused.len());
    for (binding, (_, event)) in bindings.iter().zip(&reused) {
        assert!(binding.get("gateRunId").is_none());
        assert_eq!(binding["reusedEventId"], event.event_id);
    }

    let replay = fixture
        .verdict(verify::RootVerdict::Pass, None, false)
        .unwrap();
    assert!(!replay.appended);
    assert_eq!(fixture.full_gate_count(), before);
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| {
                event.kind == "GateReused" && event.task_id.as_deref() == Some(TASK)
            })
            .count(),
        3
    );
}

#[test]
fn unreadable_raw_log_misses_and_falls_back_to_real_root_gates() {
    if !standalone_runtime_target() {
        return;
    }
    let fixture = Fixture::ready("cas-miss");
    let bundle = tierf::load_validated_collect_gate_bundle_v1(
        &fixture.root,
        ROUND,
        TASK,
        ATTEMPT,
    )
    .unwrap()
    .unwrap();
    let first = &bundle.gates[0];
    let store = orch_host::cas::Store::new(&fixture.root.join("coordination/runtime/cas"));
    fs::write(store.object_path(&first.log_sha256), b"corrupt").unwrap();
    let before = fixture.full_gate_count();
    fixture
        .verdict(verify::RootVerdict::Pass, None, false)
        .unwrap();
    assert_eq!(fixture.full_gate_count(), before + 3);
    let events = fixture.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event.kind == "GateReuseMiss" && event.task_id.as_deref() == Some(TASK)
            })
            .count(),
        1
    );
    assert!(!events.iter().any(|event| {
        event.kind == "GateReused" && event.task_id.as_deref() == Some(TASK)
    }));
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event.kind == "GateExecuted"
                    && event.task_id.as_deref() == Some(TASK)
                    && event.payload.as_ref().unwrap()["phase"] == "root"
            })
            .count(),
        3
    );
}

#[test]
fn existing_reuse_dry_run_rereads_source_proof_without_side_effects() {
    if !standalone_runtime_target() {
        return;
    }
    let fixture = Fixture::ready("dry-run-existing-cas");
    fixture
        .verdict(verify::RootVerdict::Pass, None, false)
        .unwrap();
    let bundle = tierf::load_validated_collect_gate_bundle_v1(
        &fixture.root,
        ROUND,
        TASK,
        ATTEMPT,
    )
    .unwrap()
    .unwrap();
    let store = orch_host::cas::Store::new(&fixture.root.join("coordination/runtime/cas"));
    fs::write(store.object_path(&bundle.gates[0].log_sha256), b"corrupt").unwrap();
    let before_gates = fixture.full_gate_count();
    let before_events = fixture.events().len();

    let error = fixture
        .verdict(verify::RootVerdict::Pass, None, true)
        .unwrap_err()
        .to_string();
    assert!(error.contains("CAS"), "{error}");
    assert_eq!(fixture.full_gate_count(), before_gates);
    assert_eq!(fixture.events().len(), before_events);
}

#[test]
fn active_policy_fail_and_blocked_append_zero_root_gates() {
    if !standalone_runtime_target() {
        return;
    }
    for (tag, review, verdict, reason) in [
        (
            "fail-zero-root",
            "FAIL",
            verify::RootVerdict::Fail,
            "fixture found a regression",
        ),
        (
            "blocked-zero-root",
            "BLOCKED",
            verify::RootVerdict::Blocked,
            "fixture found an external blocker",
        ),
    ] {
        let mut fixture = Fixture::ready(tag);
        fixture.prepare_terminal_review(review);
        let before = fixture.full_gate_count();
        let outcome = fixture.verdict(verdict, Some(reason), false).unwrap();
        assert!(outcome.appended && outcome.gates.is_empty(), "{tag}");
        assert_eq!(fixture.full_gate_count(), before, "{tag}");
        let events = fixture.events();
        assert!(!events.iter().any(|event| {
            event.task_id.as_deref() == Some(TASK)
                && (event.kind == "GateReused"
                || (event.kind == "GateExecuted"
                    && event.payload.as_ref().unwrap()["phase"] == "root"))
        }));
        let root = events
            .iter()
            .find(|event| {
                event.kind == "VerdictIssued"
                    && event.actor == "verifier:root"
                    && event.task_id.as_deref() == Some(TASK)
            })
            .unwrap();
        assert_eq!(root.payload.as_ref().unwrap()["gates"], serde_json::json!([]));
    }
}
