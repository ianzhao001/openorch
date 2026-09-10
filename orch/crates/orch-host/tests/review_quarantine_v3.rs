//! Explicit root review quarantine closes adjudication while the synthetic native site stays held.
//! No provider process is created by this fixture.
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


use orch_host::verify::{run_root_verdict_with_quarantine, RootVerdict};

struct Fixture { root: PathBuf, worktree: PathBuf, candidate: String, attempt: String }

fn fixture() -> Fixture {
    let recover_post_merge = false;
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

    Fixture { root, worktree, candidate, attempt: dispatched.attempt_id }
}

fn held_review(f: &Fixture, harness: &str, ordinal: u32) -> (String, Vec<orch_core::EventRecord>) {
    let wake_id = format!("01a0b902-1111-4222-8333-{ordinal:012}");
    let site_id = format!("B901-review-{harness}-g01");
    let worktree_rel = format!(".worktrees/{}-review-{harness}-g01", f.attempt);
    let target_rel = format!("orch/target/{}-review-{harness}-g01", f.attempt);
    git(&f.root, &["worktree", "add", "--detach", &worktree_rel, &f.candidate]);
    fs::create_dir_all(f.root.join(&target_rel)).unwrap();
    let output = f.root.join(format!("coordination/runtime/review-inbox/r83/{}-review-{harness}.md", f.attempt));
    fs::create_dir_all(output.parent().unwrap()).unwrap();
    fs::write(&output, b"untrusted old native answer; never a review delivery\n").unwrap();
    let log = format!("coordination/runtime/logs/{wake_id}.jsonl");
    fs::create_dir_all(f.root.join(&log).parent().unwrap()).unwrap();
    fs::write(f.root.join(&log), b"accepted\n").unwrap();
    let binding = serde_json::json!({
        "configDigest": "a".repeat(64), "requestDigest": "b".repeat(64),
        "attachmentManifestSha256": "c".repeat(64), "commandDigest": "d".repeat(64),
        "executableIdentityDigest": "e".repeat(64),
        "requestedTuple": {"provider": null, "model": null, "effort": null, "mode": null},
        "effectiveTuple": {"provider": null, "model": null, "effort": null, "mode": null},
        "driver": "smartclaw", "harness": harness,
        "observationSource": "dewusmartclaw-native-final-and-stream-v1",
        "invocationCwd": f.root, "cwdSelection": "project-root", "fixedHead": f.candidate
    });
    let identity = serde_json::json!({
        "attemptId": f.attempt, "agent": harness, "harness": harness, "wakeId": wake_id,
        "continuationId": format!("review:r83:B901:{}:review:{harness}", f.attempt),
        "providerKind": "smartclaw", "requestMessageSha256": "b".repeat(64),
        "renderedMessageSha256": "2".repeat(64), "requestedProvider": null,
        "requestedModel": null, "requestedEffort": null,
        "requestSessionId": format!("orch-wake-{wake_id}"), "probeOffset": 0,
        "logPath": f.root.join(&log), "backendState": "pending"
    });
    let lease = orch_host::ledger::event("WorkspaceLeased", "runtime:orch", Some("B901"), Some("r83"),
        serde_json::json!({"siteId": site_id, "generation": 1, "attemptId": f.attempt,
            "role": "review", "agent": harness, "reviewedHead": f.candidate, "wakeId": wake_id,
            "paths": {"worktree": worktree_rel, "target": target_rel}}));
    let mut wake_data = identity.clone();
    wake_data.as_object_mut().unwrap().extend(binding.as_object().unwrap().clone());
    wake_data["method"] = serde_json::json!("unified-channel-v1");
    wake_data["action"] = serde_json::json!("review");
    wake_data["controlWakeId"] = serde_json::json!(wake_id);
    wake_data["executable"] = serde_json::json!("/usr/bin/true");
    let wake = orch_host::ledger::event("WakeIssued", "runtime:orch", Some("B901"), Some("r83"), wake_data.clone());
    let mut request_data = wake_data;
    request_data["role"] = serde_json::json!("review");
    request_data["reviewedHead"] = serde_json::json!(f.candidate);
    request_data["reviewOutputPath"] = serde_json::json!(output);
    request_data["channelBinding"] = binding.clone();
    let request = orch_host::ledger::event("ReviewRequested", "runtime:orch", Some("B901"), Some("r83"), request_data);
    let mut receipt_data = identity;
    receipt_data.as_object_mut().unwrap().remove("harness");
    receipt_data["agentEvent"] = serde_json::json!("wake-backend-receipt");
    receipt_data["actionId"] = serde_json::json!(wake_id);
    receipt_data["receiptKind"] = serde_json::json!("smartclaw");
    receipt_data["observedSessionId"] = serde_json::json!(format!("native-{ordinal}"));
    receipt_data["probeEnd"] = serde_json::json!(9);
    receipt_data["windowSha256"] = serde_json::json!(hex::encode(Sha256::digest(b"accepted\n")));
    receipt_data["backendState"] = serde_json::json!("accepted");
    receipt_data["channelBinding"] = binding;
    let receipt = orch_host::ledger::event("AgentEventReceived", "runtime:orch", Some("B901"), Some("r83"), receipt_data);
    (wake_id, vec![lease, wake, request, receipt])
}

