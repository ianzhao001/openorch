use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn source_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap()
        .to_path_buf()
}

fn temp_root() -> PathBuf {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    source_root()
        .join("orch/target/test-tmp")
        .join(format!("v3-generic-review-{}-{seq}", std::process::id()))
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
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn commit(root: &Path, message: &str) -> String {
    git(root, &["add", "-A"]);
    git(
        root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "commit",
            "-q",
            "-m",
            message,
        ],
    );
    git(root, &["rev-parse", "HEAD"])
}

// B319 is the one recorded pre-channel schema-3 review that must remain
// replayable after the live bootstrap module is physically removed. Build its
// historical source digest from the fixture bytes themselves; this is test
// data, not a public compatibility API or a live invocation path.
fn historical_bootstrap_source_digest(root: &Path) -> String {
    let paths = [
        "coordination/adapters/test.yaml",
        "coordination/agents.yaml",
        "coordination/harnesses.yaml",
    ];
    let mut hasher = Sha256::new();
    hasher.update(b"orch-bootstrap-invocation-sources-v1\0");
    for path in paths {
        let bytes = fs::read(root.join(path)).unwrap();
        hasher.update((path.len() as u64).to_be_bytes());
        hasher.update(path.as_bytes());
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }
    hex::encode(hasher.finalize())
}

