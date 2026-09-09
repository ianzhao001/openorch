//! B185 · fixture Git/fsmonitor 隔离契约（r62 重基线版）。
//!
//! 冻结落位后不得编辑。本 seed 只依赖 tests/support 中两个尚不存在的共享 helper，
//! 所以首红必须是同时点名二者的 E0432，而不是旧 r58 的 E0583 或 counts-only 红。
//!
//! M1. `fixture_git_command` 删除 command-scope `core.fsmonitor=false` ⇒ fake hook marker 出现；
//! M2. `configure_fixture_git_env` 不移除 `GIT_CONFIG_PARAMETERS` 或不固定 slot 0 ⇒ child 红；
//! M3. 额外 `diff.renames=true` 被丢弃 ⇒ child/inspect 红；
//! M4. 任一现行 CLI/Git fixture 绕过共享边界 ⇒ inventory 红。

#![cfg(unix)]

mod support;

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use support::{configure_fixture_git_env, fixture_git_command};

const CLI_CHILD_TARGETS: [&str; 11] = [
    "cli_process_runtime.rs",
    "consult_cli.rs",
    "dispatch_override_cli.rs",
    "handshake_cli.rs",
    "guide_cli.rs",
    "review_reconcile_cli.rs",
    "root_verdict_cli.rs",
    "standalone_channel_v1.rs",
    "stale_binary_cli.rs",
    "wake_message_cli.rs",
    "wake_signal_isolation_cli.rs",
];

const DIRECT_GIT_TARGETS: [&str; 3] = [
    "cli_process_runtime.rs",
    "review_reconcile_cli.rs",
    "stale_binary_cli.rs",
];

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct FsmonitorFixture {
    root: PathBuf,
    global_config: PathBuf,
    marker: PathBuf,
}

impl FsmonitorFixture {
    fn new(name: &str) -> Self {
        let seq = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("CARGO_MANIFEST_DIR must be <worktree>/orch/crates/orch-cli")
            .join("target/test-tmp")
            .join(format!("b185-fs-{name}-{}-{seq}", std::process::id()));
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(&root).expect("create B185 fixture");

        let marker = root.join("hook-invoked");
        let hook = root.join("fake-fsmonitor-hook.sh");
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\nprintf 'invoked:%s\\n' \"$*\" >>'{}'\nprintf 'builtin:b185-token\\n'\n",
                marker.display()
            ),
        )
        .expect("write fake hook");
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).expect("make fake hook executable");

        let global_config = root.join("global.gitconfig");
        fs::write(
            &global_config,
            format!("[core]\n\tfsmonitor = {}\n", hook.display()),
        )
        .expect("write fake global config");

        let fixture = Self {
            root,
            global_config,
            marker,
        };
        fixture.bootstrap(&["init", "-b", "main"]);
        fixture.bootstrap(&[
            "config",
            "--local",
            "core.fsmonitor",
            fixture.root.join("fake-fsmonitor-hook.sh").to_str().unwrap(),
        ]);
        fs::write(fixture.root.join("tracked.txt"), "fixture\n").unwrap();
        fs::write(
            fixture.root.join(".gitignore"),
            "fake-fsmonitor-hook.sh\nglobal.gitconfig\nhook-invoked\n",
        )
        .unwrap();
        fixture.bootstrap(&["add", "tracked.txt", ".gitignore"]);
        fixture.bootstrap(&[
            "-c",
            "user.name=b185-test",
            "-c",
            "user.email=b185@example.invalid",
            "commit",
            "-m",
            "fixture",
        ]);
        assert!(!fixture.marker.exists(), "bootstrap invoked fsmonitor");
        fixture
    }

    fn bootstrap(&self, args: &[&str]) {
        let output = Command::new("git")
            .arg("-c")
            .arg("core.fsmonitor=false")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", &self.global_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|error| panic!("bootstrap git {args:?}: {error}"));
        assert!(
            output.status.success(),
            "bootstrap git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn apply_fake_config(&self, command: &mut Command) {
        command
            .env("GIT_CONFIG_GLOBAL", &self.global_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::null());
    }
}

