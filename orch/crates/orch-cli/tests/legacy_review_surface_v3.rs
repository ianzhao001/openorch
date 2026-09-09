#![cfg(feature = "selfhost")]
//! B323 · live Panel/Quorum/runtime-policy surfaces are unreachable.
//!
//! Expected red: assertion, because the pre-B323 binary still exposes the
//! legacy live writers. Historical decode/audit is intentionally not tested by
//! public reachability here.
//! M1 accept any retired/unknown dynamic card or IR key even when null/empty -> exact-key companion red.
//! M2 reread current registry/mode/adapter during v3 replay -> digest-independence companion red.
//! M3 keep Panel/runtime-policy/reconcile or retired args hidden -> this CLI parser test red.
//! M4 route generic review through legacy `(task, role)` slots -> generic-slot companion red.
//! M5 revive evidence/provider/SmartClaw/primary magic -> policy-free acceptance companion red.
//! M6 allow schema1/2 live writers to append -> legacy-write zero-byte companion red.

use std::process::Command;

fn output(args: &[&str]) -> String {
    let result = Command::new(env!("CARGO_BIN_EXE_orch"))
        .args(args)
        .output()
        .expect("run orch help");
    assert!(
        result.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).expect("UTF-8 help")
}

fn command_is_unreachable(args: &[&str]) {
    let result = Command::new(env!("CARGO_BIN_EXE_orch"))
        .args(args)
        .output()
        .expect("run retired command probe");
    assert!(
        !result.status.success(),
        "retired command still parses (possibly hidden): {args:?}"
    );
}

fn retired_argument_is_unreachable(args: &[&str], retired: &str) {
    let result = Command::new(env!("CARGO_BIN_EXE_orch"))
        .args(args)
        .output()
        .expect("run retired argument probe");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        !result.status.success(),
        "retired argument still parses: {args:?}"
    );
    assert!(
        stderr.contains("unexpected argument") && stderr.contains(retired),
        "failure did not come from rejecting {retired}: {stderr}"
    );
}

fn command_names(help: &str) -> Vec<String> {
    let mut in_commands = false;
    let mut names = Vec::new();
    for line in help.lines() {
        if line == "Commands:" {
            in_commands = true;
            continue;
        }
        if in_commands && line == "Options:" {
            break;
        }
        if in_commands {
            let trimmed = line.trim();
            if let Some(name) = (!trimmed.is_empty())
                .then(|| trimmed.split_whitespace().next())
                .flatten()
            {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    names
}

#[test]
fn live_panel_and_runtime_policy_commands_are_gone() {
    let top = output(&["--help"]);
    assert!(!top.contains("runtime-policy"));
    let review = output(&["review", "--help"]);
    assert_eq!(command_names(&review), ["deliver"]);

    let deliver = output(&["review", "deliver", "--help"]);
    assert!(deliver.contains("--harness"));
    assert!(deliver.contains("--wake-id"));
    assert!(!deliver.contains("--role"));
    assert!(!deliver.contains("--agent"));

    let wake = output(&["wake", "--help"]);
    assert!(!wake.contains("--reissue"));
    assert!(!wake.contains("--role"));

    for retired_help in [
        &["runtime-policy", "--help"][..],
        &["review", "reconcile", "--help"][..],
        &["review", "panel", "--help"][..],
    ] {
        command_is_unreachable(retired_help);
    }
    for (retired_arg, retired) in [
        (
            &[
                "review",
                "deliver",
                "B323",
                "B323-A0001",
                "--harness",
                "alpha",
                "--wake-id",
                "wake-1",
                "--role",
                "primary",
                "--help",
            ][..],
            "--role",
        ),
        (
            &[
                "review",
                "deliver",
                "B323",
                "B323-A0001",
                "--harness",
                "alpha",
                "--wake-id",
                "wake-1",
                "--agent",
                "executor-x",
                "--help",
            ][..],
            "--agent",
        ),
        (
            &["wake", "alpha", "--role", "primary", "--help"][..],
            "--role",
        ),
        (
            &["wake", "alpha", "--reissue", "wake-old", "--help"][..],
            "--reissue",
        ),
    ] {
        retired_argument_is_unreachable(retired_arg, retired);
    }
}
