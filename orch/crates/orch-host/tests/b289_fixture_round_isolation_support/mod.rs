use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use orch_core::EventRecord;
use orch_host::{ledger, plan};

/// Stable marker consumed by the frozen B289 integration contract.
pub const CONTRACT_ID: &str = "B289";

/// Fixture-only round name, intentionally absent from the live repository.
pub const SYNTHETIC_ROUND: &str = "r9999";

/// Legacy agent whose pin can be changed without an inline argv literal.
pub const AMENDABLE_AGENT: &str = "executor-pi";

const REGISTRY: &str = r#"apiVersion: orch/v1alpha1
kind: AgentRegistry
# B289 synthetic registry fixture; these bytes never come from the caller repository.

agents:
  executor-pi:
    injectable: true
    sessionId: "b289-synthetic-pi"
    provider: synthetic-provider
    model: synthetic-model-old
    effort: xhigh
    observation: {source: synthetic-pi-frames, policy: advisory}
    roles: [implement]
    wake: {argv: ["sh", "orch/scripts/wake-pi-stream.sh", "{message}"]}
    pokeHint: "synthetic implementer"

  executor-opencode:
    injectable: true
    sessionId: "b289-synthetic-reviewer"
    provider: synthetic-review-provider
    model: synthetic-inline-model
    effort: max
    observation: {source: synthetic-review-frames, policy: advisory}
    roles: [primary-review]
    wake: {argv: ["opencode", "run", "{message}", "--model", "synthetic-inline-model", "--variant", "max"]}
    pokeHint: "synthetic reviewer with an inline model literal"
"#;

const MODE: &str = r#"apiVersion: orch/v1alpha1
kind: ModeConfig
metadata: {name: b289-synthetic}

agents:
  verifier: {adapter: root-manual, tier: none}
hitl: {planSignoff: required, mergeGate: auto}
verification: {mode: root-manual-fixed-head}
liveness: {monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}
scheduling:
  allowedAgents: [executor-pi, executor-opencode]
  capacities:
    executor-pi: {agent: 1, quota: 1, roles: [implement]}
    executor-opencode: {agent: 1, quota: 1, roles: [primary-review]}
budgets: {round: {maxUsd: 1, wallMinutes: 30, maxModelWakes: 2}}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
"#;

const BINDING: &str = r#"apiVersion: orch/v1alpha1
kind: ProjectBinding
metadata: {name: b289-synthetic, bindingRevision: 1}
project: {root: ".", primaryBranch: main, ecosystems: [rust]}
workspace: {defaultIsolation: git-worktree, worktreeRoot: .worktrees}
commands:
  testFast: {argv: ["sh", "-c", "exit 0"], timeoutSeconds: 30}
gates: {fast: [testFast], merge: [testFast]}
scope: {protectedPaths: ["coordination/**"]}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
verification: {contractModes: [verify-only], independentVerifier: required-for-write}
"#;

const FROZEN_BASELINE: &str = r#"{
  "schemaVersion": 1,
  "baselineTreeSha": "0000000000000000000000000000000000000000",
  "scope": {
    "declaredPairAuditThrough": "fixture",
    "effectiveBaselineThrough": "fixture",
    "selection": "fixture contains no landed seed targets"
  },
  "counts": {
    "declaredPairsThroughR70": 0,
    "driftedDeclaredPairsThroughR70": 0,
    "missingDeclaredPairsThroughR70": 0,
    "uniqueEffectiveTargetsThroughB269": 0,
    "presentEffectiveTargets": 0,
    "effectiveTombstones": 0,
    "excludedUnrecordedMissingTargets": 0
  },
  "excludedUnrecordedMissingTargets": [],
  "targets": []
}
"#;

fn task_card() -> String {
    format!(
        "---\n\
taskId: T1\n\
round: {SYNTHETIC_ROUND}\n\
agent: {AMENDABLE_AGENT}\n\
seedProtocol: verify-only\n\
complexity: light\n\
capabilities: [implement]\n\
entryPoints: [fixture.txt]\n\
writeSet: [fixture.txt]\n\
frozenPaths: [coordination/**]\n\
gates: {{fast: [testFast]}}\n\
budgets: {{wallMinutes: 30}}\n\
requiredReviews:\n\
  - {{role: primary, agent: executor-opencode}}\n\
requiredEvidence: [b289-synthetic-round]\n\
---\n\
# B289 synthetic task\n"
    )
}

fn write_scene_inputs(root: &Path) {
    for relative in [
        "coordination/modes",
        "coordination/runtime/ledger-wal",
        &format!("coordination/rounds/{SYNTHETIC_ROUND}/tasks"),
    ] {
        fs::create_dir_all(root.join(relative)).expect("create synthetic fixture directory");
    }
    fs::write(root.join("coordination/agents.yaml"), REGISTRY)
        .expect("write synthetic agent registry");
    fs::write(root.join("coordination/modes/b289-synthetic.yaml"), MODE)
        .expect("write synthetic mode");
    fs::write(root.join("coordination/PROJECT-BINDING.yaml"), BINDING)
        .expect("write synthetic binding");
    fs::write(
        root.join("coordination/frozen-contract-baseline-v1.json"),
        FROZEN_BASELINE,
    )
    .expect("write synthetic frozen-contract baseline");
    fs::write(
        root.join(format!(
            "coordination/rounds/{SYNTHETIC_ROUND}/MODE-REF.yaml"
        )),
        "modeRef: b289-synthetic\n",
    )
    .expect("write synthetic mode binding");
    fs::write(
        root.join(format!("coordination/rounds/{SYNTHETIC_ROUND}/tasks/T1.md")),
        task_card(),
    )
    .expect("write synthetic task card");
    fs::write(
        root.join("coordination/runtime/CURRENT-ROUND"),
        format!("{SYNTHETIC_ROUND}\n"),
    )
    .expect("write synthetic current-round pointer");
    fs::write(root.join("fixture.txt"), "b289 fixture entry point\n")
        .expect("write synthetic entry point");
}

