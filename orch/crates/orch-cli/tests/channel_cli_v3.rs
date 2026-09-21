#![cfg(feature = "selfhost")]
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn source_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap()
        .to_path_buf()
}

fn temp_root(label: &str) -> PathBuf {
    source_root().join("orch/target/test-tmp").join(format!(
        "B320-channel-cli-{label}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
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
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_orch"))
        .arg("--root")
        .arg(root)
        .arg("--allow-stale-binary")
        .args(args)
        .output()
        .unwrap()
}

fn last_consult_log_and_meta(root: &Path) -> (serde_json::Value, serde_json::Value) {
    let log = fs::read_to_string(root.join("coordination/consultations/log.jsonl")).unwrap();
    let last: serde_json::Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
    let meta = serde_json::from_slice(
        &fs::read(root.join(last["dir"].as_str().unwrap()).join("meta.json")).unwrap(),
    )
    .unwrap();
    (last, meta)
}

fn remove_file_if_present(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("remove {}: {error}", path.display()),
    }
}

fn remove_dir_if_present(path: &Path) {
    match fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("remove {}: {error}", path.display()),
    }
}

fn fixture() -> PathBuf {
    fixture_with_signoff(true)
}

fn fixture_with_signoff(signed: bool) -> PathBuf {
    let root = temp_root("fixture");
    let _ = fs::remove_dir_all(&root);
    let output = Command::new("git")
        // Set these in the new repository before clone/checkout can spawn
        // detached maintenance. Fixture teardown owns all of its writers.
        .args([
            "clone",
            "-q",
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
        ])
        .arg(source_root())
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "clone: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    git(&root, &["checkout", "-q", "-B", "main", "origin/main"]);
    // The source checkout may have completed any real round.  Clone code and
    // immutable history, but create fresh protocol inputs for this fixture.
    fs::create_dir_all(root.join("coordination/runtime")).unwrap();

    remove_file_if_present(&root.join("coordination/agents.yaml"));
    remove_file_if_present(&root.join("coordination/harnesses.yaml"));
    remove_dir_if_present(&root.join("coordination/adapters"));
    remove_dir_if_present(&root.join("coordination/modes"));
    assert!(!root.join("coordination/agents.yaml").exists());
    assert!(!root.join("coordination/harnesses.yaml").exists());
    assert!(!root.join("coordination/adapters").exists());
    assert!(!root.join("coordination/modes").exists());
    git(&root, &["add", "-A"]);
    if !git(&root, &["status", "--porcelain"]).is_empty() {
        git(
            &root,
            &[
                "-c",
                "user.name=orch-test",
                "-c",
                "user.email=orch@test.invalid",
                "commit",
                "-qm",
                "remove live legacy invocation truth",
            ],
        );
    }

    assert!(!root.join("coordination/rounds/r9004").exists());
    orch_host::round::run_open_v3(&root, "r9004", "isolated channel fixture", false).unwrap();
    fs::write(
        root.join("coordination/rounds/r9004/tasks/B320.md"),
        "---\nschemaVersion: 3\ntaskId: B320\nround: r9004\n\
seedProtocol: verify-only\nredForm: assertion\ndependsOn: []\n\
entryPoints: [orch/crates/orch-host/src/harness_config.rs]\nseeds: []\n\
writeSet: [orch/crates/orch-host/src/harness_config.rs]\n\
frozenPaths: [coordination/rounds/**]\n\
gates: {fast: [testFast, testExclusive, check, checkDefault, buildDefault, buildSelfhost]}\n\
requiredEvidence: [fixture-proof]\n---\n# Synthetic channel fixture\n",
    )
    .unwrap();
    orch_host::plan::run_plan(&root).unwrap();
    if signed {
        orch_host::round::run_sign_off(&root, Some("isolated test fixture only")).unwrap();
    }
    git(
        &root,
        &["add", "coordination/rounds/r9004", "coordination/BOARD.md"],
    );
    git(
        &root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "commit",
            "-qm",
            if signed {
                "create independent signed channel fixture"
            } else {
                "create independent unsigned channel fixture"
            },
        ],
    );

    let local = root.join(".orch");
    fs::create_dir(&local).unwrap();
    let executable = local.join("fake-provider.sh");
    fs::write(
        &executable,
        b"#!/bin/sh\nprintf '%s\\n' '{\"type\":\"step_start\",\"sessionID\":\"session-alpha\",\"part\":{\"type\":\"step-start\",\"modelID\":\"model-a\"}}'\nprintf '%s\\n' '{\"type\":\"text\",\"part\":{\"text\":\"done\"}}'\nprintf '%s\\n' '{\"type\":\"step_finish\",\"part\":{\"type\":\"step-finish\",\"reason\":\"stop\"}}'\n/bin/sleep 2\n",
    )
    .unwrap();
    // Pin the concrete test interpreter, not Darwin's /bin/sh dispatcher.
    #[cfg(target_os = "macos")]
    {
        let original = fs::read(&executable).unwrap();
        let mut stable = b"#!/bin/bash\n".to_vec();
        stable.extend_from_slice(original.strip_prefix(b"#!/bin/sh\n").unwrap());
        fs::write(&executable, stable).unwrap();
    }
    let mut permissions = fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&executable, permissions).unwrap();
    fs::write(
        local.join("harnesses.yaml"),
        format!(
            "version: 1\nharnesses:\n  alpha:\n    driver: opencode\n    executable: {}\n    enabled: true\n    defaults: {{provider: local, model: model-a, effort: high}}\n    cwdPolicy: project-root\n",
            executable.display()
        ),
    )
    .unwrap();
    root
}

