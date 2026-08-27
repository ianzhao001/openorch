use orch_host::ledger::{self, RuntimeEventPayloadV1};
use orch_host::plan::{self, RuntimePolicyStateV1};
use orch_host::wake::{
    evaluate_review_panel_v1, ReviewPanelDecisionV1, ReviewPanelPolicyV1,
    ReviewPanelSeatStateV1, ReviewPanelSeatV1,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use std::process::Command;

fn seat(id: &str, role: &str, primary: bool, state: ReviewPanelSeatStateV1) -> ReviewPanelSeatV1 {
    ReviewPanelSeatV1 {
        seat_id: id.to_string(),
        generation: 1,
        role: role.to_string(),
        agent: format!("executor-{id}"),
        primary_lineage: primary,
        retry_eligible: true,
        state,
    }
}

fn policy() -> ReviewPanelPolicyV1 {
    ReviewPanelPolicyV1 {
        minimum_passes: 2,
        require_primary_pass: true,
        maximum_business_retries: 1,
        nongate_substitutes_secondary_only: true,
    }
}

#[test]
fn panel_replay_requires_terminal_quorum_primary_and_unique_voices() {
    let pending = vec![
        seat("oc", "primary", true, ReviewPanelSeatStateV1::Pass),
        seat("pi", "secondary", false, ReviewPanelSeatStateV1::Pass),
        seat("agy", "nongate", false, ReviewPanelSeatStateV1::Pending),
    ];
    assert_eq!(
        evaluate_review_panel_v1(&policy(), &pending, 0, 0),
        ReviewPanelDecisionV1::Awaiting
    );
    let early_finding = vec![
        seat("oc", "primary", true, ReviewPanelSeatStateV1::Fail),
        seat("pi", "secondary", false, ReviewPanelSeatStateV1::Pending),
        seat("agy", "nongate", false, ReviewPanelSeatStateV1::Pending),
    ];
    assert_eq!(
        evaluate_review_panel_v1(&policy(), &early_finding, 0, 0),
        ReviewPanelDecisionV1::Awaiting,
        "panel waits for all routed seats before the monotonic veto closes"
    );
    let complete = vec![
        seat("oc", "primary", true, ReviewPanelSeatStateV1::Pass),
        seat("pi", "secondary", false, ReviewPanelSeatStateV1::Pass),
        seat(
            "agy",
            "nongate",
            false,
            ReviewPanelSeatStateV1::SystemInvalid,
        ),
    ];
    assert_eq!(
        evaluate_review_panel_v1(&policy(), &complete, 0, 0),
        ReviewPanelDecisionV1::Pass
    );
    let mut duplicate = complete;
    duplicate[2].agent = duplicate[1].agent.clone();
    assert_eq!(
        evaluate_review_panel_v1(&policy(), &duplicate, 0, 0),
        ReviewPanelDecisionV1::Invalid
    );

    let substituted = vec![
        seat("oc", "primary", true, ReviewPanelSeatStateV1::Pass),
        seat(
            "pi",
            "secondary",
            false,
            ReviewPanelSeatStateV1::SystemInvalid,
        ),
        seat("agy", "nongate", false, ReviewPanelSeatStateV1::Pass),
    ];
    assert_eq!(
        evaluate_review_panel_v1(&policy(), &substituted, 0, 0),
        ReviewPanelDecisionV1::Pass
    );

    let nongate_finding_does_not_claim_formal_veto = vec![
        seat("oc", "primary", true, ReviewPanelSeatStateV1::Pass),
        seat("pi", "secondary", false, ReviewPanelSeatStateV1::Pass),
        seat("agy", "nongate", false, ReviewPanelSeatStateV1::Fail),
    ];
    assert_eq!(
        evaluate_review_panel_v1(
            &policy(),
            &nongate_finding_does_not_claim_formal_veto,
            0,
            0,
        ),
        ReviewPanelDecisionV1::Pass
    );
}

#[test]
fn runtime_v1_events_are_terminally_forbidden_after_round_close() {
    let closed = ledger::event(
        "RoundClosed",
        "runtime:orch",
        None,
        Some("r81"),
        serde_json::json!({"status": "done"}),
    );
    let activation = ledger::runtime_event_v1(
        "r81",
        None,
        RuntimeEventPayloadV1::RuntimePolicyActivated(
            ledger::RuntimePolicyActivatedPayloadV1 {
                schema_version: 1,
                policy: "review-pool-v1".to_string(),
                owner_task: "B310".to_string(),
                owner_recorded_event_id: "owner-recorded".to_string(),
                owner_merge_sha: "a".repeat(40),
                binding_sha256: "b".repeat(64),
                policy_sha256: "c".repeat(64),
                activated_at_main_sha: "d".repeat(40),
            },
        ),
    )
    .unwrap();
    assert!(ledger::validate_runtime_event_history_v1(&[closed, activation], "r81").is_err());
}

#[test]
fn policy_transition_idle_authority_is_replayed_for_attempts_and_merge_barriers() {
    let mut recorded = ledger::event(
        "TaskRecorded",
        "runtime:orch",
        Some("B310"),
        Some("r81"),
        serde_json::json!({"postMergeGates": "all-green"}),
    );
    recorded.event_id = "owner-recorded".to_string();
    let owner_merge = ledger::event(
        "MergeExecuted",
        "reviewer:orch-runtime",
        Some("B310"),
        Some("r81"),
        serde_json::json!({"mergeSha": "a".repeat(40), "policy": "no-ff"}),
    );
    let activation = || {
        ledger::runtime_event_v1(
            "r81",
            None,
            RuntimeEventPayloadV1::RuntimePolicyActivated(
                ledger::RuntimePolicyActivatedPayloadV1 {
                    schema_version: 1,
                    policy: "review-pool-v1".to_string(),
                    owner_task: "B310".to_string(),
                    owner_recorded_event_id: "owner-recorded".to_string(),
                    owner_merge_sha: "a".repeat(40),
                    binding_sha256: "b".repeat(64),
                    policy_sha256: "c".repeat(64),
                    activated_at_main_sha: "d".repeat(40),
                },
            ),
        )
        .unwrap()
    };

    let inflight = ledger::event(
        "DispatchIssued",
        "runtime:orch",
        Some("B900"),
        Some("r81"),
        serde_json::json!({
            "attemptId": "B900-A0001",
            "attemptNo": 1,
            "agent": "executor-desktop",
            "baseSha": "d".repeat(40),
            "goPath": "coordination/rounds/r81/dispatch/executor-desktop/GO-B900-A0001.md",
        }),
    );
    let error = ledger::validate_runtime_event_history_v1(
        &[
            owner_merge.clone(),
            recorded.clone(),
            inflight,
            activation(),
        ],
        "r81",
    )
    .unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains("在飞 attempt"), "{error}");

    let merge_started = ledger::event(
        "MergeStarted",
        "runtime:orch",
        Some("B901"),
        Some("r81"),
        serde_json::json!({
            "attemptId": "B901-A0001",
            "attemptNo": 1,
            "collectCompletedEventId": "collect-one",
            "headSha": "e".repeat(40),
            "mainHeadSha": "f".repeat(40),
            "verdictEventId": "verdict-one",
        }),
    );
    let error = ledger::validate_runtime_event_history_v1(
        &[owner_merge, recorded, merge_started, activation()],
        "r81",
    )
    .unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains("active merge barrier"), "{error}");
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
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn commit(root: &Path, message: &str) -> String {
    git(root, &["add", "."]);
    git(
        root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch-test@example.invalid",
            "commit",
            "-q",
            "-m",
            message,
        ],
    );
    git(root, &["rev-parse", "HEAD"])
}

