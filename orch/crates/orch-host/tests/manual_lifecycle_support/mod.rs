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
        .join(format!("v3-lifecycle-{}-{seq}", std::process::id()))
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("ORCH_MAIN_GUARD_CONTEXT", "v3-lifecycle-test")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
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
            "commit",
            "-q",
            "-m",
            message,
        ],
    );
    git(root, &["rev-parse", "HEAD"])
}

fn fixture_binding(recover_post_merge: bool) -> String {
    let source =
        fs::read_to_string(source_root().join("coordination/PROJECT-BINDING.yaml")).unwrap();
    let mut value: serde_yaml::Value = serde_yaml::from_str(&source).unwrap();
    let commands = value["commands"].as_mapping_mut().unwrap();
    for command in commands.values_mut() {
        command["argv"] = serde_yaml::to_value(vec!["/usr/bin/true"]).unwrap();
        command["timeoutSeconds"] = serde_yaml::to_value(30_u64).unwrap();
    }
    if !recover_post_merge {
        return serde_yaml::to_string(&value).unwrap();
    }
    let test_fast = commands
        .get_mut(serde_yaml::Value::String("testFast".to_string()))
        .unwrap();
    test_fast["argv"] = serde_yaml::to_value(vec![
        "/bin/sh",
        "-c",
        "set -- $(git rev-list --parents -n 1 HEAD); test $# -eq 2",
    ])
    .unwrap();
    serde_yaml::to_string(&value).unwrap()
}

