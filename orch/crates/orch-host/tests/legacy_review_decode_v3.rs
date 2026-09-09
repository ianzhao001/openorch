//! B323: closed schema-1/2 review history remains auditable without live registries.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

static NEXT: AtomicU64 = AtomicU64::new(0);

const CASES: &[(&str, &str, usize, usize, &[&str])] = &[
    (
        "r79",
        "bda0af10b5187d299cd1962ef5bc96860cb0dcf8a41a952b6140089c261a5d76",
        158,
        137_025,
        &["B303"],
    ),
    (
        "r80",
        "46b63a1b48adb0cf29e83cf2c2555b4292b457206b0fc824a3147252f9fa2db4",
        298,
        267_314,
        &["B309", "B308"],
    ),
    (
        "r81",
        "3dc388176374d3fbaabeda2fc6390d12a60d7aedd29ae7e9ed6d8c652f7af66f",
        762,
        649_358,
        &["B310", "B306", "B304", "B305"],
    ),
    (
        "r82",
        "87c9bff5a40c415eec1a147c2eea92b836ca8391efd9b5eef536e3c3c01cbe1a",
        790,
        664_441,
        &["B311", "B312", "B313", "B314"],
    ),
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap()
        .to_path_buf()
}

fn scratch_root() -> PathBuf {
    repo_root().join("orch/target/test-tmp").join(format!(
        "b323-legacy-decode-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn remove_file_if_present(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("remove {}: {error}", path.display()),
    }
}

fn remove_dir_if_present(path: &Path) {
    match fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("remove {}: {error}", path.display()),
    }
}

fn clone_fixture() -> PathBuf {
    let root = scratch_root();
    let _ = fs::remove_dir_all(&root);
    let source_head = Command::new("git")
        .args(["-C", repo_root().to_str().unwrap(), "rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(source_head.status.success());
    let source_head = String::from_utf8(source_head.stdout)
        .unwrap()
        .trim()
        .to_string();
    let output = Command::new("git")
        .args(["-c", "core.fsmonitor=false", "clone", "-q"])
        .arg(repo_root())
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "clone legacy fixture: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let main = Command::new("git")
        .args(["-c", "core.fsmonitor=false", "-C"])
        .arg(&root)
        .args(["checkout", "-q", "-B", "main", "HEAD"])
        .output()
        .unwrap();
    assert!(
        main.status.success(),
        "materialize fixture main: {}",
        String::from_utf8_lossy(&main.stderr)
    );
    let cloned_main = Command::new("git")
        .args(["-C", root.to_str().unwrap(), "rev-parse", "main"])
        .output()
        .unwrap();
    assert!(cloned_main.status.success());
    assert_eq!(
        String::from_utf8(cloned_main.stdout).unwrap().trim(),
        source_head,
        "fixture main must be the exact source candidate HEAD"
    );
    remove_file_if_present(&root.join("coordination/agents.yaml"));
    remove_file_if_present(&root.join("coordination/harnesses.yaml"));
    remove_dir_if_present(&root.join("coordination/adapters"));
    remove_dir_if_present(&root.join("coordination/modes"));
    remove_dir_if_present(&root.join("coordination/runtime/nongate-inbox"));
    root
}

fn audit(root: &Path, validate_recorded_tasks: bool) -> Vec<(String, usize, usize, usize)> {
    CASES
        .iter()
        .map(
            |(round, expected_sha, expected_lines, expected_bytes, tasks)| {
                let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
                let bytes = fs::read(&ledger_path).unwrap();
                assert_eq!(hex::encode(Sha256::digest(&bytes)), *expected_sha);
                assert_eq!(bytes.len(), *expected_bytes);
                assert_eq!(
                    bytes
                        .split(|byte| *byte == b'\n')
                        .filter(|line| !line.is_empty())
                        .count(),
                    *expected_lines
                );
                let events = orch_core::read_ledger(&ledger_path).unwrap().events;
                orch_host::legacy::validate_runtime_event_history_v1_at_root(root, &events, round)
                    .unwrap();
                let projection = orch_core::fold(&events);
                assert!(projection.round_closed, "{round} must remain closed");
                let v1 = events
                    .iter()
                    .filter(|event| {
                        orch_host::legacy::decode_runtime_event_v1(event)
                            .unwrap()
                            .is_some()
                    })
                    .count();
                if validate_recorded_tasks {
                    for task in *tasks {
                        assert_eq!(
                            projection.tasks.get(*task).and_then(|task| task.state),
                            Some(orch_core::TaskState::Recorded),
                            "{round}/{task} status drifted"
                        );
                        orch_host::legacy::validate_archived_record_chain(
                            root, round, task, &events,
                        )
                        .unwrap();
                    }
                }
                (round.to_string(), events.len(), v1, tasks.len())
            },
        )
        .collect()
}

#[test]
fn closed_review_history_ignores_current_registry_bytes_and_rejects_event_drift() {
    let root = clone_fixture();
    for retired in [
        "coordination/agents.yaml",
        "coordination/harnesses.yaml",
        "coordination/modes/relay-selfhost.yaml",
        "coordination/adapters/consult-opencode.yaml",
        "coordination/runtime/nongate-inbox",
    ] {
        assert!(
            !root.join(retired).exists(),
            "live path survived: {retired}"
        );
    }
    let baseline = audit(&root, true);
    assert!(!root.join("coordination/runtime/nongate-inbox").exists());
    assert_eq!(
        baseline.iter().map(|(_, _, v1, _)| *v1).collect::<Vec<_>>(),
        [0, 0, 89, 49]
    );

    fs::create_dir_all(root.join("coordination/adapters")).unwrap();
    fs::create_dir_all(root.join("coordination/modes")).unwrap();
    fs::write(root.join("coordination/agents.yaml"), "not: [valid\n").unwrap();
    fs::write(root.join("coordination/harnesses.yaml"), "not: [valid\n").unwrap();
    fs::write(
        root.join("coordination/adapters/poison.yaml"),
        "not: [valid\n",
    )
    .unwrap();
    fs::write(root.join("coordination/modes/poison.yaml"), "not: [valid\n").unwrap();
    assert_eq!(audit(&root, true), baseline);
    assert!(!root.join("coordination/runtime/nongate-inbox").exists());

    let ledger_path = root.join("coordination/rounds/r81/events.jsonl");
    let mut events = orch_core::read_ledger(&ledger_path).unwrap().events;
    let typed = events
        .iter_mut()
        .find(|event| {
            orch_host::legacy::decode_runtime_event_v1(event)
                .unwrap()
                .is_some()
        })
        .unwrap();
    typed.payload.as_mut().unwrap()["schemaVersion"] = serde_json::json!(999);
    assert!(orch_host::legacy::validate_runtime_event_history_v1(&events, "r81").is_err());

    fs::remove_dir_all(root).unwrap();
}