fn assert_b320_is_undispatched(root: &Path) {
    let ledger =
        orch_core::read_ledger(&root.join("coordination/rounds/r9004/events.jsonl")).unwrap();
    assert!(ledger.bad_lines.is_empty());
    assert!(!ledger.events.iter().any(|event| {
        event.task_id.as_deref() == Some("B320")
            && matches!(event.kind.as_str(), "DispatchIssued" | "WorkspaceLeased")
    }));
}

#[test]
fn channel_fixture_has_its_own_round_and_preserves_copied_history() {
    let source = source_root();
    let historical = fs::read(source.join("coordination/rounds/r83/events.jsonl")).unwrap();
    let committed = Command::new("git")
        .arg("-C")
        .arg(&source)
        .args(["show", "main:coordination/rounds/r83/events.jsonl"])
        .output()
        .unwrap();
    assert!(committed.status.success());
    let root = fixture_with_signoff(false);
    let round = fs::read_to_string(root.join("coordination/runtime/CURRENT-ROUND")).unwrap();
    let independent = !source
        .join("coordination/rounds")
        .join(round.trim())
        .exists();
    let copied_history = fs::read(root.join("coordination/rounds/r83/events.jsonl")).unwrap();
    let source_after = fs::read(source.join("coordination/rounds/r83/events.jsonl")).unwrap();
    fs::remove_dir_all(root).unwrap();
    assert!(
        independent,
        "channel fixture selected the source repository's real round"
    );
    assert_eq!(
        copied_history, committed.stdout,
        "fixture rewrote copied historical events"
    );
    assert_eq!(
        source_after, historical,
        "fixture changed the source reference ledger"
    );
}

