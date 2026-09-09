#![cfg(feature = "selfhost")]
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = orch_host::util::test_scratch_dir("b326-action-cli");
        fs::create_dir_all(root.join(".orch")).unwrap();
        fs::create_dir_all(root.join("coordination/runtime")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("coordination/BOARD.md"), "# fixture\n").unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
        fs::write(root.join(".gitignore"), ".orch/\n.worktrees/\n.cowork-temp/\ncoordination/runtime/\ncoordination/rounds/*/dispatch/\ncoordination/consultations/\norch/target/\n").unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap();
        fs::copy(source.join("coordination/PROJECT-BINDING.yaml"), root.join("coordination/PROJECT-BINDING.yaml")).unwrap();
        fs::write(root.join("question.md"), "short\n").unwrap();
        let executable = root.join(".orch/provider.sh");
        fs::write(&executable, "#!/bin/sh\nprintf 'spawned\\n' >> provider-calls\nprintf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"fixture final answer\"}'\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["add", ".gitignore", "question.md", "coordination/PROJECT-BINDING.yaml", "coordination/BOARD.md", "src/lib.rs"]);
        git(&root, &["-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture"]);
        orch_host::round::run_open_v3(&root, "r9005", "action limits fixture", false).unwrap();
        fs::write(root.join("coordination/rounds/r9005/tasks/T326.md"),
            "---\nschemaVersion: 3\ntaskId: T326\nround: r9005\nseedProtocol: verify-only\nredForm: assertion\ndependsOn: []\nentryPoints: [src/lib.rs]\nseeds: []\nwriteSet: [src/lib.rs]\nfrozenPaths: [coordination/rounds/**]\ngates: {fast: [testFast, testExclusive, check, checkDefault, buildDefault, buildSelfhost]}\nrequiredEvidence: [proof]\n---\n# Fixture\n").unwrap();
        orch_host::plan::run_plan(&root).unwrap();
        git(&root, &["add", "coordination/rounds/r9005", "coordination/BOARD.md"]);
        git(&root, &["-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "planned fixture"]);
        Self { root }
    }

    fn configure(&self, limits: &str) {
        self.configure_action("consult", limits);
    }

    fn configure_action(&self, action: &str, limits: &str) {
        fs::write(self.root.join(".orch/harnesses.yaml"), format!(
            "version: 1\nharnesses:\n  alpha:\n    driver: claude\n    executable: {}\n    enabled: true\n    {action}:\n      limits: {{{limits}}}\n    cwdPolicy: project-root\n  dsh-fixture:\n    driver: dsh\n    executable: {}\n    enabled: true\n    cwdPolicy: project-root\n",
            self.root.join(".orch/provider.sh").display(), self.root.join(".orch/provider.sh").display()
        )).unwrap();
    }

    fn sign_off(&self) {
        orch_host::round::run_sign_off(&self.root, Some("isolated action fixture only")).unwrap();
        git(&self.root, &["add", "coordination/rounds/r9005"]);
        git(&self.root, &["-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "sign fixture"]);
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_orch")).arg("--root").arg(&self.root)
            .arg("--allow-stale-binary").args(args).output().unwrap()
    }

    fn consult_meta(&self) -> serde_json::Value {
        let log = fs::read_to_string(self.root.join("coordination/consultations/log.jsonl")).unwrap();
        let record: serde_json::Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
        serde_json::from_slice(&fs::read(self.root.join(record["dir"].as_str().unwrap()).join("meta.json")).unwrap()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git").arg("-C").arg(root).args(args).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn action_discovery_uses_driver_ability_without_spawning() {
    let f = Fixture::new();
    f.configure("maxPromptBytes: 4096, timeoutSeconds: 3");
    let consult = f.run(&["harness", "list", "--action", "consult"]);
    assert!(consult.status.success(), "{}", String::from_utf8_lossy(&consult.stderr));
    let text = String::from_utf8(consult.stdout).unwrap();
    assert!(text.lines().any(|line| line.starts_with("alpha\tclaude\tsupported")));
    assert!(text.lines().any(|line| line.starts_with("dsh-fixture\tdsh\tunsupported")));
    let review = f.run(&["harness", "list", "--action", "review"]);
    assert!(review.status.success());
    assert!(String::from_utf8(review.stdout).unwrap().contains("dsh-fixture\tdsh\tsupported"));
    let normal = f.run(&["harness", "list"]);
    assert!(normal.status.success());
    assert!(String::from_utf8(normal.stdout).unwrap().contains("dsh-fixture\tdsh\tsupported"));
    assert!(!f.root.join("provider-calls").exists());
}

#[test]
fn full_rendered_prompt_is_checked_before_provider_spawn() {
    let f = Fixture::new();
    f.configure("maxPromptBytes: 20");
    assert!(fs::read(f.root.join("question.md")).unwrap().len() < 20);
    let _ = f.run(&["consult", "question.md", "--harness", "alpha"]);
    let meta = f.consult_meta();
    assert_eq!(meta["members"][0]["status"], "failed");
    assert!(meta["members"][0]["reason"].as_str().unwrap().contains("maxPromptBytes"));
    assert!(!f.root.join("provider-calls").exists(), "guard ran after spawn");
}

#[test]
fn real_consultation_records_config_cli_and_total_wall_precedence() {
    let f = Fixture::new();
    f.configure("maxPromptBytes: 4096, timeoutSeconds: 3");
    let from_config = f.run(&["consult", "question.md", "--harness", "alpha", "--total-wall-secs", "9"]);
    assert!(from_config.status.success(), "{}", String::from_utf8_lossy(&from_config.stderr));
    let first = f.consult_meta();
    assert_eq!(first["members"][0]["channelFacts"]["deadlineSecs"], 3);
    assert_eq!(first["members"][0]["channelFacts"]["limits"]["timeoutSeconds"], 3);
    let explicit = f.run(&["consult", "question.md", "--harness", "alpha", "--member-timeout-secs", "7", "--total-wall-secs", "5"]);
    assert!(explicit.status.success(), "{}", String::from_utf8_lossy(&explicit.stderr));
    let second = f.consult_meta();
    assert_eq!(second["members"][0]["channelFacts"]["deadlineSecs"], 5);
    assert_ne!(first["members"][0]["channelFacts"]["commandDigest"], second["members"][0]["channelFacts"]["commandDigest"]);
    f.configure("");
    let default = f.run(&["consult", "question.md", "--harness", "alpha", "--total-wall-secs", "8"]);
    assert!(default.status.success());
    let third = f.consult_meta();
    assert_eq!(third["members"][0]["channelFacts"]["deadlineSecs"], 8);
    assert_ne!(first["members"][0]["channelFacts"]["configDigest"], third["members"][0]["channelFacts"]["configDigest"]);
    assert_eq!(fs::read_to_string(f.root.join("provider-calls")).unwrap().lines().count(), 3);
}

#[test]
fn harness_dispatch_checks_its_complete_execute_prompt_before_spawn() {
    let f = Fixture::new();
    f.configure_action("execute", "maxPromptBytes: 1");
    f.sign_off();
    let output = f.run(&["dispatch", "T326", "--harness", "alpha"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("maxPromptBytes"), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(!f.root.join("provider-calls").exists());
    let events = orch_core::read_ledger(&f.root.join("coordination/rounds/r9005/events.jsonl")).unwrap();
    assert!(!events.events.iter().any(|event| event.kind == "WakeIssued"));
    assert!(events.events.iter().any(|event| event.kind == "DispatchIssued"), "local lineage is a separate preceding effect");
}
