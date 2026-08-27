//! Scratch-scene construction for the frozen B292 process-level contract.
//!
//! This module creates repositories, signed task state, durable nongate wake
//! facts, and receipt bytes. The frozen carrier itself spawns the real CLI and
//! reads the resulting ledger; this support never reports a verdict or seal.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use orch_core::EventRecord;

/// Stable identity checked by the frozen carrier.
pub const CONTRACT_ID: &str = "B292";

const ROUND: &str = "r9998";
const TASK: &str = "B9292";
const ATTEMPT: &str = "B9292-A0001";
const NONGATE_SEATS: [&str; 2] = ["executor-antigravity", "executor-dsh"];

const MODE: &str = r#"apiVersion: orch/v1alpha1
kind: ModeConfig
metadata: {name: b292-synthetic}

agents:
  executor: {adapter: test, tier: none}
  verifier: {adapter: root-manual, tier: none}
hitl: {planSignoff: required, mergeGate: auto}
verification: {mode: root-manual-fixed-head}
liveness: {monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}
scheduling:
  allowedAgents: [executor-claw, executor-opencode, executor-antigravity, executor-dsh]
  capacities:
    executor-claw: {agent: 2, quota: 2, roles: [implement, primary-review]}
    executor-opencode: {agent: 2, quota: 2, roles: [primary-review, secondary-review]}
    executor-antigravity: {agent: 2, quota: 2, roles: [primary-review, secondary-review]}
    executor-dsh: {agent: 2, quota: 2, roles: [primary-review, secondary-review]}
budgets: {round: {maxUsd: 1, wallMinutes: 60, maxModelWakes: 20}}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
"#;

const BINDING: &str = r#"apiVersion: orch/v1alpha1
kind: ProjectBinding
metadata: {name: b292-synthetic, bindingRevision: 1}
project: {root: ".", primaryBranch: main, ecosystems: [test]}
workspace: {defaultIsolation: git-worktree, worktreeRoot: .worktrees}
commands:
  postGate: {argv: ["sh", "-c", "exit 0"], timeoutSeconds: 30}
gates: {fast: [postGate], merge: [postGate]}
scope: {protectedPaths: ["coordination/**"]}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
verification: {contractModes: [verify-only], independentVerifier: required-for-write}
"#;

const REGISTRY: &str = r#"apiVersion: orch/v1alpha1
kind: AgentRegistry
agents:
  executor-claw:
    injectable: true
    sessionId: b292-implementer
    provider: synthetic
    model: synthetic-implementer
    effort: high
    observation: {source: fixture, policy: advisory}
    roles: [implement, primary-review]
    wake: {argv: ["sh", "-c", "exit 0", "{message}"]}
  executor-opencode:
    injectable: true
    sessionId: b292-formal
    provider: synthetic
    model: synthetic-formal
    effort: high
    observation: {source: fixture, policy: advisory}
    roles: [primary-review, secondary-review]
    wake: {argv: ["sh", "-c", "exit 0", "{message}"]}
  executor-antigravity:
    injectable: true
    sessionId: b292-agy
    provider: google
    model: gemini-3.7-flash-high
    effort: high
    observation: {source: fixture, policy: advisory}
    roles: [primary-review, secondary-review]
    wake: {argv: ["sh", "-c", "exit 0", "{message}"]}
  executor-dsh:
    injectable: true
    sessionId: b292-dsh
    provider: one-dewu-dsh
    model: deepseek-v4-pro
    effort: max
    observation: {source: fixture, policy: advisory}
    roles: [primary-review, secondary-review]
    wake: {argv: ["sh", "-c", "exit 0", "{message}"]}
"#;

