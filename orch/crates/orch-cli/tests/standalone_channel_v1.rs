//! B330 real standalone product behavior; every provider is a local owned shell fixture.
mod support;
use std::{fs, path::{Path, PathBuf}, process::Command, time::{Duration, Instant}};
use std::os::unix::fs::PermissionsExt;

fn fixture_orch_command() -> Command {
    let mut command = Command::new(support::orch_bin());
    support::configure_fixture_git_env(&mut command, &[]);
    command
}

struct Fixture { root: PathBuf }
impl Fixture {
    fn new(driver: &str, body: &str) -> Self {
        let root = orch_host::util::test_scratch_dir("b330-standalone");
        fs::create_dir_all(root.join(".orch")).unwrap();
        fs::write(root.join(".gitignore"), ".orch/harnesses.yaml\ncoordination/\n.cowork-temp/\n").unwrap();
        fs::write(root.join("question.md"), "Read only. Return the fixture answer.\n").unwrap();
        let executable = root.join(driver);
        fs::write(&executable, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(root.join(".orch/harnesses.yaml"), format!(
            "version: 1\nharnesses:\n  mock:\n    driver: {driver}\n    executable: {}\n    enabled: true\n    cwdPolicy: project-root\n", executable.display())).unwrap();
        for args in [vec!["init", "-q"], vec!["add", ".gitignore", "question.md", driver],
            vec!["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture"]] {
            let out = support::fixture_git_command(&root).args(args).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        }
        Self { root }
    }
    fn command(&self, args: &[&str]) -> std::process::Output {
        let mut command = fixture_orch_command();
        command.arg("--root").arg(&self.root).args(args).output().unwrap()
    }
    fn no_task_state(&self) {
        for path in ["coordination/PROJECT-BINDING.yaml", "coordination/runtime/CURRENT-ROUND",
            "coordination/rounds", "coordination/runtime/ledger-wal"] {
            assert!(!self.root.join(path).exists(), "standalone created task state: {path}");
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let mut safe = true;
        if let Ok(entries) = fs::read_dir(self.root.join("coordination/runtime/supervisors")) {
            for entry in entries.flatten() {
                if !entry.file_name().to_string_lossy().ends_with(".control.json") { continue; }
                let Ok(bytes) = fs::read(entry.path()) else { safe = false; continue };
                let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else { safe = false; continue };
                let Some(wake) = value["wakeId"].as_str() else { safe = false; continue };
                let Some(status) = value["statusPath"].as_str() else { safe = false; continue };
                let _ = orch_host::channel::cancel_direct_wake(&self.root, wake, "owned fixture cleanup");
                let until = Instant::now() + Duration::from_secs(5);
                loop {
                    let done = fs::read(status).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                        .is_some_and(|s| s["managedScopeTerminated"] == true);
                    if done { break; }
                    if Instant::now() >= until { safe = false; break; }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        if safe { let _ = fs::remove_dir_all(&self.root); }
        else { eprintln!("preserved unclosed owned fixture {}", self.root.display()); }
    }
}

#[test]
fn standalone_doctor_and_consult_need_only_git_and_harness_config() {
    let fixture = Fixture::new("claude", "printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"FINAL: PASS standalone\"}'");
    let doctor = fixture.command(&["doctor"]);
    assert!(doctor.status.success(), "{}", String::from_utf8_lossy(&doctor.stderr));
    let consult = fixture.command(&["consult", "question.md", "--harness", "mock", "--member-timeout-secs", "5", "--total-wall-secs", "5"]);
    assert!(consult.status.success(), "{}", String::from_utf8_lossy(&consult.stderr));
    let log = fs::read_to_string(fixture.root.join("coordination/consultations/log.jsonl")).unwrap();
    let rows = log.lines().map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()).collect::<Vec<_>>();
    assert!(!rows.is_empty());
    assert!(rows.iter().all(|row| row["round"].is_null() && row["taskId"].is_null()));
    assert!(rows.iter().any(|row| row["kind"] == "ConsultationCompleted" && row["membersOk"] == 1));
    fixture.no_task_state();
}

#[cfg(not(feature = "selfhost"))]
#[test]
fn default_refuses_broken_selfhost_markers_before_any_spawn_or_artifact() {
    for marker in ["coordination/runtime/CURRENT-ROUND", "coordination/PROJECT-BINDING.yaml",
        "coordination/rounds/r99/ROUND-IR.yaml", "coordination/rounds/r99/events.jsonl"] {
        let fixture = Fixture::new("claude", "touch native-was-started");
        let path = fixture.root.join(marker);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "invalid marker bytes\n").unwrap();
        for args in [vec!["wake", "mock", "--message", "fixture"],
            vec!["consult", "question.md", "--harness", "mock"]] {
            let output = fixture.command(&args);
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains("selfhost"));
            assert!(!fixture.root.join("native-was-started").exists());
            assert!(!fixture.root.join("coordination/consultations").exists());
            assert!(!fixture.root.join("coordination/runtime/supervisors").exists());
        }
    }
    let fixture = Fixture::new("claude", "touch native-was-started");
    let path = fixture.root.join("coordination/runtime/CURRENT-ROUND");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("missing-target", &path).unwrap();
    let output = fixture.command(&["wake", "mock", "--message", "fixture"]);
    assert!(!output.status.success());
    assert!(!fixture.root.join("native-was-started").exists());
}

#[test]
fn managed_direct_identity_survives_config_change_and_exact_cancel() {
    let fixture = Fixture::new("opencode", "printf '%s\\n' '{\"type\":\"text\",\"part\":{\"text\":\"working\"}}'\n/bin/sleep 30");
    let output = fixture.command(&["wake", "mock", "--message", "owned fixture", "--deadline-secs", "20"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let wake = stdout.split("wakeId=").nth(1).unwrap().split_whitespace().next().unwrap();
    let descriptor_path = fixture.root.join("coordination/runtime/supervisors").join(format!("{wake}.control.json"));
    let original = fs::read(&descriptor_path).unwrap();
    let descriptor: serde_json::Value = serde_json::from_slice(&original).unwrap();
    assert_eq!(descriptor["version"], 2);
    let binding = &descriptor["invocation"]["binding"];
    assert_eq!(binding["alias"], "mock");
    assert_eq!(binding["driver"], "opencode");
    assert_eq!(binding["standalone"], true);
    assert_eq!(binding["configDigest"].as_str().unwrap().len(), 64);
    assert!(binding["requestedTuple"].is_object() && binding["effectiveTuple"].is_object());
    assert!(descriptor.get("env").is_none() && descriptor["invocation"].get("env").is_none());
    assert!(descriptor["invocation"].get("argv").is_none());
    fs::write(fixture.root.join(".orch/harnesses.yaml"), "intentionally invalid current configuration\n").unwrap();
    let status = fixture.command(&["wake", "status", wake, "--json"]);
    assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
    let view: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(view["agent"], "mock");
    assert_eq!(view["providerKind"], "opencode");
    assert!(view.get("token").is_none());
    assert_eq!(fs::read(&descriptor_path).unwrap(), original);
    let cancel = fixture.command(&["wake", "cancel", wake, "--reason", "owned fixture explicit cancellation"]);
    assert!(cancel.status.success(), "{}", String::from_utf8_lossy(&cancel.stderr));
    let until = Instant::now() + Duration::from_secs(8);
    loop {
        let status = fixture.command(&["wake", "status", wake, "--json"]);
        assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
        let view: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
        if view["managedScopeTerminated"] == true { break; }
        assert!(Instant::now() < until, "owned mock did not reach confirmed termination");
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(fs::read(&descriptor_path).unwrap(), original);
    fixture.no_task_state();
}

#[test]
fn retired_leaf_implementations_are_physically_absent() {
    let sources = Path::new(env!("CARGO_MANIFEST_DIR")).join("../orch-host/src");
    for name in ["scaffold.rs", "mcp.rs", "inbox.rs", "inbox_meta.rs", "approval.rs"] {
        assert!(!sources.join(name).exists(), "retired implementation survived: {name}");
    }
}

#[test]
fn oversized_control_representation_is_rejected_before_native_spawn() {
    let fixture = Fixture::new("opencode", "touch native-was-started");
    let config = fixture.root.join(".orch/harnesses.yaml");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str(&format!("    defaults: {{provider: local, model: {}}}\n", "m".repeat(40_000)));
    fs::write(config, text).unwrap();
    let output = fixture.command(&["wake", "mock", "--message", "small prompt"]);
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "oversized control record was accepted");
    assert!(error.contains("local managed control descriptor") && error.contains("64 KiB"), "{error}");
    assert!(!fixture.root.join("native-was-started").exists());
    let dir = fixture.root.join("coordination/runtime/supervisors");
    assert!(fs::read_dir(dir).unwrap().all(|entry| !entry.unwrap().file_name().to_string_lossy().ends_with(".control.json")));
    fixture.no_task_state();
}
