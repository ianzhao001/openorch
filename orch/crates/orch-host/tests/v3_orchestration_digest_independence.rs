use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn source_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap()
        .to_path_buf()
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

fn fixture_binding() -> String {
    let source =
        fs::read_to_string(source_root().join("coordination/PROJECT-BINDING.yaml")).unwrap();
    let mut value: serde_yaml::Value = serde_yaml::from_str(&source).unwrap();
    for command in value["commands"].as_mapping_mut().unwrap().values_mut() {
        command["argv"] = serde_yaml::to_value(vec!["/usr/bin/true"]).unwrap();
        command["timeoutSeconds"] = serde_yaml::to_value(30_u64).unwrap();
    }
    serde_yaml::to_string(&value).unwrap()
}

#[test]
fn v3_ir_ignores_roster_mode_preset_and_local_harness_config_bytes() {
    let root = source_root().join("orch/target/test-tmp").join(format!(
        "v3-orchestration-independent-{}",
        std::process::id()
    ));
    if root.exists() {
        fs::remove_dir_all(&root).unwrap();
    }
    fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("coordination/BOARD.md"), "# fixture\n").unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
    fs::write(
        root.join("coordination/PROJECT-BINDING.yaml"),
        fixture_binding(),
    )
    .unwrap();
    fs::write(
        root.join(".gitignore"),
        ".orch/\n.worktrees/\n.cowork-temp/\ncoordination/runtime/\ncoordination/rounds/*/dispatch/\norch/target/\n",
    )
    .unwrap();
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
            "-q",
            "-m",
            "fixture base",
        ],
    );

    orch_host::round::run_open_v3(&root, "r83", "digest independence", false).unwrap();
    fs::write(
        root.join("coordination/rounds/r83/tasks/B902.md"),
        "---\n\
schemaVersion: 3\n\
taskId: B902\n\
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
---\n# B902\n",
    )
    .unwrap();
    let first = orch_host::plan::run_plan(&root).unwrap();
    let ir_path = root.join("coordination/rounds/r83/ROUND-IR.yaml");
    let first_bytes = fs::read(&ir_path).unwrap();

    for (path, bytes) in [
        (
            "coordination/modes/ignored.yaml",
            b"mode: changed\n".as_slice(),
        ),
        (
            "coordination/agents.yaml",
            b"agents: {changed: true}\n".as_slice(),
        ),
        (
            "coordination/harnesses.yaml",
            b"harnesses: {changed: true}\n".as_slice(),
        ),
        (
            "coordination/consult/presets.yaml",
            b"presets: [{name: changed}]\n".as_slice(),
        ),
        (
            ".orch/harnesses.yaml",
            b"harnesses: {local-only: changed}\n".as_slice(),
        ),
    ] {
        let destination = root.join(path);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(destination, bytes).unwrap();
    }

    let replay = orch_host::plan::run_plan(&root).unwrap();
    assert_eq!(replay.revision, first.revision);
    assert_eq!(replay.digest, first.digest);
    assert!(!replay.ir_written);
    assert!(!replay.event_appended);
    assert_eq!(fs::read(ir_path).unwrap(), first_bytes);
    fs::remove_dir_all(root).ok();
}