/// One isolated signed repository consumed by a spawned `orch` process.
pub struct Scene {
    /// Canonical scratch repository root passed to the real CLI.
    pub root: PathBuf,
    /// Active synthetic round.
    pub round: String,
    /// Task identity used by the process invocation.
    pub task_id: String,
    /// Exact current attempt.
    pub attempt_id: String,
    /// Fixed task branch HEAD.
    pub expected_head: String,
    /// Main HEAD that contains the signed fixture contract.
    pub expected_main: String,
    current_facts: BTreeMap<String, RuntimeFacts>,
    other_attempt_facts: BTreeMap<String, RuntimeFacts>,
}

impl Drop for Scene {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Clone)]
struct RuntimeFacts {
    wake_id: String,
    wake_event_id: String,
    lease_event_id: String,
    worktree_rel: String,
}

fn source_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("orch-host manifest must live at <root>/orch/crates/orch-host")
        .to_path_buf()
}

/// Return the workspace-default binary required by the process-level seed.
pub fn orch_binary() -> PathBuf {
    source_root().join("orch/target/debug/orch")
}

/// Bind a spawned CLI to this scratch repository and explicitly allow the
/// source-tree binary whose build imprint cannot equal the synthetic repo.
pub fn configure_fixture_command(command: &mut Command, scene: &Scene) {
    command
        .arg("--root")
        .arg(&scene.root)
        .arg("--allow-stale-binary")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
}

