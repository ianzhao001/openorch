//! B327 production regression: a slow owned fixture is released only after
//! checking that its fast sibling's complete immutable artifact is available.
//! M4 move archival back after all joins: this test must fail, then restore.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use sha2::{Digest, Sha256};
use orch_host::consult::{run_consultation, ConsultArgs};

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git").arg("-C").arg(root).args(args).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn fast_artifact_is_complete_before_the_slow_member_is_released() {
    let root = orch_host::util::test_scratch_dir("b327-real-progress");
    fs::create_dir_all(root.join(".orch")).unwrap();
    fs::create_dir_all(root.join("coordination")).unwrap();
    fs::write(root.join(".gitignore"), ".orch/harnesses.yaml\n.cowork-temp/\ncoordination/consultations/\n").unwrap();
    fs::write(root.join("question.md"), "Read-only fixture. Return the exact final answer.\n").unwrap();
    fs::write(root.join("coordination/PROJECT-BINDING.yaml"), "data:\n  forbiddenArtifactPatterns: []\n").unwrap();
    for alias in ["fast", "slow"] {
        let body = if alias == "slow" {
            "touch slow.started\nwhile [ ! -f release-slow ]; do /bin/sleep 0.02; done\n"
        } else {
            "touch fast.started\n"
        };
        let script = format!("#!/bin/sh\nset -eu\n{body}printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"FINAL: PASS {alias}\"}}'\n");
        let path = root.join(alias);
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let config = format!("version: 1\nharnesses:\n  fast:\n    driver: claude\n    executable: {}\n    enabled: true\n    cwdPolicy: project-root\n  slow:\n    driver: claude\n    executable: {}\n    enabled: true\n    cwdPolicy: project-root\n", root.join("fast").display(), root.join("slow").display());
    fs::write(root.join(".orch/harnesses.yaml"), config).unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", ".gitignore", "question.md", "coordination/PROJECT-BINDING.yaml", "fast", "slow"]);
    git(&root, &["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture"]);
    let work = root.clone();
    let runner = std::thread::spawn(move || run_consultation(&work, &ConsultArgs {
        question: "question.md".into(), harnesses: vec!["fast".into(), "slow".into()],
        attachments: vec![], member_timeout_secs: Some(30), total_wall_secs: Some(30),
    }));
    let until = Instant::now() + Duration::from_secs(10);
    let mut observed = None;
    while Instant::now() < until && observed.is_none() {
        if root.join("slow.started").exists() {
            if let Ok(entries) = fs::read_dir(root.join("coordination/consultations")) {
                for entry in entries.flatten() {
                    let dir = entry.path().join("fusion");
                    let Some(manifest) = fs::read(dir.join("0-fast.manifest.json")).ok()
                        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok()) else { continue };
                    let Some(bytes) = fs::read(dir.join("0-fast.md")).ok() else { continue };
                    if manifest["status"] == "ok"
                        && manifest["artifact"]["bytes"].as_u64() == Some(bytes.len() as u64)
                        && manifest["artifact"]["sha256"].as_str() == Some(hex::encode(Sha256::digest(&bytes)).as_str()) {
                        observed = Some(bytes);
                    }
                }
            }
        }
        if observed.is_none() { std::thread::sleep(Duration::from_millis(20)); }
    }
    // Always release and join before assertions: a failed regression cannot
    // strand the owned child or turn a test timeout into a cleanup success.
    let slow_was_waiting = root.join("slow.started").exists() && !root.join("release-slow").exists();
    fs::write(root.join("release-slow"), "release\n").unwrap();
    let outcome = runner.join().unwrap().unwrap();
    let early = observed.is_some();
    let stable = observed.as_ref().is_some_and(|bytes| fs::read(outcome.dir.join("fusion/0-fast.md")).unwrap() == *bytes);
    let statuses = outcome.members.iter().map(|member| member.status).collect::<Vec<_>>();
    fs::remove_dir_all(&root).unwrap();
    assert!(slow_was_waiting && early && stable, "fast result was not complete and stable before slow release; statuses={statuses:?}");
}