fn run_git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("spawn git for synthetic fixture");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn initialize_repository(root: &Path) {
    run_git(root, &["init", "-q"]);
    run_git(root, &["add", "."]);
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=orch fixture",
            "-c",
            "user.email=orch-fixture@example.invalid",
            "commit",
            "-q",
            "-m",
            "fixture",
        ])
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .output()
        .expect("commit synthetic fixture");
    assert!(
        output.status.success(),
        "git fixture commit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    run_git(root, &["branch", "-M", "main"]);
}

fn event(
    event_id: &str,
    kind: &str,
    actor: &str,
    task_id: Option<&str>,
    payload: serde_json::Value,
) -> EventRecord {
    EventRecord {
        event_id: event_id.to_string(),
        ts: "2026-01-01T00:00:00Z".to_string(),
        actor: actor.to_string(),
        kind: kind.to_string(),
        task_id: task_id.map(str::to_string),
        round: Some(SYNTHETIC_ROUND.to_string()),
        payload: Some(payload),
        extra: Default::default(),
    }
}

fn write_events(root: &Path, events: &[EventRecord]) {
    let mut bytes = Vec::new();
    for item in events {
        serde_json::to_writer(&mut bytes, item).expect("serialize synthetic event");
        bytes.push(b'\n');
    }
    fs::write(
        root.join(format!(
            "coordination/rounds/{SYNTHETIC_ROUND}/events.jsonl"
        )),
        &bytes,
    )
    .expect("write synthetic ledger");
    fs::write(
        root.join(format!(
            "coordination/runtime/ledger-wal/{SYNTHETIC_ROUND}.jsonl"
        )),
        bytes,
    )
    .expect("write synthetic WAL");
}

fn install_deterministic_authorization(root: &Path, revision: u32, digest: &str) {
    let validated = event(
        "00000000000000000000000001",
        "TaskValidated",
        "runtime:orch",
        None,
        plan::task_validated_payload(revision, digest),
    );
    let signed = event(
        "00000000000000000000000002",
        "PlanSignedOff",
        "user",
        None,
        plan::plan_signed_off_payload("B289 fixture sign-off", revision, digest)
            .expect("build canonical fixture sign-off"),
    );
    write_events(root, &[validated, signed]);
}

/// Build a complete scratch-local round and bind its IR to deterministic inputs.
pub fn synthesize_signed_round(label: &str) -> PathBuf {
    let root = orch_host::util::test_scratch_dir(label);
    write_scene_inputs(&root);
    initialize_repository(&root);
    let outcome = plan::run_plan(&root).expect("compile synthetic signed round");
    install_deterministic_authorization(&root, outcome.revision, &outcome.digest);
    root
}

/// Build the same local round while omitting its user sign-off event.
pub fn synthesize_unsigned_round(label: &str) -> PathBuf {
    let root = synthesize_signed_round(label);
    let ledger = orch_core::read_ledger(&root.join(format!(
        "coordination/rounds/{SYNTHETIC_ROUND}/events.jsonl"
    )))
    .expect("read synthetic ledger");
    let events = ledger
        .events
        .into_iter()
        .filter(|item| item.kind != "PlanSignedOff")
        .collect::<Vec<_>>();
    write_events(&root, &events);
    root
}

/// Build the same local round and append a terminal round-close fact.
pub fn synthesize_closed_round(label: &str) -> PathBuf {
    let root = synthesize_signed_round(label);
    ledger::append(
        &root,
        SYNTHETIC_ROUND,
        &[ledger::event(
            "RoundClosed",
            "runtime:orch",
            None,
            Some(SYNTHETIC_ROUND),
            serde_json::json!({}),
        )],
    )
    .expect("append synthetic RoundClosed");
    root
}

/// Add a canonical unresolved merge barrier to the local round ledger.
pub fn inject_unresolved_merge_barrier(root: &Path, task_id: &str) {
    let ledger = orch_core::read_ledger(&root.join(format!(
        "coordination/rounds/{SYNTHETIC_ROUND}/events.jsonl"
    )))
    .expect("read synthetic ledger before barrier injection");
    assert!(
        ledger.bad_lines.is_empty(),
        "synthetic ledger must be canonical"
    );
    let mut events = ledger.events;
    events.push(event(
        "00000000000000000000000003",
        "MergeStarted",
        "runtime:orch",
        Some(task_id),
        serde_json::json!({
            "attemptId": format!("{task_id}-A0001"),
            "attemptNo": 1,
            "headSha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "mainHeadSha": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "collectCompletedEventId": "fixture-collect",
            "verdictEventId": "fixture-verdict"
        }),
    ));
    write_events(root, &events);
}

/// Change the registry bytes without recording an audited amendment event.
pub fn hand_edit_registry_without_event(root: &Path) {
    let path = root.join("coordination/agents.yaml");
    let before = fs::read_to_string(&path).expect("read synthetic registry before hand edit");
    let after = before.replace(
        "model: synthetic-model-old",
        "model: synthetic-model-manual",
    );
    assert_ne!(
        before, after,
        "synthetic registry hand edit must change bytes"
    );
    fs::write(path, after).expect("write synthetic registry hand edit");
}