fn run_git(root: &Path, args: &[&str], label: &str) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap_or_else(|error| panic!("{label} failed to spawn: {error}"));
    assert!(
        output.status.success(),
        "{label} failed ({}):\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stdout(root: &Path, args: &[&str], label: &str) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap_or_else(|error| panic!("{label} failed to spawn: {error}"));
    assert!(
        output.status.success(),
        "{label} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap_or_else(|error| panic!("{label} produced non-UTF-8 output: {error}"))
        .trim()
        .to_string()
}

fn task_card(formal_reviewer: &str) -> String {
    format!(
        "---\n\
taskId: {TASK}\n\
round: {ROUND}\n\
agent: executor-claw\n\
seedProtocol: verify-only\n\
complexity: light\n\
capabilities: [implement]\n\
entryPoints: [feature.txt]\n\
writeSet: [feature.txt]\n\
frozenPaths: [coordination/**]\n\
gates: {{fast: [postGate]}}\n\
budgets: {{wallMinutes: 30}}\n\
requiredReviews:\n\
  - {{role: primary, agent: {formal_reviewer}}}\n\
requiredEvidence: [nongate-receipt-fixture]\n\
---\n\
# B292 synthetic task\n"
    )
}

fn initialize_repository(root: &Path) {
    run_git(root, &["init", "-q"], "initialize fixture repository");
    run_git(
        root,
        &["config", "user.email", "orch-fixture@example.invalid"],
        "configure fixture email",
    );
    run_git(
        root,
        &["config", "user.name", "orch fixture"],
        "configure fixture name",
    );
    fs::write(root.join("README.md"), "base\n").expect("write fixture README");
    fs::write(
        root.join(".gitignore"),
        ".worktrees/\n.cowork-temp/\ncoordination/runtime/\n",
    )
    .expect("write fixture gitignore");
    run_git(
        root,
        &["add", "README.md", ".gitignore"],
        "stage fixture base",
    );
    run_git(root, &["commit", "-q", "-m", "base"], "commit fixture base");
    run_git(root, &["branch", "-M", "main"], "name fixture main");

    run_git(
        root,
        &["checkout", "-q", "-b", "task/B9292"],
        "create task branch",
    );
    fs::write(root.join("feature.txt"), "nongate receipt fixture\n")
        .expect("write fixture feature");
    run_git(root, &["add", "feature.txt"], "stage fixture feature");
    run_git(
        root,
        &["commit", "-q", "-m", "fixture feature"],
        "commit fixture feature",
    );
    run_git(root, &["checkout", "-q", "main"], "return to fixture main");
    fs::create_dir_all(root.join(".worktrees")).expect("create fixture worktree root");
    run_git(
        root,
        &["worktree", "add", "-q", ".worktrees/B9292", "task/B9292"],
        "create fixture task worktree",
    );
}

fn write_contract_inputs(root: &Path, formal_reviewer: &str) {
    for relative in [
        "coordination/runtime/ledger-wal",
        "coordination/modes",
        "coordination/rounds/r9998/tasks",
        "coordination/rounds/r9998/reviews",
        "coordination/rounds/r9998/evidence",
    ] {
        fs::create_dir_all(root.join(relative)).expect("create fixture contract directory");
    }
    fs::write(
        root.join("coordination/runtime/CURRENT-ROUND"),
        format!("{ROUND}\n"),
    )
    .expect("write fixture current round");
    fs::write(root.join("coordination/modes/b292-synthetic.yaml"), MODE)
        .expect("write fixture mode");
    fs::write(root.join("coordination/PROJECT-BINDING.yaml"), BINDING)
        .expect("write fixture binding");
    fs::write(root.join("coordination/agents.yaml"), REGISTRY).expect("write fixture registry");
    fs::write(
        root.join("coordination/rounds/r9998/MODE-REF.yaml"),
        "modeRef: b292-synthetic\n",
    )
    .expect("write fixture mode ref");
    fs::write(
        root.join("coordination/rounds/r9998/tasks/B9292.md"),
        task_card(formal_reviewer),
    )
    .expect("write fixture task card");
    fs::write(root.join("coordination/BOARD.md"), "# fixture board\n")
        .expect("write fixture board");
}

fn runtime_fact_events(
    agent: &str,
    attempt_id: &str,
    fixed_head: &str,
    generation: u32,
) -> (RuntimeFacts, Vec<EventRecord>) {
    let wake_id = format!("b292-{attempt_id}-{agent}-g{generation:02}");
    let worktree_rel = format!(".worktrees/review-{attempt_id}-nongate-{agent}-g{generation:02}");
    let lease = orch_host::ledger::event(
        "WorkspaceLeased",
        "runtime:orch",
        Some(TASK),
        Some(ROUND),
        serde_json::json!({
            "agent": agent,
            "attemptId": attempt_id,
            "generation": generation,
            "paths": {
                "target": format!("orch/target/review-{attempt_id}-nongate-{agent}-g{generation:02}"),
                "worktree": worktree_rel,
            },
            "reviewedHead": fixed_head,
            "role": "nongate",
            "siteId": format!("{TASK}-nongate-{agent}-g{generation:02}"),
            "wakeId": wake_id,
        }),
    );
    let wake = orch_host::ledger::event(
        "WakeIssued",
        "runtime:orch",
        Some(TASK),
        Some(ROUND),
        serde_json::json!({
            "agent": agent,
            "attemptId": attempt_id,
            "continuationId": format!("review:{ROUND}:{TASK}:{attempt_id}:nongate:{agent}"),
            "method": "fixture",
            "wakeId": wake_id,
        }),
    );
    let facts = RuntimeFacts {
        wake_id,
        wake_event_id: wake.event_id.clone(),
        lease_event_id: lease.event_id.clone(),
        worktree_rel,
    };
    (facts, vec![lease, wake])
}

fn append_attempt_lifecycle(root: &Path, task_head: &str, base_head: &str) {
    let dispatch = orch_host::ledger::event(
        "DispatchIssued",
        "runtime:orch",
        Some(TASK),
        Some(ROUND),
        serde_json::json!({
            "taskId": TASK,
            "agent": "executor-claw",
            "attemptId": ATTEMPT,
            "attemptNo": 1,
            "goPath": "coordination/rounds/r9998/dispatch/executor-claw/GO-B9292-A0001.md",
            "baseSha": base_head,
        }),
    );
    let receipt = orch_host::ledger::event(
        "CollectGateSuccessReceipt",
        "runtime:orch",
        Some(TASK),
        Some(ROUND),
        serde_json::json!({
            "actionId": "collect-B9292-A0001",
            "attemptId": ATTEMPT,
            "attemptNo": 1,
            "agent": "executor-claw",
            "baseSha": base_head,
            "goPath": "coordination/rounds/r9998/dispatch/executor-claw/GO-B9292-A0001.md",
            "branchSha": task_head,
        }),
    );
    let collect = orch_host::ledger::event(
        "ReportCollectCompleted",
        "runtime:orch",
        Some(TASK),
        Some(ROUND),
        serde_json::json!({
            "actionId": "collect-B9292-A0001",
            "attemptId": ATTEMPT,
            "attemptNo": 1,
            "agent": "executor-claw",
            "baseSha": base_head,
            "goPath": "coordination/rounds/r9998/dispatch/executor-claw/GO-B9292-A0001.md",
            "branchSha": task_head,
            "gateReceipt": receipt.event_id,
        }),
    );
    orch_host::ledger::append(root, ROUND, &[dispatch, receipt, collect])
        .expect("append fixture attempt lifecycle");
}

fn build_scene(tag: &str, formal_reviewer: &str) -> Scene {
    let root = orch_host::util::test_scratch_dir(&format!("b292-{tag}"));
    initialize_repository(&root);
    let task_head = git_stdout(&root, &["rev-parse", "task/B9292"], "resolve task HEAD");
    let base_head = git_stdout(&root, &["rev-parse", "main"], "resolve base HEAD");
    write_contract_inputs(&root, formal_reviewer);

    orch_host::plan::run_plan(&root).expect("compile fixture ROUND-IR");
    orch_host::round::run_sign_off(&root, Some("B292 fixture sign-off"))
        .expect("sign fixture plan");
    append_attempt_lifecycle(&root, &task_head, &base_head);

    fs::write(
        root.join(format!(
            "coordination/rounds/{ROUND}/reviews/{ATTEMPT}-primary-{formal_reviewer}.md"
        )),
        format!(
            "---\ntaskId: {TASK}\nround: {ROUND}\nattemptId: {ATTEMPT}\nrole: primary\nreviewer: {formal_reviewer}\nverdict: PASS\nreviewedHead: {task_head}\n---\nfixture formal review\n"
        ),
    )
    .expect("write fixture formal review");
    fs::write(
        root.join(format!(
            "coordination/rounds/{ROUND}/evidence/{TASK}-nongate-receipt-fixture.json"
        )),
        "{\"fixture\":true}\n",
    )
    .expect("write fixture evidence");

    let mut current_facts = BTreeMap::new();
    let mut other_attempt_facts = BTreeMap::new();
    let mut durable_events = Vec::new();
    for agent in NONGATE_SEATS {
        let (current, mut events) = runtime_fact_events(agent, ATTEMPT, &task_head, 1);
        current_facts.insert(agent.to_string(), current);
        durable_events.append(&mut events);

        let (other, mut events) = runtime_fact_events(agent, "B9292-A0099", &task_head, 99);
        other_attempt_facts.insert(agent.to_string(), other);
        durable_events.append(&mut events);
    }
    orch_host::ledger::append(&root, ROUND, &durable_events)
        .expect("append fixture nongate durable facts");

    run_git(
        &root,
        &["add", "coordination"],
        "stage signed fixture contract",
    );
    run_git(
        &root,
        &["commit", "-q", "-m", "signed fixture contract"],
        "commit signed fixture contract",
    );
    let expected_main = git_stdout(&root, &["rev-parse", "main"], "resolve expected main");

    Scene {
        root,
        round: ROUND.to_string(),
        task_id: TASK.to_string(),
        attempt_id: ATTEMPT.to_string(),
        expected_head: task_head,
        expected_main,
        current_facts,
        other_attempt_facts,
    }
}

/// Build a real-verdict scene whose formal reviewer does not overlap the
/// standing nongate seats.
pub fn verdict_scene(tag: &str) -> Scene {
    build_scene(tag, "executor-opencode")
}

/// Build a real-seal scene. It has the same authorization shape as a verdict
/// scene, but the frozen test drives the complete merge lifecycle.
pub fn seal_scene(tag: &str) -> Scene {
    build_scene(tag, "executor-opencode")
}

/// Build a verdict scene with the supplied agent in the signed formal slot.
pub fn verdict_scene_with_formal_reviewer(tag: &str, reviewer: &str) -> Scene {
    build_scene(tag, reviewer)
}

fn invocation(agent: &str, root: &Path, worktree_rel: &str) -> serde_json::Value {
    let (provider, model, effort, preset) = match agent {
        "executor-antigravity" => ("google", "gemini-3.7-flash-high", "high", "direct"),
        "executor-dsh" => ("one-dewu-dsh", "deepseek-v4-pro", "max", "minimal"),
        other => panic!("unsupported fixture nongate agent: {other}"),
    };
    serde_json::json!({
        "provider": provider,
        "model": model,
        "effort": effort,
        "preset": preset,
        "cwd": root.join(worktree_rel).to_string_lossy(),
    })
}

fn write_receipt(
    scene: &Scene,
    agent: &str,
    state: &str,
    facts: &RuntimeFacts,
    terminal_reason: Option<&str>,
) {
    let path = scene.root.join(format!(
        "coordination/runtime/nongate-inbox/{}/{ATTEMPT}-{agent}.json",
        scene.round
    ));
    fs::create_dir_all(path.parent().expect("receipt parent"))
        .expect("create fixture receipt inbox");
    let bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "schemaVersion": 1,
        "round": scene.round,
        "attemptId": scene.attempt_id,
        "agent": agent,
        "fixedHead": scene.expected_head,
        "wakeId": facts.wake_id,
        "wakeIssuedEventId": facts.wake_event_id,
        "workspaceLeasedEventId": facts.lease_event_id,
        "invocation": invocation(agent, &scene.root, &facts.worktree_rel),
        "state": state,
        "terminalReason": terminal_reason,
    }))
    .expect("serialize fixture receipt");
    fs::write(path, bytes).expect("write fixture receipt");
}