fn append_generic_review(
    root: &Path,
    task: &str,
    attempt: &str,
    head: &str,
    harness: &str,
    ordinal: u32,
    verdict: &str,
) {
    let wake_id = format!("01a0b901-1111-4222-8333-{ordinal:012}");
    let worktree_rel = format!(".worktrees/review-{attempt}-review-{harness}-g01");
    let worktree = root.join(&worktree_rel);
    git(root, &["worktree", "add", "--detach", &worktree_rel, head]);
    let target_rel = format!("orch/target/review-{attempt}-review-{harness}-g01");
    fs::create_dir_all(root.join(&target_rel)).unwrap();
    let inbox_rel = format!("coordination/runtime/review-inbox/r83/{attempt}-review-{harness}.md");
    let inbox = root.join(&inbox_rel);
    fs::create_dir_all(inbox.parent().unwrap()).unwrap();
    let body = format!("substantive {harness} {verdict} result");
    let artifact = format!(
        "---\ntaskId: {task}\nround: r83\nattemptId: {attempt}\nrole: review\nreviewer: {harness}\nverdict: {verdict}\nreviewedHead: {head}\nwakeId: {wake_id}\n---\n{body}\n"
    );
    fs::write(&inbox, artifact.as_bytes()).unwrap();
    let sha = hex::encode(Sha256::digest(artifact.as_bytes()));
    let digest = |offset: u32| format!("{:064x}", ordinal * 100 + offset);
    let request_digest = digest(1);
    let rendered_digest = digest(2);
    let config_digest = digest(3);
    let attachment_digest = digest(4);
    let command_digest = digest(5);
    let executable_identity_digest = digest(6);
    let continuation = format!("review:r83:{task}:{attempt}:review:{harness}");
    let binding = serde_json::json!({
        "configDigest": config_digest,
        "requestDigest": request_digest,
        "attachmentManifestSha256": attachment_digest,
        "commandDigest": command_digest,
        "executableIdentityDigest": executable_identity_digest,
        "requestedTuple": {"provider": null, "model": null, "effort": null, "mode": null},
        "effectiveTuple": {"provider": null, "model": null, "effort": null, "mode": null},
        "driver": "claude",
        "harness": harness,
        "observationSource": "claude-stream-json",
        "invocationCwd": worktree,
        "cwdSelection": "target-worktree",
        "fixedHead": head,
    });
    let lease = orch_host::ledger::event(
        "WorkspaceLeased",
        "runtime:orch",
        Some(task),
        Some("r83"),
        serde_json::json!({
            "siteId": format!("{task}-review-{harness}-g01"), "generation": 1,
            "attemptId": attempt, "role": "review", "agent": harness,
            "reviewedHead": head, "wakeId": wake_id,
            "paths": {"worktree": worktree_rel, "target": target_rel}
        }),
    );
    let mut wake_payload = binding.clone();
    wake_payload.as_object_mut().unwrap().extend(
        serde_json::json!({
            "method": "unified-channel-v1", "action": "review",
            "attemptId": attempt, "agent": harness, "wakeId": wake_id,
            "controlWakeId": wake_id, "continuationId": continuation,
            "harnessId": "claude", "providerKind": null,
            "terminalCapability": "derived", "backendState": "managed",
            "executable": "/usr/bin/true",
            "requestMessageSha256": request_digest,
            "renderedMessageSha256": rendered_digest,
            "requestedProvider": null, "requestedModel": null,
            "requestedEffort": null, "requestedMode": null,
            "requestSessionId": null, "probeOffset": 0,
            "logPath": root.join(format!("coordination/runtime/logs/{wake_id}.jsonl")),
            "reviewOutputPath": inbox,
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
            "attemptId": attempt, "role": "review", "agent": harness,
            "harness": harness, "wakeId": wake_id, "reviewedHead": head,
            "deadlineSecs": 1800, "requestedAt": "2026-09-01T00:00:00Z"
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
            "mechanicalTerminalAbsent": false,
            "exitedNaturally": true, "hardDeadlineReached": false,
            "cancelRequestId": null, "signals": [], "logBytesRead": 1,
            "outcomeClass": "DeliveredTerminal",
            "outputPath": inbox, "outputSha256": sha,
            "finalTextSha256": hex::encode(Sha256::digest(body.as_bytes())),
            "channelBinding": binding
        }),
    );
    let released = orch_host::ledger::event(
        "WorkspaceReleased",
        "runtime:orch",
        Some(task),
        Some("r83"),
        serde_json::json!({
            "siteId": format!("{task}-review-{harness}-g01"), "generation": 1,
            "attemptId": attempt, "role": "review", "agent": harness,
            "wakeId": wake_id, "terminationEventId": terminal.event_id,
            "completionReceipt": "runtime:orch/managed-wake-terminated"
        }),
    );
    let canonical_rel = format!("coordination/rounds/r83/reviews/{attempt}-review-{harness}.md");
    let mut facts = vec![lease, wake, request.clone(), terminal.clone(), released];
    if ordinal == 2 {
        fs::create_dir_all(root.join(&canonical_rel).parent().unwrap()).unwrap();
        fs::write(root.join(&canonical_rel), artifact.as_bytes()).unwrap();
        facts.push(orch_host::ledger::event(
            "ReviewDelivered",
            "runtime:orch",
            Some(task),
            Some("r83"),
            serde_json::json!({
                "attemptId": attempt, "role": "review", "agent": harness,
                "harness": harness, "wakeId": wake_id, "reviewedHead": head,
                "requestEventId": request.event_id, "terminalEventId": terminal.event_id,
                "path": canonical_rel, "sha256": sha, "bytes": artifact.len() as u64,
                "bodyLen": body.len() as u64, "verdict": verdict
            }),
        ));
    }
    orch_host::ledger::append(root, "r83", &facts).unwrap();
    let main_before_delivery = git(root, &["rev-parse", "refs/heads/main"]);
    let delivered =
        orch_host::generic_review::deliver_generic_review(root, task, attempt, harness, &wake_id)
            .unwrap();
    assert!(!delivered.replayed);
    assert_ne!(delivered.commit_sha, main_before_delivery);
    assert_eq!(
        fs::read(root.join(&delivered.path)).unwrap(),
        artifact.as_bytes()
    );

    fs::remove_file(&inbox).unwrap();
    let ledger_path = root.join("coordination/rounds/r83/events.jsonl");
    let ledger_before = fs::read(&ledger_path).unwrap();
    let head_before = git(root, &["rev-parse", "HEAD"]);
    let replayed =
        orch_host::generic_review::deliver_generic_review(root, task, attempt, harness, &wake_id)
            .unwrap();
    assert!(replayed.replayed);
    assert_eq!(replayed.delivery_event_id, delivered.delivery_event_id);
    assert_eq!(replayed.commit_sha, head_before);
    assert_eq!(fs::read(&ledger_path).unwrap(), ledger_before);
    assert_eq!(git(root, &["rev-parse", "HEAD"]), head_before);
}

