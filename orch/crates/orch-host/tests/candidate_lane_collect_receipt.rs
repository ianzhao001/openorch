//! B306 production-path regression for active candidate receipt creation and committed replay.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use orch_host::{
    card, ledger,
    plan::{self, ResolvedCommandArgvV1},
    round, tierf,
};
use sha2::{Digest, Sha256};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

const ROUND: &str = "r90";
const TASK: &str = "B97";
const ATTEMPT: &str = "B97-A0001";

struct Fixture {
    root: PathBuf,
    policy_base_sha: String,
    selector_log: PathBuf,
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

fn setup(policy_active: bool) -> Fixture {
    let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("host crate must be below the orch workspace")
        .join("target/test-tmp")
        .join(format!(
            "b306-candidate-receipt-{}-{sequence}",
            std::process::id()
        ));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);

    for directory in [
        "coordination/runtime",
        "coordination/modes",
        "coordination/rounds/r90/tasks",
        "coordination/rounds/r90/seeds/B97",
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
    fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r90\n").unwrap();
    fs::write(root.join("product.txt"), "red\n").unwrap();
    fs::write(root.join("owner.txt"), "recorded\n").unwrap();
    fs::write(root.join("successor-extra.txt"), "old\n").unwrap();

    let seed_one = b"#[test]\nfn seed_one() { assert!(true); }\n";
    let seed_two = b"#[test]\nfn seed_two() { assert!(true); }\n";
    fs::write(
        root.join("coordination/rounds/r90/seeds/B97/seed_one.rs"),
        seed_one,
    )
    .unwrap();
    fs::write(
        root.join("coordination/rounds/r90/seeds/B97/seed_two.rs"),
        seed_two,
    )
    .unwrap();
    for reader in ["reader_one.rs", "reader_two.rs"] {
        fs::write(
            root.join(format!("orch/crates/fixture-host/tests/{reader}")),
            format!(
                "const PRODUCT: &str = include_str!(\"../../../../product.txt\");\n#[test]\nfn reader_is_bound() {{ assert!(!PRODUCT.is_empty()); }}\n"
            ),
        )
        .unwrap();
    }

    let baseline = write_json(
        &root.join("coordination/source-shape-baseline-v1.json"),
        &serde_json::json!({
            "schemaVersion": 1,
            "registeredReaders": {
                "orch/crates/fixture-host/tests/reader_one.rs": [
                    {"target": "product.txt", "mechanism": "IncludeStr"}
                ],
                "orch/crates/fixture-host/tests/reader_two.rs": [
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

    let red_green = r#"if [ "$(/bin/cat product.txt)" = green ]; then
printf 'running 2 tests\ntest seed_one ... ok\ntest seed_two ... ok\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n'
exit 0
else
printf 'running 2 tests\ntest seed_one ... FAILED\ntest seed_two ... FAILED\ntest result: FAILED. 0 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out\n'
exit 1
fi"#;
    let selector_log = root.join("selector-observed.log");
    let quoted_selector_log = format!(
        "'{}'",
        selector_log.display().to_string().replace('\'', "'\"'\"'")
    );
    let candidate_selector = format!(
        r#"if [ "$0" != -p ] || [ "$1" != fixture-host ] || [ "$2" != --test ]; then
printf 'invalid selector argv: <%s> <%s> <%s> <%s>\n' "$0" "$1" "$2" "$3" >&2
exit 41
fi
case "$3" in
seed_one|seed_two|reader_one|reader_two) ;;
*) printf 'unexpected selector target: <%s>\n' "$3" >&2; exit 42 ;;
esac
if [ "$3" = seed_one ] && [ "$(/bin/cat successor-extra.txt)" = mutate-on-gate ]; then
printf 'mutated-by-gate\n' > successor-extra.txt
fi
printf '%s\n' "$3" >> {quoted_selector_log}
printf 'selector %s ok\n' "$3""#
    );
    write_json(
        &root.join("coordination/PROJECT-BINDING.yaml"),
        &serde_json::json!({
            "project": {"ecosystems": []},
            "workspace": {"worktreeRoot": ".worktrees"},
            "commands": {
                "testFast": {"argv": ["/bin/sh", "-c", red_green], "timeoutSeconds": 30},
                "testExclusive": {"argv": ["/bin/sh", "-c", "printf exclusive-ok"], "timeoutSeconds": 30},
                "check": {"argv": ["/bin/sh", "-c", "printf check-ok"], "timeoutSeconds": 30},
                "seedTargets": {"argv": ["/bin/sh", "-c", candidate_selector], "timeoutSeconds": 30}
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
        root.join("coordination/rounds/r90/tasks/B306.md"),
        r#"---
taskId: B306
round: r90
agent: executor-desktop
seedProtocol: pure-spec
entryPoints: [owner.txt]
writeSet: [owner.txt]
frozenPaths: []
gates: {fast: [testFast, testExclusive, check]}
requiredReviews:
  - {role: primary, agent: executor-review}
requiredEvidence: [policy-owner]
budgets: {wallMinutes: 10}
---
# policy owner
"#,
    )
    .unwrap();
    fs::write(
        root.join("coordination/rounds/r90/tasks/B97.md"),
        format!(
            r#"---
taskId: B97
round: r90
agent: executor-desktop
seedProtocol: seeded-red
redForm: assertion
entryPoints: [product.txt]
seeds:
  - src: coordination/rounds/r90/seeds/B97/seed_one.rs
    target: orch/crates/fixture-host/tests/seed_one.rs
    sha256: "{}"
  - src: coordination/rounds/r90/seeds/B97/seed_two.rs
    target: orch/crates/fixture-host/tests/seed_two.rs
    sha256: "{}"
writeSet:
  - product.txt
  - orch/crates/fixture-host/tests/seed_one.rs
  - orch/crates/fixture-host/tests/seed_two.rs
frozenPaths: []
gates: {{fast: [testFast, testExclusive, check]}}
requiredReviews:
  - {{role: primary, agent: executor-review}}
requiredEvidence: [candidate-receipt]
budgets: {{wallMinutes: 30}}
---
# candidate receipt fixture
"#,
            sha256(seed_one),
            sha256(seed_two)
        ),
    )
    .unwrap();

    commit_all(&root, "candidate fixture sources");
    plan::run_plan(&root).unwrap();
    let validation = plan::validate_round_ir_readonly(&root, ROUND).unwrap();
    let signoff = plan::plan_signed_off_payload(
        "fixture sign-off",
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
    commit_all(&root, "signed candidate fixture");

    round::run_seed_verified(&root, TASK, "2", false, 0).unwrap();
    let owner_merge_sha = git(&root, &["rev-parse", "main"]);
    ledger::append(
        &root,
        ROUND,
        &[
            ledger::event(
                "MergeExecuted",
                "reviewer:orch-runtime",
                Some("B306"),
                Some(ROUND),
                serde_json::json!({"mergeSha": owner_merge_sha}),
            ),
            ledger::event(
                "TaskRecorded",
                "runtime:orch",
                Some("B306"),
                Some(ROUND),
                serde_json::json!({"postMergeGates": "all-green"}),
            ),
        ],
    )
    .unwrap();
    commit_all(&root, "record candidate policy owner");
    if policy_active {
        plan::activate_runtime_policy(&root, "candidate-lanes-v1").unwrap();
    }
    let policy_base_sha = git(&root, &["rev-parse", "main"]);

    let card_path = root.join("coordination/rounds/r90/tasks/B97.md");
    let successor_card = fs::read_to_string(&card_path).unwrap().replace(
        "  - product.txt\n",
        "  - product.txt\n  - successor-extra.txt\n  - orch/crates/fixture-host/tests/b306_unknown_runtime_reader.rs\n",
    );
    fs::write(&card_path, successor_card).unwrap();
    plan::run_plan(&root).unwrap();
    round::run_seed_verified(&root, TASK, "2", false, 0).unwrap();
    let validation = plan::validate_round_ir_readonly(&root, ROUND).unwrap();
    let signoff = plan::plan_signed_off_payload(
        "successor fixture sign-off",
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
    commit_all(&root, "sign successor card revision");

    Fixture {
        root,
        policy_base_sha,
        selector_log,
    }
}

fn prepare_attempt(fixture: &Fixture) -> (PathBuf, String) {
    prepare_attempt_variant(fixture, "authorized-by-successor\n", None)
}

fn prepare_attempt_variant(
    fixture: &Fixture,
    successor_extra: &str,
    unknown_reader: Option<&str>,
) -> (PathBuf, String) {
    let root = &fixture.root;
    let branch_base_sha = git(root, &["rev-parse", "main"]);
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
            &branch_base_sha,
        ],
    );
    let go_rel = "coordination/rounds/r90/dispatch/executor-desktop/GO-B97-A0001.md";
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
                "baseSha": fixture.policy_base_sha.clone(),
                "goPath": go_rel,
                "attemptId": ATTEMPT,
                "attemptNo": 1,
                "wakePending": true
            }),
        )],
    )
    .unwrap();

    for name in ["seed_one.rs", "seed_two.rs"] {
        fs::copy(
            root.join(format!("coordination/rounds/r90/seeds/B97/{name}")),
            worktree.join(format!("orch/crates/fixture-host/tests/{name}")),
        )
        .unwrap();
    }
    git(
        &worktree,
        &[
            "add",
            "orch/crates/fixture-host/tests/seed_one.rs",
            "orch/crates/fixture-host/tests/seed_two.rs",
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
            "seed(B97): relocate contract test",
        ],
    );
    fs::write(worktree.join("product.txt"), "green\n").unwrap();
    fs::write(worktree.join("successor-extra.txt"), successor_extra).unwrap();
    if let Some(source) = unknown_reader {
        fs::write(
            worktree.join("orch/crates/fixture-host/tests/b306_unknown_runtime_reader.rs"),
            source,
        )
        .unwrap();
    }
    git(&worktree, &["add", "product.txt", "successor-extra.txt"]);
    if unknown_reader.is_some() {
        git(
            &worktree,
            &[
                "add",
                "orch/crates/fixture-host/tests/b306_unknown_runtime_reader.rs",
            ],
        );
    }
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
    let report_rel = "coordination/rounds/r90/reports/B97-REPORT.md";
    let report_path = worktree.join(report_rel);
    fs::create_dir_all(report_path.parent().unwrap()).unwrap();
    fs::write(
        &report_path,
        format!(
            r#"---
taskId: B97
agent: executor-desktop
branch: task/B97
headSha: {implementation_sha}
wroteAt: 2026-08-25T00:00:00Z
---
## 0 执行环境自报
MODEL=test
DEPTH=test
CAPTURE=fixture
## 1 变更文件清单
product, successor-only file, and two seeds
## 2 提交序列
seed then fix
## 3 种子搬运证据
test result: FAILED. 0 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out
## 4 快门实测
fixture gates green
## 5 负向变异自证
fixture contract
## 6 我可能做错的地方
fixture simplification
shell gate semantics
"#
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
            "report(B97): candidate receipt fixture",
        ],
    );
    (worktree, git(root, &["rev-parse", "task/B97"]))
}

fn write_event_ledger(path: &Path, events: &[orch_core::EventRecord]) {
    let mut bytes = Vec::new();
    for event in events {
        serde_json::to_writer(&mut bytes, event).unwrap();
        bytes.push(b'\n');
    }
    fs::write(path, bytes).unwrap();
}

fn receipt_window_gate_events<'a>(
    events: &'a [orch_core::EventRecord],
    phase: &str,
) -> Vec<&'a orch_core::EventRecord> {
    let executing = events
        .iter()
        .rposition(|event| {
            event.kind == "ReportCollectExecuting"
                && event.task_id.as_deref() == Some(TASK)
                && event.round.as_deref() == Some(ROUND)
        })
        .expect("attempt collect executing anchor");
    let receipt = events
        .iter()
        .enumerate()
        .skip(executing + 1)
        .find(|(_, event)| {
            event.kind == "CollectGateSuccessReceipt"
                && event.task_id.as_deref() == Some(TASK)
                && event.round.as_deref() == Some(ROUND)
        })
        .map(|(position, _)| position)
        .expect("attempt collect receipt");
    events[executing + 1..receipt]
        .iter()
        .filter(|event| {
            event.kind == "GateExecuted"
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("phase"))
                    .and_then(serde_json::Value::as_str)
                    == Some(phase)
        })
        .collect()
}