impl Drop for FsmonitorFixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).ok();
    }
}

fn command_env<'a>(command: &'a Command, key: &str) -> Option<Option<&'a OsStr>> {
    command
        .get_envs()
        .find(|(candidate, _)| *candidate == OsStr::new(key))
        .map(|(_, value)| value)
}

#[test]
fn direct_fixture_git_uses_command_scope_false_and_never_invokes_the_hook() {
    let fixture = FsmonitorFixture::new("direct");
    let mut command = fixture_git_command(&fixture.root);
    let args = command
        .get_args()
        .map(|arg| arg.to_os_string())
        .collect::<Vec<_>>();
    command.args(["status", "--porcelain=v1"]);
    fixture.apply_fake_config(&mut command);
    let output = command.output().expect("run direct fixture git");

    assert_eq!(args[0], "-c");
    assert_eq!(args[1], "core.fsmonitor=false");
    assert_eq!(args[2], "-C");
    assert_eq!(args[3], fixture.root.as_os_str());
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!fixture.marker.exists(), "direct helper invoked fsmonitor");
}

#[test]
fn captured_fixture_child_gets_env_scope_false_without_losing_extra_config() {
    let fixture = FsmonitorFixture::new("child");
    let mut command = Command::new("git");
    command.env("GIT_CONFIG_PARAMETERS", "'core.fsmonitor'='true'");
    configure_fixture_git_env(&mut command, &[("diff.renames", "true")]);

    assert_eq!(command_env(&command, "GIT_CONFIG_PARAMETERS"), Some(None));
    assert_eq!(command_env(&command, "GIT_CONFIG_COUNT"), Some(Some(OsStr::new("2"))));
    assert_eq!(command_env(&command, "GIT_CONFIG_KEY_0"), Some(Some(OsStr::new("core.fsmonitor"))));
    assert_eq!(command_env(&command, "GIT_CONFIG_VALUE_0"), Some(Some(OsStr::new("false"))));
    assert_eq!(command_env(&command, "GIT_CONFIG_KEY_1"), Some(Some(OsStr::new("diff.renames"))));
    assert_eq!(command_env(&command, "GIT_CONFIG_VALUE_1"), Some(Some(OsStr::new("true"))));

    command
        .arg("-C")
        .arg(&fixture.root)
        .args(["config", "--get", "diff.renames"]);
    fixture.apply_fake_config(&mut command);
    let output = command.output().expect("run captured child");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "true");
    assert!(!fixture.marker.exists(), "child env invoked fsmonitor");
}

#[test]
fn every_captured_cli_and_direct_git_fixture_uses_the_shared_boundary() {
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    for relative in CLI_CHILD_TARGETS {
        let source = fs::read_to_string(tests.join(relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        assert!(
            source.contains("fn fixture_orch_command("),
            "{relative} must define one local captured-CLI constructor"
        );
        assert_eq!(
            source.matches("support::orch_bin()").count(),
            1,
            "{relative} must resolve Cargo's binary once in that constructor"
        );
        assert_eq!(
            source.matches("configure_fixture_git_env").count(),
            1,
            "{relative} must apply the shared child boundary once"
        );
        for forbidden in [
            "Command::new(orch_bin())",
            "Command::new(&orch_bin())",
            "Command::new(env!(\"CARGO_BIN_EXE_orch\"))",
        ] {
            assert!(!source.contains(forbidden), "{relative} bypasses via {forbidden}");
        }
    }

    for relative in DIRECT_GIT_TARGETS {
        let source = fs::read_to_string(tests.join(relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        assert!(source.contains("fixture_git_command"));
        assert!(
            !source.contains("Command::new(\"git\")")
                && !source.contains("std::process::Command::new(\"git\")"),
            "{relative} retains an ad-hoc direct Git command"
        );
    }
}