/// Run the real manual lifecycle with synthetic review facts and local fixture gates.
/// The original recovery branch remains intact; the normal branch adds live-site GC protection.
pub fn exercise(recover_post_merge: bool, after_seal: impl FnOnce(&Path, &Path)) {
    let root = temp_root();
    fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("coordination/BOARD.md"), "# fixture\n").unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn value() -> u32 { 1 }\n").unwrap();
    fs::create_dir_all(root.join("coordination/adapters")).unwrap();
    fs::write(root.join("coordination/agents.yaml"), "agents: {}\n").unwrap();
    fs::write(root.join("coordination/harnesses.yaml"), "harnesses: {}\n").unwrap();
    fs::write(
        root.join("coordination/adapters/fixture.yaml"),
        "kind: fixture\n",
    )
    .unwrap();
    fs::write(
        root.join("coordination/PROJECT-BINDING.yaml"),
        fixture_binding(recover_post_merge),
    )
    .unwrap();
    fs::write(
        root.join(".gitignore"),
        ".worktrees/\n.cowork-temp/\ncoordination/runtime/\ncoordination/rounds/*/dispatch/\norch/target/\n",
    )
    .unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    commit_all(&root, "fixture base");

    orch_host::round::run_open_v3(&root, "r83", "lifecycle fixture", false).unwrap();
    fs::write(
        root.join("coordination/rounds/r83/tasks/B901.md"),
        "---\n\
schemaVersion: 3\n\
taskId: B901\n\
round: r83\n\
seedProtocol: verify-only\n\
redForm: assertion\n\
dependsOn: []\n\
entryPoints: [src/lib.rs]\n\
seeds: []\n\
writeSet: [src/lib.rs]\n\
frozenPaths: [coordination/rounds/**]\n\
gates: {fast: [testFast, testExclusive, check]}\n\
requiredEvidence: [proof]\n\
---\n# B901\n",
    )
    .unwrap();
    orch_host::plan::run_plan(&root).unwrap();
    orch_host::round::run_sign_off(&root, Some("approved actorless fixture")).unwrap();
    assert!(matches!(
        orch_host::consult::consultation_admitted(&root).unwrap(),
        orch_host::consult::GateDecision::Admit { basis } if basis.contains("schema=3")
    ));
    commit_all(&root, "open and sign actorless fixture");

    let dispatched = orch_host::tierf::run_dispatch_local(&root, "B901").unwrap();
    let worktree = root.join(&dispatched.worktree_rel);
    if !recover_post_merge {
        let before = fs::read(worktree.join("src/lib.rs")).unwrap();
        let active_gc = orch_host::sites::reap_released_sites(&root, "r83").unwrap();
        assert!(
            active_gc.removed.is_empty(),
            "active implementation was reclaimed"
        );
        assert!(worktree.exists(), "live dispatch worktree must survive GC");
        assert_eq!(fs::read(worktree.join("src/lib.rs")).unwrap(), before);
    }
    fs::write(worktree.join("src/lib.rs"), "pub fn value() -> u32 { 2 }\n").unwrap();
    git(&worktree, &["add", "src/lib.rs"]);
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
            "feat: implement B901",
        ],
    );
    let implementation_sha = git(&worktree, &["rev-parse", "HEAD"]);
    let report_rel = "coordination/rounds/r83/reports/B901-REPORT.md";
    fs::create_dir_all(worktree.join(report_rel).parent().unwrap()).unwrap();
    fs::write(
        worktree.join(report_rel),
        format!(
            "---\ntaskId: B901\nagent: local\nbranch: task/B901\nheadSha: {implementation_sha}\nwroteAt: 2026-09-01T00:00:00Z\n---\n\
## 0 执行环境自报\nMODEL=local\nDEPTH=local\nCAPTURE=fixture\n\
## 1 变更文件清单\nsrc/lib.rs\n\
## 2 提交序列\nimplementation then report\n\
## 3 种子搬运证据\nverify-only: no seed\n\
## 4 快门实测\nfixture gates green\n\
## 5 负向变异自证\nfixture mutation restored\n\
## 6 我可能做错的地方\nfixture deliberately minimizes product code\n"
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
            "report: finish B901",
        ],
    );
    let candidate = git(&worktree, &["rev-parse", "HEAD"]);
    let collected = orch_host::tierf::run_await(&root, "B901", 10, None).unwrap();
    assert!(matches!(
        collected,
        orch_host::tierf::AwaitOutcome::Collected(_)
    ));

    append_generic_review(
        &root,
        "B901",
        &dispatched.attempt_id,
        &candidate,
        "executor-claw",
        1,
        "PASS",
    );
    append_generic_review(
        &root,
        "B901",
        &dispatched.attempt_id,
        &candidate,
        "executor-opencode",
        2,
        "FAIL",
    );

    let review_ledger =
        orch_core::read_ledger(&root.join("coordination/rounds/r83/events.jsonl")).unwrap();
    let review_main = git(&root, &["rev-parse", "HEAD"]);
    let bindings = orch_host::generic_review::validate_generic_review_facts(
        &root,
        &review_ledger.events,
        "r83",
        "B901",
        &dispatched.attempt_id,
        &candidate,
        &review_main,
    )
    .unwrap();
    assert_eq!(bindings.len(), 2);
    assert!(bindings.iter().any(|binding| binding.verdict == "PASS"));
    assert!(bindings.iter().any(|binding| binding.verdict == "FAIL"));

    let mut nonanswered = review_ledger.events.clone();
    let failed_wake = nonanswered
        .iter()
        .find(|event| {
            event.kind == "ManagedWakeTerminated"
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("agent"))
                    .and_then(serde_json::Value::as_str)
                    == Some("executor-claw")
        })
        .and_then(|event| {
            event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("wakeId"))
                .and_then(serde_json::Value::as_str)
        })
        .unwrap()
        .to_string();
    let terminal = nonanswered
        .iter_mut()
        .find(|event| {
            event.kind == "ManagedWakeTerminated"
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("wakeId"))
                    .and_then(serde_json::Value::as_str)
                    == Some(failed_wake.as_str())
        })
        .unwrap();
    let terminal_payload = terminal.payload.as_mut().unwrap();
    terminal_payload["state"] = serde_json::json!("failed");
    terminal_payload["completionReason"] = serde_json::json!("operational-error");
    terminal_payload["outcomeClass"] = serde_json::json!("OperationalError");
    terminal_payload["terminalSeen"] = serde_json::json!(false);
    terminal_payload["turnEnded"] = serde_json::json!(false);
    terminal_payload["exitedNaturally"] = serde_json::json!(false);
    terminal_payload["outputPath"] = serde_json::Value::Null;
    terminal_payload["outputSha256"] = serde_json::Value::Null;
    terminal_payload["finalTextSha256"] = serde_json::Value::Null;
    nonanswered.retain(|event| {
        !(event.kind == "ReviewDelivered"
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("wakeId"))
                .and_then(serde_json::Value::as_str)
                == Some(failed_wake.as_str()))
    });
    assert_eq!(
        orch_host::generic_review::validate_generic_review_facts(
            &root,
            &nonanswered,
            "r83",
            "B901",
            &dispatched.attempt_id,
            &candidate,
            &review_main,
        )
        .unwrap()
        .len(),
        1,
        "a trusted failed terminal closes its request without fabricating an artifact"
    );

    let mut missing_release = review_ledger.events.clone();
    let release_position = missing_release
        .iter()
        .position(|event| event.kind == "WorkspaceReleased")
        .unwrap();
    missing_release.remove(release_position);
    assert!(orch_host::generic_review::validate_generic_review_facts(
        &root,
        &missing_release,
        "r83",
        "B901",
        &dispatched.attempt_id,
        &candidate,
        &review_main,
    )
    .is_err());

    let mut drifted_binding = review_ledger.events.clone();
    let request = drifted_binding
        .iter_mut()
        .find(|event| event.kind == "ReviewRequested")
        .unwrap();
    request.payload.as_mut().unwrap()["configDigest"] = serde_json::json!("f".repeat(64));
    assert!(orch_host::generic_review::validate_generic_review_facts(
        &root,
        &drifted_binding,
        "r83",
        "B901",
        &dispatched.attempt_id,
        &candidate,
        &review_main,
    )
    .is_err());

    let mut smuggled_delivery = review_ledger.events.clone();
    let delivery = smuggled_delivery
        .iter_mut()
        .find(|event| event.kind == "ReviewDelivered")
        .unwrap();
    delivery.payload.as_mut().unwrap()["panelId"] = serde_json::json!("legacy-panel");
    assert!(orch_host::generic_review::validate_generic_review_facts(
        &root,
        &smuggled_delivery,
        "r83",
        "B901",
        &dispatched.attempt_id,
        &candidate,
        &review_main,
    )
    .is_err());

    let evidence = root.join("coordination/rounds/r83/evidence/B901-proof.json");
    fs::create_dir_all(evidence.parent().unwrap()).unwrap();
    fs::write(&evidence, b"{\"kind\":\"fixture-proof\"}\n").unwrap();
    let expected_main = commit_all(&root, "bind collect and evidence");
    orch_host::verify::run_root_verdict(
        &root,
        "B901",
        &dispatched.attempt_id,
        &candidate,
        &expected_main,
        orch_host::verify::RootVerdict::Pass,
        None,
        true,
    )
    .unwrap();
    let verdict = orch_host::verify::run_root_verdict(
        &root,
        "B901",
        &dispatched.attempt_id,
        &candidate,
        &expected_main,
        orch_host::verify::RootVerdict::Pass,
        None,
        false,
    )
    .unwrap();
    assert!(verdict.appended);
    if recover_post_merge {
        let seal_error =
            orch_host::close::run_seal(&root, "B901", &dispatched.attempt_id, &candidate)
                .unwrap_err()
                .to_string();
        assert!(
            seal_error.contains("合并后门") && seal_error.contains("红"),
            "{seal_error}"
        );
        let merge_sha = git(&root, &["rev-parse", "main"]);
        fs::write(root.join("repair.ok"), "postmerge repaired\n").unwrap();
        git(&root, &["add", "repair.ok"]);
        git(
            &root,
            &[
                "-c",
                "user.name=orch-test",
                "-c",
                "user.email=orch@test.invalid",
                "commit",
                "-q",
                "-m",
                "repair schema3 postmerge gate",
            ],
        );
        let tip_sha = git(&root, &["rev-parse", "HEAD"]);
        let recovery = orch_host::close::run_record_at_tip(&root, "B901").unwrap();
        let proof = recovery.relaxation.expect("record-at-tip relaxation proof");
        assert_eq!(proof.merge_sha, merge_sha);
        assert_eq!(proof.tip_sha, tip_sha);
        assert_eq!(proof.files, vec!["repair.ok".to_string()]);
    } else {
        let sealed =
            orch_host::close::run_seal(&root, "B901", &dispatched.attempt_id, &candidate).unwrap();
        assert!(!sealed.replayed_complete);
    }

    let ledger =
        orch_core::read_ledger(&root.join("coordination/rounds/r83/events.jsonl")).unwrap();
    for kind in [
        "VerdictIssued",
        "MergeStarted",
        "MergeExecuted",
        "TaskRecorded",
    ] {
        assert_eq!(
            ledger
                .events
                .iter()
                .filter(|event| event.kind == kind && event.task_id.as_deref() == Some("B901"))
                .count(),
            1,
            "{kind}"
        );
    }
    let recorded_position = ledger
        .events
        .iter()
        .position(|event| event.kind == "TaskRecorded")
        .unwrap();
    let relaxed_position = ledger.events.iter().position(|event| {
        event.kind == "EscalationRaised"
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("stage"))
                .and_then(serde_json::Value::as_str)
                == Some("RecordGateRelaxed")
    });
    let retired_positions = ledger
        .events
        .iter()
        .enumerate()
        .filter_map(|(position, event)| {
            (event.kind == "SiteRetired" && event.task_id.as_deref() == Some("B901"))
                .then_some(position)
        })
        .collect::<Vec<_>>();
    assert!(!retired_positions.is_empty());
    if recover_post_merge {
        let relaxed_position =
            relaxed_position.expect("original recovery must record its relaxation");
        assert!(retired_positions
            .iter()
            .all(|position| recorded_position < *position && *position < relaxed_position));
    } else {
        assert!(
            relaxed_position.is_none(),
            "normal seal must not use a recovery waiver"
        );
        assert!(retired_positions
            .iter()
            .all(|position| recorded_position < *position));
    }
    orch_host::verify::validate_archived_record_chain(&root, "r83", "B901", &ledger.events)
        .unwrap();
    let audit = orch_host::verify::audit_recorded_review_bindings(&root).unwrap();
    assert!(audit
        .findings
        .iter()
        .all(|finding| { finding.level != orch_host::verify::ReviewBindingAuditLevel::Fail }));
    let replay =
        orch_host::close::run_seal(&root, "B901", &dispatched.attempt_id, &candidate).unwrap();
    assert!(replay.replayed_complete);
    after_seal(&root, &worktree);
    let moved = root.with_file_name(format!(
        "{}-moved",
        root.file_name().unwrap().to_string_lossy()
    ));
    if moved.exists() {
        fs::remove_dir_all(&moved).unwrap();
    }
    fs::rename(&root, &moved).unwrap();
    let moved_ledger =
        orch_core::read_ledger(&moved.join("coordination/rounds/r83/events.jsonl")).unwrap();
    orch_host::verify::validate_archived_record_chain(&moved, "r83", "B901", &moved_ledger.events)
        .unwrap();
    let _ = fs::remove_dir_all(moved);
}
