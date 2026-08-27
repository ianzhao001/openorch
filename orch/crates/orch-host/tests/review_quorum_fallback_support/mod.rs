use orch_host::card::{NongateSeat, RequiredReview, ReviewFallback, ReviewQuorumPolicy};
use orch_host::verify::{
    evaluate_review_quorum, ReviewQuorumDecision, ReviewResult, ReviewResultState,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const ATTEMPT: &str = "B303-A0001";
const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn policy() -> ReviewQuorumPolicy {
    ReviewQuorumPolicy {
        minimum_substantive: 2,
        nongate_may_substitute_failed_formal: true,
        minimum_nongate_pass_for_substitution: 1,
    }
}

fn result(
    agent: &str,
    role: &str,
    delivery: &str,
    state: ReviewResultState,
) -> ReviewResult {
    ReviewResult {
        agent: agent.to_string(),
        role: role.to_string(),
        attempt_id: ATTEMPT.to_string(),
        reviewed_head: HEAD.to_string(),
        delivery_event_id: delivery.to_string(),
        state,
    }
}

#[test]
fn legacy_required_review_bytes_do_not_gain_a_null_fallback() {
    let historical = RequiredReview {
        role: "primary".to_string(),
        agent: "executor-opencode".to_string(),
    };
    let encoded = serde_json::to_value(&historical).unwrap();
    assert_eq!(
        encoded,
        serde_json::json!({"role": "primary", "agent": "executor-opencode"})
    );
    assert_eq!(
        serde_json::from_value::<RequiredReview>(encoded).unwrap(),
        historical
    );
}

#[test]
fn signed_fallback_nongate_and_quorum_shapes_are_closed() {
    let fallback: ReviewFallback =
        serde_yaml::from_str("role: primary\nfallbackAgent: executor-pi\n").unwrap();
    assert_eq!(fallback.fallback_agent, "executor-pi");
    assert!(serde_yaml::from_str::<RequiredReview>(
        "role: primary\nagent: executor-opencode\nfallbackAgent: executor-pi\n"
    )
    .is_err());
    let seat: NongateSeat =
        serde_yaml::from_str("agent: executor-dsh\npreset: minimal\n").unwrap();
    assert_eq!(seat.preset, "minimal");
    let parsed: ReviewQuorumPolicy = serde_yaml::from_str(
        "minimumSubstantive: 2\nnongateMaySubstituteFailedFormal: true\nminimumNongatePassForSubstitution: 1\n",
    )
    .unwrap();
    assert_eq!(parsed, policy());
    assert!(serde_yaml::from_str::<ReviewQuorumPolicy>(
        "minimumSubstantive: 2\nnongateMaySubstituteFailedFormal: true\nminimumNongatePassForSubstitution: 1\nunknown: true\n"
    )
    .is_err());
}

#[test]
fn channel_failures_are_zero_votes_and_pending_formal_never_hides() {
    for zero_vote in [
        ReviewResultState::ChannelError,
        ReviewResultState::Empty,
        ReviewResultState::TimedOut,
    ] {
        assert_eq!(
            evaluate_review_quorum(
                &policy(),
                &[
                    result(
                        "executor-opencode",
                        "primary",
                        "formal-pass",
                        ReviewResultState::FormalPass,
                    ),
                    result("executor-dsh", "nongate", "zero", zero_vote),
                ],
            ),
            ReviewQuorumDecision::Insufficient
        );
    }
    assert_eq!(
        evaluate_review_quorum(
            &policy(),
            &[
                result(
                    "executor-opencode",
                    "primary",
                    "pending",
                    ReviewResultState::Pending,
                ),
                result(
                    "executor-pi",
                    "secondary",
                    "formal-pass",
                    ReviewResultState::FormalPass,
                ),
                result(
                    "executor-dsh",
                    "nongate",
                    "nongate-pass",
                    ReviewResultState::NongatePass,
                ),
            ],
        ),
        ReviewQuorumDecision::Insufficient
    );
}

#[test]
fn findings_are_monotonic_and_duplicate_voices_are_invalid() {
    for finding in [ReviewResultState::FormalFail, ReviewResultState::Blocked] {
        assert_eq!(
            evaluate_review_quorum(
                &policy(),
                &[
                    result(
                        "executor-pi",
                        "secondary",
                        "formal-pass",
                        ReviewResultState::FormalPass,
                    ),
                    result(
                        "executor-dsh",
                        "nongate",
                        "nongate-pass",
                        ReviewResultState::NongatePass,
                    ),
                    result("executor-opencode", "primary", "finding", finding),
                ],
            ),
            ReviewQuorumDecision::BlockedByFinding
        );
    }
    assert_eq!(
        evaluate_review_quorum(
            &policy(),
            &[
                result(
                    "executor-dsh",
                    "nongate",
                    "delivery-1",
                    ReviewResultState::NongatePass,
                ),
                result(
                    "executor-dsh",
                    "secondary",
                    "delivery-2",
                    ReviewResultState::FormalPass,
                ),
            ],
        ),
        ReviewQuorumDecision::Invalid
    );
}

#[test]
fn quorum_events_extend_the_catalog_without_rewriting_the_frozen_lifecycle_seam() {
    let quorum_kinds = [
        "ReviewFallbackSelected",
        "NongateReviewDelivered",
        "ReviewSeatSubstituted",
    ];
    for kind in quorum_kinds {
        assert!(orch_core::known_event_kinds().contains(&kind));
        assert!(orch_core::is_known_event_kind(kind));
        assert!(!orch_core::REVIEW_LIFECYCLE_EVENT_KINDS.contains(&kind));
    }

    let mut events = vec![orch_core::EventRecord {
        event_id: "catalog-0".to_string(),
        ts: "2026-08-23T00:00:00Z".to_string(),
        actor: "runtime:orch".to_string(),
        kind: "DispatchIssued".to_string(),
        task_id: Some("B303".to_string()),
        round: Some("r79".to_string()),
        payload: Some(serde_json::json!({"attemptId": ATTEMPT})),
        extra: Default::default(),
    }];
    events.extend(
        quorum_kinds
            .into_iter()
            .enumerate()
            .map(|(index, kind)| orch_core::EventRecord {
                event_id: format!("catalog-{}", index + 1),
                ts: format!("2026-08-23T00:00:0{}Z", index + 1),
                actor: "runtime:orch".to_string(),
                kind: kind.to_string(),
                task_id: Some("B303".to_string()),
                round: Some("r79".to_string()),
                payload: Some(serde_json::json!({"attemptId": ATTEMPT})),
                extra: Default::default(),
            }),
    );
    let projection = orch_core::fold(&events);
    assert!(projection.unknown_kinds.is_empty());
    assert_eq!(
        projection.tasks["B303"].state,
        Some(orch_core::TaskState::Dispatched)
    );
}

const RT_ROUND: &str = "r9997";
const RT_TASK: &str = "B9303";
const RT_ATTEMPT: &str = "B9303-A0001";

const RT_MODE: &str = r#"apiVersion: orch/v1alpha1
kind: ModeConfig
metadata: {name: b303-quorum}
agents:
  executor: {adapter: test, tier: none}
  verifier: {adapter: root-manual, tier: none}
hitl: {planSignoff: required, mergeGate: auto}
verification: {mode: root-manual-fixed-head}
liveness: {monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}
scheduling:
  allowedAgents: [executor-desktop, executor-opencode, executor-fallback, executor-pi, executor-dsh]
  capacities:
    executor-desktop: {agent: 2, quota: 2, roles: [implement]}
    executor-opencode: {agent: 2, quota: 2, roles: [primary-review]}
    executor-fallback: {agent: 2, quota: 2, roles: [primary-review]}
    executor-pi: {agent: 2, quota: 2, roles: [secondary-review]}
    executor-dsh: {agent: 2, quota: 2, roles: [nongate-review]}
budgets: {round: {maxUsd: 1, wallMinutes: 60, maxModelWakes: 20}}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
"#;

const RT_BINDING: &str = r#"apiVersion: orch/v1alpha1
kind: ProjectBinding
metadata: {name: b303-quorum, bindingRevision: 1}
project: {root: ".", primaryBranch: main, ecosystems: [test]}
workspace: {defaultIsolation: git-worktree, worktreeRoot: .worktrees}
commands:
  postGate: {argv: ["/bin/sh", "-c", "exit 0"], timeoutSeconds: 30}
gates: {fast: [postGate], merge: [postGate]}
scope: {protectedPaths: ["coordination/**"]}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
verification: {contractModes: [verify-only], independentVerifier: required-for-write}
"#;

const RT_REGISTRY: &str = r#"apiVersion: orch/v1alpha1
kind: AgentRegistry
agents:
  executor-desktop:
    injectable: true
    sessionId: b303-implementer
    provider: synthetic
    model: synthetic-implementer
    effort: high
    observation: {source: fixture, policy: advisory}
    roles: [implement]
    wake: {argv: ["/bin/sh", "-c", "exit 0", "{message}"]}
  executor-opencode:
    injectable: true
    sessionId: b303-primary
    provider: synthetic
    model: synthetic-primary
    effort: high
    observation: {source: fixture, policy: advisory}
    roles: [primary-review]
    wake: {argv: ["/bin/sh", "-c", "exit 0", "{message}"]}
  executor-pi:
    injectable: true
    sessionId: b303-secondary
    provider: synthetic
    model: synthetic-secondary
    effort: xhigh
    observation: {source: fixture, policy: advisory}
    roles: [secondary-review]
    wake: {argv: ["/bin/sh", "-c", "exit 0", "{message}"]}
  executor-fallback:
    injectable: true
    sessionId: b303-fallback
    provider: synthetic
    model: synthetic-fallback
    effort: high
    observation: {source: fixture, policy: advisory}
    roles: [primary-review]
    wake: {argv: ["/bin/sh", "-c", "exit 0", "{message}"]}
  executor-dsh:
    injectable: true
    sessionId: b303-nongate
    provider: synthetic
    model: synthetic-nongate
    effort: max
    observation: {source: fixture, policy: advisory}
    roles: [nongate-review]
    wake: {argv: ["/bin/sh", "-c", "exit 0", "{message}"]}
"#;

struct RootScene {
    root: PathBuf,
    head: String,
    main: String,
}

impl Drop for RootScene {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn source_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .unwrap()
        .to_path_buf()
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn commit(root: &Path, message: &str) {
    git(
        root,
        &[
            "-c",
            "user.name=B303",
            "-c",
            "user.email=b303@example.invalid",
            "commit",
            "-q",
            "-m",
            message,
        ],
    );
}

fn review_bytes(role: &str, agent: &str, verdict: &str, head: &str) -> String {
    format!(
        "---\ntaskId: {RT_TASK}\nround: {RT_ROUND}\nattemptId: {RT_ATTEMPT}\nrole: {role}\nreviewer: {agent}\nverdict: {verdict}\nreviewedHead: {head}\n---\nsubstantive {role} {verdict}\n"
    )
}

fn wake_and_request(
    role: &str,
    agent: &str,
    head: &str,
) -> (String, Vec<orch_core::EventRecord>) {
    let wake_id = format!("wake-{role}-{agent}");
    let wake = orch_host::ledger::event(
        "WakeIssued",
        "runtime:orch",
        Some(RT_TASK),
        Some(RT_ROUND),
        serde_json::json!({
            "wakeId": wake_id,
            "attemptId": RT_ATTEMPT,
            "agent": agent,
        }),
    );
    let request = orch_host::ledger::event(
        "ReviewRequested",
        "runtime:orch",
        Some(RT_TASK),
        Some(RT_ROUND),
        serde_json::json!({
            "attemptId": RT_ATTEMPT,
            "role": role,
            "agent": agent,
            "wakeId": wake_id,
            "reviewedHead": head,
            "deadlineSecs": 1800,
            "requestedAt": wake.ts,
        }),
    );
    (wake_id, vec![wake, request])
}

fn task_card() -> String {
    format!(
        "---\n\
taskId: {RT_TASK}\n\
round: {RT_ROUND}\n\
agent: executor-desktop\n\
seedProtocol: verify-only\n\
complexity: light\n\
capabilities: [implement]\n\
entryPoints: [feature.txt]\n\
writeSet: [feature.txt]\n\
frozenPaths: [coordination/**]\n\
gates: {{fast: [postGate]}}\n\
budgets: {{wallMinutes: 30}}\n\
requiredReviews:\n\
  - {{role: primary, agent: executor-opencode}}\n\
  - {{role: secondary, agent: executor-pi}}\n\
reviewFallbacks:\n\
  - {{role: primary, fallbackAgent: executor-fallback}}\n\
nongateSeats:\n\
  - {{agent: executor-dsh, preset: minimal}}\n\
reviewQuorum: {{minimumSubstantive: 2, nongateMaySubstituteFailedFormal: true, minimumNongatePassForSubstitution: 1}}\n\
requiredEvidence: [review-quorum-fixture]\n\
---\n\
# B303 root quorum fixture\n"
    )
}

fn build_root_scene(nongate_verdict: &str, fallback_terminal_seen: bool) -> RootScene {
    let root = orch_host::util::test_scratch_dir(&format!(
        "b303-root-{}",
        nongate_verdict.to_ascii_lowercase()
    ));
    git(&root, &["init", "-q"]);
    fs::write(root.join("README.md"), "base\n").unwrap();
    fs::write(
        root.join(".gitignore"),
        ".worktrees/\n.cowork-temp/\ncoordination/runtime/\n",
    )
    .unwrap();
    git(&root, &["add", "README.md", ".gitignore"]);
    commit(&root, "base");
    git(&root, &["branch", "-M", "main"]);
    git(&root, &["checkout", "-q", "-b", "task/B9303"]);
    fs::write(root.join("feature.txt"), "candidate\n").unwrap();
    git(&root, &["add", "feature.txt"]);
    commit(&root, "candidate");
    let head = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["checkout", "-q", "main"]);
    fs::create_dir_all(root.join(".worktrees")).unwrap();
    git(
        &root,
        &["worktree", "add", "-q", ".worktrees/B9303", "task/B9303"],
    );

    for rel in [
        "coordination/runtime/ledger-wal",
        "coordination/modes",
        "coordination/rounds/r9997/tasks",
        "coordination/rounds/r9997/reviews",
        "coordination/rounds/r9997/evidence",
        "coordination/runtime/review-inbox/r9997",
        "coordination/runtime/nongate-inbox/r9997",
        ".worktrees/review-B9303-A0001-nongate-executor-dsh-g01",
    ] {
        fs::create_dir_all(root.join(rel)).unwrap();
    }
    fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r9997\n").unwrap();
    fs::write(root.join("coordination/modes/b303-quorum.yaml"), RT_MODE).unwrap();
    fs::write(root.join("coordination/PROJECT-BINDING.yaml"), RT_BINDING).unwrap();
    fs::write(root.join("coordination/agents.yaml"), RT_REGISTRY).unwrap();
    fs::copy(
        source_root().join("coordination/harnesses.yaml"),
        root.join("coordination/harnesses.yaml"),
    )
    .unwrap();
    fs::write(
        root.join("coordination/rounds/r9997/MODE-REF.yaml"),
        "modeRef: b303-quorum\n",
    )
    .unwrap();
    fs::write(
        root.join("coordination/rounds/r9997/tasks/B9303.md"),
        task_card(),
    )
    .unwrap();
    fs::write(root.join("coordination/BOARD.md"), "# fixture\n").unwrap();

    orch_host::plan::run_plan(&root).unwrap();
    orch_host::round::run_sign_off(&root, Some("B303 fixture sign-off")).unwrap();
    let base = git(&root, &["rev-parse", "main"]);
    let dispatch = orch_host::ledger::event(
        "DispatchIssued",
        "runtime:orch",
        Some(RT_TASK),
        Some(RT_ROUND),
        serde_json::json!({
            "attemptId": RT_ATTEMPT,
            "attemptNo": 1,
            "agent": "executor-desktop",
            "baseSha": base,
            "goPath": "coordination/rounds/r9997/dispatch/executor-desktop/GO-B9303-A0001.md",
        }),
    );
    let gate_receipt = orch_host::ledger::event(
        "CollectGateSuccessReceipt",
        "runtime:orch",
        Some(RT_TASK),
        Some(RT_ROUND),
        serde_json::json!({
            "actionId": "collect-B9303-A0001",
            "attemptId": RT_ATTEMPT,
            "attemptNo": 1,
            "agent": "executor-desktop",
            "baseSha": base,
            "goPath": "coordination/rounds/r9997/dispatch/executor-desktop/GO-B9303-A0001.md",
            "branchSha": head,
        }),
    );
    let collect = orch_host::ledger::event(
        "ReportCollectCompleted",
        "runtime:orch",
        Some(RT_TASK),
        Some(RT_ROUND),
        serde_json::json!({
            "actionId": "collect-B9303-A0001",
            "attemptId": RT_ATTEMPT,
            "attemptNo": 1,
            "agent": "executor-desktop",
            "baseSha": base,
            "goPath": "coordination/rounds/r9997/dispatch/executor-desktop/GO-B9303-A0001.md",
            "branchSha": head,
            "gateReceipt": gate_receipt.event_id,
        }),
    );
    let (primary_wake, mut primary_events) =
        wake_and_request("primary", "executor-opencode", &head);
    let (_, mut secondary_events) = wake_and_request("secondary", "executor-pi", &head);
    let primary_terminal = orch_host::ledger::event(
        "ManagedWakeTerminated",
        "runtime:orch",
        Some(RT_TASK),
        Some(RT_ROUND),
        serde_json::json!({
            "wakeId": primary_wake,
            "agent": "executor-opencode",
            "outcomeClass": "TruncatedNoTerminal",
            "managedScopeTerminated": true,
        }),
    );
    let primary_terminal_id = primary_terminal.event_id.clone();
    let (fallback_wake_id, mut fallback_events) =
        wake_and_request("primary", "executor-fallback", &head);
    let selection = orch_host::ledger::event(
        "ReviewFallbackSelected",
        "runtime:orch",
        Some(RT_TASK),
        Some(RT_ROUND),
        serde_json::json!({
            "attemptId": RT_ATTEMPT,
            "role": "primary",
            "fromAgent": "executor-opencode",
            "toAgent": "executor-fallback",
            "reviewedHead": head,
            "sourceWakeId": primary_wake,
            "terminalEventId": primary_terminal_id,
            "targetWakeId": fallback_wake_id,
        }),
    );
    let selection_id = selection.event_id.clone();
    for event in &mut fallback_events {
        event.payload.as_mut().unwrap()["reviewFallbackEventId"] =
            serde_json::json!(selection_id);
    }
    let fallback_terminal = orch_host::ledger::event(
        "ManagedWakeTerminated",
        "runtime:orch",
        Some(RT_TASK),
        Some(RT_ROUND),
        serde_json::json!({
            "wakeId": fallback_wake_id,
            "agent": "executor-fallback",
            "outcomeClass": "TruncatedNoTerminal",
            "managedScopeTerminated": true,
        }),
    );
    let secondary_delivery = orch_host::ledger::event(
        "ReviewDelivered",
        "runtime:orch",
        Some(RT_TASK),
        Some(RT_ROUND),
        serde_json::json!({
            "attemptId": RT_ATTEMPT,
            "role": "secondary",
            "agent": "executor-pi",
            "bodyLen": "substantive secondary PASS".len(),
        }),
    );
    let nongate_wake_id = "wake-nongate-dsh";
    let lease = orch_host::ledger::event(
        "WorkspaceLeased",
        "runtime:orch",
        Some(RT_TASK),
        Some(RT_ROUND),
        serde_json::json!({
            "agent": "executor-dsh",
            "attemptId": RT_ATTEMPT,
            "generation": 1,
            "paths": {
                "target": "orch/target/review-B9303-A0001-nongate-executor-dsh-g01",
                "worktree": ".worktrees/review-B9303-A0001-nongate-executor-dsh-g01",
            },
            "reviewedHead": head,
            "role": "nongate",
            "siteId": "B9303-nongate-executor-dsh-g01",
            "wakeId": nongate_wake_id,
        }),
    );
    let nongate_wake = orch_host::ledger::event(
        "WakeIssued",
        "runtime:orch",
        Some(RT_TASK),
        Some(RT_ROUND),
        serde_json::json!({
            "agent": "executor-dsh",
            "attemptId": RT_ATTEMPT,
            "wakeId": nongate_wake_id,
        }),
    );
    let lease_id = lease.event_id.clone();
    let nongate_wake_event_id = nongate_wake.event_id.clone();
    let mut events = vec![dispatch, gate_receipt, collect];
    events.append(&mut primary_events);
    events.push(primary_terminal);
    events.push(selection);
    events.append(&mut fallback_events);
    if fallback_terminal_seen {
        events.push(fallback_terminal);
    }
    events.append(&mut secondary_events);
    events.push(secondary_delivery);
    events.push(lease);
    events.push(nongate_wake);
    orch_host::ledger::append(&root, RT_ROUND, &events).unwrap();

    fs::write(
        root.join(format!(
            "coordination/rounds/{RT_ROUND}/reviews/{RT_ATTEMPT}-secondary-executor-pi.md"
        )),
        review_bytes("secondary", "executor-pi", "PASS", &head),
    )
    .unwrap();
    fs::write(
        root.join(format!(
            "coordination/runtime/review-inbox/{RT_ROUND}/{RT_ATTEMPT}-nongate-executor-dsh.md"
        )),
        review_bytes("nongate", "executor-dsh", nongate_verdict, &head),
    )
    .unwrap();
    fs::write(
        root.join(format!(
            "coordination/runtime/nongate-inbox/{RT_ROUND}/{RT_ATTEMPT}-executor-dsh.json"
        )),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schemaVersion": 1,
            "round": RT_ROUND,
            "attemptId": RT_ATTEMPT,
            "agent": "executor-dsh",
            "fixedHead": head,
            "wakeId": nongate_wake_id,
            "wakeIssuedEventId": nongate_wake_event_id,
            "workspaceLeasedEventId": lease_id,
            "invocation": {
                "provider": "synthetic",
                "model": "synthetic-nongate",
                "effort": "max",
                "preset": "minimal",
                "cwd": root.join(".worktrees/review-B9303-A0001-nongate-executor-dsh-g01"),
            },
            "state": "answered",
            "terminalReason": null,
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        root.join(format!(
            "coordination/rounds/{RT_ROUND}/evidence/{RT_TASK}-review-quorum-fixture.json"
        )),
        "{\"fixture\":true}\n",
    )
    .unwrap();

    assert_eq!(orch_host::wake::reconcile_review_transitions(&root).unwrap(), 1);
    assert!(orch_host::verify::nongate_attempt_receipt_warnings(
        &root,
        RT_TASK,
        RT_ATTEMPT,
        &head,
    )
    .is_empty());
    git(&root, &["add", "coordination"]);
    commit(&root, "signed review evidence");
    let main = git(&root, &["rev-parse", "main"]);
    RootScene { root, head, main }
}

#[test]
fn real_root_substitutes_one_formal_channel_with_one_durable_nongate_voice() {
    let scene = build_root_scene("PASS", true);
    let outcome = orch_host::verify::run_root_verdict(
        &scene.root,
        RT_TASK,
        RT_ATTEMPT,
        &scene.head,
        &scene.main,
        orch_host::verify::RootVerdict::Pass,
        None,
        false,
    )
    .unwrap();
    assert!(outcome.appended);
    let ledger = orch_core::read_ledger(
        &scene
            .root
            .join(format!("coordination/rounds/{RT_ROUND}/events.jsonl")),
    )
    .unwrap();
    let substitution = ledger
        .events
        .iter()
        .position(|event| event.kind == "ReviewSeatSubstituted")
        .unwrap();
    let verdict = ledger
        .events
        .iter()
        .position(|event| event.kind == "VerdictIssued")
        .unwrap();
    assert_eq!(substitution + 1, verdict);
    let payload: orch_host::verify::RootVerdictPayload = serde_json::from_value(
        ledger.events[verdict].payload.clone().unwrap(),
    )
    .unwrap();
    assert_eq!(payload.reviews.len(), 2);
    let nongate = payload
        .reviews
        .iter()
        .find(|binding| binding.role == "nongate")
        .unwrap();
    assert_eq!(nongate.substituted_role.as_deref(), Some("primary"));
    assert_eq!(
        nongate.substituted_agent.as_deref(),
        Some("executor-fallback")
    );
    let authorization = orch_host::verify::validate_root_merge_authorization(
        &scene.root,
        RT_ROUND,
        RT_TASK,
        &ledger.events,
    )
    .unwrap();
    assert_eq!(authorization.head_sha, scene.head);
    orch_host::close::run_merge(&scene.root, RT_TASK).unwrap();
    let recorded = orch_core::read_ledger(
        &scene
            .root
            .join(format!("coordination/rounds/{RT_ROUND}/events.jsonl")),
    )
    .unwrap();
    orch_host::verify::validate_archived_record_chain(
        &scene.root,
        RT_ROUND,
        RT_TASK,
        &recorded.events,
    )
    .unwrap();
    let receipt_path = scene.root.join(format!(
        "coordination/runtime/nongate-inbox/{RT_ROUND}/{RT_ATTEMPT}-executor-dsh.json"
    ));
    let receipt_bytes = fs::read(&receipt_path).unwrap();
    fs::remove_file(&receipt_path).unwrap();
    assert!(orch_host::verify::validate_archived_record_chain(
        &scene.root,
        RT_ROUND,
        RT_TASK,
        &recorded.events,
    )
    .is_err());
    fs::write(&receipt_path, receipt_bytes).unwrap();
    orch_host::verify::validate_archived_record_chain(
        &scene.root,
        RT_ROUND,
        RT_TASK,
        &recorded.events,
    )
    .unwrap();
    let removed_review = format!(
        "coordination/rounds/{RT_ROUND}/reviews/{RT_ATTEMPT}-nongate-executor-dsh.md"
    );
    fs::remove_file(scene.root.join(&removed_review)).unwrap();
    git(&scene.root, &["add", "-u", &removed_review]);
    commit(&scene.root, "delete bound review artifact");
    let error = orch_host::verify::validate_archived_record_chain(
        &scene.root,
        RT_ROUND,
        RT_TASK,
        &recorded.events,
    )
    .unwrap_err();
    assert!(error.to_string().contains("后来从 current main 删除"), "{error:#}");
}

#[test]
fn real_root_never_lets_a_nongate_finding_be_overwritten_by_passes() {
    let scene = build_root_scene("FAIL", true);
    let error = orch_host::verify::run_root_verdict(
        &scene.root,
        RT_TASK,
        RT_ATTEMPT,
        &scene.head,
        &scene.main,
        orch_host::verify::RootVerdict::Pass,
        None,
        true,
    )
    .unwrap_err();
    assert!(error.to_string().contains("BlockedByFinding"), "{error:#}");
}

#[test]
fn real_root_refuses_nongate_substitution_before_signed_fallback_exhaustion() {
    let scene = build_root_scene("PASS", false);
    let error = orch_host::verify::run_root_verdict(
        &scene.root,
        RT_TASK,
        RT_ATTEMPT,
        &scene.head,
        &scene.main,
        orch_host::verify::RootVerdict::Pass,
        None,
        true,
    )
    .unwrap_err();
    assert!(error.to_string().contains("Insufficient"), "{error:#}");
}

#[test]
fn real_root_keeps_a_late_original_formal_finding_monotonic() {
    let mut scene = build_root_scene("PASS", true);
    let rel = format!(
        "coordination/rounds/{RT_ROUND}/reviews/{RT_ATTEMPT}-primary-executor-opencode.md"
    );
    fs::write(
        scene.root.join(&rel),
        review_bytes("primary", "executor-opencode", "FAIL", &scene.head),
    )
    .unwrap();
    git(&scene.root, &["add", &rel]);
    commit(&scene.root, "late original finding");
    scene.main = git(&scene.root, &["rev-parse", "main"]);
    let error = orch_host::verify::run_root_verdict(
        &scene.root,
        RT_TASK,
        RT_ATTEMPT,
        &scene.head,
        &scene.main,
        orch_host::verify::RootVerdict::Pass,
        None,
        true,
    )
    .unwrap_err();
    assert!(error.to_string().contains("monotonic FAIL"), "{error:#}");
}

#[test]
fn real_root_rejects_a_duplicate_fallback_selection_from_repeated_ticks() {
    let mut scene = build_root_scene("PASS", true);
    let ledger_path = scene
        .root
        .join(format!("coordination/rounds/{RT_ROUND}/events.jsonl"));
    let ledger = orch_core::read_ledger(&ledger_path).unwrap();
    let original = ledger
        .events
        .iter()
        .find(|event| event.kind == "ReviewFallbackSelected")
        .unwrap();
    let duplicate = orch_host::ledger::event(
        "ReviewFallbackSelected",
        "runtime:orch",
        Some(RT_TASK),
        Some(RT_ROUND),
        original.payload.clone().unwrap(),
    );
    orch_host::ledger::append(&scene.root, RT_ROUND, &[duplicate]).unwrap();
    git(
        &scene.root,
        &["add", &format!("coordination/rounds/{RT_ROUND}/events.jsonl")],
    );
    commit(&scene.root, "duplicate fallback selection");
    scene.main = git(&scene.root, &["rev-parse", "main"]);
    let error = orch_host::verify::run_root_verdict(
        &scene.root,
        RT_TASK,
        RT_ATTEMPT,
        &scene.head,
        &scene.main,
        orch_host::verify::RootVerdict::Pass,
        None,
        true,
    )
    .unwrap_err();
    assert!(error.to_string().contains("重复 ReviewFallbackSelected"), "{error:#}");
}
