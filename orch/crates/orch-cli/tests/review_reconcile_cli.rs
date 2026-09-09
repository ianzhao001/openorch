#![cfg(feature = "selfhost")]
//! B323 CLI contract: review has one generic delivery path and no legacy writer.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn fixture_orch_command(extras: &[(&str, &str)]) -> Command {
    let mut command = Command::new(support::orch_bin());
    support::configure_fixture_git_env(&mut command, extras);
    command
}

fn run(args: &[&str]) -> Output {
    fixture_orch_command(&[])
        .args(args)
        .output()
        .expect("run orch")
}

fn run_at(root: &Path, args: &[&str]) -> Output {
    fixture_orch_command(&[])
        .arg("--root")
        .arg(root)
        .arg("--allow-stale-binary")
        .args(args)
        .output()
        .expect("run fixture orch")
}

fn source_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap()
        .to_path_buf()
}

fn fixture_git(root: &Path, args: &[&str]) -> String {
    let output = support::fixture_git_command(root)
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

fn commit_all(root: &Path, message: &str) -> String {
    fixture_git(root, &["add", "-A"]);
    fixture_git(
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
    fixture_git(root, &["rev-parse", "HEAD"])
}

fn snapshot_regular_files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(base: &Path, current: &Path, snapshot: &mut BTreeMap<PathBuf, Vec<u8>>) {
        if !current.exists() {
            return;
        }
        let mut entries = fs::read_dir(current)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        entries.sort();
        for path in entries {
            let metadata = fs::symlink_metadata(&path).unwrap();
            assert!(
                !metadata.file_type().is_symlink(),
                "fixture tree contains symlink"
            );
            if metadata.is_dir() {
                walk(base, &path, snapshot);
            } else {
                assert!(
                    metadata.is_file(),
                    "fixture tree contains non-regular entry"
                );
                snapshot.insert(
                    path.strip_prefix(base).unwrap().to_path_buf(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }

    let mut snapshot = BTreeMap::new();
    walk(root, root, &mut snapshot);
    snapshot
}

fn fixture_binding() -> String {
    let source =
        fs::read_to_string(source_root().join("coordination/PROJECT-BINDING.yaml")).unwrap();
    let mut rewritten = source
        .lines()
        .map(|line| {
            let indent = &line[..line.len() - line.trim_start().len()];
            if line.trim_start().starts_with("argv:") {
                format!("{indent}argv: [\"/usr/bin/true\"]")
            } else if line.trim_start().starts_with("timeoutSeconds:") {
                format!("{indent}timeoutSeconds: 30")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    rewritten.push('\n');
    rewritten
}

fn schema3_repo_without_a_review_request() -> PathBuf {
    let root = source_root().join("orch/target/test-tmp").join(format!(
        "b323-generic-deliver-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("coordination/BOARD.md"), "# fixture\n").unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn value() -> u32 { 1 }\n").unwrap();
    fs::write(
        root.join("coordination/PROJECT-BINDING.yaml"),
        fixture_binding(),
    )
    .unwrap();
    fs::write(
        root.join(".gitignore"),
        ".orch/\n.worktrees/\n.cowork-temp/\ncoordination/runtime/\ncoordination/rounds/*/dispatch/\norch/target/\n",
    )
    .unwrap();
    fixture_git(&root, &["init", "-q", "-b", "main"]);
    commit_all(&root, "fixture base");

    orch_host::round::run_open_v3(&root, "r83", "generic deliver refusal", false).unwrap();
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
    orch_host::round::run_sign_off(&root, Some("approved fixture")).unwrap();
    commit_all(&root, "signed actorless fixture");

    root
}

fn install_closed_legacy_pending_receipt(root: &Path) -> (PathBuf, PathBuf) {
    let round = "r82";
    let wake_id = "019fb082-f94f-4896-8b85-ac0597fbfdfa";
    let agent = "executor-opencode";
    let continuation = format!("manual:{round}:{wake_id}:{agent}");
    let request_digest =
        orch_host::wake::wake_request_message_sha256("closed legacy probe", &continuation).unwrap();
    let log_path = root.join("coordination/runtime/logs/closed-legacy-opencode.jsonl");
    fs::create_dir_all(log_path.parent().unwrap()).unwrap();
    fs::write(
        &log_path,
        b"{\"type\":\"step_start\",\"sessionID\":\"ses_closed\",\"part\":{\"type\":\"step-start\"}}\n",
    )
    .unwrap();

    let events = [
        orch_host::ledger::event(
            "RoundOpened",
            "runtime:orch",
            None,
            Some(round),
            serde_json::json!({}),
        ),
        orch_host::ledger::event(
            "WakeIssued",
            "runtime:orch",
            None,
            Some(round),
            serde_json::json!({
                "wakeId": wake_id,
                "controlWakeId": wake_id,
                "runtimeLimit": null,
                "continuationId": continuation,
                "attemptId": null,
                "agent": agent,
                "providerKind": "opencode",
                "requestMessageSha256": request_digest,
                "renderedMessageSha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "requestSessionId": null,
                "backendState": "pending",
                "pid": 42,
                "logPath": log_path,
                "probeOffset": 0,
                "probeEnd": 0,
                "method": "typed-runtime"
            }),
        ),
        orch_host::ledger::event(
            "RoundClosed",
            "runtime:orch",
            None,
            Some(round),
            serde_json::json!({}),
        ),
    ];
    let mut bytes = Vec::new();
    for event in events {
        serde_json::to_writer(&mut bytes, &event).unwrap();
        bytes.push(b'\n');
    }
    let ledger = root.join("coordination/rounds/r82/events.jsonl");
    let wal = root.join("coordination/runtime/ledger-wal/r82.jsonl");
    fs::create_dir_all(ledger.parent().unwrap()).unwrap();
    fs::create_dir_all(wal.parent().unwrap()).unwrap();
    fs::write(&ledger, &bytes).unwrap();
    fs::write(&wal, bytes).unwrap();
    (ledger, wal)
}

#[test]
fn review_help_exposes_only_generic_deliver() {
    let output = run(&["review", "--help"]);
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("deliver"));
    for retired in ["reconcile", "panel", "primary", "secondary", "nongate"] {
        assert!(
            !help.contains(retired),
            "retired review surface leaked: {retired}"
        );
    }

    let deliver = run(&["review", "deliver", "--help"]);
    assert!(deliver.status.success());
    let help = String::from_utf8(deliver.stdout).unwrap();
    assert!(help.contains("--harness"));
    assert!(help.contains("--wake-id"));
    assert!(!help.contains("--role"));
    assert!(!help.contains("--agent"));
}

#[test]
fn every_legacy_review_writer_is_syntax_unreachable() {
    for args in [
        &["runtime-policy", "activate", "review-pool-v1"][..],
        &["review", "reconcile", "B323", "--attempt", "B323-A0001"][..],
        &[
            "review",
            "panel",
            "select",
            "B323",
            "--attempt",
            "B323-A0001",
            "--seat",
            "primary:legacy",
        ][..],
        &[
            "review",
            "deliver",
            "B323",
            "B323-A0001",
            "--role",
            "primary",
            "--agent",
            "legacy",
        ][..],
        &["wake", "alpha", "--role", "primary"][..],
        &["wake", "alpha", "--reissue", "old-wake"][..],
    ] {
        let output = run(args);
        assert!(
            !output.status.success(),
            "retired writer still parses: {args:?}"
        );
    }
}

#[test]
fn generic_deliver_requires_both_dynamic_identity_fields() {
    for args in [
        &["review", "deliver", "B323", "B323-A0001"][..],
        &[
            "review",
            "deliver",
            "B323",
            "B323-A0001",
            "--harness",
            "alpha",
        ][..],
        &[
            "review",
            "deliver",
            "B323",
            "B323-A0001",
            "--wake-id",
            "wake-alpha",
        ][..],
    ] {
        let output = run(args);
        assert!(
            !output.status.success(),
            "incomplete generic identity parsed: {args:?}"
        );
    }
}

#[test]
fn generic_deliver_without_an_exact_request_is_side_effect_free() {
    let root = schema3_repo_without_a_review_request();
    let ledger = root.join("coordination/rounds/r83/events.jsonl");
    let wal = root.join("coordination/runtime/ledger-wal/r83.jsonl");
    let reviews = root.join("coordination/rounds/r83/reviews");
    let inbox = root.join("coordination/runtime/review-inbox");
    let canonical = reviews.join("B901-A0001-review-alpha.md");
    let staged = inbox.join("r83/B901-A0001-review-alpha.md");
    let ledger_before = fs::read(&ledger).unwrap();
    let wal_before = fs::read(&wal).unwrap();
    let reviews_before = snapshot_regular_files(&reviews);
    let inbox_before = snapshot_regular_files(&inbox);
    let head_before = fixture_git(&root, &["rev-parse", "HEAD"]);

    let output = run_at(
        &root,
        &[
            "review",
            "deliver",
            "B901",
            "B901-A0001",
            "--harness",
            "alpha",
            "--wake-id",
            "wake-alpha",
        ],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("generic review"),
        "unexpected refusal: {stderr}"
    );
    assert!(
        stderr.contains("WakeIssued"),
        "unexpected refusal: {stderr}"
    );
    assert_eq!(fs::read(&ledger).unwrap(), ledger_before);
    assert_eq!(fs::read(&wal).unwrap(), wal_before);
    assert_eq!(snapshot_regular_files(&reviews), reviews_before);
    assert_eq!(snapshot_regular_files(&inbox), inbox_before);
    assert!(!canonical.exists());
    assert!(!staged.exists());
    assert_eq!(fixture_git(&root, &["rev-parse", "HEAD"]), head_before);
    assert!(fixture_git(&root, &["status", "--porcelain"]).is_empty());
    let events = orch_core::read_ledger(&ledger).unwrap().events;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "ReviewDelivered")
            .count(),
        0
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn sites_gc_does_not_reconcile_a_closed_legacy_round() {
    let root = schema3_repo_without_a_review_request();
    let (ledger, wal) = install_closed_legacy_pending_receipt(&root);
    let ledger_before = fs::read(&ledger).unwrap();
    let wal_before = fs::read(&wal).unwrap();

    let output = run_at(&root, &["sites", "gc", "--round", "r82"]);
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(&ledger).unwrap(), ledger_before);
    assert_eq!(fs::read(&wal).unwrap(), wal_before);
    let read = orch_core::read_ledger(&ledger).unwrap();
    assert!(read.bad_lines.is_empty());
    assert!(!read.events.iter().any(|event| {
        matches!(
            event.kind.as_str(),
            "AgentEventReceived" | "ManagedWakeTerminated" | "WorkspaceReleased"
        )
    }));
    fs::remove_dir_all(root).unwrap();
}