fn remove_attempt_worktree(root: &Path, worktree: &Path) {
    git(
        root,
        &["worktree", "remove", "--force", worktree.to_str().unwrap()],
    );
    git(root, &["branch", "-D", "task/B97"]);
}

#[test]
fn active_candidate_receipt_preserves_duplicate_refs_and_replays_from_commits() {
    let fixture = setup(true);
    let (worktree, candidate_sha) = prepare_attempt(&fixture);
    let root = &fixture.root;
    let card = card::load(root, ROUND, TASK).unwrap();
    assert_eq!(card.meta.seeds.len(), 2);

    tierf::run_await(root, TASK, 10, None).unwrap();
    let events = orch_core::read_ledger(&root.join("coordination/rounds/r90/events.jsonl"))
        .unwrap()
        .events;
    let collect_gates = receipt_window_gate_events(&events, "collect");
    let red_replay_gates = receipt_window_gate_events(&events, "red-replay");
    assert_eq!(red_replay_gates.len(), 1);
    let red_replay_payload = red_replay_gates[0].payload.as_ref().unwrap();
    assert_eq!(red_replay_payload["commandRef"], "testFast");
    let red_replay_event_id = red_replay_gates[0].event_id.clone();
    let red_replay_run_id = red_replay_payload["gateRunId"]
        .as_str()
        .unwrap()
        .to_string();
    let refs = collect_gates
        .iter()
        .map(|event| {
            let payload = event.payload.as_ref().unwrap().as_object().unwrap();
            assert_eq!(payload.len(), 10, "GateExecuted must remain exact ten-key");
            payload["commandRef"].as_str().unwrap().to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        refs,
        [
            "seedTargets",
            "seedTargets",
            "sourceReaderClosure",
            "sourceReaderClosure",
            "check"
        ]
    );
    assert_eq!(
        fs::read_to_string(&fixture.selector_log)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        ["seed_one", "seed_two", "reader_one", "reader_two"],
        "the spawned candidate argv must carry each exact derived selector"
    );
    let run_ids = collect_gates
        .iter()
        .map(|event| {
            event.payload.as_ref().unwrap()["gateRunId"]
                .as_str()
                .unwrap()
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(run_ids.len(), 5);

    let before_replay = collect_gates.len();
    tierf::run_await(root, TASK, 10, None).unwrap();
    let replayed = orch_core::read_ledger(&root.join("coordination/rounds/r90/events.jsonl"))
        .unwrap()
        .events;
    assert_eq!(
        replayed
            .iter()
            .filter(|event| {
                event.kind == "GateExecuted"
                    && event.payload.as_ref().unwrap()["phase"] == "collect"
            })
            .count(),
        before_replay,
        "durable replay reran candidate gates"
    );

    remove_attempt_worktree(root, &worktree);
    let bundle = tierf::load_validated_collect_gate_bundle_v1(root, ROUND, TASK, ATTEMPT)
        .unwrap()
        .expect("active candidate receipt bundle");
    assert_eq!(bundle.candidate_sha, candidate_sha);
    assert_eq!(
        bundle
            .gates
            .iter()
            .map(|gate| gate.command_ref.as_str())
            .collect::<Vec<_>>(),
        refs.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert!(bundle
        .gates
        .windows(2)
        .all(|pair| pair[0].sequence + 1 == pair[1].sequence));
    assert!(bundle.gates.iter().all(|gate| {
        gate.source_event_id != red_replay_event_id && gate.gate_run_id != red_replay_run_id
    }));
    let expected_commands = [
        ("seedTargets", "seedTargets", "seed_one"),
        ("seedTargets", "seedTargets", "seed_two"),
        ("sourceReaderClosure", "seedTargets", "reader_one"),
        ("sourceReaderClosure", "seedTargets", "reader_two"),
    ]
    .into_iter()
    .map(
        |(command_ref, binding_command_ref, test)| ResolvedCommandArgvV1 {
            command_ref: command_ref.to_string(),
            binding_command_ref: binding_command_ref.to_string(),
            derived_argv: vec![
                "-p".into(),
                "fixture-host".into(),
                "--test".into(),
                test.into(),
            ],
        },
    )
    .chain(std::iter::once(ResolvedCommandArgvV1 {
        command_ref: "check".into(),
        binding_command_ref: "check".into(),
        derived_argv: Vec::new(),
    }))
    .collect::<Vec<_>>();
    let expected_digest = plan::resolved_command_argv_digest_at_policy_base(
        root,
        &bundle.policy_base_sha,
        &expected_commands,
    )
    .unwrap();
    assert_eq!(bundle.resolved_command_digest, expected_digest);

    let ledger_path = root.join("coordination/rounds/r90/events.jsonl");
    let original_ledger = fs::read(&ledger_path).unwrap();
    let mut reordered = orch_core::read_ledger(&ledger_path).unwrap().events;
    let duplicate_positions = reordered
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "GateExecuted"
                && event.payload.as_ref().unwrap()["phase"] == "collect"
                && event.payload.as_ref().unwrap()["commandRef"] == "seedTargets"
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(duplicate_positions.len(), 2);
    reordered.swap(duplicate_positions[0], duplicate_positions[1]);
    write_event_ledger(&ledger_path, &reordered);
    tierf::load_validated_collect_gate_bundle_v1(root, ROUND, TASK, ATTEMPT)
        .expect_err("same-commandRef event order drift must fail positional replay");
    fs::write(&ledger_path, &original_ledger).unwrap();

    let mut gate_run_id_drift = orch_core::read_ledger(&ledger_path).unwrap().events;
    gate_run_id_drift
        .iter_mut()
        .find(|event| {
            event.kind == "GateExecuted" && event.payload.as_ref().unwrap()["phase"] == "collect"
        })
        .unwrap()
        .payload
        .as_mut()
        .unwrap()["gateRunId"] = serde_json::json!("forged-gate-run-id");
    write_event_ledger(&ledger_path, &gate_run_id_drift);
    tierf::load_validated_collect_gate_bundle_v1(root, ROUND, TASK, ATTEMPT)
        .expect_err("gateRunId drift must fail positional replay");
    fs::write(&ledger_path, &original_ledger).unwrap();

    let alternate_policy_base = git(root, &["rev-parse", "main"]);
    assert_ne!(alternate_policy_base, fixture.policy_base_sha);
    let mut base_drift = orch_core::read_ledger(&ledger_path).unwrap().events;
    base_drift
        .iter_mut()
        .find(|event| event.kind == "DispatchIssued" && event.task_id.as_deref() == Some(TASK))
        .unwrap()
        .payload
        .as_mut()
        .unwrap()["baseSha"] = serde_json::json!(alternate_policy_base);
    write_event_ledger(&ledger_path, &base_drift);
    tierf::load_validated_collect_gate_bundle_v1(root, ROUND, TASK, ATTEMPT)
        .expect_err("DispatchIssued policy base drift must fail committed replay");
    fs::write(&ledger_path, &original_ledger).unwrap();

    let mut dispatch_drift = orch_core::read_ledger(&ledger_path).unwrap().events;
    dispatch_drift
        .iter_mut()
        .find(|event| event.kind == "DispatchIssued" && event.task_id.as_deref() == Some(TASK))
        .unwrap()
        .payload
        .as_mut()
        .unwrap()["agent"] = serde_json::json!("forged-agent");
    write_event_ledger(&ledger_path, &dispatch_drift);
    tierf::load_validated_collect_gate_bundle_v1(root, ROUND, TASK, ATTEMPT)
        .expect_err("attestation identity must not self-authorize over DispatchIssued drift");
    fs::write(&ledger_path, &original_ledger).unwrap();

    for key in [
        "resolvedCommandDigest",
        "sourceReaderDescriptorSha256",
        "sourceReaderBaseSha256",
    ] {
        let mut receipt_drift = orch_core::read_ledger(&ledger_path).unwrap().events;
        let payload = receipt_drift
            .iter_mut()
            .find(|event| event.kind == "CollectGateSuccessReceipt")
            .unwrap()
            .payload
            .as_mut()
            .unwrap();
        assert!(payload.get(key).is_some(), "active receipt must bind {key}");
        payload[key] = serde_json::json!("0".repeat(64));
        write_event_ledger(&ledger_path, &receipt_drift);
        assert!(
            tierf::load_validated_collect_gate_bundle_v1(root, ROUND, TASK, ATTEMPT).is_err(),
            "{key} drift must fail committed replay"
        );
        fs::write(&ledger_path, &original_ledger).unwrap();
    }

    let mut forged = orch_core::read_ledger(&ledger_path).unwrap().events;
    let gate = forged
        .iter_mut()
        .find(|event| {
            event.kind == "GateExecuted" && event.payload.as_ref().unwrap()["phase"] == "collect"
        })
        .unwrap();
    gate.payload.as_mut().unwrap()["eleventhKey"] = serde_json::json!(true);
    write_event_ledger(&ledger_path, &forged);
    let error = tierf::load_validated_collect_gate_bundle_v1(root, ROUND, TASK, ATTEMPT)
        .expect_err("an eleventh GateExecuted key must fail committed replay");
    assert!(format!("{error:#}").contains("精确十键"));
    fs::write(&ledger_path, original_ledger).unwrap();
    tierf::load_validated_collect_gate_bundle_v1(root, ROUND, TASK, ATTEMPT)
        .unwrap()
        .expect("restored committed replay");
}

#[test]
fn unknown_reader_escalation_precedes_fast_gates_and_is_replay_causal() {
    let fixture = setup(true);
    let unknown_reader = r#"use std::fs as disk;
use std::fs::OpenOptions;

#[test]
fn reads_runtime_subject() {
    let _ = disk::read("../../../../product.txt").unwrap();
    let _ = OpenOptions::new()
        .read(true)
        .open("../../../../product.txt")
        .unwrap();
}
"#;
    let (worktree, candidate_sha) = prepare_attempt_variant(
        &fixture,
        "authorized-by-successor\n",
        Some(unknown_reader),
    );
    let root = &fixture.root;

    tierf::run_await(root, TASK, 10, None).unwrap();
    let ledger_path = root.join("coordination/rounds/r90/events.jsonl");
    let events = orch_core::read_ledger(&ledger_path).unwrap().events;
    let escalation_positions = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "GateLaneEscalated"
                && event.task_id.as_deref() == Some(TASK)
                && event.payload.as_ref().unwrap()["attemptId"] == ATTEMPT
        })
        .map(|(position, _)| position)
        .collect::<Vec<_>>();
    assert_eq!(escalation_positions.len(), 1);
    let first_gate_position = events
        .iter()
        .position(|event| {
            event.kind == "GateExecuted"
                && event.task_id.as_deref() == Some(TASK)
                && event.payload.as_ref().unwrap()["phase"] == "collect"
        })
        .unwrap();
    let receipt_position = events
        .iter()
        .position(|event| {
            event.kind == "CollectGateSuccessReceipt"
                && event.task_id.as_deref() == Some(TASK)
        })
        .unwrap();
    assert!(escalation_positions[0] < first_gate_position);
    assert!(first_gate_position < receipt_position);
    assert_eq!(
        receipt_window_gate_events(&events, "collect")
            .iter()
            .map(|event| event.payload.as_ref().unwrap()["commandRef"]
                .as_str()
                .unwrap())
            .collect::<Vec<_>>(),
        ["testFast", "testExclusive", "check"]
    );
    let escalated_receipt = events
        .iter()
        .find(|event| {
            event.kind == "CollectGateSuccessReceipt"
                && event.task_id.as_deref() == Some(TASK)
        })
        .and_then(|event| event.payload.as_ref())
        .expect("escalated collect receipt");
    for key in [
        "sourceReaderDescriptorSha256",
        "sourceReaderBaseSha256",
    ] {
        assert!(
            escalated_receipt
                .get(key)
                .and_then(serde_json::Value::as_str)
                .is_some_and(|digest| digest.len() == 64),
            "fast escalation receipt must retain authenticated {key}"
        );
    }

    tierf::run_await(root, TASK, 10, None).unwrap();
    let replayed = orch_core::read_ledger(&ledger_path).unwrap().events;
    assert_eq!(
        replayed
            .iter()
            .filter(|event| {
                event.kind == "GateLaneEscalated"
                    && event.task_id.as_deref() == Some(TASK)
            })
            .count(),
        1,
        "completed replay duplicated the monotonic escalation"
    );

    remove_attempt_worktree(root, &worktree);
    let bundle = tierf::load_validated_collect_gate_bundle_v1(root, ROUND, TASK, ATTEMPT)
        .unwrap()
        .expect("escalated fast receipt bundle");
    assert_eq!(bundle.candidate_sha, candidate_sha);
    assert_eq!(
        bundle
            .gates
            .iter()
            .map(|gate| gate.command_ref.as_str())
            .collect::<Vec<_>>(),
        ["testFast", "testExclusive", "check"]
    );

    let original_ledger = fs::read(&ledger_path).unwrap();
    for key in [
        "sourceReaderDescriptorSha256",
        "sourceReaderBaseSha256",
    ] {
        let mut drifted = orch_core::read_ledger(&ledger_path).unwrap().events;
        drifted
            .iter_mut()
            .find(|event| {
                event.kind == "CollectGateSuccessReceipt"
                    && event.task_id.as_deref() == Some(TASK)
            })
            .unwrap()
            .payload
            .as_mut()
            .unwrap()[key] = serde_json::json!("0".repeat(64));
        write_event_ledger(&ledger_path, &drifted);
        assert!(
            tierf::load_validated_collect_gate_bundle_v1(root, ROUND, TASK, ATTEMPT).is_err(),
            "escalated {key} drift must fail committed replay"
        );
        fs::write(&ledger_path, &original_ledger).unwrap();
    }
    let mut reordered = orch_core::read_ledger(&ledger_path).unwrap().events;
    let escalation = reordered
        .iter()
        .position(|event| {
            event.kind == "GateLaneEscalated" && event.task_id.as_deref() == Some(TASK)
        })
        .unwrap();
    let first_gate = reordered
        .iter()
        .position(|event| {
            event.kind == "GateExecuted"
                && event.task_id.as_deref() == Some(TASK)
                && event.payload.as_ref().unwrap()["phase"] == "collect"
        })
        .unwrap();
    assert!(escalation < first_gate);
    reordered.swap(escalation, first_gate);
    write_event_ledger(&ledger_path, &reordered);
    let error = tierf::load_validated_collect_gate_bundle_v1(root, ROUND, TASK, ATTEMPT)
        .expect_err("an escalation after the first collect gate must not authorize replay");
    assert!(format!("{error:#}").contains("先于首条 collect GateExecuted"));
    fs::write(&ledger_path, original_ledger).unwrap();
}

#[test]
fn a_gate_that_mutates_the_subject_tree_cannot_mint_a_receipt() {
    let fixture = setup(true);
    let (_worktree, _) = prepare_attempt_variant(&fixture, "mutate-on-gate\n", None);
    let root = &fixture.root;

    let error = match tierf::run_await(root, TASK, 10, None) {
        Ok(_) => panic!("a gate-mutated subject tree must fail the collect"),
        Err(error) => error,
    };
    assert!(format!("{error:#}").contains("subject tree 漂移"));
    let events = orch_core::read_ledger(&root.join("coordination/rounds/r90/events.jsonl"))
        .unwrap()
        .events;
    assert!(!events.iter().any(|event| {
        event.kind == "CollectGateSuccessReceipt" && event.task_id.as_deref() == Some(TASK)
    }));
    assert!(!events.iter().any(|event| {
        event.kind == "GateExecuted"
            && event.task_id.as_deref() == Some(TASK)
            && event.payload.as_ref().unwrap()["phase"] == "collect"
    }));
}

#[test]
fn dormant_candidate_policy_keeps_fast_receipt_and_committed_replay() {
    let fixture = setup(false);
    let (worktree, candidate_sha) = prepare_attempt(&fixture);
    let root = &fixture.root;

    tierf::run_await(root, TASK, 10, None).unwrap();
    let events = orch_core::read_ledger(&root.join("coordination/rounds/r90/events.jsonl"))
        .unwrap()
        .events;
    let collect_gates = receipt_window_gate_events(&events, "collect");
    let collect_refs = collect_gates
        .iter()
        .map(|event| {
            event.payload.as_ref().unwrap()["commandRef"]
                .as_str()
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(collect_refs, ["testFast", "testExclusive", "check"]);
    assert!(
        !fixture.selector_log.exists(),
        "dormant policy must not execute derived candidate selectors"
    );

    let red_replay_gates = receipt_window_gate_events(&events, "red-replay");
    assert_eq!(red_replay_gates.len(), 1);
    let red_replay_payload = red_replay_gates[0].payload.as_ref().unwrap();
    assert_eq!(red_replay_payload["commandRef"], "testFast");
    let red_replay_event_id = red_replay_gates[0].event_id.clone();
    let red_replay_run_id = red_replay_payload["gateRunId"]
        .as_str()
        .unwrap()
        .to_string();

    let before_replay = collect_gates.len();
    tierf::run_await(root, TASK, 10, None).unwrap();
    let replayed = orch_core::read_ledger(&root.join("coordination/rounds/r90/events.jsonl"))
        .unwrap()
        .events;
    assert_eq!(
        receipt_window_gate_events(&replayed, "collect").len(),
        before_replay,
        "durable dormant replay reran fast collect gates"
    );

    remove_attempt_worktree(root, &worktree);
    let bundle = tierf::load_validated_collect_gate_bundle_v1(root, ROUND, TASK, ATTEMPT)
        .unwrap()
        .expect("dormant fast receipt bundle");
    assert_eq!(bundle.candidate_sha, candidate_sha);
    assert_eq!(
        bundle
            .gates
            .iter()
            .map(|gate| gate.command_ref.as_str())
            .collect::<Vec<_>>(),
        ["testFast", "testExclusive", "check"]
    );
    assert!(bundle.gates.iter().all(|gate| {
        gate.source_event_id != red_replay_event_id && gate.gate_run_id != red_replay_run_id
    }));
    let expected_digest = plan::resolved_command_argv_digest_at_policy_base(
        root,
        &bundle.policy_base_sha,
        &[
            ResolvedCommandArgvV1 {
                command_ref: "testFast".into(),
                binding_command_ref: "testFast".into(),
                derived_argv: Vec::new(),
            },
            ResolvedCommandArgvV1 {
                command_ref: "testExclusive".into(),
                binding_command_ref: "testExclusive".into(),
                derived_argv: Vec::new(),
            },
            ResolvedCommandArgvV1 {
                command_ref: "check".into(),
                binding_command_ref: "check".into(),
                derived_argv: Vec::new(),
            },
        ],
    )
    .unwrap();
    assert_eq!(bundle.resolved_command_digest, expected_digest);
}
