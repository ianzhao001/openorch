use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use orch_host::harness_config::{
    load_harness_config_snapshot, parse_harness_config_snapshot, HarnessAction, HarnessAvailability,
};

fn temp_root(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("orch-b319-{label}-{}-{nonce}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q"]);
    fs::write(root.join(".gitignore"), ".orch/harnesses.yaml\n").unwrap();
    fs::create_dir(root.join(".orch")).unwrap();
    root
}

fn git(root: &Path, args: &[&str]) {
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
}

fn config(model: &str) -> String {
    format!(
        "version: 1\nharnesses:\n  alpha:\n    driver: opencode\n    executable: /bin/echo\n    enabled: true\n    defaults: {{provider: local, model: {model}, effort: high, mode: normal}}\n    cwdPolicy: project-root\n"
    )
}

#[test]
fn captured_snapshot_survives_disk_replacement_and_next_action_gets_new_digest() {
    let root = temp_root("snapshot");
    let path = root.join(".orch/harnesses.yaml");
    fs::write(&path, config("model-a")).unwrap();
    let first = load_harness_config_snapshot(&root).unwrap();
    fs::write(&path, config("model-b")).unwrap();
    let second = load_harness_config_snapshot(&root).unwrap();

    assert_eq!(
        first
            .resolve("alpha", HarnessAction::Execute)
            .unwrap()
            .model(),
        Some("model-a")
    );
    assert_eq!(
        second
            .resolve("alpha", HarnessAction::Execute)
            .unwrap()
            .model(),
        Some("model-b")
    );
    assert_ne!(first.sha256(), second.sha256());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn linked_worktree_reads_the_main_repository_snapshot() {
    let root = temp_root("linked-main");
    let config_path = root.join(".orch/harnesses.yaml");
    fs::write(&config_path, config("main-model")).unwrap();
    fs::write(root.join("tracked.txt"), "base\n").unwrap();
    git(&root, &["add", ".gitignore", "tracked.txt"]);
    git(
        &root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    );
    let linked = root.with_extension("linked");
    let linked_text = linked.to_str().unwrap();
    git(
        &root,
        &["worktree", "add", "-q", "-b", "linked", linked_text],
    );

    let snapshot = load_harness_config_snapshot(&linked).unwrap();
    assert_eq!(
        snapshot.source_path(),
        fs::canonicalize(&config_path).unwrap().as_path()
    );
    assert_eq!(
        snapshot
            .resolve("alpha", HarnessAction::Execute)
            .unwrap()
            .model(),
        Some("main-model")
    );

    git(&root, &["worktree", "remove", "--force", linked_text]);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn loader_rejects_symlinked_parent_and_leaf_before_read() {
    let parent_root = temp_root("parent-symlink");
    fs::remove_dir(parent_root.join(".orch")).unwrap();
    let real = parent_root.join("real-config");
    fs::create_dir(&real).unwrap();
    fs::write(real.join("harnesses.yaml"), config("model-a")).unwrap();
    symlink(&real, parent_root.join(".orch")).unwrap();
    let error = load_harness_config_snapshot(&parent_root)
        .unwrap_err()
        .to_string();
    assert!(error.contains("symlink"), "{error}");
    fs::remove_dir_all(parent_root).unwrap();

    let leaf_root = temp_root("leaf-symlink");
    let target = leaf_root.join("target.yaml");
    fs::write(&target, config("model-a")).unwrap();
    symlink(&target, leaf_root.join(".orch/harnesses.yaml")).unwrap();
    let error = load_harness_config_snapshot(&leaf_root)
        .unwrap_err()
        .to_string();
    assert!(error.contains("symlink"), "{error}");
    fs::remove_dir_all(leaf_root).unwrap();
}

#[test]
fn selected_unavailable_alias_fails_without_fallback() {
    let yaml = r#"
version: 1
harnesses:
  consult-only:
    driver: cursor
    executable: /bin/echo
    enabled: true
    cwdPolicy: project-root
  peer:
    driver: opencode
    executable: /bin/echo
    enabled: true
    cwdPolicy: project-root
"#;
    let snapshot =
        parse_harness_config_snapshot(Path::new("/repo/.orch/harnesses.yaml"), yaml).unwrap();
    assert!(snapshot
        .resolve("consult-only", HarnessAction::Review)
        .unwrap_err()
        .to_string()
        .contains("does not support review"));
    assert!(snapshot.resolve("peer", HarnessAction::Review).is_ok());
}

#[test]
fn selected_unsupported_tuple_fails_without_poisoning_a_peer() {
    let yaml = r#"
version: 1
harnesses:
  bad-tuple:
    driver: agy
    executable: /bin/echo
    enabled: true
    defaults: {effort: ultra}
    cwdPolicy: project-root
  peer:
    driver: opencode
    executable: /bin/echo
    enabled: true
    defaults: {effort: ultra}
    cwdPolicy: project-root
"#;
    let snapshot =
        parse_harness_config_snapshot(Path::new("/repo/.orch/harnesses.yaml"), yaml).unwrap();
    assert!(snapshot
        .resolve("bad-tuple", HarnessAction::Execute)
        .unwrap_err()
        .to_string()
        .contains("does not support effort"));
    assert!(snapshot.resolve("peer", HarnessAction::Execute).is_ok());
    let rows = snapshot.discover_without_tokens();
    assert!(matches!(
        rows[0].availability(),
        HarnessAvailability::Unsupported(_)
    ));
    assert!(matches!(
        rows[1].availability(),
        HarnessAvailability::Supported
    ));
}

#[test]
fn unknown_and_missing_rows_do_not_erase_supported_peers() {
    let yaml = r#"
version: 1
harnesses:
  missing:
    driver: opencode
    executable: /definitely/missing/orch-b319
    enabled: true
    cwdPolicy: project-root
  peer:
    driver: opencode
    executable: /bin/echo
    enabled: true
    cwdPolicy: project-root
  unknown:
    driver: future-driver
    executable: /bin/echo
    enabled: true
    cwdPolicy: project-root
"#;
    let snapshot =
        parse_harness_config_snapshot(Path::new("/repo/.orch/harnesses.yaml"), yaml).unwrap();
    let rows = snapshot.discover_without_tokens();
    assert_eq!(rows.len(), 3);
    assert!(matches!(
        rows[0].availability(),
        HarnessAvailability::Unsupported(_)
    ));
    assert!(matches!(
        rows[1].availability(),
        HarnessAvailability::Supported
    ));
    assert!(matches!(
        rows[2].availability(),
        HarnessAvailability::Unknown(_)
    ));
    assert!(snapshot.resolve("peer", HarnessAction::Consult).is_ok());
}

#[test]
fn exact_schema_and_effort_vocabulary_fail_closed() {
    for (fragment, needle) in [
        ("    timeout: 30\n", "timeout"),
        ("    env: {}\n", "env"),
        ("    defaults: {effort: gigantic}\n", "effort"),
    ] {
        let yaml = format!(
            "version: 1\nharnesses:\n  bad:\n    driver: opencode\n    executable: /bin/echo\n    enabled: true\n{fragment}    cwdPolicy: project-root\n"
        );
        let error = parse_harness_config_snapshot(Path::new("/repo/.orch/harnesses.yaml"), &yaml)
            .unwrap_err()
            .to_string();
        assert!(error.contains(needle), "{error}");
    }

    let missing_enabled = r#"
version: 1
harnesses:
  bad:
    driver: opencode
    executable: /bin/echo
    cwdPolicy: project-root
"#;
    let error =
        parse_harness_config_snapshot(Path::new("/repo/.orch/harnesses.yaml"), missing_enabled)
            .unwrap_err()
            .to_string();
    assert!(error.contains("enabled"), "{error}");
}

#[test]
fn loader_requires_the_exact_gitignore_rule() {
    let root = temp_root("not-ignored");
    fs::write(root.join(".gitignore"), ".orch/machine.yaml\n").unwrap();
    fs::write(root.join(".orch/harnesses.yaml"), config("model-a")).unwrap();
    let error = load_harness_config_snapshot(&root).unwrap_err().to_string();
    assert!(error.contains("gitignore"), "{error}");
    fs::remove_dir_all(root).unwrap();
}
