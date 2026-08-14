//! B180 CLI integration: `orch review reconcile` is an active-round, exact,
//! provider-free repair command backed only by committed review artifacts.

mod support;

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;

const ROUND: &str = "r180";
const TASK: &str = "B180";
const ATTEMPT: &str = "B180-A0001";
const AGENT: &str = "executor-claw";
const REVIEWED_HEAD: &str = "0123456789012345678901234567890123456789";
const WRONG_HEAD: &str = "1123456789012345678901234567890123456789";

fn complete_review(head: &str, extras: &str, body: &str) -> Vec<u8> {
    format!(
        "---\ntaskId: {TASK}\nround: {ROUND}\nattemptId: {ATTEMPT}\nrole: primary\nreviewer: {AGENT}\nverdict: PASS\nreviewedHead: {head}\n{extras}---\n{body}\n"
    )
    .into_bytes()
}

fn fixture_orch_command(extras: &[(&str, &str)]) -> Command {
    let mut command = Command::new(support::orch_bin());
    support::configure_fixture_git_env(&mut command, extras);
    command
}

fn unique_root(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let suffix = SEQ.fetch_add(1, Ordering::Relaxed);
    let orch_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap();
    let root = orch_dir.join("target/test-tmp").join(format!(
        "b180-review-reconcile-{name}-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    root
}

fn git(root: &Path, args: &[&str]) {
    let output = support::fixture_git_command(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_orch(root: &Path, args: &[&str]) -> std::process::Output {
    fixture_orch_command(&[])
        .arg("--root")
        .arg(root)
        .arg("--allow-stale-binary")
        .args(args)
        .output()
        .expect("launch orch")
}

fn review_rel() -> String {
    format!("coordination/rounds/{ROUND}/reviews/{ATTEMPT}-primary-{AGENT}.md")
}

fn setup_active_repo_with_collect(name: &str, review: &[u8], include_collect: bool) -> PathBuf {
    let root = unique_root(name);
    for rel in [
        "coordination/runtime",
        "coordination/modes",
        "coordination/rounds/r180/tasks",
        "coordination/rounds/r180/reviews",
    ] {
        fs::create_dir_all(root.join(rel)).unwrap();
    }
    fs::write(
        root.join("coordination/runtime/CURRENT-ROUND"),
        format!("{ROUND}\n"),
    )
    .unwrap();
    fs::write(
        root.join("coordination/modes/test.yaml"),
        r#"agents:
  executor: {adapter: test, tier: none}
  verifier: {adapter: root-manual, tier: none}
hitl: {mergeGate: auto}
verification: {mode: root-manual-fixed-head}
liveness: {monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}
scheduling:
  allowedAgents: [executor-desktop, executor-claw, executor-opencode]
  capacities:
    executor-desktop: {agent: 2, quota: 2, roles: [implement]}
    executor-claw: {agent: 2, quota: 2, roles: [primary-review]}
    executor-opencode: {agent: 2, quota: 2, roles: [secondary-review]}
budgets: {round: {wallMinutes: 60, maxModelWakes: 20}}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
"#,
    )
    .unwrap();
    fs::write(
        root.join("coordination/PROJECT-BINDING.yaml"),
        "scope: {protectedPaths: [\"coordination/**\"]}\n\
         git: {pushPolicy: forbidden}\n\
         commands:\n  testFast: {argv: [\"sh\", \"-c\", \"exit 0\"], timeoutSeconds: 30}\n",
    )
    .unwrap();
    fs::write(
        root.join(format!("coordination/rounds/{ROUND}/tasks/{TASK}.md")),
        format!(
            "---\ntaskId: {TASK}\nround: {ROUND}\nagent: executor-desktop\n\
             seedProtocol: pure-spec\nentryPoints: [fixture.txt]\nwriteSet: [fixture.txt]\n\
             frozenPaths: [coordination/**]\ngates: {{fast: [testFast]}}\n\
             budgets: {{wallMinutes: 10}}\nrequiredReviews:\n\
               - {{role: primary, agent: {AGENT}}}\n\
             requiredEvidence: [review-reconcile]\n---\nfixture\n"
        ),
    )
    .unwrap();

    // A registered fake provider makes spawn count observable. Reconcile must
    // never load or execute it.
    let provider = root.join("provider-marker.sh");
    fs::write(&provider, "#!/bin/sh\nprintf x >> provider-spawns\n").unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        root.join("coordination/agents.yaml"),
        serde_json::to_vec_pretty(&json!({
            "agents": {
                AGENT: {
                    "injectable": true,
                    "sessionId": "fake-review-session",
                    "wake": {"argv": [provider.display().to_string()]},
                    "pokeHint": "unused"
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    orch_host::plan::run_plan(&root).unwrap();
    orch_host::round::run_sign_off(&root, Some("B180 CLI fixture")).unwrap();
    let mut review_events = Vec::new();
    if include_collect {
        review_events.push(orch_host::ledger::event(
            "ReportCollectCompleted",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            json!({
                "attemptId": ATTEMPT,
                "branchSha": REVIEWED_HEAD,
            }),
        ));
    }
    review_events.push(orch_host::ledger::event(
        "ReviewRequested",
        "runtime:orch",
        Some(TASK),
        Some(ROUND),
        json!({
            "attemptId": ATTEMPT,
            "role": "primary",
            "agent": AGENT,
            "deadlineSecs": 1800,
            "requestedAt": "2026-07-31T00:00:00Z"
        }),
    ));
    orch_host::ledger::append(&root, ROUND, &review_events).unwrap();
    fs::write(root.join(review_rel()), review).unwrap();

    git(&root, &["init", "-q"]);
    git(&root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(
        &root,
        &[
            "add",
            "--",
            "provider-marker.sh",
            "coordination/agents.yaml",
            "coordination/PROJECT-BINDING.yaml",
            "coordination/modes/test.yaml",
            "coordination/runtime/CURRENT-ROUND",
            "coordination/rounds/r180/tasks/B180.md",
            "coordination/rounds/r180/events.jsonl",
            &review_rel(),
        ],
    );
    git(
        &root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch-test@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    );
    root
}

fn setup_active_repo(name: &str, review: &[u8]) -> PathBuf {
    setup_active_repo_with_collect(name, review, true)
}

fn event_count(root: &Path, kind: &str) -> usize {
    orch_core::read_ledger(&root.join(format!("coordination/rounds/{ROUND}/events.jsonl")))
        .unwrap()
        .events
        .iter()
        .filter(|event| event.kind == kind)
        .count()
}

fn relative_files(root: &Path, rel: &str) -> BTreeSet<String> {
    let base = root.join(rel);
    let mut files = BTreeSet::new();
    let Ok(entries) = fs::read_dir(&base) else {
        return files;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_file()) {
            files.insert(entry.file_name().to_string_lossy().into_owned());
        }
    }
    files
}

#[test]
fn review_reconcile_help_and_parameters_are_exact() {
    let root = unique_root("help");
    let help = run_orch(&root, &["review", "reconcile", "--help"]);
    assert!(help.status.success());
    let stdout = String::from_utf8_lossy(&help.stdout);
    assert!(stdout.contains("--attempt <ATTEMPT>"), "{stdout}");
    assert!(!stdout.contains("--role"));
    assert!(!stdout.contains("--agent"));

    let missing = run_orch(&root, &["review", "reconcile", TASK]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("--attempt"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn review_reconcile_cli_is_idempotent_and_provider_free() {
    let body = "committed cli body";
    let root = setup_active_repo(
        "success",
        &complete_review(
            REVIEWED_HEAD,
            "verifier: executor-claw\nindependence: isolated\n",
            body,
        ),
    );
    let before_counts = [
        event_count(&root, "WakeIssued"),
        event_count(&root, "ReviewRequested"),
        event_count(&root, "AgentEventReceived"),
    ];
    let logs_before = relative_files(&root, "coordination/runtime/logs");
    let reservations_before = relative_files(&root, "coordination/runtime/model-wake-reservations");

    let first = run_orch(&root, &["review", "reconcile", TASK, "--attempt", ATTEMPT]);
    assert!(
        first.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    let first_stdout = String::from_utf8_lossy(&first.stdout);
    assert!(first_stdout.contains("task=B180"), "{first_stdout}");
    assert!(
        first_stdout.contains("attempt=B180-A0001"),
        "{first_stdout}"
    );
    assert!(first_stdout.contains("delivered=1"), "{first_stdout}");
    let first_stderr = String::from_utf8_lossy(&first.stderr);
    assert!(
        first_stderr.contains("fields=independence,verifier"),
        "{first_stderr}"
    );

    let second = run_orch(&root, &["review", "reconcile", TASK, "--attempt", ATTEMPT]);
    assert!(second.status.success());
    assert!(String::from_utf8_lossy(&second.stdout).contains("delivered=0"));
    assert_eq!(event_count(&root, "ReviewDelivered"), 1);

    let after_counts = [
        event_count(&root, "WakeIssued"),
        event_count(&root, "ReviewRequested"),
        event_count(&root, "AgentEventReceived"),
    ];
    assert_eq!(after_counts, before_counts);
    assert_eq!(
        relative_files(&root, "coordination/runtime/logs"),
        logs_before
    );
    assert_eq!(
        relative_files(&root, "coordination/runtime/model-wake-reservations"),
        reservations_before
    );
    assert!(!root.join("provider-spawns").exists());
    assert_eq!(
        fs::read(root.join(format!("coordination/rounds/{ROUND}/events.jsonl"))).unwrap(),
        fs::read(root.join(format!("coordination/runtime/ledger-wal/{ROUND}.jsonl"))).unwrap(),
        "ledger and WAL must be exact mirrors"
    );

    let invalid = run_orch(
        &root,
        &["review", "reconcile", TASK, "--attempt", "B180-A1"],
    );
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("canonical"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn review_reconcile_cli_propagates_bad_blob_and_bad_ledger() {
    let bad_blob = setup_active_repo("bad-blob", &[0xff, 0xfe]);
    let output = run_orch(
        &bad_blob,
        &["review", "reconcile", TASK, "--attempt", ATTEMPT],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("UTF-8"));
    assert_eq!(event_count(&bad_blob, "ReviewDelivered"), 0);
    assert_eq!(event_count(&bad_blob, "ActionRejected"), 0);
    assert!(!bad_blob.join("provider-spawns").exists());
    fs::remove_dir_all(bad_blob).unwrap();

    let bad_ledger = setup_active_repo(
        "bad-ledger",
        &complete_review(REVIEWED_HEAD, "", "substantive"),
    );
    let ledger = bad_ledger.join(format!("coordination/rounds/{ROUND}/events.jsonl"));
    use std::io::Write;
    fs::OpenOptions::new()
        .append(true)
        .open(&ledger)
        .unwrap()
        .write_all(b"not-json\n")
        .unwrap();
    let output = run_orch(
        &bad_ledger,
        &["review", "reconcile", TASK, "--attempt", ATTEMPT],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("坏账本"));
    assert!(!bad_ledger.join("provider-spawns").exists());
    fs::remove_dir_all(bad_ledger).unwrap();
}

#[test]
fn review_reconcile_cli_rejects_missing_identity_wrong_head_and_missing_collect() {
    let missing_reviewer = String::from_utf8(complete_review(
        REVIEWED_HEAD,
        "verifier: executor-claw\n",
        "substantive",
    ))
    .unwrap()
    .replace("reviewer: executor-claw\n", "")
    .into_bytes();
    let missing = setup_active_repo("missing-reviewer", &missing_reviewer);
    let output = run_orch(
        &missing,
        &["review", "reconcile", TASK, "--attempt", ATTEMPT],
    );
    assert!(!output.status.success());
    assert_eq!(event_count(&missing, "ReviewDelivered"), 0);
    assert!(!missing.join("provider-spawns").exists());
    fs::remove_dir_all(missing).unwrap();

    let wrong = setup_active_repo(
        "wrong-head",
        &complete_review(WRONG_HEAD, "", "substantive"),
    );
    let output = run_orch(&wrong, &["review", "reconcile", TASK, "--attempt", ATTEMPT]);
    assert!(!output.status.success());
    assert_eq!(event_count(&wrong, "ReviewDelivered"), 0);
    assert!(!wrong.join("provider-spawns").exists());
    fs::remove_dir_all(wrong).unwrap();

    let no_collect = setup_active_repo_with_collect(
        "missing-collect",
        &complete_review(REVIEWED_HEAD, "", "substantive"),
        false,
    );
    let output = run_orch(
        &no_collect,
        &["review", "reconcile", TASK, "--attempt", ATTEMPT],
    );
    assert!(!output.status.success());
    assert_eq!(event_count(&no_collect, "ReviewDelivered"), 0);
    assert!(!no_collect.join("provider-spawns").exists());
    fs::remove_dir_all(no_collect).unwrap();
}
