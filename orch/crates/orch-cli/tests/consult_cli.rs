//! B322 CLI contract: consultation membership is explicit and action-local.

mod support;

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

fn temp_root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("orch workspace")
        .join("target/test-tmp")
        .join(format!(
            "b322-consult-cli-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
    fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn consult_help_exposes_only_explicit_membership() {
    let output = run(&["consult", "--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("--harness <ALIAS>"));
    assert!(stdout.contains("--attach"));
    for retired in ["--preset", "--judge", "--no-judge"] {
        assert!(
            !stdout.contains(retired),
            "retired surface leaked: {retired}"
        );
    }
}

#[test]
fn consult_requires_at_least_one_harness() {
    let output = run(&["consult", "question.md"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--harness"), "{stderr}");
}

#[test]
fn retired_grouping_flags_are_unreachable() {
    for retired in ["--preset", "--judge", "--no-judge"] {
        let output = run(&[
            "consult",
            "question.md",
            "--harness",
            "alpha",
            retired,
            "retired",
        ]);
        assert!(!output.status.success(), "{retired}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("unexpected argument"),
            "{retired}: {stderr}"
        );
    }
}

#[test]
fn repeated_harness_flags_reach_runtime_in_caller_order() {
    let root = temp_root();
    let question = root.join("question.md");
    fs::write(&question, "question\n").unwrap();
    let output = run(&[
        "--root",
        root.to_str().unwrap(),
        "consult",
        question.to_str().unwrap(),
        "--harness",
        "alpha",
        "--harness",
        "beta",
    ]);
    assert!(
        !output.status.success(),
        "fixture intentionally lacks config"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("unexpected argument"), "{stderr}");
    assert!(
        !stderr.contains("required arguments were not provided"),
        "{stderr}"
    );
    fs::remove_dir_all(root).unwrap();
}