/// Write a complete receipt bound to the current attempt's durable facts.
pub fn write_bound_receipt(scene: &Scene, agent: &str, state: &str) {
    let facts = scene
        .current_facts
        .get(agent)
        .unwrap_or_else(|| panic!("missing current fixture facts for {agent}"));
    let reason = (state != "answered").then_some("exact fixture terminal reason");
    write_receipt(scene, agent, state, facts, reason);
}

/// Write a non-answered receipt while intentionally omitting its mandatory
/// exact terminal reason.
pub fn write_bound_receipt_without_terminal_reason(scene: &Scene, agent: &str, state: &str) {
    let facts = scene
        .current_facts
        .get(agent)
        .unwrap_or_else(|| panic!("missing current fixture facts for {agent}"));
    write_receipt(scene, agent, state, facts, None);
}

/// Write a structurally complete receipt whose event ids do not exist.
pub fn write_unbound_receipt(scene: &Scene, agent: &str, state: &str) {
    let current = scene
        .current_facts
        .get(agent)
        .unwrap_or_else(|| panic!("missing current fixture facts for {agent}"));
    let forged = RuntimeFacts {
        wake_id: "forged-wake".to_string(),
        wake_event_id: "forged-wake-event".to_string(),
        lease_event_id: "forged-lease-event".to_string(),
        worktree_rel: current.worktree_rel.clone(),
    };
    let reason = (state != "answered").then_some("exact fixture terminal reason");
    write_receipt(scene, agent, state, &forged, reason);
}

/// Write a receipt for the current attempt that borrows both event ids from a
/// different attempt in the same round.
pub fn write_receipt_bound_to_other_attempt(scene: &Scene, agent: &str, state: &str) {
    let facts = scene
        .other_attempt_facts
        .get(agent)
        .unwrap_or_else(|| panic!("missing cross-attempt fixture facts for {agent}"));
    let reason = (state != "answered").then_some("exact fixture terminal reason");
    write_receipt(scene, agent, state, facts, reason);
}
