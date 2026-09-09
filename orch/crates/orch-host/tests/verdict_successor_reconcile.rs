//! B323: the legacy committed-review reconciler is a zero-side-effect tombstone action.

use std::fs;
use std::path::Path;
use std::process::Command;

use orch_host::util::test_scratch_dir;
use orch_host::wake::reconcile_committed_review_delivery_slots;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "core.fsmonitor=false", "-C"])
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

#[test]
fn legacy_reconcile_is_repeatably_refused_without_git_ledger_wal_or_artifact_effects() {
    let root = test_scratch_dir("b323-retired-review-reconcile");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("coordination/rounds/r58/reviews")).unwrap();
    fs::create_dir_all(root.join("coordination/runtime/ledger-wal")).unwrap();
    fs::write(root.join(".gitignore"), "coordination/runtime/\n").unwrap();
    let ledger = root.join("coordination/rounds/r58/events.jsonl");
    let wal = root.join("coordination/runtime/ledger-wal/r58.jsonl");
    fs::write(&ledger, b"").unwrap();
    fs::write(&wal, b"").unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["add", "-A"]);
    git(
        &root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "commit",
            "-qm",
            "retired reconcile fixture",
        ],
    );
    let head = git(&root, &["rev-parse", "HEAD"]);
    let ledger_before = fs::read(&ledger).unwrap();
    let wal_before = fs::read(&wal).unwrap();

    for _ in 0..2 {
        let error = reconcile_committed_review_delivery_slots(
            &root,
            "B180",
            "B180-A0001",
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("legacy committed review reconciliation 已退役"));
    }

    assert_eq!(fs::read(&ledger).unwrap(), ledger_before);
    assert_eq!(fs::read(&wal).unwrap(), wal_before);
    assert_eq!(git(&root, &["rev-parse", "HEAD"]), head);
    assert!(git(&root, &["status", "--porcelain"]).is_empty());
    assert!(
        fs::read_dir(root.join("coordination/rounds/r58/reviews"))
            .unwrap()
            .next()
            .is_none()
    );
    let source = include_str!("../src/wake.rs");
    assert!(!source.contains("fn reconcile_committed_review_delivery_slots_with_hook"));
    assert!(!source.contains("fn reconcile_review_delivery_slots"));
    fs::remove_dir_all(root).unwrap();
}