fn write_events(root: &Path, events: &[orch_core::EventRecord]) {
    let mut bytes = Vec::new();
    for event in events {
        serde_json::to_writer(&mut bytes, event).unwrap();
        bytes.push(b'\n');
    }
    fs::write(root.join("coordination/rounds/r81/events.jsonl"), &bytes).unwrap();
    let wal = root.join("coordination/runtime/ledger-wal/r81.jsonl");
    if wal.parent().unwrap().is_dir() {
        fs::write(wal, bytes).unwrap();
    }
}

#[test]
fn pre_policy_binding_short_circuits_before_modern_ir_digest_is_required() {
    let root = orch_host::util::test_scratch_dir("b310-pre-policy-round-ir");
    git(&root, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(root.join("coordination/rounds/r48")).unwrap();
    fs::write(
        root.join("coordination/PROJECT-BINDING.yaml"),
        "schemaVersion: 1\nproject: legacy-pre-policy\n",
    )
    .unwrap();
    // This is the historical r48 shape: a valid RoundIr under today's serde
    // defaults, but no sourceBindings.bindingSha256 because that contract did
    // not exist yet.  Absence of runtimePolicies must select compatibility
    // before the modern digest is required.
    fs::write(
        root.join("coordination/rounds/r48/ROUND-IR.yaml"),
        "round: r48\nrevision: 1\npolicy:\n  pushPolicy: forbidden\n  mergePolicy: ff-only-else-no-ff\n  autoMergeOnPass: true\ntasks: []\n",
    )
    .unwrap();
    let base = commit(&root, "pre-policy signed fixture");

    let error = plan::resolve_runtime_policy_at(&root, "r48", "review-pool-v1", &base)
        .unwrap_err();
    let detail = format!("{error:#}");
    assert!(
        detail.contains("缺 runtimePolicies envelope"),
        "compatibility discriminator must be the first refusal: {detail}"
    );
    assert!(
        !detail.contains("bindingSha256 漂移"),
        "a pre-policy IR must not be forced through a future digest: {detail}"
    );
    fs::remove_dir_all(root).ok();
}

#[test]
fn policy_mode_replays_from_each_dispatch_base_without_future_reselection() {
    let root = orch_host::util::test_scratch_dir("b310-policy-as-of");
    git(&root, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(root.join("coordination/rounds/r81")).unwrap();
    let binding = r#"runtimePolicies:
  schemaVersion: 1
  policies:
    review-pool-v1:
      schemaVersion: 1
      ownerTask: B310
      initialState: dormant
      scope: round
      minimumPasses: 2
      requirePrimaryPass: true
      initialSeatCount: 3
      minimumGateEligible: 2
      maximumBusinessRetries: 1
      nongateSubstitutesRole: secondary
      candidates:
        - {agent: executor-opencode, role: primary, lineage: primary, retryEligible: true}
        - {agent: executor-dsh, role: primary, lineage: primary, retryEligible: true, fallbackFor: executor-opencode}
        - {agent: executor-pi, role: secondary, lineage: secondary, retryEligible: true}
        - {agent: executor-antigravity, role: nongate, lineage: nongate, retryEligible: false}
"#;
    fs::write(root.join("coordination/PROJECT-BINDING.yaml"), binding).unwrap();
    let binding_sha = hex::encode(Sha256::digest(binding.as_bytes()));
    let ir_text = format!(
        "schemaVersion: 2\nround: r81\nrevision: 1\nsourceBindings:\n  bindingSha256: {binding_sha}\ntasks: []\n"
    );
    fs::write(root.join("coordination/rounds/r81/ROUND-IR.yaml"), &ir_text).unwrap();
    let ir: plan::RoundIr = serde_yaml::from_str(&ir_text).unwrap();
    let digest = plan::validation_digest(&ir);
    let mut events = vec![
        ledger::event(
            "TaskValidated",
            "runtime:orch",
            None,
            Some("r81"),
            plan::task_validated_payload(1, &digest),
        ),
        ledger::event(
            "PlanSignedOff",
            "user",
            None,
            Some("r81"),
            plan::plan_signed_off_payload("test", 1, &digest).unwrap(),
        ),
    ];
    write_events(&root, &events);
    let first = commit(&root, "signed policy fixture");

    let mut merge = ledger::event(
        "MergeExecuted",
        "reviewer:orch-runtime",
        Some("B310"),
        Some("r81"),
        serde_json::json!({"mergeSha": first, "policy": "no-ff"}),
    );
    merge.event_id = "owner-merge".to_string();
    let mut recorded = ledger::event(
        "TaskRecorded",
        "runtime:orch",
        Some("B310"),
        Some("r81"),
        serde_json::json!({"postMergeGates": "all-green"}),
    );
    recorded.event_id = "owner-recorded".to_string();
    events.extend([merge, recorded]);
    write_events(&root, &events);
    let dormant_base = commit(&root, "owner recorded");
    let dormant = plan::resolve_runtime_policy_at(&root, "r81", "review-pool-v1", &dormant_base)
        .unwrap();
    assert_eq!(dormant.state, RuntimePolicyStateV1::Dormant);

    let activation = ledger::runtime_event_v1(
        "r81",
        None,
        RuntimeEventPayloadV1::RuntimePolicyActivated(
            ledger::RuntimePolicyActivatedPayloadV1 {
                schema_version: 1,
                policy: "review-pool-v1".to_string(),
                owner_task: "B310".to_string(),
                owner_recorded_event_id: "owner-recorded".to_string(),
                owner_merge_sha: first,
                binding_sha256: dormant.binding_sha256.clone(),
                policy_sha256: dormant.policy_sha256.clone(),
                activated_at_main_sha: dormant_base.clone(),
            },
        ),
    )
    .unwrap();
    let activation_id = activation.event_id.clone();
    events.push(activation);
    write_events(&root, &events);
    let active_base = commit(&root, "activate policy");
    assert_eq!(
        plan::resolve_runtime_policy_at(&root, "r81", "review-pool-v1", &active_base)
            .unwrap()
            .state,
        RuntimePolicyStateV1::Active
    );

    let deactivation = ledger::runtime_event_v1(
        "r81",
        None,
        RuntimeEventPayloadV1::RuntimePolicyDeactivated(
            ledger::RuntimePolicyDeactivatedPayloadV1 {
                schema_version: 1,
                policy: "review-pool-v1".to_string(),
                activation_event_id: activation_id,
                binding_sha256: dormant.binding_sha256,
                policy_sha256: dormant.policy_sha256,
                deactivated_at_main_sha: active_base.clone(),
                reason: "test-deactivation".to_string(),
            },
        ),
    )
    .unwrap();
    events.push(deactivation);
    write_events(&root, &events);
    let dormant_again = commit(&root, "deactivate policy");
    assert_eq!(
        plan::resolve_runtime_policy_at(&root, "r81", "review-pool-v1", &dormant_again)
            .unwrap()
            .state,
        RuntimePolicyStateV1::Dormant
    );
    assert_eq!(
        plan::resolve_runtime_policy_at(&root, "r81", "review-pool-v1", &dormant_base)
            .unwrap()
            .state,
        RuntimePolicyStateV1::Dormant,
        "future activation/deactivation must not rewrite an older base"
    );
    fs::remove_dir_all(root).ok();
}

#[test]
fn routed_seat_reserves_capacity_before_wake_and_terminal_releases_it() {
    let route = ledger::runtime_event_v1(
        "r81",
        Some("B901"),
        RuntimeEventPayloadV1::ReviewSeatRouted(ledger::ReviewSeatRoutedPayloadV1 {
            schema_version: 1,
            panel_id: "panel-capacity".to_string(),
            seat_id: "seat-capacity".to_string(),
            generation: 1,
            wake_id: "wake-capacity".to_string(),
            attempt_id: "B901-A0001".to_string(),
            attempt_no: 1,
            role: "primary".to_string(),
            agent: "executor-opencode".to_string(),
            lineage: "primary".to_string(),
            reviewed_head: "a".repeat(40),
            policy_base_sha: "b".repeat(40),
            deadline_secs: 3_600,
            retry_eligible: true,
            route_kind: "initial".to_string(),
            selected_event_id: "selected-capacity".to_string(),
            source_seat_id: None,
            source_generation: None,
            source_terminal_event_id: None,
        }),
    )
    .unwrap();
    let load = orch_host::scheduler::agent_inflight_load_from_events(
        std::slice::from_ref(&route),
        "r81",
    )
    .unwrap();
    assert_eq!(load["executor-opencode"].len(), 1);

    let closed = ledger::runtime_event_v1(
        "r81",
        Some("B901"),
        RuntimeEventPayloadV1::ReviewPanelClosed(ledger::ReviewPanelClosedPayloadV1 {
            schema_version: 1,
            panel_id: "panel-capacity".to_string(),
            attempt_id: "B901-A0001".to_string(),
            attempt_no: 1,
            reviewed_head: "a".repeat(40),
            policy_base_sha: "b".repeat(40),
            outcome: "pool-exhausted".to_string(),
            reason: "unreachable".to_string(),
            terminal_seat_count: 1,
            pass_count: 0,
            primary_pass: false,
        }),
    )
    .unwrap();
    let blocked = ledger::event(
        "AttemptBlocked",
        "runtime:orch",
        Some("B901"),
        Some("r81"),
        serde_json::json!({
            "attemptId": "B901-A0001",
            "attemptNo": 1,
            "agent": "runtime-review-panel",
            "stage": "review-panel-exhausted",
            "reason": "unreachable",
        }),
    );
    let load = orch_host::scheduler::agent_inflight_load_from_events(
        &[route.clone(), closed, blocked],
        "r81",
    )
    .unwrap();
    assert_eq!(
        load["executor-opencode"].len(),
        1,
        "panel/attempt closure cannot free a still-running managed wake"
    );
    let recorded = ledger::event(
        "TaskRecorded",
        "runtime:orch",
        Some("B901"),
        Some("r81"),
        serde_json::json!({"postMergeGates": "all-green"}),
    );
    let load = orch_host::scheduler::agent_inflight_load_from_events(
        &[route.clone(), recorded],
        "r81",
    )
    .unwrap();
    assert_eq!(load["executor-opencode"].len(), 1);

    for (label, actor, task, agent, scope) in [
        ("wrong actor", "user", "B901", "executor-opencode", true),
        (
            "wrong task",
            "runtime:orch",
            "B902",
            "executor-opencode",
            true,
        ),
        ("wrong agent", "runtime:orch", "B901", "executor-pi", true),
        (
            "scope not terminated",
            "runtime:orch",
            "B901",
            "executor-opencode",
            false,
        ),
    ] {
        let malformed = ledger::event(
            "ManagedWakeTerminated",
            actor,
            Some(task),
            Some("r81"),
            serde_json::json!({
                "wakeId": "wake-capacity",
                "agent": agent,
                "managedScopeTerminated": scope,
            }),
        );
        assert!(
            orch_host::scheduler::agent_inflight_load_from_events(
                &[route.clone(), malformed],
                "r81"
            )
            .is_err(),
            "{label} must fail closed rather than release capacity"
        );
    }
    let managed_terminal = ledger::event(
        "ManagedWakeTerminated",
        "runtime:orch",
        Some("B901"),
        Some("r81"),
        serde_json::json!({
            "wakeId": "wake-capacity",
            "agent": "executor-opencode",
            "managedScopeTerminated": true,
        }),
    );
    let load = orch_host::scheduler::agent_inflight_load_from_events(
        &[route.clone(), managed_terminal],
        "r81",
    )
    .unwrap();
    assert!(load.get("executor-opencode").is_none());

    let terminal = ledger::runtime_event_v1(
        "r81",
        Some("B901"),
        RuntimeEventPayloadV1::ReviewSeatTerminated(
            ledger::ReviewSeatTerminatedPayloadV1 {
                schema_version: 1,
                panel_id: "panel-capacity".to_string(),
                seat_id: "seat-capacity".to_string(),
                generation: 1,
                wake_id: "wake-capacity".to_string(),
                attempt_id: "B901-A0001".to_string(),
                attempt_no: 1,
                role: "primary".to_string(),
                agent: "executor-opencode".to_string(),
                lineage: "primary".to_string(),
                reviewed_head: "a".repeat(40),
                policy_base_sha: "b".repeat(40),
                state: "system-terminal-invalid".to_string(),
                terminal_event_id: "action-rejected".to_string(),
                delivery_event_id: None,
                reason: "admission failed".to_string(),
            },
        ),
    )
    .unwrap();
    let load = orch_host::scheduler::agent_inflight_load_from_events(&[route, terminal], "r81")
        .unwrap();
    assert!(load.get("executor-opencode").is_none());
}

fn repository_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .unwrap()
        .to_path_buf()
}

#[test]
fn production_select_commits_routes_before_rejected_wakes_and_closes_impossible_panel() {
    let root = orch_host::util::test_scratch_dir("b310-production-panel-e2e");
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("README.md"), "base\n").unwrap();
    fs::write(
        root.join(".gitignore"),
        "coordination/runtime/\n.worktrees/\n.cowork-temp/\n",
    )
    .unwrap();
    git(&root, &["add", "README.md", ".gitignore"]);
    git(
        &root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch-test@example.invalid",
            "commit",
            "-q",
            "-m",
            "base",
        ],
    );
    let owner_merge_sha = git(&root, &["rev-parse", "HEAD"]);
    for rel in [
        "coordination/runtime",
        "coordination/modes",
        "coordination/rounds/r81/tasks",
        "coordination/runtime/ledger-wal",
    ] {
        fs::create_dir_all(root.join(rel)).unwrap();
    }
    fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r81\n").unwrap();
    fs::write(
        root.join("coordination/modes/panel.yaml"),
        r#"apiVersion: orch/v1alpha1
kind: ModeConfig
metadata: {name: panel}
agents:
  executor: {adapter: test, tier: none}
  verifier: {adapter: root-manual, tier: none}
hitl: {planSignoff: required, mergeGate: auto}
verification: {mode: root-manual-fixed-head}
liveness: {monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}
scheduling:
  allowedAgents: [executor-desktop, executor-opencode, executor-dsh, executor-pi, executor-antigravity, executor-claw2]
  capacities:
    executor-desktop: {agent: 2, quota: 2, roles: [implement]}
    executor-opencode: {agent: 2, quota: 2, roles: [primary-review]}
    executor-dsh: {agent: 2, quota: 2, roles: [primary-review]}
    executor-pi: {agent: 2, quota: 2, roles: [secondary-review]}
    executor-antigravity: {agent: 2, quota: 2, roles: [nongate-review]}
    executor-claw2: {agent: 2, quota: 2, roles: [nongate-review]}
budgets: {round: {maxUsd: 1, wallMinutes: 60, maxModelWakes: 20}}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
"#,
    )
    .unwrap();
    fs::write(
        root.join("coordination/PROJECT-BINDING.yaml"),
        r#"apiVersion: orch/v1alpha1
kind: ProjectBinding
metadata: {name: panel, bindingRevision: 1}
project: {root: ".", primaryBranch: main, ecosystems: [test]}
workspace: {defaultIsolation: git-worktree, worktreeRoot: .worktrees}
commands:
  postGate: {argv: ["/bin/sh", "-c", "exit 0"], timeoutSeconds: 30}
gates: {fast: [postGate], merge: [postGate]}
runtimePolicies:
  schemaVersion: 1
  policies:
    review-pool-v1:
      schemaVersion: 1
      ownerTask: B310
      initialState: dormant
      scope: round
      minimumPasses: 2
      requirePrimaryPass: true
      initialSeatCount: 3
      minimumGateEligible: 2
      maximumBusinessRetries: 1
      nongateSubstitutesRole: secondary
      candidates:
        - {agent: executor-opencode, role: primary, lineage: primary, retryEligible: true}
        - {agent: executor-dsh, role: primary, lineage: primary, retryEligible: true, fallbackFor: executor-opencode}
        - {agent: executor-pi, role: secondary, lineage: secondary, retryEligible: true}
        - {agent: executor-antigravity, role: nongate, lineage: nongate, retryEligible: false}
        - {agent: executor-claw2, role: nongate, lineage: nongate, retryEligible: false}
scope: {protectedPaths: ["coordination/**"]}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
verification: {contractModes: [verify-only], independentVerifier: required-for-write}
"#,
    )
    .unwrap();
    fs::write(
        root.join("coordination/agents.yaml"),
        r#"apiVersion: orch/v1alpha1
kind: AgentRegistry
agents:
  executor-desktop:
    injectable: true
    sessionId: implementer
    provider: synthetic
    model: synthetic
    effort: high
    observation: {source: fixture, policy: advisory}
    roles: [implement]
    wake: {argv: ["/bin/sh", "-c", "exit 0", "{message}"]}
  executor-opencode:
    injectable: true
    sessionId: primary
    provider: synthetic
    model: synthetic
    effort: high
    observation: {source: fixture, policy: advisory}
    roles: [primary-review]
    wake: {argv: ["/bin/sh", "-c", "exit 0", "{message}"]}
  executor-dsh:
    injectable: true
    sessionId: primary-fallback
    provider: synthetic
    model: synthetic
    effort: high
    observation: {source: fixture, policy: advisory}
    roles: [primary-review]
    wake: {argv: ["/bin/sh", "-c", "exit 0", "{message}"]}
  executor-pi:
    injectable: true
    sessionId: secondary
    provider: synthetic
    model: synthetic
    effort: high
    observation: {source: fixture, policy: advisory}
    roles: [secondary-review]
    wake: {argv: ["/bin/sh", "-c", "exit 0", "{message}"]}
  executor-antigravity:
    injectable: true
    sessionId: nongate
    provider: synthetic
    model: synthetic
    effort: high
    observation: {source: fixture, policy: advisory}
    roles: [nongate-review]
    wake: {argv: ["/bin/sh", "-c", "exit 0", "{message}"]}
  executor-claw2:
    injectable: true
    sessionId: nongate-backfill
    provider: synthetic
    model: synthetic
    effort: high
    observation: {source: fixture, policy: advisory}
    roles: [nongate-review]
    wake: {argv: ["/bin/sh", "-c", "exit 0", "{message}"]}
"#,
    )
    .unwrap();
    fs::copy(
        repository_root().join("coordination/harnesses.yaml"),
        root.join("coordination/harnesses.yaml"),
    )
    .unwrap();
    fs::write(
        root.join("coordination/rounds/r81/MODE-REF.yaml"),
        "modeRef: panel\n",
    )
    .unwrap();
    fs::write(
        root.join("coordination/rounds/r81/tasks/B900.md"),
        r#"---
taskId: B900
round: r81
agent: executor-desktop
seedProtocol: verify-only
complexity: light
capabilities: [implement]
entryPoints: [feature.txt]
writeSet: [feature.txt]
frozenPaths: [coordination/**]
gates: {fast: [postGate]}
budgets: {wallMinutes: 30}
requiredReviews:
  - {role: primary, agent: executor-opencode}
  - {role: secondary, agent: executor-pi}
nongateSeats:
  - {agent: executor-antigravity, preset: minimal}
reviewQuorum: {minimumSubstantive: 2, nongateMaySubstituteFailedFormal: true, minimumNongatePassForSubstitution: 1}
requiredEvidence: [one, two, three]
---
# B900 panel fixture
"#,
    )
    .unwrap();
    fs::write(root.join("coordination/BOARD.md"), "# fixture\n").unwrap();

    orch_host::plan::run_plan(&root).unwrap();
    orch_host::round::run_sign_off(&root, Some("panel fixture sign-off")).unwrap();
    let mut recorded = ledger::event(
        "TaskRecorded",
        "runtime:orch",
        Some("B310"),
        Some("r81"),
        serde_json::json!({"postMergeGates": "all-green"}),
    );
    recorded.event_id = "owner-recorded".to_string();
    let owner_verdict = ledger::event(
        "VerdictIssued",
        "verifier:root",
        Some("B310"),
        Some("r81"),
        serde_json::json!({
            "verdict": "PASS",
            "irRevision": 1,
            "validationDigest": "f".repeat(64),
            "attemptId": "B310-A0001",
            "attemptNo": 1,
            "implementerAgent": "executor-desktop",
            "headSha": owner_merge_sha,
            "mainHeadSha": owner_merge_sha,
            "collectCompletedEventId": "owner-collect",
            "reviews": [],
            "evidence": [],
            "gates": [],
        }),
    );
    let owner_started = ledger::event(
        "MergeStarted",
        "runtime:orch",
        Some("B310"),
        Some("r81"),
        serde_json::json!({
            "attemptId": "B310-A0001",
            "attemptNo": 1,
            "headSha": owner_merge_sha,
            "mainHeadSha": owner_merge_sha,
            "collectCompletedEventId": "owner-collect",
            "verdictEventId": owner_verdict.event_id,
        }),
    );
    let merge = ledger::event(
        "MergeExecuted",
        "reviewer:orch-runtime",
        Some("B310"),
        Some("r81"),
        serde_json::json!({"mergeSha": owner_merge_sha, "policy": "no-ff"}),
    );
    let mut bootstrap_events =
        orch_core::read_ledger(&root.join("coordination/rounds/r81/events.jsonl"))
            .unwrap()
            .events;
    bootstrap_events.extend([owner_verdict, owner_started, merge, recorded]);
    write_events(&root, &bootstrap_events);
    let _ = commit(&root, "owner recorded");
    let activation = orch_host::plan::activate_runtime_policy(&root, "review-pool-v1")
        .unwrap();
    let active_base = activation.commit_sha;

    git(&root, &["branch", "task/B900"]);
    git(&root, &["checkout", "-q", "task/B900"]);
    fs::write(root.join("feature.txt"), "candidate\n").unwrap();
    let reviewed_head = commit(&root, "candidate");
    git(&root, &["checkout", "-q", "main"]);

    let dispatch = ledger::event(
        "DispatchIssued",
        "runtime:orch",
        Some("B900"),
        Some("r81"),
        serde_json::json!({
            "attemptId": "B900-A0001",
            "attemptNo": 1,
            "agent": "executor-desktop",
            "baseSha": active_base,
            "goPath": "coordination/rounds/r81/dispatch/executor-desktop/GO-B900-A0001.md",
        }),
    );
    let collect = ledger::event(
        "ReportCollectCompleted",
        "runtime:orch",
        Some("B900"),
        Some("r81"),
        serde_json::json!({
            "attemptId": "B900-A0001",
            "branchSha": reviewed_head,
        }),
    );
    ledger::append(&root, "r81", &[dispatch, collect]).unwrap();
    let _ = commit(&root, "dispatch fixture");

    let selected = orch_host::wake::select_review_panel_v1(
        &root,
        "B900",
        "B900-A0001",
        &[
            "primary:executor-opencode".to_string(),
            "secondary:executor-pi".to_string(),
            "nongate:executor-antigravity".to_string(),
        ],
    )
    .unwrap();
    assert_eq!(selected.routes.len(), 3);
    assert_eq!(selected.spawned, 0);
    let committed_events = git(
        &root,
        &[
            "show",
            &format!(
                "{}:coordination/rounds/r81/events.jsonl",
                selected.commit_sha
            ),
        ],
    );
    assert_eq!(committed_events.matches("ReviewPanelSelected").count(), 1);
    assert_eq!(committed_events.matches("ReviewSeatRouted").count(), 3);
    assert_eq!(committed_events.matches("ActionRejected").count(), 0);
    let selected_history = committed_events
        .lines()
        .map(|line| serde_json::from_str::<orch_core::EventRecord>(line).unwrap())
        .collect::<Vec<_>>();
    let route_index = selected_history
        .iter()
        .position(|event| event.kind == "ReviewSeatRouted")
        .unwrap();
    for (label, value) in [
        ("unsigned agent", serde_json::json!("executor-evil")),
        ("forged deadline", serde_json::json!(1)),
    ] {
        let mut forged = selected_history.clone();
        let key = if label == "unsigned agent" {
            "agent"
        } else {
            "deadlineSecs"
        };
        forged[route_index].payload.as_mut().unwrap()[key] = value;
        ledger::validate_runtime_event_history_v1(&forged, "r81").unwrap();
        assert!(
            ledger::validate_runtime_event_history_v1_at_root(&root, &forged, "r81").is_err(),
            "{label} must fail repository-backed authority"
        );
    }

    orch_host::wake::reconcile_review_attempt_v1(&root, "B900", "B900-A0001").unwrap();
    let initial_terminals =
        orch_core::read_ledger(&root.join("coordination/rounds/r81/events.jsonl"))
        .unwrap()
        .events;
    assert_eq!(
        initial_terminals
            .iter()
            .filter(|event| event.kind == "ReviewSeatTerminated")
            .count(),
        3
    );
    assert!(!initial_terminals
        .iter()
        .any(|event| event.kind == "ReviewPanelClosed"));

    let primary_backfill = orch_host::wake::backfill_review_panel_v1(
        &root,
        "B900",
        "B900-A0001",
        "primary:executor-dsh",
    )
    .unwrap();
    assert_eq!(primary_backfill.routes[0].agent, "executor-dsh");
    let nongate_backfill = orch_host::wake::backfill_review_panel_v1(
        &root,
        "B900",
        "B900-A0001",
        "nongate:executor-claw2",
    )
    .unwrap();
    assert_eq!(nongate_backfill.routes[0].agent, "executor-claw2");
    orch_host::wake::reconcile_review_attempt_v1(&root, "B900", "B900-A0001").unwrap();

    let events = orch_core::read_ledger(&root.join("coordination/rounds/r81/events.jsonl"))
        .unwrap()
        .events;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "ReviewSeatTerminated")
            .count(),
        5
    );
    assert!(events.iter().any(|event| event.kind == "ReviewPanelClosed"));
    assert!(events.iter().any(|event| {
        event.kind == "AttemptBlocked"
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("stage"))
                .and_then(serde_json::Value::as_str)
                == Some("review-panel-exhausted")
    }));
    assert!(orch_host::wake::retry_review_panel_v1(
        &root,
        "B900",
        "B900-A0001",
        &selected.routes[0].seat_id,
    )
    .is_err());
    assert!(orch_host::wake::backfill_review_panel_v1(
        &root,
        "B900",
        "B900-A0001",
        "primary:executor-dsh",
    )
    .is_err());

    // Crash replay on a successor attempt: canonical bytes and the complete
    // promotion batch exist in the working ledger, but no accounting commit
    // does. The shared reconciler must commit that exact suffix once.
    let policy_base2 = git(&root, &["rev-parse", "main"]);
    let dispatch2 = ledger::event(
        "DispatchIssued",
        "runtime:orch",
        Some("B900"),
        Some("r81"),
        serde_json::json!({
            "attemptId": "B900-A0002",
            "attemptNo": 2,
            "agent": "executor-desktop",
            "baseSha": policy_base2,
            "goPath": "coordination/rounds/r81/dispatch/executor-desktop/GO-B900-A0002.md",
        }),
    );
    let action_id2 = "collect-action-a0002";
    let mut receipt2 = ledger::event(
        "CollectGateSuccessReceipt",
        "runtime:orch",
        Some("B900"),
        Some("r81"),
        serde_json::json!({
            "actionId": action_id2,
            "attemptId": "B900-A0002",
            "attemptNo": 2,
            "agent": "executor-desktop",
            "baseSha": policy_base2,
            "goPath": "coordination/rounds/r81/dispatch/executor-desktop/GO-B900-A0002.md",
            "branchSha": reviewed_head,
        }),
    );
    receipt2.event_id = "collect-receipt-a0002".to_string();
    let collect2 = ledger::event(
        "ReportCollectCompleted",
        "runtime:orch",
        Some("B900"),
        Some("r81"),
        serde_json::json!({
            "actionId": action_id2,
            "gateReceipt": receipt2.event_id,
            "attemptId": "B900-A0002",
            "attemptNo": 2,
            "agent": "executor-desktop",
            "baseSha": policy_base2,
            "goPath": "coordination/rounds/r81/dispatch/executor-desktop/GO-B900-A0002.md",
            "branchSha": reviewed_head,
        }),
    );
    ledger::append(&root, "r81", &[dispatch2, receipt2, collect2]).unwrap();
    let _ = commit(&root, "dispatch crash-replay successor");
    let resolved2 = plan::resolve_runtime_policy_at(
        &root,
        "r81",
        "review-pool-v1",
        &policy_base2,
    )
    .unwrap();
    assert_eq!(resolved2.state, RuntimePolicyStateV1::Active);

    let selected2 = ledger::runtime_event_v1(
        "r81",
        Some("B900"),
        RuntimeEventPayloadV1::ReviewPanelSelected(ledger::ReviewPanelSelectedPayloadV1 {
            schema_version: 1,
            panel_id: "panel-crash".to_string(),
            attempt_id: "B900-A0002".to_string(),
            attempt_no: 2,
            reviewed_head: reviewed_head.clone(),
            policy_base_sha: policy_base2.clone(),
            policy: "review-pool-v1".to_string(),
            policy_sha256: resolved2.policy_sha256,
            seat_count: 3,
            seat_ids: [
                "seat-crash".to_string(),
                "seat-secondary".to_string(),
                "seat-nongate".to_string(),
            ],
        }),
    )
    .unwrap();
    let selected2_event_id = selected2.event_id.clone();
    let route = |seat_id: &str,
                 wake_id: &str,
                 role: &str,
                 agent: &str,
                 retry_eligible: bool| {
        ledger::runtime_event_v1(
            "r81",
            Some("B900"),
            RuntimeEventPayloadV1::ReviewSeatRouted(ledger::ReviewSeatRoutedPayloadV1 {
                schema_version: 1,
                panel_id: "panel-crash".to_string(),
                seat_id: seat_id.to_string(),
                generation: 1,
                wake_id: wake_id.to_string(),
                attempt_id: "B900-A0002".to_string(),
                attempt_no: 2,
                role: role.to_string(),
                agent: agent.to_string(),
                lineage: role.to_string(),
                reviewed_head: reviewed_head.clone(),
                policy_base_sha: policy_base2.clone(),
                deadline_secs: orch_host::wake::review_deadline_secs(role, 3),
                retry_eligible,
                route_kind: "initial".to_string(),
                selected_event_id: selected2.event_id.clone(),
                source_seat_id: None,
                source_generation: None,
                source_terminal_event_id: None,
            }),
        )
        .unwrap()
    };
    let primary2 = route(
        "seat-crash",
        "wake-crash",
        "primary",
        "executor-dsh",
        true,
    );
    let secondary2 = route(
        "seat-secondary",
        "wake-secondary",
        "secondary",
        "executor-pi",
        true,
    );
    let nongate2 = route(
        "seat-nongate",
        "wake-nongate",
        "nongate",
        "executor-antigravity",
        false,
    );
    ledger::append(
        &root,
        "r81",
        &[selected2, primary2.clone(), secondary2.clone(), nongate2.clone()],
    )
    .unwrap();
    let _ = commit(&root, "route crash-replay panel");

    let worktree_rel = ".worktrees/review-B900-A0002-primary-executor-dsh-g01";
    let staging_rel = format!(
        "{worktree_rel}/.cowork-temp/review-spool/B900-A0002-seat-crash-g1-wake-crash.md"
    );
    let staging = root.join(&staging_rel);
    fs::create_dir_all(staging.parent().unwrap()).unwrap();
    let review = format!(
        "---\ntaskId: B900\nround: r81\nattemptId: B900-A0002\nrole: primary\nreviewer: executor-dsh\nverdict: PASS\nreviewedHead: {reviewed_head}\nseatId: seat-crash\ngeneration: 1\nwakeId: wake-crash\npolicyBaseSha: {policy_base2}\n---\nsubstantive DSH turn/end crash replay\n"
    )
    .into_bytes();
    fs::write(&staging, &review).unwrap();
    let sha256 = hex::encode(Sha256::digest(&review));
    let expectation = orch_host::verify::ReviewContractExpectation::panel_exact(
        "B900",
        "r81",
        "B900-A0002",
        "primary",
        "executor-dsh",
        &reviewed_head,
        "seat-crash",
        1,
        "wake-crash",
        &policy_base2,
    )
    .unwrap();
    let checked = orch_host::verify::check_review_artifact_contract(&review, &expectation)
        .unwrap()
        .unwrap();
    let canonical_rel =
        "coordination/rounds/r81/reviews/B900-A0002-seat-crash-g1-wake-crash.md";

    let lease = ledger::event(
        "WorkspaceLeased",
        "runtime:orch",
        Some("B900"),
        Some("r81"),
        serde_json::json!({
            "siteId": "B900-primary-executor-dsh-g01",
            "generation": 1,
            "attemptId": "B900-A0002",
            "role": "primary",
            "agent": "executor-dsh",
            "reviewedHead": reviewed_head,
            "wakeId": "wake-crash",
            "paths": {"worktree": worktree_rel, "target": "orch/target/review-B900-A0002-primary-executor-dsh-g01"},
        }),
    );
    let wake = ledger::event(
        "WakeIssued",
        "runtime:orch",
        Some("B900"),
        Some("r81"),
        serde_json::json!({
            "wakeId": "wake-crash",
            "attemptId": "B900-A0002",
            "agent": "executor-dsh",
            "panelId": "panel-crash",
            "seatId": "seat-crash",
            "generation": 1,
            "routeEventId": primary2.event_id,
            "policyBaseSha": policy_base2,
            "reviewedHead": reviewed_head,
            "reviewOutputPath": staging.display().to_string(),
        }),
    );
    let mut managed = ledger::event(
        "ManagedWakeTerminated",
        "runtime:orch",
        Some("B900"),
        Some("r81"),
        serde_json::json!({
            "wakeId": "wake-crash",
            "agent": "executor-dsh",
            "state": "answered",
            "exactReason": "turn/end",
            "outputPath": staging.display().to_string(),
            "outputSha256": sha256,
            "managedScopeTerminated": true,
        }),
    );
    managed.event_id = "terminal-crash".to_string();
    let mut secondary_managed = ledger::event(
        "ManagedWakeTerminated",
        "runtime:orch",
        Some("B900"),
        Some("r81"),
        serde_json::json!({
            "wakeId": "wake-secondary",
            "agent": "executor-pi",
            "state": "empty",
            "exactReason": "answered without a contract-valid artifact",
            "managedScopeTerminated": true,
        }),
    );
    secondary_managed.event_id = "business-seat-secondary".to_string();
    let secondary_terminal = ledger::runtime_event_v1(
        "r81",
        Some("B900"),
        RuntimeEventPayloadV1::ReviewSeatTerminated(
            ledger::ReviewSeatTerminatedPayloadV1 {
                schema_version: 1,
                panel_id: "panel-crash".to_string(),
                seat_id: "seat-secondary".to_string(),
                generation: 1,
                wake_id: "wake-secondary".to_string(),
                attempt_id: "B900-A0002".to_string(),
                attempt_no: 2,
                role: "secondary".to_string(),
                agent: "executor-pi".to_string(),
                lineage: "secondary".to_string(),
                reviewed_head: reviewed_head.clone(),
                policy_base_sha: policy_base2.clone(),
                state: "business-invalid".to_string(),
                terminal_event_id: secondary_managed.event_id.clone(),
                delivery_event_id: None,
                reason: "answered without a contract-valid artifact".to_string(),
            },
        ),
    )
    .unwrap();
    let invalid_terminal = |route: &orch_core::EventRecord,
                            seat_id: &str,
                            wake_id: &str,
                            role: &str,
                            agent: &str| {
        let mut rejected = ledger::event(
            "ActionRejected",
            "runtime:orch",
            Some("B900"),
            Some("r81"),
            serde_json::json!({
                "actionId": wake_id,
                "attemptId": "B900-A0002",
                "stage": "review-panel-wake",
                "reason": "fixture admission failure",
            }),
        );
        rejected.event_id = format!("rejected-{seat_id}");
        let terminal = ledger::runtime_event_v1(
            "r81",
            Some("B900"),
            RuntimeEventPayloadV1::ReviewSeatTerminated(
                ledger::ReviewSeatTerminatedPayloadV1 {
                    schema_version: 1,
                    panel_id: "panel-crash".to_string(),
                    seat_id: seat_id.to_string(),
                    generation: 1,
                    wake_id: wake_id.to_string(),
                    attempt_id: "B900-A0002".to_string(),
                    attempt_no: 2,
                    role: role.to_string(),
                    agent: agent.to_string(),
                    lineage: role.to_string(),
                    reviewed_head: reviewed_head.clone(),
                    policy_base_sha: policy_base2.clone(),
                    state: "system-terminal-invalid".to_string(),
                    terminal_event_id: rejected.event_id.clone(),
                    delivery_event_id: None,
                    reason: "fixture admission failure".to_string(),
                },
            ),
        )
        .unwrap();
        assert_eq!(
            route
                .payload
                .as_ref()
                .and_then(|payload| payload.get("wakeId"))
                .and_then(serde_json::Value::as_str),
            Some(wake_id)
        );
        [rejected, terminal]
    };
    let [nongate_rejected, nongate_terminal] = invalid_terminal(
        &nongate2,
        "seat-nongate",
        "wake-nongate",
        "nongate",
        "executor-antigravity",
    );
    ledger::append(
        &root,
        "r81",
        &[
            lease,
            wake,
            secondary_managed,
            secondary_terminal,
            nongate_rejected,
            nongate_terminal,
        ],
    )
    .unwrap();
    let _ = commit(&root, "stage crash-replay inputs");

    let retry = orch_host::wake::retry_review_panel_v1(
        &root,
        "B900",
        "B900-A0002",
        "seat-secondary",
    )
    .unwrap();
    assert_eq!(retry.routes.len(), 1);
    assert_eq!(retry.routes[0].seat_id, "seat-secondary");
    assert_eq!(retry.routes[0].generation, 2);
    let retry_replay = orch_host::wake::retry_review_panel_v1(
        &root,
        "B900",
        "B900-A0002",
        "seat-secondary",
    )
    .unwrap();
    assert!(retry_replay.replayed);
    assert_eq!(retry_replay.routes[0].wake_id, retry.routes[0].wake_id);
    orch_host::wake::reconcile_review_attempt_v1(&root, "B900", "B900-A0002").unwrap();

    let retry_events = orch_core::read_ledger(&root.join("coordination/rounds/r81/events.jsonl"))
        .unwrap()
        .events;
    let retry_terminal = retry_events
        .iter()
        .find(|event| {
            matches!(
                ledger::decode_runtime_event_v1(event),
                Ok(Some(RuntimeEventPayloadV1::ReviewSeatTerminated(ref terminal)))
                    if terminal.panel_id == "panel-crash"
                        && terminal.seat_id == "seat-secondary"
                        && terminal.generation == 2
                        && terminal.state == "system-terminal-invalid"
            )
        })
        .unwrap();
    let backfill_route = ledger::runtime_event_v1(
        "r81",
        Some("B900"),
        RuntimeEventPayloadV1::ReviewSeatRouted(ledger::ReviewSeatRoutedPayloadV1 {
            schema_version: 1,
            panel_id: "panel-crash".to_string(),
            seat_id: "seat-backfill-pass".to_string(),
            generation: 1,
            wake_id: "wake-backfill-pass".to_string(),
            attempt_id: "B900-A0002".to_string(),
            attempt_no: 2,
            role: "nongate".to_string(),
            agent: "executor-claw2".to_string(),
            lineage: "nongate".to_string(),
            reviewed_head: reviewed_head.clone(),
            policy_base_sha: policy_base2.clone(),
            deadline_secs: orch_host::wake::review_deadline_secs("nongate", 3),
            retry_eligible: false,
            route_kind: "backfill".to_string(),
            selected_event_id: selected2_event_id,
            source_seat_id: Some("seat-secondary".to_string()),
            source_generation: Some(2),
            source_terminal_event_id: Some(retry_terminal.event_id.clone()),
        }),
    )
    .unwrap();
    ledger::append(&root, "r81", std::slice::from_ref(&backfill_route)).unwrap();
    let _ = commit(&root, "route successful nongate backfill");

    let backfill_worktree = ".worktrees/review-B900-A0002-nongate-executor-claw2-g01";
    let backfill_staging_rel = format!(
        "{backfill_worktree}/.cowork-temp/review-spool/B900-A0002-seat-backfill-pass-g1-wake-backfill-pass.md"
    );
    let backfill_staging = root.join(&backfill_staging_rel);
    fs::create_dir_all(backfill_staging.parent().unwrap()).unwrap();
    let backfill_review = format!(
        "---\ntaskId: B900\nround: r81\nattemptId: B900-A0002\nrole: nongate\nreviewer: executor-claw2\nverdict: PASS\nreviewedHead: {reviewed_head}\nseatId: seat-backfill-pass\ngeneration: 1\nwakeId: wake-backfill-pass\npolicyBaseSha: {policy_base2}\n---\nsubstantive secondary substitution\n"
    )
    .into_bytes();
    fs::write(&backfill_staging, &backfill_review).unwrap();
    let backfill_sha = hex::encode(Sha256::digest(&backfill_review));
    let backfill_lease = ledger::event(
        "WorkspaceLeased",
        "runtime:orch",
        Some("B900"),
        Some("r81"),
        serde_json::json!({
            "siteId": "B900-nongate-executor-claw2-g01",
            "generation": 1,
            "attemptId": "B900-A0002",
            "role": "nongate",
            "agent": "executor-claw2",
            "reviewedHead": reviewed_head,
            "wakeId": "wake-backfill-pass",
            "paths": {"worktree": backfill_worktree, "target": "orch/target/review-B900-A0002-nongate-executor-claw2-g01"},
        }),
    );
    let backfill_wake = ledger::event(
        "WakeIssued",
        "runtime:orch",
        Some("B900"),
        Some("r81"),
        serde_json::json!({
            "wakeId": "wake-backfill-pass",
            "attemptId": "B900-A0002",
            "agent": "executor-claw2",
            "panelId": "panel-crash",
            "seatId": "seat-backfill-pass",
            "generation": 1,
            "routeEventId": backfill_route.event_id,
            "policyBaseSha": policy_base2,
            "reviewedHead": reviewed_head,
            "reviewOutputPath": backfill_staging.display().to_string(),
        }),
    );
    let backfill_managed = ledger::event(
        "ManagedWakeTerminated",
        "runtime:orch",
        Some("B900"),
        Some("r81"),
        serde_json::json!({
            "wakeId": "wake-backfill-pass",
            "agent": "executor-claw2",
            "state": "answered",
            "exactReason": "turn/end",
            "outputPath": backfill_staging.display().to_string(),
            "outputSha256": backfill_sha,
            "managedScopeTerminated": true,
        }),
    );
    ledger::append(
        &root,
        "r81",
        &[backfill_lease, backfill_wake, backfill_managed, managed.clone()],
    )
    .unwrap();
    let _ = commit(&root, "record successful backfill and DSH terminals");

    orch_host::wake::install_review_no_clobber_for_test(&root, canonical_rel, &review).unwrap();

    let mut delivery = ledger::event(
        "ReviewDelivered",
        "runtime:orch",
        Some("B900"),
        Some("r81"),
        serde_json::json!({
            "attemptId": "B900-A0002",
            "role": "primary",
            "agent": "executor-dsh",
            "bodyLen": checked.substantive_body_len(),
            "panelId": "panel-crash",
            "seatId": "seat-crash",
            "generation": 1,
            "wakeId": "wake-crash",
            "policyBaseSha": policy_base2,
            "reviewedHead": reviewed_head,
        }),
    );
    delivery.event_id = "delivery-crash".to_string();
    let promotion = ledger::runtime_event_v1(
        "r81",
        Some("B900"),
        RuntimeEventPayloadV1::ReviewSpoolPromoted(ledger::ReviewSpoolPromotedPayloadV1 {
            schema_version: 1,
            panel_id: "panel-crash".to_string(),
            seat_id: "seat-crash".to_string(),
            generation: 1,
            wake_id: "wake-crash".to_string(),
            attempt_id: "B900-A0002".to_string(),
            attempt_no: 2,
            role: "primary".to_string(),
            agent: "executor-dsh".to_string(),
            reviewed_head: reviewed_head.clone(),
            policy_base_sha: policy_base2.clone(),
            staging_path: staging_rel,
            canonical_path: canonical_rel.to_string(),
            sha256,
            bytes: review.len() as u64,
            body_len: checked.substantive_body_len() as u64,
            verdict: "PASS".to_string(),
            terminal_event_id: managed.event_id,
            delivery_event_id: delivery.event_id.clone(),
        }),
    )
    .unwrap();
    let seat_terminal = ledger::runtime_event_v1(
        "r81",
        Some("B900"),
        RuntimeEventPayloadV1::ReviewSeatTerminated(
            ledger::ReviewSeatTerminatedPayloadV1 {
                schema_version: 1,
                panel_id: "panel-crash".to_string(),
                seat_id: "seat-crash".to_string(),
                generation: 1,
                wake_id: "wake-crash".to_string(),
                attempt_id: "B900-A0002".to_string(),
                attempt_no: 2,
                role: "primary".to_string(),
                agent: "executor-dsh".to_string(),
                lineage: "primary".to_string(),
                reviewed_head: reviewed_head.clone(),
                policy_base_sha: policy_base2,
                state: "pass".to_string(),
                terminal_event_id: "terminal-crash".to_string(),
                delivery_event_id: Some("delivery-crash".to_string()),
                reason: "substantive PASS crash replay".to_string(),
            },
        ),
    )
    .unwrap();
    ledger::append(&root, "r81", &[promotion, delivery, seat_terminal]).unwrap();
    let committed_before = git(
        &root,
        &["show", "main:coordination/rounds/r81/events.jsonl"],
    );
    assert!(!committed_before.contains("ReviewSpoolPromoted"));

    orch_host::wake::reconcile_review_attempt_v1(&root, "B900", "B900-A0002").unwrap();
    orch_host::wake::reconcile_review_attempt_v1(&root, "B900", "B900-A0002").unwrap();
    let recovered = orch_core::read_ledger(&root.join("coordination/rounds/r81/events.jsonl"))
        .unwrap()
        .events;
    assert_eq!(
        recovered
            .iter()
            .filter(|event| event.kind == "ReviewSpoolPromoted"
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("attemptId"))
                    .and_then(serde_json::Value::as_str)
                    == Some("B900-A0002"))
            .count(),
        2
    );
    assert_eq!(
        recovered
            .iter()
            .filter(|event| event.kind == "ReviewDelivered"
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("attemptId"))
                    .and_then(serde_json::Value::as_str)
                    == Some("B900-A0002"))
            .count(),
        1
    );
    assert_eq!(
        recovered
            .iter()
            .filter(|event| event.kind == "NongateReviewDelivered"
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("attemptId"))
                    .and_then(serde_json::Value::as_str)
                    == Some("B900-A0002"))
            .count(),
        1
    );
    assert!(recovered.iter().any(|event| {
        matches!(
            ledger::decode_runtime_event_v1(event),
            Ok(Some(RuntimeEventPayloadV1::ReviewPanelClosed(ref closed)))
                if closed.panel_id == "panel-crash"
                    && closed.outcome == "pass"
                    && closed.pass_count == 2
            && closed.primary_pass
        )
    }));

    fs::create_dir_all(root.join("coordination/rounds/r81/evidence")).unwrap();
    for evidence_id in ["one", "two", "three"] {
        fs::write(
            root.join(format!(
                "coordination/rounds/r81/evidence/B900-{evidence_id}.json"
            )),
            format!("{{\"id\":\"{evidence_id}\",\"status\":\"pass\"}}\n"),
        )
        .unwrap();
    }
    let _ = commit(&root, "bind panel evidence");
    git(
        &root,
        &["worktree", "add", "-q", ".worktrees/B900", "task/B900"],
    );
    orch_host::close::run_seal(&root, "B900", "B900-A0002", &reviewed_head).unwrap();
    let archived = orch_core::read_ledger(&root.join("coordination/rounds/r81/events.jsonl"))
        .unwrap()
        .events;
    assert!(archived.iter().any(|event| {
        event.kind == "VerdictIssued"
            && event.actor == "verifier:root"
            && event.task_id.as_deref() == Some("B900")
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("attemptId"))
                .and_then(serde_json::Value::as_str)
                == Some("B900-A0002")
    }));
    assert!(archived.iter().any(|event| {
        event.kind == "TaskRecorded" && event.task_id.as_deref() == Some("B900")
    }));
    orch_host::verify::validate_archived_record_chain(&root, "r81", "B900", &archived).unwrap();
    fs::remove_dir_all(root).ok();
}