fn unified_receiptless_failure_chain(
    root: &Path,
    task: &str,
    attempt: &str,
    head: &str,
) -> Vec<orch_core::EventRecord> {
    let harness = "agy";
    let wake_id = "01a0b321-7777-4888-8999-aaaabbbbcccc";
    let worktree_rel = ".worktrees/review-B319-A0001-review-agy-g01";
    let cwd = root.join(worktree_rel);
    let requested = serde_json::json!({
        "provider": "antigravity", "model": "fixture-model", "effort": "high", "mode": null
    });
    let binding = serde_json::json!({
        "attachmentManifestSha256": "1".repeat(64),
        "commandDigest": "2".repeat(64),
        "configDigest": "3".repeat(64),
        "cwdSelection": "target-worktree",
        "driver": "agy",
        "effectiveTuple": requested,
        "executableIdentityDigest": "4".repeat(64),
        "fixedHead": head,
        "harness": harness,
        "invocationCwd": cwd,
        "observationSource": "agy-conversation-db-and-cli-log",
        "requestDigest": "5".repeat(64),
        "requestedTuple": requested
    });
    let lease = orch_host::ledger::event(
        "WorkspaceLeased",
        "runtime:orch",
        Some(task),
        Some("r83"),
        serde_json::json!({
            "siteId": "B319-review-agy-g01", "generation": 1,
            "attemptId": attempt, "role": "review", "agent": harness,
            "reviewedHead": head, "wakeId": wake_id,
            "paths": {"worktree": worktree_rel, "target": "orch/target/review-agy"}
        }),
    );
    let continuation = format!("review:r83:{task}:{attempt}:review:{harness}");
    let mut wake_payload = binding.clone();
    wake_payload.as_object_mut().unwrap().extend(
        serde_json::json!({
            "action": "review", "method": "unified-channel-v1",
            "attemptId": attempt, "agent": harness, "harness": harness,
            "wakeId": wake_id, "continuationId": continuation,
            "providerKind": "agy", "backendState": "pending",
            "executable": "/usr/bin/true",
            "requestMessageSha256": "5".repeat(64),
            "renderedMessageSha256": "6".repeat(64)
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let wake = orch_host::ledger::event(
        "WakeIssued",
        "runtime:orch",
        Some(task),
        Some("r83"),
        wake_payload.clone(),
    );
    let mut request_payload = wake_payload;
    request_payload.as_object_mut().unwrap().extend(
        serde_json::json!({
            "role": "review", "reviewedHead": head,
            "reviewOutputPath": root.join("coordination/runtime/review-inbox/r83/unused-agy.md")
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let request = orch_host::ledger::event(
        "ReviewRequested",
        "runtime:orch",
        Some(task),
        Some("r83"),
        request_payload,
    );
    let terminal = orch_host::ledger::event(
        "ManagedWakeTerminated",
        "runtime:orch",
        Some(task),
        Some("r83"),
        serde_json::json!({
            "agent": harness, "wakeId": wake_id, "channelBinding": binding,
            "state": "failed", "completionReason": "natural-exit",
            "exactReason": "backend acceptance receipt absent before terminal reconciliation",
            "outcomeClass": "TruncatedNoTerminal", "terminalSeen": false,
            "turnEnded": false, "exitedNaturally": false,
            "hardDeadlineReached": false, "managedScopeTerminated": true,
            "mechanicalTerminalAbsent": false, "cancelRequestId": null,
            "signals": ["TERM"], "logBytesRead": 36,
            "outputPath": null, "outputSha256": null, "finalTextSha256": null
        }),
    );
    let release = orch_host::ledger::event(
        "WorkspaceReleased",
        "runtime:orch",
        Some(task),
        Some("r83"),
        serde_json::json!({
            "siteId": "B319-review-agy-g01", "generation": 1,
            "attemptId": attempt, "role": "review", "agent": harness,
            "wakeId": wake_id,
            "completionReceipt": "runtime:orch/managed-wake-terminated",
            "terminationEventId": terminal.event_id
        }),
    );
    vec![lease, wake, request, terminal, release]
}

#[test]
fn generic_review_is_fact_bound_without_vote_policy() {
    let root = temp_root();
    fs::create_dir_all(root.join("coordination/adapters")).unwrap();
    fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    fs::write(root.join("coordination/agents.yaml"), "agents: {}\n").unwrap();
    fs::write(root.join("coordination/harnesses.yaml"), "harnesses: {}\n").unwrap();
    fs::write(root.join("coordination/adapters/test.yaml"), "kind: test\n").unwrap();
    fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r83\n").unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    let base = commit(&root, "base invocation sources");

    let task = "B319";
    let attempt = "B319-A0001";
    let harness = "executor-claw";
    let wake_id = "01a0b319-1111-4222-8333-444455556666";
    let head = "b".repeat(40);
    let cwd = root.join(".worktrees/review-B319-A0001-review-executor-claw-g01");
    fs::create_dir_all(&cwd).unwrap();
    let inbox =
        root.join("coordination/runtime/review-inbox/r83/B319-A0001-review-executor-claw.md");
    fs::create_dir_all(inbox.parent().unwrap()).unwrap();
    let artifact = format!(
        "---\ntaskId: {task}\nround: r83\nattemptId: {attempt}\nrole: review\nreviewer: {harness}\nverdict: FAIL\nreviewedHead: {head}\nwakeId: {wake_id}\n---\nsubstantive finding without mechanical veto\n"
    );
    fs::write(&inbox, artifact.as_bytes()).unwrap();
    let sha = hex::encode(Sha256::digest(artifact.as_bytes()));
    let canonical_rel = "coordination/rounds/r83/reviews/B319-A0001-review-executor-claw.md";
    fs::create_dir_all(root.join(canonical_rel).parent().unwrap()).unwrap();
    fs::write(root.join(canonical_rel), artifact.as_bytes()).unwrap();

    let dispatch = orch_host::ledger::event(
        "DispatchIssued",
        "runtime:orch",
        Some(task),
        Some("r83"),
        serde_json::json!({
            "agent": "local", "method": "local-worktree", "baseSha": base,
            "goPath": "coordination/rounds/r83/dispatch/local/GO-B319-A0001.md",
            "attemptId": attempt, "attemptNo": 1, "reassignment": false,
            "overrideAmbiguousActive": false, "wakePending": false
        }),
    );
    let collect = orch_host::ledger::event(
        "ReportCollectCompleted",
        "runtime:orch",
        Some(task),
        Some("r83"),
        serde_json::json!({"attemptId": attempt, "branchSha": head}),
    );
    let request_sha = "c".repeat(64);
    let source_sha = historical_bootstrap_source_digest(&root);
    let attachment_sha = "a".repeat(64);
    let effective_invocation_sha = "f".repeat(64);
    let lease = orch_host::ledger::event(
        "WorkspaceLeased",
        "runtime:orch",
        Some(task),
        Some("r83"),
        serde_json::json!({
            "siteId": "B319-review-executor-claw-g01", "generation": 1,
            "attemptId": attempt, "role": "review", "agent": harness,
            "reviewedHead": head, "wakeId": wake_id,
            "paths": {"worktree": cwd.strip_prefix(&root).unwrap(), "target": "orch/target/review"}
        }),
    );
    let snapshot_fields = serde_json::json!({
        "bootstrapSourceSha256": source_sha,
        "requestMessageSha256": request_sha,
        "attachmentManifestSha256": attachment_sha,
        "invocationCwd": cwd,
        "bootstrapRepositoryRoot": root,
        "fixedHead": head,
        "bootstrapExecutable": "/usr/bin/true",
        "bootstrapRequestedProvider": "fixture-provider",
        "bootstrapRequestedModel": "fixture-model",
        "bootstrapRequestedEffort": "fixture-effort",
        "effectiveInvocationSha256": effective_invocation_sha,
    });
    let mut wake_payload = snapshot_fields.clone();
    let wake_object = wake_payload.as_object_mut().unwrap();
    let continuation = format!("review:r83:{task}:{attempt}:review:{harness}");
    let rendered_sha = "d".repeat(64);
    let log_path = root.join("coordination/runtime/logs/bootstrap-review.log");
    wake_object.extend(
        serde_json::json!({
            "attemptId": attempt, "agent": harness, "wakeId": wake_id,
            "continuationId": continuation, "providerKind": "opencode",
            "requestedProvider": "fixture-provider", "requestedModel": "fixture-model",
            "requestedEffort": "fixture-effort",
            "renderedMessageSha256": rendered_sha, "requestSessionId": null,
            "backendState": "pending", "logPath": log_path, "probeOffset": 0
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let wake = orch_host::ledger::event(
        "WakeIssued",
        "runtime:orch",
        Some(task),
        Some("r83"),
        wake_payload,
    );
    let mut request_payload = snapshot_fields;
    request_payload.as_object_mut().unwrap().extend(
        serde_json::json!({
            "attemptId": attempt, "role": "review", "agent": harness,
            "harness": harness, "wakeId": wake_id, "reviewedHead": head
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let request = orch_host::ledger::event(
        "ReviewRequested",
        "runtime:orch",
        Some(task),
        Some("r83"),
        request_payload,
    );
    let terminal = orch_host::ledger::event(
        "ManagedWakeTerminated",
        "runtime:orch",
        Some(task),
        Some("r83"),
        serde_json::json!({
            "agent": harness, "wakeId": wake_id, "state": "answered",
            "completionReason": "natural-exit", "exactReason": "fixture-terminal",
            "terminalSeen": true, "turnEnded": true, "managedScopeTerminated": true,
            "exitedNaturally": true, "hardDeadlineReached": false,
            "cancelRequestId": null, "signals": [], "logBytesRead": 1,
            "outcomeClass": "DeliveredTerminal",
            "outputPath": inbox, "outputSha256": sha
        }),
    );
    let receipt = orch_host::ledger::event(
        "AgentEventReceived",
        "runtime:orch",
        Some(task),
        Some("r83"),
        serde_json::json!({
            "agentEvent": "wake-backend-receipt", "actionId": wake_id, "wakeId": wake_id,
            "continuationId": continuation, "attemptId": attempt, "agent": harness,
            "providerKind": "opencode", "receiptKind": "opencode",
            "requestMessageSha256": request_sha, "renderedMessageSha256": rendered_sha,
            "requestSessionId": null, "observedSessionId": "session-fixture",
            "logPath": log_path, "probeOffset": 0, "probeEnd": 1,
            "windowSha256": "e".repeat(64), "backendState": "accepted"
        }),
    );
    let delivery = orch_host::ledger::event(
        "ReviewDelivered",
        "runtime:orch",
        Some(task),
        Some("r83"),
        serde_json::json!({
            "attemptId": attempt, "role": "review", "agent": harness, "harness": harness,
            "wakeId": wake_id, "reviewedHead": head, "requestEventId": request.event_id,
            "terminalEventId": terminal.event_id, "path": canonical_rel, "sha256": sha,
            "bytes": artifact.len() as u64,
            "bodyLen": "substantive finding without mechanical veto".len() as u64,
            "verdict": "FAIL"
        }),
    );
    let root_event = orch_host::ledger::event(
        "VerdictIssued",
        "verifier:root",
        Some(task),
        Some("r83"),
        serde_json::json!({"attemptId": attempt, "verdict": "PASS", "headSha": head}),
    );
    let recorded = orch_host::ledger::event(
        "TaskRecorded",
        "runtime:orch",
        Some(task),
        Some("r83"),
        serde_json::json!({"postMergeGates": "all-green"}),
    );
    let events = vec![
        dispatch,
        collect,
        lease,
        wake,
        request,
        receipt.clone(),
        terminal,
        delivery,
        root_event,
        recorded,
    ];
    let main_sha = commit(&root, "bind generic review artifact");
    let bindings = orch_host::generic_review::validate_generic_review_facts(
        &root, &events, "r83", task, attempt, &head, &main_sha,
    )
    .unwrap();
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].verdict, "FAIL");

    let root_position = events
        .iter()
        .position(|event| event.kind == "VerdictIssued")
        .unwrap();
    let mut with_receiptless_failure = events.clone();
    with_receiptless_failure.splice(
        root_position..root_position,
        unified_receiptless_failure_chain(&root, task, attempt, &head),
    );
    assert_eq!(
        orch_host::generic_review::validate_generic_review_facts(
            &root,
            &with_receiptless_failure,
            "r83",
            task,
            attempt,
            &head,
            &main_sha,
        )
        .unwrap()
        .len(),
        1,
        "a receiptless closed failure is terminal but contributes no review binding"
    );
    // The cancellation has no provider receipt or answer. Its exact managed
    // terminal/release chain must close the sibling without adding a vote.
    let mut with_receiptless_cancel = with_receiptless_failure.clone();
    let cancel_position = with_receiptless_cancel
        .iter()
        .position(|event| {
            event.kind == "ManagedWakeTerminated"
                && event.payload.as_ref().unwrap()["agent"] == "agy"
        })
        .unwrap();
    let payload = with_receiptless_cancel[cancel_position].payload.as_mut().unwrap();
    payload["state"] = serde_json::json!("canceled");
    payload["completionReason"] = serde_json::json!("manual-cancel");
    payload["outcomeClass"] = serde_json::json!("StoppedByAuthenticatedCancel");
    payload["cancelRequestId"] = serde_json::json!("01a07475-d4b1-4f5a-ae6d-7dcf221dfadd");
    let canceled_bindings = orch_host::generic_review::validate_generic_review_facts(
        &root, &with_receiptless_cancel, "r83", task, attempt, &head, &main_sha,
    )
    .expect("authenticated canceled sibling must be a closed zero-vote fact");
    assert_eq!(canceled_bindings.len(), 1);
    assert_eq!(canceled_bindings[0].verdict, "FAIL");

    for (key, value) in [
        ("state", serde_json::json!("answered")),
        ("completionReason", serde_json::json!("natural-exit")),
        ("outcomeClass", serde_json::json!("TruncatedNoTerminal")),
        ("exactReason", serde_json::json!("unrelated failure")),
        ("hardDeadlineReached", serde_json::json!(true)),
        ("exitedNaturally", serde_json::json!(true)),
        ("managedScopeTerminated", serde_json::json!(false)),
        ("terminalSeen", serde_json::json!(true)),
        ("turnEnded", serde_json::json!(true)),
        ("mechanicalTerminalAbsent", serde_json::json!(true)),
        ("cancelRequestId", serde_json::Value::Null),
        ("cancelRequestId", serde_json::json!("")),
        ("cancelRequestId", serde_json::json!("   ")),
        ("cancelRequestId", serde_json::json!("unanchored-label")),
        ("cancelRequestId", serde_json::json!("01A07475-d4b1-4f5a-ae6d-7dcf221dfadd")),
        ("outputPath", serde_json::json!("/repo/forged-review.md")),
        ("outputSha256", serde_json::json!("a".repeat(64))),
        ("finalTextSha256", serde_json::json!("b".repeat(64))),
    ] {
        let mut invalid = with_receiptless_cancel.clone();
        invalid[cancel_position].payload.as_mut().unwrap()[key] = value;
        assert!(
            orch_host::generic_review::validate_generic_review_facts(
                &root, &invalid, "r83", task, attempt, &head, &main_sha,
            ).is_err(),
            "receiptless canceled sibling accepted invalid {key}"
        );
    }
    for key in [
        "cancelRequestId", "managedScopeTerminated", "hardDeadlineReached",
        "exitedNaturally", "outputPath", "outputSha256", "finalTextSha256",
    ] {
        let mut invalid = with_receiptless_cancel.clone();
        invalid[cancel_position].payload.as_mut().unwrap().as_object_mut().unwrap().remove(key);
        assert!(orch_host::generic_review::validate_generic_review_facts(
            &root, &invalid, "r83", task, attempt, &head, &main_sha,
        ).is_err(), "receiptless canceled sibling accepted missing {key}");
    }
    let mut bad_cancel_binding = with_receiptless_cancel.clone();
    bad_cancel_binding[cancel_position].payload.as_mut().unwrap()["channelBinding"]["configDigest"] =
        serde_json::json!("f".repeat(64));
    assert!(orch_host::generic_review::validate_generic_review_facts(
        &root, &bad_cancel_binding, "r83", task, attempt, &head, &main_sha,
    ).is_err());
    let mut canceled_without_release = with_receiptless_cancel.clone();
    canceled_without_release.remove(cancel_position + 1);
    assert!(orch_host::generic_review::validate_generic_review_facts(
        &root, &canceled_without_release, "r83", task, attempt, &head, &main_sha,
    ).is_err());
    let mut canceled_nonadjacent_release = with_receiptless_cancel.clone();
    canceled_nonadjacent_release.insert(cancel_position + 1, orch_host::ledger::event(
        "Observation", "runtime:orch", None, Some("r83"), serde_json::json!({}),
    ));
    assert!(orch_host::generic_review::validate_generic_review_facts(
        &root, &canceled_nonadjacent_release, "r83", task, attempt, &head, &main_sha,
    ).is_err());
    let mut canceled_with_delivery = with_receiptless_cancel.clone();
    canceled_with_delivery.insert(cancel_position + 2, orch_host::ledger::event(
        "ReviewDelivered", "runtime:orch", Some(task), Some("r83"),
        serde_json::json!({
            "attemptId": attempt, "role": "review", "harness": "agy", "agent": "agy",
            "wakeId": "01a0b321-7777-4888-8999-aaaabbbbcccc"
        }),
    ));
    assert!(orch_host::generic_review::validate_generic_review_facts(
        &root, &canceled_with_delivery, "r83", task, attempt, &head, &main_sha,
    ).is_err());
    let mut canceled_with_unrelated_receipt = with_receiptless_cancel.clone();
    canceled_with_unrelated_receipt.push(orch_host::ledger::event(
        "AgentEventReceived", "runtime:orch", None, Some("r83"),
        serde_json::json!({
            "agentEvent": "wake-backend-receipt", "agent": "agy", "actionId": "other-wake",
            "wakeId": "other-wake", "backendState": "accepted"
        }),
    ));
    assert_eq!(orch_host::generic_review::validate_generic_review_facts(
        &root, &canceled_with_unrelated_receipt, "r83", task, attempt, &head, &main_sha,
    ).unwrap().len(), 1, "unrelated receipt must not change canceled sibling identity");
    let mut wrong_binding = with_receiptless_failure.clone();
    let failed_terminal = wrong_binding
        .iter_mut()
        .find(|event| {
            event.kind == "ManagedWakeTerminated"
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("agent"))
                    .and_then(serde_json::Value::as_str)
                    == Some("agy")
        })
        .unwrap();
    failed_terminal.payload.as_mut().unwrap()["channelBinding"]["configDigest"] =
        serde_json::json!("f".repeat(64));
    assert!(orch_host::generic_review::validate_generic_review_facts(
        &root,
        &wrong_binding,
        "r83",
        task,
        attempt,
        &head,
        &main_sha,
    )
    .is_err());
    let mut missing_release = with_receiptless_failure.clone();
    missing_release.retain(|event| {
        !(event.kind == "WorkspaceReleased"
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("agent"))
                .and_then(serde_json::Value::as_str)
                == Some("agy"))
    });
    assert!(orch_host::generic_review::validate_generic_review_facts(
        &root,
        &missing_release,
        "r83",
        task,
        attempt,
        &head,
        &main_sha,
    )
    .is_err());
    let mut answered_without_receipt = with_receiptless_failure.clone();
    let failed_terminal = answered_without_receipt
        .iter_mut()
        .find(|event| {
            event.kind == "ManagedWakeTerminated"
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("agent"))
                    .and_then(serde_json::Value::as_str)
                    == Some("agy")
        })
        .unwrap();
    failed_terminal.payload.as_mut().unwrap()["state"] = serde_json::json!("answered");
    assert!(orch_host::generic_review::validate_generic_review_facts(
        &root,
        &answered_without_receipt,
        "r83",
        task,
        attempt,
        &head,
        &main_sha,
    )
    .is_err());
    let mut later_taskless_receipt = with_receiptless_failure.clone();
    later_taskless_receipt.push(orch_host::ledger::event(
        "AgentEventReceived",
        "runtime:orch",
        None,
        Some("r83"),
        serde_json::json!({
            "agentEvent": "wake-backend-receipt", "agent": "agy",
            "actionId": "different-wake", "wakeId": "different-wake",
            "backendState": "accepted"
        }),
    ));
    assert_eq!(
        orch_host::generic_review::validate_generic_review_facts(
            &root,
            &later_taskless_receipt,
            "r83",
            task,
            attempt,
            &head,
            &main_sha,
        )
        .unwrap()
        .len(),
        1,
        "a later taskless receipt cannot alter exact review identity"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        fs::remove_file(root.join(canonical_rel)).unwrap();
        symlink(&artifact, root.join(canonical_rel)).unwrap();
        let symlink_main = commit(&root, "commit review as symlink blob");
        fs::remove_file(root.join(canonical_rel)).unwrap();
        fs::write(root.join(canonical_rel), artifact.as_bytes()).unwrap();
        assert!(orch_host::generic_review::validate_generic_review_facts(
            &root,
            &events,
            "r83",
            task,
            attempt,
            &head,
            &symlink_main,
        )
        .is_err());
    }
    let live_shaped_history = events
        .iter()
        .filter(|event| !matches!(event.kind.as_str(), "VerdictIssued" | "TaskRecorded"))
        .cloned()
        .collect::<Vec<_>>();
    assert!(orch_host::generic_review::validate_generic_review_facts(
        &root,
        &live_shaped_history,
        "r83",
        task,
        attempt,
        &head,
        &main_sha,
    )
    .is_err());
    fs::remove_dir_all(&cwd).unwrap();
    assert_eq!(
        orch_host::generic_review::validate_generic_review_facts(
            &root, &events, "r83", task, attempt, &head, &main_sha,
        )
        .unwrap()
        .len(),
        1,
        "archived replay must not depend on a GC-eligible review worktree"
    );

    let mut receipt_only = events.clone();
    let terminal_position = receipt_only
        .iter()
        .position(|event| event.kind == "ManagedWakeTerminated")
        .unwrap();
    receipt_only.remove(terminal_position);
    let delivery = receipt_only
        .iter_mut()
        .find(|event| event.kind == "ReviewDelivered")
        .unwrap();
    delivery.payload.as_mut().unwrap()["terminalEventId"] = serde_json::json!(receipt.event_id);
    assert_eq!(
        orch_host::generic_review::validate_generic_review_facts(
            &root,
            &receipt_only,
            "r83",
            task,
            attempt,
            &head,
            &main_sha,
        )
        .unwrap()
        .len(),
        1
    );
    let load =
        orch_host::legacy::agent_inflight_load_from_events(&receipt_only, "r83").unwrap();
    assert!(load
        .values()
        .flatten()
        .all(|item| item.kind != orch_host::legacy::LoadKind::Review));

    let failed_harness = "executor-opencode";
    let failed_wake = "01a0b319-7777-4888-8999-aaaabbbbcccc";
    let failed_lease = orch_host::ledger::event(
        "WorkspaceLeased",
        "runtime:orch",
        Some(task),
        Some("r83"),
        serde_json::json!({
            "siteId": "B319-review-executor-opencode-g01", "generation": 1,
            "attemptId": attempt, "role": "review", "agent": failed_harness,
            "reviewedHead": head, "wakeId": failed_wake,
            "paths": {
                "worktree": ".worktrees/review-B319-A0001-review-executor-opencode-g01",
                "target": "orch/target/review-B319-A0001-review-executor-opencode-g01"
            }
        }),
    );
    let failed_rejection = orch_host::ledger::event(
        "ActionRejected",
        "runtime:orch",
        Some(task),
        Some("r83"),
        serde_json::json!({
            "actionId": failed_wake, "operation": "wake",
            "reason": "schema 3 review rejected before provider spawn: missing executable",
            "exitCode": 2, "alert": true,
            "attemptId": attempt, "attemptNo": 1
        }),
    );
    let failed_retirement = orch_host::ledger::event(
        "SiteRetired",
        "runtime:orch",
        Some(task),
        Some("r83"),
        serde_json::json!({
            "siteId": "B319-review-executor-opencode-g01", "generation": 1,
            "taskId": task, "attemptId": attempt, "role": "review",
            "agent": failed_harness, "wakeId": failed_wake,
            "trigger": "pre-spawn-rejected",
            "retireEventId": failed_rejection.event_id
        }),
    );
    let mut with_pre_spawn_rejection = receipt_only.clone();
    let root_position = with_pre_spawn_rejection
        .iter()
        .position(|event| event.kind == "VerdictIssued")
        .unwrap();
    with_pre_spawn_rejection.splice(
        root_position..root_position,
        [failed_lease, failed_rejection, failed_retirement],
    );
    assert_eq!(
        orch_host::generic_review::validate_generic_review_facts(
            &root,
            &with_pre_spawn_rejection,
            "r83",
            task,
            attempt,
            &head,
            &main_sha,
        )
        .unwrap()
        .len(),
        1,
        "an atomically retired no-spawn lease is not a phantom review request"
    );
    let mut missing_retirement = with_pre_spawn_rejection.clone();
    missing_retirement.retain(|event| {
        !(event.kind == "SiteRetired"
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("wakeId"))
                .and_then(serde_json::Value::as_str)
                == Some(failed_wake))
    });
    assert!(orch_host::generic_review::validate_generic_review_facts(
        &root,
        &missing_retirement,
        "r83",
        task,
        attempt,
        &head,
        &main_sha,
    )
    .is_err());

    let recorded = receipt_only
        .iter()
        .find(|event| event.kind == "TaskRecorded")
        .unwrap()
        .clone();
    let retirement = orch_host::ledger::event(
        "SiteRetired",
        "runtime:orch",
        Some(task),
        Some("r83"),
        serde_json::json!({
            "siteId": "B319-review-executor-claw-g01", "generation": 1,
            "taskId": task, "attemptId": attempt, "role": "review",
            "agent": harness, "wakeId": wake_id,
            "trigger": "task-recorded", "retireEventId": recorded.event_id
        }),
    );
    let mut retired_after_record = receipt_only.clone();
    retired_after_record.push(retirement);
    assert_eq!(
        orch_host::generic_review::validate_generic_review_facts(
            &root,
            &retired_after_record,
            "r83",
            task,
            attempt,
            &head,
            &main_sha,
        )
        .unwrap()
        .len(),
        1,
        "a canonical TaskRecorded retirement is cleanup-only for receipt-only SmartClaw"
    );

    let mut cleanup_only = receipt_only.clone();
    let cleanup_terminal = events
        .iter()
        .find(|event| event.kind == "ManagedWakeTerminated")
        .unwrap()
        .clone();
    let cleanup_release = orch_host::sites::workspace_release_for_termination(
        &cleanup_only,
        "r83",
        &cleanup_terminal,
    )
    .unwrap()
    .unwrap();
    cleanup_only.push(cleanup_terminal);
    cleanup_only.push(cleanup_release);
    assert_eq!(
        orch_host::generic_review::validate_generic_review_facts(
            &root,
            &cleanup_only,
            "r83",
            task,
            attempt,
            &head,
            &main_sha,
        )
        .unwrap()
        .len(),
        1,
        "post-record terminal/release is cleanup-only and cannot rewrite the receipt binding"
    );
    cleanup_only.pop();
    assert!(orch_host::generic_review::validate_generic_review_facts(
        &root,
        &cleanup_only,
        "r83",
        task,
        attempt,
        &head,
        &main_sha,
    )
    .is_err());
    let mut rejection_then_receipt = receipt_only.clone();
    let receipt_position = rejection_then_receipt
        .iter()
        .position(|event| event.kind == "AgentEventReceived")
        .unwrap();
    rejection_then_receipt.insert(
        receipt_position,
        orch_host::ledger::event(
            "ActionRejected",
            "runtime:orch",
            Some(task),
            Some("r83"),
            serde_json::json!({
                "actionId": wake_id, "attemptId": attempt, "attemptNo": 1,
                "operation": "wake-backend-receipt", "exitCode": 5,
                "alert": true, "reason": "receipt observation timed out"
            }),
        ),
    );
    assert_eq!(
        orch_host::generic_review::validate_generic_review_facts(
            &root,
            &rejection_then_receipt,
            "r83",
            task,
            attempt,
            &head,
            &main_sha,
        )
        .unwrap()
        .len(),
        1,
        "a later exact accepted receipt supersedes an earlier observation rejection"
    );

    let mut conflicting_receipt = receipt_only.clone();
    let mut conflict = receipt.clone();
    conflict.event_id = ulid::Ulid::new().to_string();
    conflict.payload.as_mut().unwrap()["backendState"] = serde_json::json!("rejected");
    let root_position = conflicting_receipt
        .iter()
        .position(|event| event.kind == "VerdictIssued")
        .unwrap();
    conflicting_receipt.insert(root_position, conflict);
    assert!(orch_host::generic_review::validate_generic_review_facts(
        &root,
        &conflicting_receipt,
        "r83",
        task,
        attempt,
        &head,
        &main_sha,
    )
    .is_err());

    let mut late_terminal = events.clone();
    let terminal_position = late_terminal
        .iter()
        .position(|event| event.kind == "ManagedWakeTerminated")
        .unwrap();
    let terminal = late_terminal.remove(terminal_position);
    late_terminal.push(terminal);
    assert!(orch_host::generic_review::validate_generic_review_facts(
        &root,
        &late_terminal,
        "r83",
        task,
        attempt,
        &head,
        &main_sha,
    )
    .is_err());

    let mut zero_pre_review = events
        .iter()
        .filter(|event| {
            matches!(
                event.kind.as_str(),
                "DispatchIssued" | "ReportCollectCompleted" | "VerdictIssued"
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    zero_pre_review.push(
        events
            .iter()
            .find(|event| event.kind == "WakeIssued")
            .unwrap()
            .clone(),
    );
    assert!(orch_host::generic_review::validate_generic_review_facts(
        &root,
        &zero_pre_review,
        "r83",
        task,
        attempt,
        &head,
        &main_sha,
    )
    .is_err());
    let _ = fs::remove_dir_all(root);
}