fn events(f: &Fixture) -> Vec<orch_core::EventRecord> {
    orch_core::read_ledger(&f.root.join("coordination/rounds/r83/events.jsonl")).unwrap().events
}

fn block(f: &Fixture, main: &str, ids: &[String], reason: &str, dry: bool) -> anyhow::Result<orch_host::verify::RootVerdictOutcome> {
    run_root_verdict_with_quarantine(&f.root, "B901", &f.attempt, &f.candidate, main,
        RootVerdict::Blocked, Some(reason), dry, ids)
}

fn block_cli(f: &Fixture, main: &str, ids: &[String], reason: &str, dry: bool) -> std::process::Output {
    // The CLI integration target makes Cargo build this matching ordinary binary.
    let binary = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("orch");
    let mut command = Command::new(binary);
    command.arg("--root").arg(&f.root).arg("--allow-stale-binary")
        .args(["verdict", "B901", "--attempt", &f.attempt, "--expected-head", &f.candidate,
            "--expected-main", main, "--verdict", "blocked", "--reason", reason]);
    for id in ids { command.arg("--quarantine-review").arg(id); }
    if dry { command.arg("--dry-run"); }
    command.output().unwrap()
}

#[test]
fn quarantine_dry_run_atomic_blocked_and_replay_preserve_the_unknown_native_site() {
    let f = fixture();
    let (wake, facts) = held_review(&f, "held-native", 1);
    orch_host::ledger::append(&f.root, "r83", &facts).unwrap();
    commit_all(&f.root, "record collected candidate and accepted review request");
    let ledger = f.root.join("coordination/rounds/r83/events.jsonl");
    let before = fs::read(&ledger).unwrap();
    let before_count = events(&f).len();
    let main = git(&f.root, &["rev-parse", "main"]);
    let reason = "unverifiable review; native scope remains held";
    let ids = vec![wake.clone()];
    let preview = block(&f, &main, &ids, reason, true).unwrap();
    assert!(preview.dry_run && !preview.appended);
    let cli_preview = block_cli(&f, &main, &ids, reason, true);
    assert!(cli_preview.status.success(), "{}", String::from_utf8_lossy(&cli_preview.stderr));
    assert_eq!(fs::read(&ledger).unwrap(), before);
    assert_eq!(git(&f.root, &["rev-parse", "main"]), main);
    let written = block_cli(&f, &main, &ids, reason, false);
    assert!(written.status.success(), "{}", String::from_utf8_lossy(&written.stderr));
    let after = events(&f);
    let all_delta = &after[before_count..];
    assert!(all_delta.len() >= 2);
    let delta = &all_delta[all_delta.len() - 2..];
    assert!(all_delta[..all_delta.len() - 2].iter().all(|event|
        matches!(event.kind.as_str(), "GateExecuted")),
        "only existing gate accounting may precede the atomic quarantine/BLOCKED pair");
    assert_eq!(delta[0].kind, "ActionRejected");
    let q = delta[0].payload.as_ref().unwrap();
    assert_eq!(q.as_object().unwrap().len(), 7);
    assert_eq!(q["operation"], "review-quarantine");
    assert_eq!(q["actionId"], wake);
    assert_eq!(q["exitCode"], 5);
    assert_eq!(q["alert"], true);
    assert_eq!(delta[1].kind, "VerdictIssued");
    assert_eq!(delta[1].payload.as_ref().unwrap()["verdict"], "BLOCKED");
    assert!(!all_delta.iter().any(|e| matches!(e.kind.as_str(),
        "ManagedWakeTerminated" | "WorkspaceReleased" | "SiteRetired" | "ReviewDelivered")));
    let stable = fs::read(&ledger).unwrap();
    assert!(!block(&f, &main, &ids, reason, false).unwrap().appended);
    assert!(block(&f, &main, &ids, "different reason", false).is_err());
    assert!(block(&f, &main, &[], reason, false).is_err());
    assert_eq!(fs::read(&ledger).unwrap(), stable);
    let load = orch_host::legacy::agent_inflight_load_from_events(&after, "r83").unwrap();
    assert_eq!(load.get("held-native").unwrap().len(), 1);
    assert!(orch_host::sites::reap_released_sites(&f.root, "r83").unwrap().removed.is_empty());
    assert!(f.root.join(format!(".worktrees/{}-review-held-native-g01", f.attempt)).exists());
    assert!(orch_host::generic_review::deliver_generic_review(&f.root, "B901", &f.attempt, "held-native", &wake).is_err());
    assert_eq!(fs::read(&ledger).unwrap(), stable);
    commit_all(&f.root, "preserve failed review adjudication");
    let successor = orch_host::tierf::run_dispatch_local(&f.root, "B901").unwrap();
    assert_eq!(successor.attempt_id, "B901-A0002");
    assert!(f.worktree.exists());
    let baseline = git(&f.root, &["rev-parse", "main"]);
    git(&f.worktree, &["-c", "user.name=orch-test", "-c", "user.email=orch@test.invalid",
        "merge", "--no-ff", "-m", "inherit failed-attempt history", &baseline]);
    let next_head = renew_report_and_collect(&f.root, &f.worktree);
    append_generic_review(&f.root, "B901", &successor.attempt_id, &next_head, "fresh-native", 3, "PASS");
    let evidence = f.root.join("coordination/rounds/r83/evidence/B901-proof.json");
    fs::create_dir_all(evidence.parent().unwrap()).unwrap();
    fs::write(&evidence, serde_json::to_vec(&serde_json::json!({"fixture": true, "candidate": next_head})).unwrap()).unwrap();
    commit_all(&f.root, "record controlled successor evidence");
    let sealed = orch_host::close::run_seal(&f.root, "B901", &successor.attempt_id, &next_head).unwrap();
    assert!(!sealed.replayed_complete);
    let final_events = events(&f);
    assert!(final_events.iter().any(|e| e.kind == "TaskRecorded"));
    assert!(!final_events.iter().any(|e|
        matches!(e.kind.as_str(), "ManagedWakeTerminated" | "WorkspaceReleased" | "SiteRetired" | "ReviewDelivered")
            && e.payload.as_ref().and_then(|p| p.get("wakeId")).and_then(serde_json::Value::as_str) == Some(wake.as_str())));
    assert!(f.root.join(format!(".worktrees/{}-review-held-native-g01", f.attempt)).exists(),
        "successor Recorded must not retire the unknown native site");
    let remaining = orch_host::legacy::agent_inflight_load_from_events(&final_events, "r83").unwrap();
    assert_eq!(remaining.get("held-native").unwrap().len(), 1);
    // This fixture never spawned a provider. Real held sites cannot be removed this way.
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
fn quarantine_requires_blocked_and_every_launched_request_to_be_accounted_for() {
    let f = fixture();
    let (one, first) = held_review(&f, "held-one", 1);
    let (two, second) = held_review(&f, "held-two", 2);
    let mut all = first; all.extend(second);
    orch_host::ledger::append(&f.root, "r83", &all).unwrap();
    commit_all(&f.root, "record both accepted review requests");
    let main = git(&f.root, &["rev-parse", "main"]);
    let ledger = f.root.join("coordination/rounds/r83/events.jsonl");
    let before = fs::read(&ledger).unwrap();
    for verdict in [RootVerdict::Pass, RootVerdict::Fail] {
        assert!(run_root_verdict_with_quarantine(&f.root, "B901", &f.attempt, &f.candidate,
            &main, verdict, Some("not an authorized quarantine verdict"), true, &[one.clone()]).is_err());
    }
    assert!(block(&f, &main, &[one.clone()], "missing second review", true).is_err());
    assert!(block(&f, &main, &[one.clone(), one.clone()], "duplicate ids", true).is_err());
    assert!(block(&f, &main, &["foreign-wake".to_string()], "foreign id", true).is_err());
    assert!(block(&f, &main, &[one.clone(), two.clone(), "foreign-wake".to_string()],
        "all real reviews plus an orphan selection", true).is_err());
    assert_eq!(fs::read(&ledger).unwrap(), before);
    block(&f, &main, &[two, one], "both held reviews", true).unwrap();
    assert_eq!(fs::read(&ledger).unwrap(), before);
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
fn new_quarantine_must_not_treat_its_prospective_root_as_portable_archive_authority() {
    let f = fixture();
    let (wake, mut facts) = held_review(&f, "foreign-cwd", 1);
    let foreign = f.root.join("another-repository");
    for event in &mut facts {
        let payload = event.payload.as_mut().unwrap();
        if matches!(event.kind.as_str(), "WakeIssued" | "ReviewRequested") {
            payload["invocationCwd"] = serde_json::json!(foreign);
        }
        if let Some(binding) = payload.get_mut("channelBinding") {
            binding["invocationCwd"] = serde_json::json!(foreign);
        }
    }
    orch_host::ledger::append(&f.root, "r83", &facts).unwrap();
    let main = commit_all(&f.root, "coherent but foreign request cwd");
    let ledger = f.root.join("coordination/rounds/r83/events.jsonl");
    let before = fs::read(&ledger).unwrap();
    assert!(block(&f, &main, &[wake], "live cwd must match", true).is_err());
    assert_eq!(fs::read(&ledger).unwrap(), before);
    fs::remove_dir_all(&f.root).unwrap();
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


fn renew_report_and_collect(root: &Path, worktree: &Path) -> String {
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

    candidate
}
