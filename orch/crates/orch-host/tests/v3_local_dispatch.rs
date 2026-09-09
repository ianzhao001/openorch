use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

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
        .join(format!("v3-local-dispatch-{}-{seq}", std::process::id()))
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("ORCH_MAIN_GUARD_CONTEXT", "v3-local-dispatch-test")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn commit_all(root: &Path, message: &str) {
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
}

#[test]
fn schema3_local_dispatch_is_no_wake_replayable_and_supports_local_successors() {
    let root = temp_root();
    fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("coordination/BOARD.md"), "# fixture\n").unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn baseline() {}\n").unwrap();
    fs::write(
        root.join(".gitignore"),
        ".worktrees/\n.cowork-temp/\ncoordination/runtime/\ncoordination/rounds/*/dispatch/\norch/target/\n",
    )
    .unwrap();
    fs::copy(
        source_root().join("coordination/PROJECT-BINDING.yaml"),
        root.join("coordination/PROJECT-BINDING.yaml"),
    )
    .unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    commit_all(&root, "fixture base");

    orch_host::round::run_open_v3(&root, "r83", "local fixture", false).unwrap();
    let card = root.join("coordination/rounds/r83/tasks/T1.md");
    fs::write(
        &card,
        "---\n\
schemaVersion: 3\n\
taskId: T1\n\
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
---\n# T1\n",
    )
    .unwrap();
    let plan = orch_host::plan::run_plan(&root).unwrap();
    assert_eq!(plan.revision, 1);
    orch_host::round::run_sign_off(&root, Some("fixture approval")).unwrap();
    commit_all(&root, "open and sign schema3 fixture");

    let first = orch_host::tierf::run_dispatch_local(&root, "T1").unwrap();
    assert!(!first.replayed);
    assert_eq!(first.attempt_id, "T1-A0001");
    assert_eq!(
        git(&root.join(&first.worktree_rel), &["rev-parse", "HEAD"]),
        first.base_sha
    );
    let replay = orch_host::tierf::run_dispatch_local(&root, "T1").unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.attempt_id, first.attempt_id);

    orch_host::ledger::append(
        &root,
        "r83",
        &[orch_host::ledger::event(
            "VerdictIssued",
            "verifier:root",
            Some("T1"),
            Some("r83"),
            serde_json::json!({
                "attemptId": first.attempt_id,
                "verdict": "FAIL",
                "reason": "fixture retry"
            }),
        )],
    )
    .unwrap();
    let successor = orch_host::tierf::run_dispatch_local(&root, "T1").unwrap();
    assert_eq!(successor.attempt_id, "T1-A0002");
    assert!(!successor.replayed);

    let ledger =
        orch_core::read_ledger(&root.join("coordination/rounds/r83/events.jsonl")).unwrap();
    assert_eq!(
        ledger
            .events
            .iter()
            .filter(|event| event.kind == "DispatchIssued")
            .count(),
        2
    );
    assert_eq!(
        ledger
            .events
            .iter()
            .filter(|event| event.kind == "WorkspaceLeased")
            .count(),
        2
    );
    assert!(!ledger.events.iter().any(|event| event.kind == "WakeIssued"));
    let dispatch = ledger
        .events
        .iter()
        .rev()
        .find(|event| event.kind == "DispatchIssued")
        .unwrap();
    assert_eq!(dispatch.payload.as_ref().unwrap()["agent"], "local");
    assert_eq!(dispatch.payload.as_ref().unwrap()["wakePending"], false);
    assert!(!root.join("coordination/runtime/POKE-local.txt").exists());

    let _ = fs::remove_dir_all(root);
}
