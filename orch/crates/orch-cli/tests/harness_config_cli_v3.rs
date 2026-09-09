use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_root(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "orch-b319-cli-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(root.join(".orch")).unwrap();
    let init = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["init", "-q"])
        .output()
        .unwrap();
    assert!(init.status.success());
    fs::write(root.join(".gitignore"), ".orch/harnesses.yaml\n").unwrap();
    root
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_orch"))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn list_and_lint_are_token_free_read_only_surfaces() {
    let root = temp_root("valid");
    fs::write(
        root.join(".orch/harnesses.yaml"),
        r#"version: 1
harnesses:
  alpha:
    driver: opencode
    executable: /bin/echo
    enabled: true
    defaults: {provider: local, model: model-a, effort: high, mode: normal}
    cwdPolicy: project-root
"#,
    )
    .unwrap();

    let list = run(&root, &["harness", "list"]);
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    let stdout = String::from_utf8(list.stdout).unwrap();
    assert!(stdout.contains("alpha\topencode\tsupported\t—"));
    assert!(stdout.contains("sha256="));

    let lint = run(&root, &["harness", "lint"]);
    assert!(
        lint.status.success(),
        "{}",
        String::from_utf8_lossy(&lint.stderr)
    );
    assert!(String::from_utf8(lint.stdout)
        .unwrap()
        .contains("aliases=1"));
    assert!(!root.join("spawned-by-harness-list").exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn list_preserves_supported_peer_while_lint_rejects_unknown_row() {
    let root = temp_root("row-local");
    fs::write(
        root.join(".orch/harnesses.yaml"),
        r#"version: 1
harnesses:
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
"#,
    )
    .unwrap();

    let list = run(&root, &["harness", "list"]);
    assert!(list.status.success());
    let stdout = String::from_utf8(list.stdout).unwrap();
    assert!(stdout.contains("peer\topencode\tsupported"));
    assert!(stdout.contains("unknown\tfuture-driver\tunknown"));

    let lint = run(&root, &["harness", "lint"]);
    assert!(!lint.status.success());
    assert!(String::from_utf8_lossy(&lint.stderr).contains("unknown=unknown"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn help_and_guide_cover_both_harness_leaves() {
    let help = Command::new(env!("CARGO_BIN_EXE_orch"))
        .args(["harness", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    let stdout = String::from_utf8(help.stdout).unwrap();
    assert!(stdout.contains("list"));
    assert!(stdout.contains("lint"));

    let guide = Command::new(env!("CARGO_BIN_EXE_orch"))
        .args(["guide", "--check"])
        .output()
        .unwrap();
    assert!(
        guide.status.success(),
        "{}",
        String::from_utf8_lossy(&guide.stderr)
    );
}

#[test]
fn malformed_or_missing_config_fails_without_creating_it() {
    let root = temp_root("missing");
    let missing = run(&root, &["harness", "list"]);
    assert!(!missing.status.success());
    assert!(!root.join(".orch/harnesses.yaml").exists());

    fs::write(
        root.join(".orch/harnesses.yaml"),
        "version: 1\nharnesses:\n  bad:\n    driver: opencode\n    executable: opencode\n    enabled: true\n    cwdPolicy: project-root\n",
    )
    .unwrap();
    let malformed = run(&root, &["harness", "lint"]);
    assert!(!malformed.status.success());
    assert!(String::from_utf8_lossy(&malformed.stderr).contains("executable"));
    fs::remove_dir_all(root).unwrap();
}
