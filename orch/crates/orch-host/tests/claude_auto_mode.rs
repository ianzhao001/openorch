//! B346 immutable model-free seed for explicit Claude auto mode.
//! Exercises public prepare/render/preflight/execute with a fixture-owned executable.
//! Fake transport success is not native model acceptance, native history or a vote.
//! Negative mutations, individually detected and restored:
//! M1 remove auto flag; M2 wrong auto value; M3 add bypass alongside auto;
//! M4 admit unknown mode/provider; M5 change legacy default flags;
//! M6 lose model/effort; M7 split prompt/change action cwd or lose USER;
//! M8 remove substantive Claude public mode docs or existing Pi/CodeBuddy guidance.
//! Seven cases: the three auto actions, mode digest and new docs start red;
//! legacy and unsupported-boundary cases intentionally start green.
#![cfg(unix)]
use orch_host::channel::{
    capture_attachment_manifest_v1, preflight_invocation_v1, prepare_invocation,
    render_invocation_v1, run_preflighted_invocation_v1, InvocationAction, InvocationContextV1,
    InvocationRequest, RenderedInvocationV1,
};
use orch_host::harness_config::parse_harness_config_snapshot;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

struct Fixture {
    root: PathBuf,
    worktree: PathBuf,
    executable: PathBuf,
    head: String,
}
fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}
impl Fixture {
    fn new() -> Self {
        let parent = std::env::current_dir().unwrap().join(".cowork-temp");
        fs::create_dir_all(&parent).unwrap();
        let root = parent.join(format!("B346-auto-{}", ulid::Ulid::new()));
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "core.fsmonitor", "false"]);
        fs::write(root.join("tracked.txt"), "fixture\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "fixture",
            ],
        );
        let head = git(&root, &["rev-parse", "HEAD"]);
        let worktree = root.join("linked");
        git(
            &root,
            &[
                "worktree",
                "add",
                "--detach",
                worktree.to_str().unwrap(),
                &head,
            ],
        );
        let executable = root.join("fake-claude.sh");
        fs::write(&executable, b"#!/bin/sh\nprintf x >> \"$0.invoked\"\nprintf 'cwd=%s\\0user=%s\\0' \"$PWD\" \"${USER-}\"\nprintf '%s\\0' \"$@\"\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            root,
            worktree,
            executable,
            head,
        }
    }
    fn render(
        &self,
        action: InvocationAction,
        mode: Option<&str>,
        provider: Option<&str>,
    ) -> anyhow::Result<RenderedInvocationV1> {
        let mut defaults = serde_json::json!({"model":"qwen3.8-max", "effort":"max"});
        if let Some(mode) = mode {
            defaults["mode"] = mode.into();
        }
        if let Some(provider) = provider {
            defaults["provider"] = provider.into();
        }
        let policy = if action == InvocationAction::Consult {
            "project-root"
        } else {
            "target-worktree"
        };
        let yaml = format!("version: 1\nharnesses:\n  claude-fixture:\n    driver: claude\n    executable: {}\n    enabled: true\n    defaults: {}\n    cwdPolicy: {}\n", self.executable.display(), defaults, policy);
        let snapshot =
            parse_harness_config_snapshot(&self.root.join(".orch/harnesses.yaml"), &yaml)?;
        let prepared = prepare_invocation(
            &snapshot,
            InvocationRequest {
                alias: "claude-fixture".into(),
                action,
                prompt: "one argument with spaces, \"quotes\", and\na second line".into(),
                project_root: self.root.clone(),
                target_worktree: self.worktree.clone(),
                target_head: self.head.clone(),
                attachments: capture_attachment_manifest_v1(&[])?,
            },
        )?;
        render_invocation_v1(
            prepared,
            InvocationContextV1 {
                action_id: "B346-fixture-action".into(),
                wake_id: "B346-fixture-wake".into(),
                round: "r88".into(),
                task_id: "B346".into(),
                attempt_id: "B346-A0001".into(),
                review_output: (action == InvocationAction::Review)
                    .then(|| self.worktree.join("review.md")),
                orch_executable: self.executable.clone(),
                deadline_secs: 30,
            },
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn pair_count(args: &[String], key: &str, value: &str) -> usize {
    args.windows(2)
        .filter(|p| p[0] == key && p[1] == value)
        .count()
}
fn assert_auto(action: InvocationAction) {
    let f = Fixture::new();
    let rendered = f
        .render(action, Some("auto"), None)
        .expect("explicit Claude auto must render");
    let argv = rendered.argv().to_vec();
    assert_eq!(pair_count(&argv, "--permission-mode", "auto"), 1);
    assert_eq!(argv.iter().filter(|a| *a == "--permission-mode").count(), 1);
    assert_eq!(argv.iter().filter(|a| *a == "--verbose").count(), 1);
    assert!(!argv.iter().any(|s| s == "--dangerously-skip-permissions"));
    assert_eq!(pair_count(&argv, "--model", "qwen3.8-max"), 1);
    assert_eq!(pair_count(&argv, "--effort", "max"), 1);
    assert_eq!(pair_count(&argv, "--output-format", "stream-json"), 1);
    let cwd = if action == InvocationAction::Consult {
        &f.root
    } else {
        &f.worktree
    };
    assert_eq!(rendered.cwd(), cwd);
    assert_eq!(
        rendered.env().get("USER"),
        std::env::var("USER")
            .ok()
            .filter(|v| !v.is_empty())
            .as_ref()
    );
    let output = run_preflighted_invocation_v1(preflight_invocation_v1(rendered).unwrap()).unwrap();
    assert!(output.success()); // Fake transport exit only, never a native terminal or vote.
    assert_eq!(
        output.stdout.last(),
        Some(&0),
        "fake argv stream must terminate with NUL"
    );
    let fields: Vec<_> = output.stdout.split(|b| *b == 0).collect();
    assert!(fields.len() >= 4 && fields.last() == Some(&b"".as_slice()));
    assert_eq!(fields[0], format!("cwd={}", cwd.display()).as_bytes());
    assert_eq!(
        fields[1],
        format!("user={}", std::env::var("USER").unwrap_or_default()).as_bytes()
    );
    let actual: Vec<_> = fields[2..fields.len() - 1]
        .iter()
        .map(|b| std::str::from_utf8(b).unwrap())
        .collect();
    let expected: Vec<_> = argv[1..].iter().map(String::as_str).collect();
    assert_eq!(
        actual, expected,
        "actual argv boundaries must match fixed render"
    );
    assert_eq!(
        pair_count(
            &argv,
            "-p",
            "one argument with spaces, \"quotes\", and\na second line"
        ),
        1
    );
}
#[test]
fn claude_auto_consult_uses_native_mode_without_bypass() {
    assert_auto(InvocationAction::Consult);
}
#[test]
fn claude_auto_review_uses_native_mode_without_bypass() {
    assert_auto(InvocationAction::Review);
}
#[test]
fn claude_auto_execute_uses_native_mode_without_bypass() {
    assert_auto(InvocationAction::Execute);
}
#[test]
fn claude_unset_mode_preserves_legacy_arguments() {
    let f = Fixture::new();
    for action in [
        InvocationAction::Consult,
        InvocationAction::Review,
        InvocationAction::Execute,
    ] {
        let r = f.render(action, None, None).unwrap();
        assert_eq!(
            r.argv()
                .iter()
                .filter(|a| *a == "--dangerously-skip-permissions")
                .count(),
            1
        );
        assert!(!r.argv().iter().any(|a| a == "--permission-mode"));
        assert_eq!(pair_count(r.argv(), "--model", "qwen3.8-max"), 1);
        assert_eq!(pair_count(r.argv(), "--effort", "max"), 1);
        assert_eq!(pair_count(r.argv(), "--output-format", "stream-json"), 1);
        assert_eq!(r.argv().iter().filter(|a| *a == "--verbose").count(), 1);
        assert_eq!(
            pair_count(
                r.argv(),
                "-p",
                "one argument with spaces, \"quotes\", and\na second line"
            ),
            1
        );
    }
}
#[test]
fn claude_unknown_mode_and_provider_are_rejected_before_spawn() {
    let f = Fixture::new();
    for action in [
        InvocationAction::Consult,
        InvocationAction::Review,
        InvocationAction::Execute,
    ] {
        assert!(f.render(action, Some("not-modeled"), None).is_err());
        assert!(f.render(action, Some("auto"), Some("not-modeled")).is_err());
        assert!(f.render(action, None, Some("not-modeled")).is_err());
    }
    assert!(!f.executable.with_extension("sh.invoked").exists());
}
#[test]
fn claude_mode_change_updates_bound_command_digest() {
    let f = Fixture::new();
    let a = f
        .render(InvocationAction::Consult, Some("auto"), None)
        .unwrap();
    let b = f
        .render(InvocationAction::Consult, Some("auto"), None)
        .unwrap();
    let legacy = f.render(InvocationAction::Consult, None, None).unwrap();
    assert_eq!(a.argv(), b.argv());
    assert_eq!(a.command_digest(), b.command_digest());
    assert_ne!(a.argv(), legacy.argv());
    assert_ne!(a.command_digest(), legacy.command_digest());
}
#[test]
fn claude_auto_public_docs_describe_mode_boundary() {
    let source =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/channel.rs")).unwrap();
    let prefix = &source[..source.find("pub fn render_invocation_v1(").unwrap()];
    let docs = prefix
        .lines()
        .rev()
        .take_while(|line| line.trim_start().starts_with("///") || line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        docs.contains("Claude")
            && docs.contains("auto")
            && docs.contains("--permission-mode")
            && docs.contains("--dangerously-skip-permissions"),
        "public renderer docs must explain Claude explicit/legacy mode boundary"
    );
    assert!(
        docs.contains("Pi") && docs.contains("CodeBuddy"),
        "existing native-history guidance must remain"
    );
}
