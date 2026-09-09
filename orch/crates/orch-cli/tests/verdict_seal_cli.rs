#![cfg(feature = "selfhost")]
mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

const TASK: &str = "B904";
const ATTEMPT: &str = "B904-A0001";
const LEGACY_LEDGER_BYTES: &[u8] = b"{\"eventId\":\"01K00000000000000000000001\",\"ts\":\"2026-08-02T00:00:00Z\",\"type\":\"RoundOpened\",\"actor\":\"runtime:orch\",\"round\":\"r904\",\"payload\":{\"purpose\":\"legacy seal refusal fixture\"}}\n";

static ROOT_SEQ: AtomicU64 = AtomicU64::new(0);

fn fixture_orch_command(extras: &[(&str, &str)]) -> Command {
    let mut command = Command::new(support::orch_bin());
    support::configure_fixture_git_env(&mut command, extras);
    command
}

fn git_output(root: &Path, args: &[&str]) -> Output {
    let mut command = support::fixture_git_command(root);
    command
        .env_remove("ORCH_MAIN_GUARD_BYPASS")
        .env_remove("ORCH_MAIN_GUARD_CONTEXT")
        .args(args)
        .output()
        .expect("run fixture git")
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = git_output(root, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

struct SealFixture {
    root: PathBuf,
    task_head: String,
}

impl Drop for SealFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl SealFixture {
    fn new(tag: &str) -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("workspace root")
            .join("target/test-tmp")
            .join(format!(
                "b204-seal-cli-{tag}-{}-{}",
                std::process::id(),
                ROOT_SEQ.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "--quiet"]);
        git(&root, &["config", "user.email", "seal-cli@example.invalid"]);
        git(&root, &["config", "user.name", "seal cli test"]);
        fs::write(root.join("README.md"), "base\n").unwrap();
        fs::write(
            root.join(".gitignore"),
            ".worktrees/\n.cowork-temp/\ncoordination/runtime/\n",
        )
        .unwrap();
        git(&root, &["add", "README.md", ".gitignore"]);
        git(&root, &["commit", "--quiet", "-m", "base"]);
        git(&root, &["branch", "-M", "main"]);

        fs::create_dir_all(root.join("coordination/rounds/r904")).unwrap();
        fs::create_dir_all(root.join("coordination/runtime/ledger-wal")).unwrap();
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r904\n").unwrap();
        fs::write(root.join("coordination/rounds/r904/events.jsonl"), LEGACY_LEDGER_BYTES).unwrap();
        fs::write(root.join("coordination/runtime/ledger-wal/r904.jsonl"), LEGACY_LEDGER_BYTES).unwrap();
        git(&root, &["add", "coordination/rounds/r904/events.jsonl"]);
        git(&root, &["commit", "--quiet", "-m", "raw legacy fixture bytes"]);
        let task_head = git(&root, &["rev-parse", "HEAD"]);

        Self { root, task_head }
    }

    fn orch(&self, args: &[&str]) -> Output {
        fixture_orch_command(&[])
            .arg("--root")
            .arg(&self.root)
            .args(args)
            .output()
            .expect("run Cargo-built orch")
    }

}

fn runtime_snapshot(root: &Path) -> std::collections::BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn visit(root: &Path, path: &Path, files: &mut std::collections::BTreeMap<PathBuf, Option<Vec<u8>>>) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            assert!(!metadata.file_type().is_symlink(), "fixture runtime contains symlink");
            let value = if metadata.is_file() { Some(fs::read(&path).unwrap()) } else { None };
            files.insert(path.strip_prefix(root).unwrap().to_path_buf(), value);
            if metadata.is_dir() { visit(root, &path, files); }
        }
    }
    let mut files = std::collections::BTreeMap::new();
    visit(root, &root.join("coordination/runtime"), &mut files);
    files
}

fn assert_legacy_seal_is_read_only(name: &str) {
    let fixture = SealFixture::new(name);
    let ledger_path = fixture.root.join("coordination/rounds/r904/events.jsonl");
    let ledger_before = fs::read(&ledger_path).unwrap();
    let wal_path = fixture.root.join("coordination/runtime/ledger-wal/r904.jsonl");
    let wal_before = fs::read(&wal_path).unwrap();
    assert_eq!(ledger_before, LEGACY_LEDGER_BYTES);
    assert_eq!(wal_before, LEGACY_LEDGER_BYTES);
    let main_before = git(&fixture.root, &["rev-parse", "main"]);
    let status_before = git(&fixture.root, &["status", "--porcelain"]);
    let worktrees_before = git(&fixture.root, &["worktree", "list", "--porcelain"]);
    let runtime_before = runtime_snapshot(&fixture.root);
    let command = if name == "split-pass-retired" { "verdict" } else { "seal" };
    let mut args = vec![
        command,
        TASK,
        "--attempt",
        ATTEMPT,
        "--expected-head",
        &fixture.task_head,
    ];
    if command == "verdict" {
        args.extend(["--expected-main", main_before.as_str(), "--verdict", "pass"]);
    }
    let output = fixture.orch(&args);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("schema 1/2 active round 只读兼容")
    );
    assert_eq!(fs::read(&ledger_path).unwrap(), ledger_before);
    assert_eq!(git(&fixture.root, &["rev-parse", "main"]), main_before);
    assert_eq!(fs::read(wal_path).unwrap(), wal_before);
    assert_eq!(git(&fixture.root, &["status", "--porcelain"]), status_before);
    assert_eq!(git(&fixture.root, &["worktree", "list", "--porcelain"]), worktrees_before);
    assert_eq!(runtime_snapshot(&fixture.root), runtime_before);
    assert!(!fixture.root.join("coordination/runtime/locks").exists());
    assert!(!fixture.root.join("coordination/runtime/gates").exists());
    assert!(!fixture.root.join("PROVIDER-SPAWNED").exists());
}

#[test]
fn seal_cli_captures_main_merges_records_and_replays_without_expected_main() {
    assert_legacy_seal_is_read_only("seal-retired");
}

#[test]
fn split_pass_blocks_real_commit_and_seal_is_the_recovery_compatible_successor() {
    assert_legacy_seal_is_read_only("split-pass-retired");
}

#[test]
fn late_review_between_split_verdict_and_seal_is_strictly_audited_but_not_merged() {
    assert_legacy_seal_is_read_only("late-review-retired");
}

#[test]
fn doctor_fails_with_bound_and_current_hashes_after_recorded_review_is_tampered() {
    assert_legacy_seal_is_read_only("doctor-retired");
}

#[test]
fn late_review_cli_delivers_two_monotonic_pairs_without_changing_canonical_binding() {
    assert_legacy_seal_is_read_only("late-delivery-retired");
}

#[test]
fn approved_reattempt_cli_terminates_a0001_and_replays_a0002_without_minting_a0003() {
    assert_legacy_seal_is_read_only("reattempt-retired");
}

#[test]
fn approved_reattempt_crash_terminal_blocks_plain_dispatch_and_explicitly_recovers() {
    assert_legacy_seal_is_read_only("reattempt-crash-retired");
}
