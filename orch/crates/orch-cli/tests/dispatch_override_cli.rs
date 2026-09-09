#![cfg(feature = "selfhost")]
mod support;

use std::process::Command;

fn fixture_orch_command(extras: &[(&str, &str)]) -> Command {
    let mut command = Command::new(support::orch_bin());
    support::configure_fixture_git_env(&mut command, extras);
    command
}

fn help(subcommand: &str) -> String {
    let output = fixture_orch_command(&[])
        .args([subcommand, "--help"])
        .output()
        .expect("run orch help");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("help is utf-8")
}

#[test]
fn dispatch_exposes_explicit_ambiguous_active_override() {
    let output = help("dispatch");
    assert!(
        output.contains("--override-ambiguous-active"),
        "dispatch help must expose the explicit planner override:\n{output}"
    );

    let source = include_str!("../src/main.rs");
    assert!(
        source.contains(
            "run_dispatch_with_override(\n        root,\n        task,\n        no_wake,\n        override_ambiguous_active,"
        ),
        "CLI override must be forwarded to the guarded host dispatch entry point"
    );
}
