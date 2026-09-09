//! The production graph is resolved for the CLI package independently of the
//! feature union used to compile this integration test in the whole workspace.
use std::{path::Path, process::Command};

#[test]
fn default_cli_actual_graph_excludes_ui_and_host_selfhost() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap();
    let output = Command::new(env!("CARGO"))
        .current_dir(root)
        .args(["tree", "-p", "orch-cli", "--no-default-features", "--locked", "--manifest-path",
               "orch/Cargo.toml", "--edges", "normal", "--prefix", "none", "--format", "{p}|{f}"])
        .output().expect("run actual Cargo graph");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let tree = String::from_utf8(output.stdout).unwrap();
    for name in ["orch-ui", "ratatui", "crossterm", "axum", "tokio"] {
        assert!(!tree.lines().any(|line| line.starts_with(&format!("{name} v"))),
                "default CLI contains UI dependency {name}: {tree}");
    }
    let hosts = tree.lines().filter(|line| line.starts_with("orch-host v")).collect::<Vec<_>>();
    assert_eq!(hosts.len(), 1, "one actual host dependency must be checked: {tree}");
    let features = hosts[0].split_once('|').expect("Cargo feature output").1;
    assert!(!features.split(',').any(|feature| feature.trim() == "selfhost"),
            "default CLI enabled task modules through host features: {hosts:?}");
}