#[test]
fn schema3_wake_uses_only_local_config_and_records_one_snapshot() {
    let root = fixture();
    let list = run(&root, &["harness", "list"]);
    assert!(list.status.success());
    let list_stdout = String::from_utf8(list.stdout).unwrap();
    let config_digest = list_stdout
        .split("sha256=")
        .nth(1)
        .and_then(|suffix| suffix.split_whitespace().next())
        .unwrap()
        .to_string();
    let output = run(&root, &["wake", "alpha", "--message", "answer once"]);
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let round = fs::read_to_string(root.join("coordination/runtime/CURRENT-ROUND"))
        .unwrap()
        .trim()
        .to_string();
    let read =
        orch_core::read_ledger(&root.join(format!("coordination/rounds/{round}/events.jsonl")))
            .unwrap();
    let wakes = read
        .events
        .iter()
        .filter(|event| {
            event.kind == "WakeIssued"
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("harness"))
                    .and_then(serde_json::Value::as_str)
                    == Some("alpha")
        })
        .collect::<Vec<_>>();
    let [wake] = wakes.as_slice() else {
        panic!("expected one alpha WakeIssued, got {}", wakes.len());
    };
    let payload = wake.payload.as_ref().unwrap();
    assert_eq!(payload["action"], "consult");
    assert_eq!(payload["driver"], "opencode");
    assert_eq!(payload["configDigest"], config_digest);
    assert_eq!(payload["method"], "unified-channel-v1");
    assert!(payload["attemptId"].is_null());
    assert!(payload["continuationId"]
        .as_str()
        .unwrap()
        .starts_with("manual:r9004:"));
    for key in [
        "requestDigest",
        "attachmentManifestSha256",
        "commandDigest",
        "executableIdentityDigest",
    ] {
        let value = payload[key].as_str().unwrap();
        assert_eq!(value.len(), 64, "{key}");
    }
    assert!(!root.join("coordination/agents.yaml").exists());
    assert!(!root.join("coordination/harnesses.yaml").exists());
    assert!(!root.join("coordination/adapters").exists());
    assert!(!root.join("coordination/modes").exists());
    std::thread::sleep(Duration::from_secs(8));
    let ledger_path = root.join("coordination/rounds/r9004/events.jsonl");
    let before_dry = fs::read(&ledger_path).unwrap();
    let preview = run(&root, &["sites", "gc", "--round", "r9004", "--dry-run"]);
    assert!(preview.status.success());
    assert_eq!(
        fs::read(&ledger_path).unwrap(),
        before_dry,
        "dry-run cannot recover even a real pending receipt"
    );
    let applied = run(&root, &["sites", "gc", "--round", "r9004"]);
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let reconciled =
        orch_core::read_ledger(&root.join(format!("coordination/rounds/{round}/events.jsonl")))
            .unwrap();
    let receipt = reconciled
        .events
        .iter()
        .find(|event| {
            event.kind == "AgentEventReceived"
                && event.task_id.is_none()
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("wakeId"))
                    .and_then(serde_json::Value::as_str)
                    == payload["wakeId"].as_str()
        })
        .expect("manual wake must reconcile one taskless receipt");
    assert!(receipt.payload.as_ref().unwrap()["attemptId"].is_null());
    assert_eq!(
        receipt.payload.as_ref().unwrap()["channelBinding"]["requestDigest"],
        payload["requestDigest"]
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dispatch_xor_and_missing_member_schema_reject_without_side_effects() {
    let root = fixture();
    let round = fs::read_to_string(root.join("coordination/runtime/CURRENT-ROUND"))
        .unwrap()
        .trim()
        .to_string();
    let ledger = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let before = fs::read(&ledger).unwrap();
    let dispatch = run(&root, &["dispatch", "B320"]);
    assert!(!dispatch.status.success());
    assert!(String::from_utf8_lossy(&dispatch.stderr).contains("--local 或 --harness"));
    assert_eq!(fs::read(&ledger).unwrap(), before);

    let question = root.join(".orch/question.md");
    fs::write(&question, "question\n").unwrap();
    let consult = run(
        &root,
        &[
            "consult",
            question.to_str().unwrap(),
            "--harness",
            "missing",
        ],
    );
    assert!(!consult.status.success());
    let (last, meta) = last_consult_log_and_meta(&root);
    assert_eq!(last["membersOk"], 0);
    assert_eq!(meta["members"][0]["status"], "failed");
    assert_eq!(
        meta["members"][0]["channelFacts"]["stage"],
        "prepare-or-render-rejected"
    );
    let failed_manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(
            root.join(last["dir"].as_str().unwrap())
                .join("fusion/0-missing.manifest.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(failed_manifest["status"], "failed");
    assert!(failed_manifest["invocation"].is_null());
    assert_eq!(failed_manifest["artifact"]["bytes"], 0);
    assert_eq!(fs::read(&ledger).unwrap(), before);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn harness_dispatch_keeps_local_owner_and_replays_without_a_second_spawn() {
    let root = fixture();
    assert_b320_is_undispatched(&root);
    // A valid provider may need more than one second to produce its receipt.
    // Schema 3 has no signed wallMinutes; its absent legacy field is not a SLA.
    let executable = root.join(".orch/fake-provider.sh");
    let script = fs::read_to_string(&executable).unwrap();
    fs::write(
        &executable,
        {
            let (interpreter, body) = script.split_once('\n').unwrap();
            format!("{interpreter}\n/bin/sleep 2\n{body}")
        },
    )
    .unwrap();
    let first = run(&root, &["dispatch", "B320", "--harness", "alpha"]);
    assert!(
        first.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    let second = run(&root, &["dispatch", "B320", "--harness", "alpha"]);
    assert!(second.status.success());
    std::thread::sleep(Duration::from_secs(12));
    let ledger_path = root.join("coordination/rounds/r9004/events.jsonl");
    let before_dry = fs::read(&ledger_path).unwrap();
    let preview = run(&root, &["sites", "gc", "--round", "r9004", "--dry-run"]);
    assert!(preview.status.success());
    assert_eq!(
        fs::read(&ledger_path).unwrap(),
        before_dry,
        "dry-run cannot recover even a real pending receipt"
    );
    let applied = run(&root, &["sites", "gc", "--round", "r9004"]);
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let third = run(&root, &["dispatch", "B320", "--harness", "alpha"]);
    assert!(third.status.success());

    let read =
        orch_core::read_ledger(&root.join("coordination/rounds/r9004/events.jsonl")).unwrap();
    let dispatches = read
        .events
        .iter()
        .filter(|event| event.kind == "DispatchIssued" && event.task_id.as_deref() == Some("B320"))
        .collect::<Vec<_>>();
    let [dispatch] = dispatches.as_slice() else {
        panic!("expected one dispatch, got {}", dispatches.len());
    };
    let dispatch_payload = dispatch.payload.as_ref().unwrap();
    assert_eq!(dispatch_payload["agent"], "local");
    assert_eq!(dispatch_payload["method"], "harness-channel");
    assert_eq!(dispatch_payload["harness"], "alpha");

    let wakes = read
        .events
        .iter()
        .filter(|event| {
            event.kind == "WakeIssued"
                && event.task_id.as_deref() == Some("B320")
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("action"))
                    .and_then(serde_json::Value::as_str)
                    == Some("execute")
        })
        .collect::<Vec<_>>();
    assert_eq!(wakes.len(), 1, "replay must not spawn a second provider");
    let wake_payload = wakes[0].payload.as_ref().unwrap();
    assert_eq!(
        wake_payload["runtimeLimit"]["effectiveSecs"], 7200,
        "absent schema-3 wallMinutes must not become a one-second deadline"
    );
    let attempt = wake_payload["attemptId"].as_str().unwrap();
    assert_eq!(
        wake_payload["continuationId"],
        format!("implementation:r9004:B320:{attempt}:alpha")
    );
    let wake_id = wake_payload["wakeId"].as_str().unwrap();
    let receipts = read
        .events
        .iter()
        .filter(|event| {
            event.kind == "AgentEventReceived"
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("agentEvent"))
                    .and_then(serde_json::Value::as_str)
                    == Some("wake-backend-receipt")
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("wakeId"))
                    .and_then(serde_json::Value::as_str)
                    == Some(wake_id)
        })
        .collect::<Vec<_>>();
    let [receipt] = receipts.as_slice() else {
        panic!("expected one bound backend receipt, got {}", receipts.len());
    };
    let binding = &receipt.payload.as_ref().unwrap()["channelBinding"];
    assert_eq!(binding["configDigest"], wake_payload["configDigest"]);
    assert_eq!(binding["requestDigest"], wake_payload["requestDigest"]);
    assert_eq!(binding["effectiveTuple"], wake_payload["effectiveTuple"]);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn schema3_consult_uses_explicit_members_and_config_for_spawn() {
    let root = fixture();
    let question = root.join(".orch/question.md");
    let attachment_a = root.join(".orch/attachment-a.md");
    let attachment_b = root.join(".orch/attachment-b.md");
    fs::write(&question, "answer the fixture\n").unwrap();
    fs::write(&attachment_a, "first attachment\n").unwrap();
    fs::write(&attachment_b, "second attachment\n").unwrap();
    let ledger = root.join("coordination/rounds/r9004/events.jsonl");
    let before = fs::read(&ledger).unwrap();
    assert!(
        orch_core::read_ledger(&ledger)
            .unwrap()
            .events
            .iter()
            .any(|event| event.kind == "PlanSignedOff"),
        "fixture must prove consult remains live after sign-off"
    );
    let output = run(
        &root,
        &[
            "consult",
            question.to_str().unwrap(),
            "--harness",
            "alpha",
            "--attach",
            attachment_a.to_str().unwrap(),
            "--attach",
            attachment_b.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(&ledger).unwrap(), before);
    let (last, meta) = last_consult_log_and_meta(&root);
    assert_eq!(last["invocation"], "unified-channel-v1");
    assert_eq!(last["membersOk"], 1);
    assert_eq!(
        meta["attachmentManifestSha256"],
        last["attachmentManifestSha256"]
    );
    assert_eq!(meta["attachments"][0]["path"], ".orch/attachment-a.md");
    assert_eq!(meta["attachments"][1]["path"], ".orch/attachment-b.md");
    let channel = &meta["members"][0]["channelFacts"];
    assert_eq!(channel["driver"], "opencode");
    assert_eq!(channel["requestedTuple"], channel["effectiveTuple"]);
    assert_eq!(channel["effectiveTuple"]["model"], "model-a");
    assert_eq!(channel["observedTuple"]["model"], "model-a");
    assert_eq!(channel["observedTuple"]["modelMatch"], "matched");
    assert_eq!(channel["receipt"]["source"], "native");
    assert_eq!(channel["receipt"]["status"], "unknown");
    assert_eq!(channel["terminal"]["status"], "answered");
    assert_eq!(channel["terminal"]["turnEnded"], true);
    assert_eq!(
        channel["terminal"]["finalTextSha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    for key in [
        "configDigest",
        "requestDigest",
        "attachmentManifestSha256",
        "commandDigest",
        "executableIdentityDigest",
    ] {
        assert_eq!(channel[key].as_str().unwrap().len(), 64, "{key}");
    }
    let member_artifact = fs::read_to_string(
        root.join(last["dir"].as_str().unwrap())
            .join("fusion/0-alpha.md"),
    )
    .unwrap();
    assert_eq!(member_artifact, "done");
    let summary =
        fs::read_to_string(root.join(last["dir"].as_str().unwrap()).join("summary.md")).unwrap();
    assert!(summary.contains("index, not a judge or verdict"));
    assert!(
        !summary.contains("done"),
        "summary must not copy or overwrite answers"
    );
    let member_manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(
            root.join(last["dir"].as_str().unwrap())
                .join("fusion/0-alpha.manifest.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        member_manifest["invocation"]["configDigest"],
        channel["configDigest"]
    );
    assert_eq!(
        member_manifest["channelFacts"]["commandDigest"],
        channel["commandDigest"]
    );
    assert_eq!(
        member_manifest["invocation"]["orderedAttachmentManifestDigest"],
        meta["attachmentManifestSha256"]
    );
    assert_eq!(member_manifest["artifact"]["bytes"], 4);

    let executable = root.join(".orch/fake-provider.sh");
    fs::write(
        &executable,
        b"#!/bin/sh\nprintf '%s\\n' '{\"type\":\"step_start\",\"sessionID\":\"session-alpha\",\"part\":{\"type\":\"step-start\",\"modelID\":\"model-b\"}}'\nprintf '%s\\n' '{\"type\":\"text\",\"part\":{\"text\":\"done\"}}'\nprintf '%s\\n' '{\"type\":\"step_finish\",\"part\":{\"type\":\"step-finish\",\"reason\":\"stop\"}}'\n",
    )
    .unwrap();
    assert!(!run(
        &root,
        &["consult", question.to_str().unwrap(), "--harness", "alpha",],
    )
    .status
    .success());
    let (mismatch_log, mismatch_meta) = last_consult_log_and_meta(&root);
    assert_eq!(mismatch_log["membersOk"], 0);
    assert_eq!(mismatch_meta["members"][0]["status"], "failed");
    assert_eq!(
        mismatch_meta["members"][0]["channelFacts"]["observedTuple"]["modelMatch"],
        "mismatch"
    );

    fs::write(
        &executable,
        b"#!/bin/sh\nprintf '%s\\n' '{\"type\":\"step_start\",\"sessionID\":\"session-alpha\",\"part\":{\"type\":\"step-start\",\"modelID\":\"model-a\"}}'\n",
    )
    .unwrap();
    assert!(!run(
        &root,
        &["consult", question.to_str().unwrap(), "--harness", "alpha",],
    )
    .status
    .success());
    let (progress_log, progress_meta) = last_consult_log_and_meta(&root);
    assert_eq!(progress_log["membersOk"], 0);
    assert_eq!(progress_meta["members"][0]["status"], "failed");
    assert_eq!(
        progress_meta["members"][0]["channelFacts"]["terminal"]["status"],
        "empty"
    );

    fs::write(&executable, b"#!/bin/sh\n/bin/sleep 5\n").unwrap();
    let timeout_started = Instant::now();
    assert!(!run(
        &root,
        &[
            "consult",
            question.to_str().unwrap(),
            "--harness",
            "alpha",
            "--member-timeout-secs",
            "900",
            "--total-wall-secs",
            "1",
        ],
    )
    .status
    .success());
    assert!(
        timeout_started.elapsed() < Duration::from_secs(20),
        "total-wall execution plus bounded cleanup took {:?}",
        timeout_started.elapsed()
    );
    let (timeout_log, timeout_meta) = last_consult_log_and_meta(&root);
    assert_eq!(timeout_log["membersOk"], 0);
    assert_eq!(timeout_meta["members"][0]["status"], "timedOut");
    assert_eq!(
        timeout_meta["members"][0]["channelFacts"]["terminal"]["status"],
        "timedOut"
    );

    fs::write(
        root.join("coordination/scripts/wake-dclaw.sh"),
        b"#!/bin/sh\nprintf '%s\\n' '{\"type\":\"result\",\"result\":\"done\"}'\n",
    )
    .unwrap();
    fs::write(
        root.join(".orch/harnesses.yaml"),
        format!(
            "version: 1\nharnesses:\n  alpha:\n    driver: dclaw\n    executable: {}\n    enabled: true\n    cwdPolicy: project-root\n",
            executable.display()
        ),
    )
    .unwrap();
    let dclaw = run(
        &root,
        &["consult", question.to_str().unwrap(), "--harness", "alpha"],
    );
    assert!(!dclaw.status.success());
    let (dclaw_log, dclaw_meta) = last_consult_log_and_meta(&root);
    assert_eq!(dclaw_log["membersOk"], 0);
    assert_eq!(dclaw_meta["members"][0]["status"], "failed");
    assert_eq!(
        dclaw_meta["members"][0]["channelFacts"]["stage"],
        "prepare-or-render-rejected"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn schema3_consult_is_also_admitted_before_plan_signoff() {
    let root = fixture_with_signoff(false);
    let events =
        orch_core::read_ledger(&root.join("coordination/rounds/r9004/events.jsonl")).unwrap();
    assert!(!events
        .events
        .iter()
        .any(|event| event.kind == "PlanSignedOff"));
    let question = root.join(".orch/question.md");
    fs::write(&question, "answer before signoff\n").unwrap();
    let output = run(
        &root,
        &["consult", question.to_str().unwrap(), "--harness", "alpha"],
    );
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let (last, _) = last_consult_log_and_meta(&root);
    assert_eq!(last["membersOk"], 1);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn schema3_consult_rejects_a_symlink_attachment_before_artifacts() {
    let root = fixture();
    let question = root.join(".orch/question.md");
    let real = root.join(".orch/real-attachment.md");
    let linked = root.join(".orch/linked-attachment.md");
    fs::write(&question, "answer safely\n").unwrap();
    fs::write(&real, "captured bytes\n").unwrap();
    std::os::unix::fs::symlink(&real, &linked).unwrap();
    let log = root.join("coordination/consultations/log.jsonl");
    let before = fs::read(&log).unwrap_or_default();
    let output = run(
        &root,
        &[
            "consult",
            question.to_str().unwrap(),
            "--harness",
            "alpha",
            "--attach",
            linked.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("symlink"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(&log).unwrap_or_default(), before);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn schema3_consult_protocol_error_is_member_local_and_archived() {
    let root = fixture();
    let alpha = root.join(".orch/fake-provider.sh");
    let beta = root.join(".orch/bad-provider.sh");
    fs::write(
        &beta,
        b"#!/bin/sh\nprintf '%s\\n' '{\"type\":\"step_finish\",\"sessionID\":\"B\",\"part\":{\"type\":\"step-finish\",\"reason\":\"stop\"}}'\nprintf '%s\\n' '{\"type\":\"step_finish\",\"sessionID\":\"B\",\"part\":{\"type\":\"step-finish\",\"reason\":\"stop\"}}'\n",
    )
    .unwrap();
    fs::set_permissions(&beta, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        root.join(".orch/harnesses.yaml"),
        format!(
            "version: 1\nharnesses:\n  alpha:\n    driver: opencode\n    executable: {}\n    enabled: true\n    defaults: {{provider: local, model: model-a, effort: high}}\n    cwdPolicy: project-root\n  beta:\n    driver: opencode\n    executable: {}\n    enabled: true\n    defaults: {{provider: local, model: model-a, effort: high}}\n    cwdPolicy: project-root\n  gamma:\n    driver: dclaw\n    executable: {}\n    enabled: true\n    cwdPolicy: project-root\n",
            alpha.display(),
            beta.display(),
            alpha.display()
        ),
    )
    .unwrap();
    let question = root.join(".orch/question.md");
    fs::write(&question, "answer the fixture\n").unwrap();
    let output = run(
        &root,
        &[
            "consult",
            question.to_str().unwrap(),
            "--harness",
            "beta",
            "--harness",
            "gamma",
            "--harness",
            "alpha",
        ],
    );
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let (last, meta) = last_consult_log_and_meta(&root);
    assert_eq!(last["membersOk"], 1);
    assert_eq!(last["membersFailed"], 2);
    assert_eq!(meta["members"][0]["status"], "failed");
    assert_eq!(
        meta["members"][0]["channelFacts"]["terminal"]["status"],
        "protocol-error"
    );
    assert_eq!(meta["members"][1]["status"], "failed");
    assert_eq!(
        meta["members"][1]["channelFacts"]["stage"],
        "prepare-or-render-rejected"
    );
    assert_eq!(meta["members"][2]["status"], "ok");
    let first_artifact = fs::read_to_string(
        root.join(last["dir"].as_str().unwrap())
            .join("fusion/0-beta.manifest.json"),
    )
    .unwrap();
    assert!(first_artifact.contains("terminal-protocol-error"));
    let second_artifact = fs::read_to_string(
        root.join(last["dir"].as_str().unwrap())
            .join("fusion/1-gamma.manifest.json"),
    )
    .unwrap();
    assert!(second_artifact.contains("prepare-or-render-rejected"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn tool_only_and_terminal_digest_drift_do_not_erase_a_valid_sibling() {
    let root = fixture();
    let alpha = root.join(".orch/fake-provider.sh");
    let tool_only = root.join(".orch/tool-only.sh");
    let digest_drift = root.join(".orch/digest-drift.sh");
    fs::write(
        &tool_only,
        b"#!/bin/sh\nprintf '%s\n' '{\"type\":\"step_start\",\"sessionID\":\"tool\",\"part\":{\"type\":\"step-start\",\"modelID\":\"model-a\"}}'\nprintf '%s\n' '{\"type\":\"tool_use\",\"part\":{\"type\":\"tool\"}}'\nprintf '%s\n' '{\"type\":\"step_finish\",\"part\":{\"type\":\"step-finish\",\"reason\":\"stop\"}}'\n",
    )
    .unwrap();
    fs::write(
        &digest_drift,
        b"#!/bin/sh\nprintf '%s\n' '{\"type\":\"result\",\"result\":\"digest-answer\",\"finalTextSha256\":\"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee\",\"providerData\":{\"model\":\"glm-5.2\"}}'\n",
    )
    .unwrap();
    fs::set_permissions(&tool_only, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&digest_drift, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        root.join(".orch/harnesses.yaml"),
        format!(
            "version: 1\nharnesses:\n  tool:\n    driver: opencode\n    executable: {}\n    enabled: true\n    defaults: {{provider: local, model: model-a, effort: high}}\n    cwdPolicy: project-root\n  digest:\n    driver: codebuddy\n    executable: {}\n    enabled: true\n    defaults: {{model: glm-5.2, effort: high}}\n    cwdPolicy: project-root\n  alpha:\n    driver: opencode\n    executable: {}\n    enabled: true\n    defaults: {{provider: local, model: model-a, effort: high}}\n    cwdPolicy: project-root\n",
            tool_only.display(),
            digest_drift.display(),
            alpha.display(),
        ),
    )
    .unwrap();
    let question = root.join(".orch/question.md");
    fs::write(&question, "classify members\n").unwrap();
    let output = run(
        &root,
        &[
            "consult",
            question.to_str().unwrap(),
            "--harness",
            "tool",
            "--harness",
            "digest",
            "--harness",
            "alpha",
        ],
    );
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let (last, meta) = last_consult_log_and_meta(&root);
    assert_eq!(last["membersOk"], 1);
    assert_eq!(last["membersFailed"], 2);
    assert_eq!(
        meta["members"][0]["channelFacts"]["terminal"]["toolOnly"],
        true
    );
    assert!(meta["members"][0]["reason"]
        .as_str()
        .unwrap()
        .contains("tool-only"));
    assert!(meta["members"][1]["reason"]
        .as_str()
        .unwrap()
        .contains("bytes"));
    assert_eq!(meta["members"][2]["status"], "ok");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn explicit_consult_members_start_concurrently_and_keep_input_order() {
    let root = fixture();
    let marker_a = root.join(".orch/member-a.started");
    let marker_b = root.join(".orch/member-b.started");
    let script = |own: &Path, peer: &Path, session: &str, answer: &str| {
        format!(
            "#!/bin/sh\nset -eu\n: > '{}'\ni=0\nwhile [ ! -f '{}' ] && [ \"$i\" -lt 1000 ]; do i=$((i + 1)); /bin/sleep 0.01; done\n[ -f '{}' ] || exit 9\nprintf '%s\\n' '{{\"type\":\"step_start\",\"sessionID\":\"{}\",\"part\":{{\"type\":\"step-start\",\"modelID\":\"model-a\"}}}}'\nprintf '%s\\n' '{{\"type\":\"text\",\"part\":{{\"text\":\"{}\"}}}}'\nprintf '%s\\n' '{{\"type\":\"step_finish\",\"part\":{{\"type\":\"step-finish\",\"reason\":\"stop\"}}}}'\n",
            own.display(),
            peer.display(),
            peer.display(),
            session,
            answer,
        )
    };
    let executable_a = root.join(".orch/concurrent-a.sh");
    let executable_b = root.join(".orch/concurrent-b.sh");
    fs::write(
        &executable_a,
        script(&marker_a, &marker_b, "session-a", "answer-a"),
    )
    .unwrap();
    fs::write(
        &executable_b,
        script(&marker_b, &marker_a, "session-b", "answer-b"),
    )
    .unwrap();
    fs::set_permissions(&executable_a, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&executable_b, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        root.join(".orch/harnesses.yaml"),
        format!(
            "version: 1\nharnesses:\n  alpha:\n    driver: opencode\n    executable: {}\n    enabled: true\n    defaults: {{provider: local, model: model-a, effort: high}}\n    cwdPolicy: project-root\n  beta:\n    driver: opencode\n    executable: {}\n    enabled: true\n    defaults: {{provider: local, model: model-a, effort: high}}\n    cwdPolicy: project-root\n",
            executable_a.display(),
            executable_b.display(),
        ),
    )
    .unwrap();
    let question = root.join(".orch/question.md");
    fs::write(&question, "answer concurrently\n").unwrap();
    let output = run(
        &root,
        &[
            "consult",
            question.to_str().unwrap(),
            "--harness",
            "alpha",
            "--harness",
            "beta",
            "--member-timeout-secs",
            "15",
            "--total-wall-secs",
            "20",
        ],
    );
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let (last, meta) = last_consult_log_and_meta(&root);
    assert_eq!(last["membersOk"], 2, "both members must cross the barrier");
    assert_eq!(last["harnesses"], serde_json::json!(["alpha", "beta"]));
    assert_eq!(meta["membership"]["source"], "explicit");
    assert_eq!(meta["members"][0]["harness"], "alpha");
    assert_eq!(meta["members"][1]["harness"], "beta");
    assert_eq!(
        fs::read_to_string(
            root.join(last["dir"].as_str().unwrap())
                .join("fusion/0-alpha.md")
        )
        .unwrap(),
        "answer-a"
    );
    assert_eq!(
        fs::read_to_string(
            root.join(last["dir"].as_str().unwrap())
                .join("fusion/1-beta.md")
        )
        .unwrap(),
        "answer-b"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn wake_help_names_the_harness_address() {
    let output = Command::new(env!("CARGO_BIN_EXE_orch"))
        .args(["wake", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("<HARNESS>"));
}
