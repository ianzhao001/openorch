//! Real CLI storage maintenance without an active IR; no historical repair or growth on refusal.
#![cfg(feature = "selfhost")]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn fixture(label: &str) -> PathBuf {
    let root = orch_host::util::test_scratch_dir(label);
    assert!(Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .arg(&root)
        .status()
        .unwrap()
        .success());
    fs::write(root.join("source.txt"), b"source").unwrap();
    fs::write(
        root.join(".gitignore"),
        "coordination/runtime/\n.orch/\norch/target/\n.cowork-temp/\n",
    )
    .unwrap();
    for args in [
        vec!["add", "."],
        vec![
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    ] {
        assert!(Command::new("git")
            .args(args)
            .current_dir(&root)
            .status()
            .unwrap()
            .success());
    }
    fs::create_dir_all(root.join("coordination")).unwrap();
    fs::write(root.join("coordination/BOARD.md"), "# Fixture board\n").unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap()
        .join("coordination/PROJECT-BINDING.yaml");
    fs::copy(source, root.join("coordination/PROJECT-BINDING.yaml")).unwrap();
    root
}
fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_orch"))
        .arg("--root")
        .arg(root)
        .args(args)
        .current_dir(root)
        .output()
        .unwrap()
}
fn manifest(root: &Path) -> Vec<(PathBuf, u64, u64, u64, std::time::SystemTime, Vec<u8>)> {
    let mut rows = Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        let m = fs::symlink_metadata(&path).unwrap();
        rows.push((
            path.clone(),
            m.dev(),
            m.ino(),
            m.len(),
            m.modified().unwrap(),
            if m.is_file() {
                fs::read(&path).unwrap()
            } else {
                vec![]
            },
        ));
        if m.is_dir() && !m.file_type().is_symlink() {
            rows.extend(manifest(&path));
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}
#[test]
fn closed_round_gc_dry_run_apply_and_latest_report_are_real_cli_paths() {
    let root = fixture("maintenance-cli-closed");
    let ledger = root.join("coordination/rounds/rClosed/events.jsonl");
    fs::create_dir_all(ledger.parent().unwrap()).unwrap();
    let bytes=serde_json::to_vec(&serde_json::json!({"eventId":"closed","ts":"2026-09-08T00:00:00Z","actor":"runtime:orch","type":"RoundClosed","round":"rClosed","payload":{}})).unwrap();
    fs::write(&ledger, &bytes).unwrap();
    fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "rClosed\n").unwrap();
    let cache = root.join("orch/target/consult-unregistered/cache");
    fs::create_dir_all(cache.parent().unwrap()).unwrap();
    fs::write(&cache, b"keep").unwrap();
    let before = manifest(&root);
    let dry = run(&root, &["sites", "gc", "--dry-run"]);
    assert!(
        dry.status.success(),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&dry.stdout).unwrap();
    assert_eq!(report["dryRun"], true);
    assert_eq!(manifest(&root), before);
    let applied = run(&root, &["sites", "gc"]);
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert_eq!(fs::read(&ledger).unwrap(), bytes);
    assert_eq!(fs::read(&cache).unwrap(), b"keep");
    let after = manifest(&root);
    let latest = run(&root, &["sites", "cache", "status", "--last-maintenance"]);
    assert!(
        latest.status.success(),
        "{}",
        String::from_utf8_lossy(&latest.stderr)
    );
    let saved: serde_json::Value = serde_json::from_slice(&latest.stdout).unwrap();
    assert_eq!(saved["dryRun"], false);
    assert_eq!(saved["removedLogicalBytes"], 0);
    assert_eq!(manifest(&root), after);
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn round_open_uses_floor_and_keeps_cleanup_reachable() {
    let root = fixture("maintenance-cli-floor");
    fs::create_dir_all(root.join(".orch")).unwrap();
    fs::write(
        root.join(".orch/machine.yaml"),
        format!("storage:\n  floorBytes: {}\n", u64::MAX),
    )
    .unwrap();
    let denied = run(&root, &["round", "open", "rNew", "--purpose", "fixture"]);
    assert!(!denied.status.success());
    assert!(
        String::from_utf8_lossy(&denied.stderr).contains("round-open storage admission refused"),
        "{}",
        String::from_utf8_lossy(&denied.stderr)
    );
    assert!(!root.join("coordination/rounds/rNew").exists());
    assert!(!root.join(".git/hooks/reference-transaction").exists());
    let rescue = run(&root, &["sites", "gc"]);
    assert!(
        rescue.status.success(),
        "{}",
        String::from_utf8_lossy(&rescue.stderr)
    );
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn round_open_allows_cleanup_errors_when_space_is_sufficient() {
    let root = fixture("maintenance-cli-ample");
    let old = root.join("coordination/rounds/rBad/events.jsonl");
    fs::create_dir_all(old.parent().unwrap()).unwrap();
    fs::write(&old, b"broken old ledger\n").unwrap();
    let opened = run(&root, &["round", "open", "rNew", "--purpose", "fixture"]);
    assert!(
        opened.status.success(),
        "{}",
        String::from_utf8_lossy(&opened.stderr)
    );
    assert_eq!(fs::read(&old).unwrap(), b"broken old ledger\n");
    assert!(root.join("coordination/rounds/rNew/events.jsonl").is_file());
    let report = orch_host::reclaim::latest_maintenance_report(&root)
        .unwrap()
        .unwrap();
    assert!(report.items.iter().any(|i| i.disposition == "failed"));
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn failed_native_space_probe_prevents_round_growth_but_not_maintenance() {
    let root = fixture("maintenance-cli-probe-failure");
    let bin = root.join("probe-bin");
    fs::create_dir(&bin).unwrap();
    let df = bin.join("df");
    fs::write(&df, b"#!/bin/sh\nexit 9\n").unwrap();
    fs::set_permissions(&df, fs::Permissions::from_mode(0o700)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let command = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_orch"))
            .arg("--root")
            .arg(&root)
            .args(args)
            .env("PATH", &path)
            .current_dir(&root)
            .output()
            .unwrap()
    };
    let denied = command(&["round", "open", "rNew", "--purpose", "fixture"]);
    assert!(!denied.status.success());
    assert!(!root.join("coordination/rounds/rNew").exists());
    let rescue = command(&["sites", "gc"]);
    assert!(
        rescue.status.success(),
        "{}",
        String::from_utf8_lossy(&rescue.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&rescue.stdout).unwrap();
    assert!(!report["filesystemsAfter"][0]["error"].is_null());
    fs::remove_dir_all(root).unwrap();
}
