#![cfg(feature = "selfhost")]
//! Real CLI subprocess regressions moved out of the binary unit-test harness.
//! Cargo guarantees `CARGO_BIN_EXE_orch` for this integration target, so every
//! case launches the executable built by the current invocation.

mod support;

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn fixture_orch_command(extras: &[(&str, &str)]) -> Command {
    let mut command = Command::new(support::orch_bin());
    support::configure_fixture_git_env(&mut command, extras);
    command
}

#[cfg(unix)]
mod fixture_fsmonitor_pipe_holder_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    use std::process::{Output, Stdio};
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    fn fixture_root() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
        let seq = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|path| path.parent())
            .expect("CARGO_MANIFEST_DIR must be below the orch workspace")
            .join("target/test-tmp")
            .join(format!(
                "b185-fsmonitor-pipe-holder-{}-{seq}",
                std::process::id()
            ));
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(&root).expect("create pipe-holder fixture root");
        root
    }

    fn run_fixture_git(root: &std::path::Path, args: &[&str]) {
        let output = support::fixture_git_command(root)
            .args(args)
            .output()
            .expect("launch fixture git");
        assert!(
            output.status.success(),
            "fixture git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn spawn_captured(
        mut command: Command,
    ) -> (u32, Receiver<std::io::Result<Output>>, JoinHandle<()>) {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let child = command.spawn().expect("spawn captured fixture git");
        let pgid = child.id();
        let (sender, receiver) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let _ = sender.send(child.wait_with_output());
        });
        (pgid, receiver, waiter)
    }

    fn signal_group(pgid: u32, signal: &str) -> bool {
        Command::new("/bin/kill")
            .args([signal, "--", &format!("-{pgid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn group_alive(pgid: u32) -> bool {
        signal_group(pgid, "-0")
    }

    fn terminate_exact_group(pgid: u32) {
        assert!(signal_group(pgid, "-TERM"), "TERM exact PGID {pgid}");
        let deadline = Instant::now() + Duration::from_secs(2);
        while group_alive(pgid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if group_alive(pgid) {
            assert!(signal_group(pgid, "-KILL"), "KILL exact PGID {pgid}");
        }
    }

    fn await_group_gone(pgid: u32) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while group_alive(pgid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!group_alive(pgid), "exact PGID {pgid} survived cleanup");
    }

    #[test]
    fn detached_fsmonitor_pipe_holder_replay_is_bounded_and_clean() {
        let root = fixture_root();
        let marker = root.join("fsmonitor-hook-invoked");
        let hook = root.join("pipe-holder-fsmonitor.sh");
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\nprintf invoked >'{}'\n(trap '' HUP; exec /bin/sleep 60) 1>&2 2>&2 &\nprintf 'builtin:b185-token\\n'\n",
                marker.display()
            ),
        )
        .expect("write pipe-holder hook");
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).expect("make pipe-holder hook executable");

        run_fixture_git(&root, &["init", "-b", "main"]);
        run_fixture_git(
            &root,
            &[
                "config",
                "--local",
                "core.fsmonitor",
                hook.to_str().unwrap(),
            ],
        );

        let mut unisolated = support::fixture_git_command(&root);
        unisolated
            .arg("-c")
            .arg(format!("core.fsmonitor={}", hook.display()))
            .args(["status", "--porcelain=v1"]);
        let (blocked_pgid, blocked_receiver, blocked_waiter) = spawn_captured(unisolated);
        let blocked = match blocked_receiver.recv_timeout(Duration::from_secs(3)) {
            Err(RecvTimeoutError::Timeout) => true,
            Ok(output) => {
                blocked_waiter
                    .join()
                    .expect("join unexpectedly fast waiter");
                panic!(
                    "unisolated fsmonitor unexpectedly completed: {:?}: {}",
                    output.as_ref().map(|value| value.status),
                    output
                        .as_ref()
                        .map(|value| String::from_utf8_lossy(&value.stderr).into_owned())
                        .unwrap_or_else(|error| error.to_string())
                );
            }
            Err(RecvTimeoutError::Disconnected) => {
                blocked_waiter.join().expect("join disconnected waiter");
                panic!("unisolated fsmonitor waiter disconnected")
            }
        };
        let marker_seen = marker.is_file();
        terminate_exact_group(blocked_pgid);
        let _blocked_output = blocked_receiver
            .recv_timeout(Duration::from_secs(3))
            .expect("drain waiter after exact-PGID cleanup")
            .expect("collect killed unisolated git output");
        blocked_waiter.join().expect("join drained waiter");
        await_group_gone(blocked_pgid);
        assert!(blocked, "unisolated replay must hit the three-second bound");
        assert!(marker_seen, "unisolated replay never invoked the fake hook");

        fs::remove_file(&marker).expect("clear unisolated marker");
        let mut isolated = support::fixture_git_command(&root);
        isolated.args(["status", "--porcelain=v1"]);
        let (isolated_pgid, isolated_receiver, isolated_waiter) = spawn_captured(isolated);
        let isolated_output = match isolated_receiver.recv_timeout(Duration::from_secs(3)) {
            Ok(output) => output.expect("collect isolated fixture git output"),
            Err(error) => {
                if group_alive(isolated_pgid) {
                    terminate_exact_group(isolated_pgid);
                }
                let _ = isolated_receiver.recv_timeout(Duration::from_secs(3));
                isolated_waiter
                    .join()
                    .expect("join timed-out isolated waiter");
                panic!("isolated fixture git exceeded the three-second bound: {error}")
            }
        };
        isolated_waiter.join().expect("join isolated waiter");
        await_group_gone(isolated_pgid);
        assert!(
            isolated_output.status.success(),
            "isolated fixture git failed: {}",
            String::from_utf8_lossy(&isolated_output.stderr)
        );
        assert!(!marker.exists(), "shared boundary still invoked fsmonitor");
        fs::remove_dir_all(root).ok();
    }
}



mod current_command_tests {
    use super::*;

    const ROUND: &str = "r51-b151-current";

    fn worktree_orch_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("CARGO_MANIFEST_DIR 应形如 <worktree>/orch/crates/orch-cli")
            .to_path_buf()
    }

    fn temp_root(name: &str) -> PathBuf {
        let dir = worktree_orch_dir()
            .join("target")
            .join("test-tmp")
            .join(format!("b151-current-{name}-{}", std::process::id()));
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_fixture(root: &std::path::Path, events: &[&str]) {
        fs::create_dir_all(root.join("coordination/runtime")).unwrap();
        fs::create_dir_all(root.join(format!("coordination/rounds/{ROUND}/tasks"))).unwrap();
        fs::write(
            root.join("coordination/runtime/CURRENT-ROUND"),
            format!("{ROUND}\n"),
        )
        .unwrap();
        fs::write(
            root.join(format!("coordination/rounds/{ROUND}/events.jsonl")),
            format!("{}\n", events.join("\n")),
        )
        .unwrap();
    }

    #[test]
    fn orch_current_is_idempotent_and_consistent() {
        let root = temp_root("idempotent");
        write_fixture(
            &root,
            &[
                r#"{"eventId":"e1","ts":"2026-07-28T01:00:00Z","actor":"runtime:orch","type":"RoundOpened","round":"r51-b151-current"}"#,
                r#"{"eventId":"e2","ts":"2026-07-28T01:01:00Z","actor":"runtime:orch","type":"PlanSignedOff","round":"r51-b151-current"}"#,
                r#"{"eventId":"e3","ts":"2026-07-28T01:02:00Z","actor":"runtime:orch","type":"DispatchIssued","taskId":"BDEMO","round":"r51-b151-current","payload":{"agent":"executor-opencode","attemptId":"BDEMO-A0001","attemptNo":1}}"#,
            ],
        );
        let output1 = fixture_orch_command(&[])
            .arg("--root")
            .arg(&root)
            .arg("current")
            .output()
            .expect("启动 orch current 失败");
        assert!(
            output1.status.success(),
            "第一次 orch current 失败: {}",
            String::from_utf8_lossy(&output1.stderr)
        );
        let current_md_path = root.join("coordination/CURRENT.md");
        assert!(current_md_path.is_file(), "CURRENT.md 未生成");
        let first_bytes = fs::read(&current_md_path).unwrap();

        let output2 = fixture_orch_command(&[])
            .arg("--root")
            .arg(&root)
            .arg("current")
            .output()
            .expect("第二次启动 orch current 失败");
        assert!(
            output2.status.success(),
            "第二次 orch current 失败: {}",
            String::from_utf8_lossy(&output2.stderr)
        );
        let second_bytes = fs::read(&current_md_path).unwrap();
        let src1 = String::from_utf8(first_bytes).unwrap();
        let src2 = String::from_utf8(second_bytes).unwrap();
        assert!(
            src1.lines().any(|l| l.trim() == format!("round: {ROUND}")),
            "CURRENT.md 缺 round 行: {src1}"
        );
        assert!(
            src1.lines().any(|l| l.trim().starts_with("main:")),
            "CURRENT.md 缺 main 行: {src1}"
        );
        let main_value = src1
            .lines()
            .map(|l| l.trim())
            .find(|l| l.starts_with("main:"))
            .map(|l| l["main:".len()..].trim().to_string())
            .expect("CURRENT.md 应含 main: 行");
        assert!(
            orch_host::binding::current_md_consistent(&src1, ROUND, &main_value),
            "第一次 CURRENT.md 一致性判定失败（main={main_value}）"
        );
        assert!(
            orch_host::binding::current_md_consistent(&src2, ROUND, &main_value),
            "第二次 CURRENT.md 一致性判定失败（main={main_value}）"
        );
        let r1 = src1
            .lines()
            .find(|l| l.trim().starts_with("round:"))
            .unwrap();
        let r2 = src2
            .lines()
            .find(|l| l.trim().starts_with("round:"))
            .unwrap();
        assert_eq!(r1, r2, "round 行在两次跑间漂移");
        let m1 = src1
            .lines()
            .find(|l| l.trim().starts_with("main:"))
            .unwrap();
        let m2 = src2
            .lines()
            .find(|l| l.trim().starts_with("main:"))
            .unwrap();
        assert_eq!(m1, m2, "main 行在两次跑间漂移");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn orch_current_refuses_bad_ledger() {
        let root = temp_root("bad-ledger");
        fs::create_dir_all(root.join("coordination/runtime")).unwrap();
        fs::create_dir_all(root.join(format!("coordination/rounds/{ROUND}"))).unwrap();
        fs::write(
            root.join("coordination/runtime/CURRENT-ROUND"),
            format!("{ROUND}\n"),
        )
        .unwrap();
        fs::write(
            root.join(format!("coordination/rounds/{ROUND}/events.jsonl")),
            "this is not json\n",
        )
        .unwrap();
        let output = fixture_orch_command(&[])
            .arg("--root")
            .arg(&root)
            .arg("current")
            .output()
            .expect("启动 orch current 失败");
        assert!(
            !output.status.success(),
            "坏账本应拒绝：{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            !root.join("coordination/CURRENT.md").exists(),
            "坏账本不得生成 CURRENT.md"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn doctor_reports_current_md_missing_as_warn() {
        let root = temp_root("doctor-missing");
        write_fixture(
            &root,
            &[
                r#"{"eventId":"e1","ts":"2026-07-28T01:00:00Z","actor":"runtime:orch","type":"RoundOpened","round":"r51-b151-current"}"#,
            ],
        );
        let output = fixture_orch_command(&[])
            .arg("--root")
            .arg(&root)
            .arg("doctor")
            .output()
            .expect("启动 orch doctor 失败");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("CURRENT.md"),
            "doctor 应含 CURRENT.md 增项: {stdout}"
        );
        assert!(
            stdout.contains("orch current"),
            "缺失应提示 `orch current` 生成命令: {stdout}"
        );
        assert!(
            stdout.contains('⚠')
                && stdout
                    .lines()
                    .any(|l| l.contains("CURRENT.md") && l.contains('⚠')),
            "CURRENT.md 缺失应为 Warn(⚠)而非 Fail: {stdout}"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn doctor_shows_current_md_consistent_after_generate() {
        let root = temp_root("doctor-after-gen");
        write_fixture(
            &root,
            &[
                r#"{"eventId":"e1","ts":"2026-07-28T01:00:00Z","actor":"runtime:orch","type":"RoundOpened","round":"r51-b151-current"}"#,
            ],
        );
        let gen = fixture_orch_command(&[])
            .arg("--root")
            .arg(&root)
            .arg("current")
            .output()
            .expect("启动 orch current 失败");
        assert!(
            gen.status.success(),
            "{}",
            String::from_utf8_lossy(&gen.stderr)
        );
        let doc = fixture_orch_command(&[])
            .arg("--root")
            .arg(&root)
            .arg("doctor")
            .output()
            .expect("启动 orch doctor 失败");
        let stdout = String::from_utf8_lossy(&doc.stdout);
        assert!(
            stdout.contains("CURRENT.md") && stdout.contains("一致"),
            "doctor 应示 CURRENT.md 一致: {stdout}"
        );
        fs::remove_dir_all(&root).unwrap();
    }
}

mod ledger_recover_cli_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    fn worktree_orch_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|path| path.parent())
            .expect("CARGO_MANIFEST_DIR 应形如 <worktree>/orch/crates/orch-cli")
            .to_path_buf()
    }

    fn temp_root(name: &str) -> PathBuf {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = worktree_orch_dir().join("target/test-tmp").join(format!(
            "b163-ledger-cli-{name}-{}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("coordination/runtime/ledger-wal")).unwrap();
        fs::create_dir_all(root.join("coordination/rounds/rCli")).unwrap();
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "rCli\n").unwrap();
        root
    }

    #[test]
    fn cli_dry_run_uses_current_round_and_does_not_write() {
        let root = temp_root("dry");
        let ledger = b"{\"eventId\":\"01A\",\"type\":\"RoundOpened\"}\n";
        let wal = b"{\"eventId\":\"01A\",\"type\":\"RoundOpened\"}\n{\"eventId\":\"01B\",\"type\":\"TaskRecorded\"}\n";
        fs::write(root.join("coordination/rounds/rCli/events.jsonl"), ledger).unwrap();
        fs::write(root.join("coordination/runtime/ledger-wal/rCli.jsonl"), wal).unwrap();
        let output = fixture_orch_command(&[])
            .arg("--root")
            .arg(&root)
            .args(["ledger", "recover"])
            .output()
            .expect("启动 orch ledger recover 失败");
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("round=rCli"), "{stdout}");
        assert!(stdout.contains("Append 1 行"), "{stdout}");
        assert!(stdout.contains("kind=TaskRecorded"), "{stdout}");
        assert_eq!(
            fs::read(root.join("coordination/rounds/rCli/events.jsonl")).unwrap(),
            ledger
        );
        assert!(!root
            .join("coordination/runtime/ledger-wal/recovery-log.jsonl")
            .exists());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn cli_apply_refuses_divergence_at_human_line_without_mutation() {
        let root = temp_root("diverged");
        let ledger = b"{\"eventId\":\"01A\"}\n{\"eventId\":\"ledger-only\"}\n";
        let wal = b"{\"eventId\":\"01A\"}\n{\"eventId\":\"wal-only\"}\n";
        fs::write(root.join("coordination/rounds/rCli/events.jsonl"), ledger).unwrap();
        fs::write(root.join("coordination/runtime/ledger-wal/rCli.jsonl"), wal).unwrap();
        let output = fixture_orch_command(&[])
            .arg("--root")
            .arg(&root)
            .args(["ledger", "recover", "--apply"])
            .output()
            .expect("启动 orch ledger recover --apply 失败");
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("第 2 行"), "{stderr}");
        assert_eq!(
            fs::read(root.join("coordination/rounds/rCli/events.jsonl")).unwrap(),
            ledger
        );
        assert_eq!(
            fs::read(root.join("coordination/runtime/ledger-wal/rCli.jsonl")).unwrap(),
            wal
        );
        fs::remove_dir_all(root).ok();
    }
}

mod closed_round_artifact_doctor_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

    fn run_git(root: &std::path::Path, args: &[&str]) {
        let output = support::fixture_git_command(root)
            .args(args)
            .output()
            .expect("启动 fixture git 失败");
        assert!(
            output.status.success(),
            "git {args:?} 失败: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn fixture(name: &str) -> PathBuf {
        let seq = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|path| path.parent())
            .unwrap()
            .join("target/test-tmp")
            .join(format!(
                "b176-closed-round-{name}-{}-{seq}",
                std::process::id()
            ));
        if root.exists() {
            fs::remove_dir_all(&root).unwrap();
        }
        fs::create_dir_all(root.join("coordination/scripts")).unwrap();
        for round in ["r53", "r57", "r58"] {
            fs::create_dir_all(root.join(format!("coordination/rounds/{round}/reviews"))).unwrap();
            fs::create_dir_all(root.join(format!("coordination/rounds/{round}/reports"))).unwrap();
            fs::create_dir_all(root.join(format!(
                "coordination/rounds/{round}/dispatch/executor-desktop"
            )))
            .unwrap();
        }
        fs::write(
            root.join(".gitignore"),
            "coordination/rounds/*/dispatch/\ncoordination/runtime/\n.worktrees/\n",
        )
        .unwrap();
        fs::write(
            root.join(".gitattributes"),
            "coordination/rounds/*/events.jsonl merge=union\n",
        )
        .unwrap();
        fs::write(root.join("coordination/BOARD.md"), "# board\n").unwrap();
        let wait = root.join("coordination/scripts/wait-dispatch.sh");
        fs::write(&wait, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&wait).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&wait, permissions).unwrap();
        }
        let closed = r#"{"eventId":"e1","ts":"2026-07-28T01:00:00Z","actor":"runtime:orch","type":"RoundClosed","round":"r53"}"#;
        let r57_closed = r#"{"eventId":"e2","ts":"2026-07-29T01:00:00Z","actor":"runtime:orch","type":"RoundClosed","round":"r57"}"#;
        let open = r#"{"eventId":"e3","ts":"2026-07-30T01:00:00Z","actor":"runtime:orch","type":"RoundOpened","round":"r58"}"#;
        fs::write(
            root.join("coordination/rounds/r53/events.jsonl"),
            format!("{closed}\n"),
        )
        .unwrap();
        fs::write(
            root.join("coordination/rounds/r57/events.jsonl"),
            format!("{r57_closed}\n"),
        )
        .unwrap();
        fs::write(
            root.join("coordination/rounds/r58/events.jsonl"),
            format!("{open}\n"),
        )
        .unwrap();
        fs::write(
            root.join("coordination/rounds/r53/reviews/B158-A0001-secondary-executor-opencode.md"),
            "committed review\n",
        )
        .unwrap();
        fs::write(
            root.join("coordination/rounds/r53/reports/executor-desktop-SUMMARY.md"),
            "committed summary\n",
        )
        .unwrap();
        fs::write(
            root.join("coordination/rounds/r53/dispatch/executor-desktop/GO-B158-A0001.md.ack"),
            "committed ack\n",
        )
        .unwrap();
        fs::write(
            root.join("coordination/rounds/r57/reviews/B170-A0001-primary-executor-claw.md"),
            "committed review with inaccurate cleanup sentence\n",
        )
        .unwrap();
        fs::write(
            root.join("coordination/rounds/r58/reviews/B179-A0001-primary-executor-claw.md"),
            "committed active review\n",
        )
        .unwrap();

        let output = support::fixture_git_command(&root)
            .args(["init", "-b", "main"])
            .output()
            .expect("git init 启动失败");
        assert!(output.status.success(), "git init 失败");
        run_git(&root, &["add", "-A"]);
        run_git(
            &root,
            &[
                "-c",
                "user.name=orch-test",
                "-c",
                "user.email=orch-test@example.invalid",
                "commit",
                "-m",
                "fixture",
            ],
        );
        root
    }

    fn run_doctor(root: &std::path::Path) -> std::process::Output {
        fixture_orch_command(&[("diff.renames", "true")])
            .arg("--root")
            .arg(root)
            .arg("doctor")
            .output()
            .expect("启动 orch doctor 失败")
    }

    fn assert_tracked_finding(output: &std::process::Output, relative: &str) {
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!output.status.success(), "脏改必须令 doctor 非零: {stdout}");
        assert!(stdout.contains(relative), "必须点名文件: {stdout}");
        assert!(
            stdout.contains(&format!("git show HEAD:{relative} > {relative}")),
            "HEAD 已有 blob 必须给出有效恢复提示: {stdout}"
        );
    }

    #[test]
    fn doctor_reports_both_real_closed_round_review_replays_without_writing() {
        for (name, relative, corrected) in [
            (
                "r53-large-rewrite",
                "coordination/rounds/r53/reviews/B158-A0001-secondary-executor-opencode.md",
                "rewritten after close\nwith a large or tiny correction\n",
            ),
            (
                "r57-honest-correction",
                "coordination/rounds/r57/reviews/B170-A0001-primary-executor-claw.md",
                "honest correction: the cleanup command did not run\n",
            ),
        ] {
            let root = fixture(name);
            let path = root.join(relative);
            fs::write(&path, corrected).unwrap();
            let before = fs::read(&path).unwrap();
            let first = run_doctor(&root);
            let second = run_doctor(&root);
            assert_tracked_finding(&first, relative);
            assert_eq!(first.status, second.status, "重复运行结论必须一致");
            assert_eq!(first.stdout, second.stdout, "重复运行输出必须一致");
            assert_eq!(fs::read(&path).unwrap(), before, "doctor 不得自动修复");
            fs::remove_dir_all(root).ok();
        }
    }

    #[test]
    fn staged_rename_outside_audit_domain_still_names_the_old_source() {
        let root = fixture("rename-outside");
        let source = "coordination/rounds/r53/reviews/B158-A0001-secondary-executor-opencode.md";
        let target = "archived/B158-A0001-secondary-executor-opencode.md";
        fs::create_dir_all(root.join("archived")).unwrap();
        run_git(&root, &["mv", source, target]);
        let target_bytes = fs::read(root.join(target)).unwrap();

        let output = run_doctor(&root);
        assert_tracked_finding(&output, source);
        assert!(!root.join(source).exists(), "doctor 不得写回旧源");
        assert_eq!(fs::read(root.join(target)).unwrap(), target_bytes);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn staged_rename_to_summary_still_names_the_old_source() {
        let root = fixture("rename-summary");
        let source = "coordination/rounds/r53/reviews/B158-A0001-secondary-executor-opencode.md";
        let target = "coordination/rounds/r53/reports/executor-opencode-SUMMARY.md";
        run_git(&root, &["mv", source, target]);
        let target_bytes = fs::read(root.join(target)).unwrap();

        let output = run_doctor(&root);
        assert_tracked_finding(&output, source);
        assert!(!root.join(source).exists(), "doctor 不得写回旧源");
        assert_eq!(fs::read(root.join(target)).unwrap(), target_bytes);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn new_closed_artifacts_get_manual_quarantine_guidance_not_git_show() {
        let root = fixture("new-additions");
        let untracked = "coordination/rounds/r53/reviews/late-untracked.md";
        let staged = "coordination/rounds/r53/evidence/late-staged.json";
        fs::create_dir_all(root.join("coordination/rounds/r53/evidence")).unwrap();
        fs::write(root.join(untracked), b"late untracked review\n").unwrap();
        fs::write(root.join(staged), b"{\"late\":true}\n").unwrap();
        run_git(&root, &["add", staged]);
        let untracked_before = fs::read(root.join(untracked)).unwrap();
        let staged_before = fs::read(root.join(staged)).unwrap();

        let first = run_doctor(&root);
        let second = run_doctor(&root);
        let stdout = String::from_utf8_lossy(&first.stdout);
        assert!(!first.status.success(), "新增产物必须报红: {stdout}");
        for relative in [untracked, staged] {
            assert!(stdout.contains(relative), "必须点名新增文件: {stdout}");
            assert!(
                !stdout.contains(&format!("git show HEAD:{relative}")),
                "HEAD 无 blob 时不得建议必败的 git show: {stdout}"
            );
        }
        assert!(
            stdout.contains("coordination/runtime/closed-round-quarantine/")
                && stdout.contains("doctor 不自动处理")
                && stdout.contains("定点删除")
                && stdout.contains("人工处置"),
            "必须给出中性的人工隔离/删除提示: {stdout}"
        );
        assert!(
            !stdout.contains("checkout") && !stdout.contains("reset") && !stdout.contains("rm -rf"),
            "人工提示不得含仓库禁用/破坏性命令: {stdout}"
        );
        assert_eq!(first.status, second.status, "重复运行结论必须一致");
        assert_eq!(first.stdout, second.stdout, "重复运行输出必须一致");
        assert_eq!(fs::read(root.join(untracked)).unwrap(), untracked_before);
        assert_eq!(fs::read(root.join(staged)).unwrap(), staged_before);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn true_post_close_summary_and_open_round_work_stay_benign() {
        let root = fixture("benign-writes");
        let summary = "coordination/rounds/r53/reports/executor-opencode-SUMMARY.md";
        fs::write(root.join(summary), b"summary created after RoundClosed\n").unwrap();
        fs::write(
            root.join("coordination/rounds/r53/dispatch/executor-desktop/GO-B158-A0001.md.ack"),
            b"post-close ack change\n",
        )
        .unwrap();
        fs::write(
            root.join("coordination/rounds/r58/reviews/B179-A0001-primary-executor-claw.md"),
            b"active review correction\n",
        )
        .unwrap();
        let before = fs::read(root.join(summary)).unwrap();

        let first = run_doctor(&root);
        let second = run_doctor(&root);
        assert!(
            first.status.success(),
            "合法写入不应令 doctor 失败: {}\n{}",
            String::from_utf8_lossy(&first.stdout),
            String::from_utf8_lossy(&first.stderr)
        );
        assert_eq!(first.status, second.status, "重复运行结论必须一致");
        assert_eq!(first.stdout, second.stdout, "重复运行输出必须一致");
        assert_eq!(fs::read(root.join(summary)).unwrap(), before);
        fs::remove_dir_all(root).ok();
    }
}

mod wake_supervisor_cli_tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::os::unix::process::CommandExt as _;
    use std::process::{Child, ChildStdin, Command, Output, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    const PROVIDER_MODE_ENV: &str = "ORCH_B177_CLI_PROVIDER_MODE";
    const DETACHED_TARGET_MODE_ENV: &str = "ORCH_B189_DETACHED_TARGET_MODE";
    const DETACHED_PREFIX_ENV: &str = "ORCH_B189_DETACHED_PREFIX";
    const PROVIDER_TEST: &str = "wake_supervisor_cli_tests::b177_test_only_provider_child";
    static DETACHED_HUP_COUNT: AtomicUsize = AtomicUsize::new(0);
    static DETACHED_TERM_COUNT: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" {
        #[link_name = "kill"]
        fn libc_kill(pid: i32, signal: i32) -> i32;
        fn signal(signal: i32, handler: usize) -> usize;
        fn setsid() -> i32;
    }

    extern "C" fn count_detached_hup(_: i32) {
        DETACHED_HUP_COUNT.fetch_add(1, Ordering::SeqCst);
    }

    extern "C" fn count_detached_term(_: i32) {
        DETACHED_TERM_COUNT.fetch_add(1, Ordering::SeqCst);
    }

    fn configure_isolated_session(command: &mut Command) {
        // SAFETY: setsid(2) is async-signal-safe and the closure allocates
        // nothing between fork and exec. This helper is compiled only in tests.
        unsafe {
            command.pre_exec(|| {
                if setsid() < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }

    fn ignore_test_signal(signal_number: i32) {
        // SAFETY: the copied test-only provider installs SIG_IGN for a fixed
        // POSIX signal before entering its deliberate stubborn loop.
        let previous = unsafe { signal(signal_number, 1usize) };
        assert_ne!(previous, usize::MAX, "failed to ignore {signal_number}");
    }

    fn count_test_signal(signal_number: i32, handler: extern "C" fn(i32)) {
        // SAFETY: the handler only performs a lock-free atomic increment and
        // is installed by the detached test process before it publishes ready.
        let previous = unsafe { signal(signal_number, handler as usize) };
        assert_ne!(previous, usize::MAX, "failed to count {signal_number}");
    }

    fn write_new_mode_0600(path: &std::path::Path, bytes: &[u8]) {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }

    fn detached_marker_path(ready: &std::path::Path, prefix: &str) -> PathBuf {
        ready.join(format!("{prefix}-identity.json"))
    }

    fn detached_signal_path(ready: &std::path::Path, prefix: &str) -> PathBuf {
        ready.join(format!("{prefix}-signals.json"))
    }

    fn detached_exit_path(ready: &std::path::Path, prefix: &str) -> PathBuf {
        ready.join(format!("{prefix}-exit"))
    }

    fn install_provider_harness(program: &std::path::Path) {
        fs::copy(std::env::current_exe().unwrap(), program).unwrap();
        fs::set_permissions(program, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn provider_argv(program: &std::path::Path, secret: Option<&str>) -> Vec<String> {
        let mut argv = vec![
            program.to_string_lossy().into_owned(),
            "--exact".to_string(),
            PROVIDER_TEST.to_string(),
            "--nocapture".to_string(),
        ];
        if let Some(secret) = secret {
            // `--skip <filter>` is a legal libtest pair. The filter does not
            // match PROVIDER_TEST, so it keeps the secret in provider argv
            // without exposing it through the supervisor argv.
            argv.push("--skip".to_string());
            argv.push(secret.to_string());
        }
        argv
    }

    fn wait_for_file(path: &std::path::Path, deadline: Instant) {
        while !path.is_file() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(path.is_file(), "timed out waiting for {}", path.display());
    }

    fn emit_terminal() {
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(
                b"\n{\"type\":\"step_finish\",\"part\":{\"type\":\"step-finish\",\"reason\":\"stop\"}}\n",
            )
            .unwrap();
        stdout.flush().unwrap();
    }

    fn assert_unique_exact_terminal_record(log_path: &std::path::Path) {
        let log = fs::read_to_string(log_path).unwrap();
        let exact_records = log
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|record| {
                record.get("type").and_then(|value| value.as_str()) == Some("step_finish")
                    && record
                        .get("part")
                        .and_then(|part| part.get("type"))
                        .and_then(|value| value.as_str())
                        == Some("step-finish")
                    && record
                        .get("part")
                        .and_then(|part| part.get("reason"))
                        .and_then(|value| value.as_str())
                        == Some("stop")
            })
            .count();
        assert_eq!(
            exact_records, 1,
            "test provider log must contain exactly one independent exact terminal JSON record"
        );
    }

    fn assert_observed_receipted_managed_cleanup(status: &serde_json::Value) {
        assert_eq!(status["containmentCapability"], "observed-receipted");
        assert_eq!(status["containmentClaim"], "managed-scope-terminated");
        assert_eq!(status["managedScopeTerminated"], true);
        assert_eq!(status["forkComplete"], false);
        assert_eq!(status["processTreeTerminated"], false);
    }

    #[test]
    fn b177_test_only_provider_child() {
        let Ok(mode) = std::env::var(PROVIDER_MODE_ENV) else {
            return;
        };
        let ready = std::env::current_dir().unwrap().join("ready");
        match mode.as_str() {
            "detached-launcher" => {
                let target = std::env::var(DETACHED_TARGET_MODE_ENV).unwrap();
                let prefix = std::env::var(DETACHED_PREFIX_ENV).unwrap();
                fs::create_dir_all(&ready).unwrap();
                let grandchild = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", PROVIDER_TEST, "--nocapture", "--test-threads=1"])
                    .current_dir(std::env::current_dir().unwrap())
                    .env(PROVIDER_MODE_ENV, target)
                    .env(DETACHED_PREFIX_ENV, &prefix)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap();
                write_new_mode_0600(
                    &ready.join(format!("{prefix}-pid-offer")),
                    format!("{}\n", grandchild.id()).as_bytes(),
                );
                // Returning exits and is reaped by the launcher owner. The
                // exec'd grandchild keeps the inherited C-created SID/PGID.
                drop(grandchild);
            }
            "detached-sentinel" => {
                let prefix = std::env::var(DETACHED_PREFIX_ENV).unwrap();
                fs::create_dir_all(&ready).unwrap();
                DETACHED_HUP_COUNT.store(0, Ordering::SeqCst);
                DETACHED_TERM_COUNT.store(0, Ordering::SeqCst);
                count_test_signal(1, count_detached_hup);
                count_test_signal(15, count_detached_term);

                let pid = std::process::id();
                let reparent_deadline = Instant::now() + Duration::from_secs(5);
                let receipt = loop {
                    if let Some(receipt) =
                        orch_host::wake::inspect_managed_process_credential(pid).unwrap()
                    {
                        if receipt.observed_ppid == 1 {
                            break receipt;
                        }
                    }
                    assert!(
                        Instant::now() < reparent_deadline,
                        "detached fixture was not reparented"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                };
                let marker = serde_json::json!({
                    "pid": receipt.pid,
                    "uid": receipt.uid,
                    "ppid": receipt.observed_ppid,
                    "pgid": receipt.pgid,
                    "sid": receipt.sid,
                    "birthIdentity": receipt.birth_identity,
                    "executableSummary": receipt.executable_summary,
                });
                write_new_mode_0600(
                    &detached_marker_path(&ready, &prefix),
                    &serde_json::to_vec(&marker).unwrap(),
                );
                let signal_path = detached_signal_path(&ready, &prefix);
                write_new_mode_0600(
                    &signal_path,
                    &serde_json::to_vec(&serde_json::json!({"hup": 0, "term": 0})).unwrap(),
                );

                while !detached_exit_path(&ready, &prefix).is_file() {
                    fs::write(
                        &signal_path,
                        serde_json::to_vec(&serde_json::json!({
                            "hup": DETACHED_HUP_COUNT.load(Ordering::SeqCst),
                            "term": DETACHED_TERM_COUNT.load(Ordering::SeqCst),
                        }))
                        .unwrap(),
                    )
                    .unwrap();
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            "double-fork-provider" => {
                fs::create_dir_all(&ready).unwrap();
                let mut child_command = Command::new(std::env::current_exe().unwrap());
                child_command
                    .args(["--exact", PROVIDER_TEST, "--nocapture", "--test-threads=1"])
                    .current_dir(std::env::current_dir().unwrap())
                    .env(PROVIDER_MODE_ENV, "detached-launcher")
                    .env(DETACHED_TARGET_MODE_ENV, "detached-sentinel")
                    .env(DETACHED_PREFIX_ENV, "detached")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                configure_isolated_session(&mut child_command);
                let mut child = child_command.spawn().unwrap();
                let child_pid = child.id();
                assert!(child.wait().unwrap().success(), "C failed before reap");
                wait_for_file(
                    &detached_marker_path(&ready, "detached"),
                    Instant::now() + Duration::from_secs(5),
                );
                fs::write(
                    ready.join("provider-ready"),
                    format!("{} {child_pid}\n", std::process::id()),
                )
                .unwrap();
                emit_terminal();
                wait_for_file(
                    &ready.join("exit-now"),
                    Instant::now() + Duration::from_secs(5),
                );
            }
            "natural" => {
                let deadline = Instant::now() + Duration::from_secs(5);
                fs::create_dir_all(&ready).unwrap();
                fs::write(
                    ready.join("provider-ready"),
                    format!("{}\n", std::process::id()),
                )
                .unwrap();
                emit_terminal();
                wait_for_file(&ready.join("exit-now"), deadline);
            }
            "exec-after-offer" => {
                fs::create_dir_all(&ready).unwrap();
                fs::write(
                    ready.join("provider-ready"),
                    format!("{}\n", std::process::id()),
                )
                .unwrap();
                wait_for_file(
                    &ready.join("exec-now"),
                    Instant::now() + Duration::from_secs(5),
                );
                let replacement_ready = ready.join("replacement-ready");
                let error = Command::new("/bin/sh")
                    .args([
                        "-c",
                        "printf 'ready\\n' > \"$1\"; exec /bin/sleep 60",
                        "orch-b177-cli-test-replacement",
                        replacement_ready.to_string_lossy().as_ref(),
                    ])
                    .exec();
                panic!("exec test replacement failed: {error}");
            }
            "stubborn" => {
                ignore_test_signal(1);
                ignore_test_signal(15);
                emit_terminal();
                loop {
                    std::thread::sleep(Duration::from_secs(60));
                }
            }
            other => panic!("unknown B177 CLI provider mode: {other}"),
        }
    }

    fn exact_group_alive(pgid: u32) -> bool {
        Command::new("/bin/kill")
            .args(["-0", "--", &format!("-{pgid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    struct HiddenSupervisorTestGuard {
        child: Option<Child>,
        control: Option<ChildStdin>,
    }

    impl HiddenSupervisorTestGuard {
        fn wait_output(&mut self) -> Output {
            self.child
                .take()
                .expect("hidden supervisor guard lost Child")
                .wait_with_output()
                .unwrap()
        }
    }

    impl Drop for HiddenSupervisorTestGuard {
        fn drop(&mut self) {
            self.control.take();
            if let Some(child) = self.child.take() {
                // The hidden supervisor is the only owner of its provider
                // Child. Closing the control pipe asks that custodian to run
                // its normal fail-closed cleanup; killing or detaching it here
                // could orphan a just-spawned provider before OFFER exists.
                // `wait_with_output` also drains the piped diagnostic stream.
                let _ = child.wait_with_output();
            }
        }
    }

    #[derive(Debug, Clone)]
    struct DetachedIdentity {
        pid: u32,
        uid: u32,
        ppid: u32,
        pgid: u32,
        sid: u32,
        birth_identity: String,
        executable_summary: String,
    }

    fn read_detached_identity(ready: &std::path::Path, prefix: &str) -> DetachedIdentity {
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(detached_marker_path(ready, prefix)).unwrap())
                .unwrap();
        let number = |field: &str| {
            u32::try_from(value[field].as_u64().unwrap())
                .unwrap_or_else(|_| panic!("detached marker field out of range: {field}"))
        };
        DetachedIdentity {
            pid: number("pid"),
            uid: number("uid"),
            ppid: number("ppid"),
            pgid: number("pgid"),
            sid: number("sid"),
            birth_identity: value["birthIdentity"].as_str().unwrap().to_string(),
            executable_summary: value["executableSummary"].as_str().unwrap().to_string(),
        }
    }

    fn fresh_detached_identity_matches(identity: &DetachedIdentity) -> bool {
        orch_host::wake::inspect_managed_process_credential(identity.pid)
            .ok()
            .flatten()
            .is_some_and(|fresh| {
                fresh.pid == identity.pid
                    && fresh.uid == identity.uid
                    && fresh.observed_ppid == identity.ppid
                    && fresh.pgid == identity.pgid
                    && fresh.sid == identity.sid
                    && fresh.birth_identity == identity.birth_identity
                    && fresh.executable_summary == identity.executable_summary
            })
    }

    fn raw_global_snapshot_contains(pid: u32) -> bool {
        orch_host::wake::managed_process_topology_snapshot_contains(pid).unwrap_or(false)
    }

    fn read_detached_signal_counts(ready: &std::path::Path, prefix: &str) -> (u64, u64) {
        let path = detached_signal_path(ready, prefix);
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Ok(bytes) = fs::read(&path) {
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    if let (Some(hup), Some(term)) = (value["hup"].as_u64(), value["term"].as_u64())
                    {
                        return (hup, term);
                    }
                }
            }
            assert!(
                Instant::now() < deadline,
                "signal counter stayed unreadable"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    struct DetachedSentinelOwner {
        ready: PathBuf,
        prefix: String,
        identity: Option<DetachedIdentity>,
    }

    impl DetachedSentinelOwner {
        fn pending(ready: PathBuf, prefix: &str) -> Self {
            Self {
                ready,
                prefix: prefix.to_string(),
                identity: None,
            }
        }

        fn capture(&mut self) -> &DetachedIdentity {
            if self.identity.is_none() {
                wait_for_file(
                    &detached_marker_path(&self.ready, &self.prefix),
                    Instant::now() + Duration::from_secs(5),
                );
                self.identity = Some(read_detached_identity(&self.ready, &self.prefix));
            }
            self.identity.as_ref().unwrap()
        }

        fn exact_cleanup(&mut self) {
            if self.identity.is_none() {
                let marker = detached_marker_path(&self.ready, &self.prefix);
                let deadline = Instant::now() + Duration::from_secs(2);
                while !marker.is_file() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                if marker.is_file() {
                    self.identity = Some(read_detached_identity(&self.ready, &self.prefix));
                }
            }
            let Some(identity) = self.identity.as_ref() else {
                return;
            };
            if fresh_detached_identity_matches(identity) {
                let _ = fs::write(detached_exit_path(&self.ready, &self.prefix), b"exit\n");
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            while fresh_detached_identity_matches(identity) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if fresh_detached_identity_matches(identity) {
                // The exact PID/UID/birth/PGID/SID tuple was revalidated just
                // above; this fallback is only RAII cleanup for a broken test
                // control loop and never participates in product discovery.
                if let Ok(pid) = i32::try_from(identity.pid) {
                    // SAFETY: the full immutable receipt and current topology
                    // were revalidated immediately above.
                    let _ = unsafe { libc_kill(pid, 9) };
                }
                let deadline = Instant::now() + Duration::from_secs(2);
                while fresh_detached_identity_matches(identity) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    impl Drop for DetachedSentinelOwner {
        fn drop(&mut self) {
            self.exact_cleanup();
        }
    }

    fn spawn_unrelated_detached_sentinel(
        program: &std::path::Path,
        root: &std::path::Path,
        prefix: &str,
    ) -> DetachedSentinelOwner {
        let ready = root.join("ready");
        fs::create_dir_all(&ready).unwrap();
        let mut command = Command::new(program);
        command
            .args(["--exact", PROVIDER_TEST, "--nocapture", "--test-threads=1"])
            .current_dir(root)
            .env(PROVIDER_MODE_ENV, "detached-launcher")
            .env(DETACHED_TARGET_MODE_ENV, "detached-sentinel")
            .env(DETACHED_PREFIX_ENV, prefix)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_isolated_session(&mut command);
        let mut launcher = command.spawn().unwrap();
        assert!(launcher.wait().unwrap().success());
        wait_for_file(
            &detached_marker_path(&ready, prefix),
            Instant::now() + Duration::from_secs(5),
        );
        let identity = read_detached_identity(&ready, prefix);
        assert_eq!(identity.ppid, 1);
        assert!(fresh_detached_identity_matches(&identity));
        DetachedSentinelOwner {
            ready,
            prefix: prefix.to_string(),
            identity: Some(identity),
        }
    }

    #[test]
    fn b177_hidden_supervisor_guard_waits_for_custodian_after_control_eof() {
        let root = orch_host::util::test_scratch_dir("b177-guard-custodian-eof");
        let marker = root.join("custodian-finished");
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "read ignored || true; sleep 0.1; printf 'done\\n' > \"$1\"",
                "orch-b177-guard-custodian",
                marker.to_string_lossy().as_ref(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let control = child.stdin.take().unwrap();
        let guard = HiddenSupervisorTestGuard {
            child: Some(child),
            control: Some(control),
        };
        drop(guard);
        assert_eq!(fs::read(&marker).unwrap(), b"done\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn b177_empty_hidden_supervisor_guard_never_touches_an_unrelated_group() {
        let mut victim = Command::new("/bin/sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let victim_pgid = victim.id();
        drop(HiddenSupervisorTestGuard {
            child: None,
            control: None,
        });
        let group_was_alive = exact_group_alive(victim_pgid);
        let status_before_cleanup = victim.try_wait();
        let kill_result = victim.kill();
        let wait_result = victim.wait();

        assert!(group_was_alive);
        assert!(
            matches!(&status_before_cleanup, Ok(None)),
            "unrelated victim changed before owned cleanup: {status_before_cleanup:?}"
        );
        assert!(
            kill_result.is_ok(),
            "owned victim kill failed: {kill_result:?}"
        );
        assert!(
            wait_result.is_ok(),
            "owned victim wait failed: {wait_result:?}"
        );
    }

    fn run_b189_double_fork_containment_case(index: usize) {
        let root = orch_host::util::test_scratch_dir(&format!("b189-double-fork-{index}"));
        let logs = root.join("coordination/runtime/logs");
        let supervisors = root.join("coordination/runtime/supervisors");
        let bin = root.join("bin");
        let ready = root.join("ready");
        fs::create_dir_all(&logs).unwrap();
        fs::create_dir_all(&supervisors).unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&ready).unwrap();
        let program = bin.join("opencode");
        install_provider_harness(&program);

        // The twin is deliberately unrelated to the provider but has the same
        // uid, nearly the same birth time, PPID=1, and an isolated stale-leader
        // SID/PGID shape. Similarity must never grant signal authority.
        let mut twin = spawn_unrelated_detached_sentinel(&program, &root, "twin");
        let log_path = logs.join("wake-opencode.jsonl");
        fs::write(&log_path, "").unwrap();
        let token = format!("{:032x}", 0xb189_0000usize + index);
        let wake_id = format!("019fc189-0000-4000-8000-{index:012x}");
        let ack_path = supervisors.join(format!("{token}.ack.json"));
        let status_path = supervisors.join(format!("{token}.status.json"));
        let spec = serde_json::json!({
            "version": 1,
            "protocolRevision": 3,
            "wakeId": wake_id,
            "runtimeLimit": {
                "requestedReviewDeadlineSecs": null,
                "effectiveSecs": 21600,
                "source": "ordinary-default"
            },
            "token": token,
            "argv": provider_argv(&program, None),
            "cwd": root.clone(),
            "logPath": log_path.clone(),
            "ackPath": ack_path.clone(),
            "statusPath": status_path.clone(),
            "policy": {
                "naturalExitGraceMs": 3000,
                "termGraceMs": 2000,
                "pollIntervalMs": 20
            },
            "baseline": []
        });
        let mut supervisor_command = fixture_orch_command(&[]);
        supervisor_command
            .args(["--root", root.to_str().unwrap(), "__wake-supervise"])
            .env(PROVIDER_MODE_ENV, "double-fork-provider")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        configure_isolated_session(&mut supervisor_command);
        let mut child = supervisor_command.spawn().unwrap();
        let control = child.stdin.take().unwrap();
        let mut supervisor = HiddenSupervisorTestGuard {
            child: Some(child),
            control: Some(control),
        };
        let mut detached = DetachedSentinelOwner::pending(ready.clone(), "detached");
        let mut framed = serde_json::to_vec(&spec).unwrap();
        framed.push(b'\n');
        supervisor
            .control
            .as_mut()
            .unwrap()
            .write_all(&framed)
            .unwrap();
        supervisor.control.as_mut().unwrap().flush().unwrap();

        let barrier_deadline = Instant::now() + Duration::from_secs(8);
        while (!(ack_path.is_file() && ready.join("provider-ready").is_file()))
            && Instant::now() < barrier_deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(ack_path.is_file(), "double-fork supervisor never offered");
        assert!(
            ready.join("provider-ready").is_file(),
            "P did not reap C after D reparented"
        );
        let offer: serde_json::Value =
            serde_json::from_slice(&fs::read(&ack_path).unwrap()).unwrap();
        let provider_pgid = u32::try_from(offer["providerPid"].as_u64().unwrap()).unwrap();
        let detached_identity = detached.capture().clone();
        let twin_identity = twin.capture().clone();
        assert_eq!(detached_identity.ppid, 1);
        assert_eq!(twin_identity.ppid, 1);
        assert_eq!(detached_identity.uid, twin_identity.uid);
        assert!(raw_global_snapshot_contains(detached_identity.pid));
        assert!(raw_global_snapshot_contains(twin_identity.pid));

        fs::remove_file(&ack_path).unwrap();
        supervisor
            .control
            .as_mut()
            .unwrap()
            .write_all(format!("ACCEPT {token}\n").as_bytes())
            .unwrap();
        supervisor.control.as_mut().unwrap().flush().unwrap();
        supervisor.control.take();
        wait_for_file(&ack_path, Instant::now() + Duration::from_secs(2));
        let accepted: serde_json::Value =
            serde_json::from_slice(&fs::read(&ack_path).unwrap()).unwrap();
        assert_eq!(accepted["phase"], "ACCEPTED");
        fs::remove_file(&ack_path).unwrap();
        fs::write(ready.join("exit-now"), b"exit\n").unwrap();

        let output = supervisor.wait_output();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let status: serde_json::Value =
            serde_json::from_slice(&fs::read(&status_path).unwrap()).unwrap();
        assert_observed_receipted_managed_cleanup(&status);
        assert_eq!(status["ownedHelpers"], 0);
        assert!(status["error"].is_null());
        assert_unique_exact_terminal_record(&log_path);
        assert!(!exact_group_alive(provider_pgid));

        // Remaining alive is only the KILL proof. TERM/HUP are independently
        // accounted by signal handlers and therefore cannot hide a delivery.
        assert!(fresh_detached_identity_matches(&detached_identity));
        assert!(fresh_detached_identity_matches(&twin_identity));
        assert_eq!(read_detached_signal_counts(&ready, "detached"), (0, 0));
        assert_eq!(read_detached_signal_counts(&ready, "twin"), (0, 0));

        // Fresh PID/UID/birth/PGID/SID revalidation occurs inside exact_cleanup
        // immediately before the per-fixture control file is published.
        detached.exact_cleanup();
        twin.exact_cleanup();
        assert!(!fresh_detached_identity_matches(&detached_identity));
        assert!(!fresh_detached_identity_matches(&twin_identity));
        drop(detached);
        drop(twin);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn b189_unobserved_double_fork_and_unrelated_twin_are_never_signaled() {
        for index in 0..10 {
            run_b189_double_fork_containment_case(index);
        }
    }

    #[test]
    fn b177_hidden_supervisor_bypasses_round_preflight_and_keeps_spec_off_argv() {
        let root = orch_host::util::test_scratch_dir("b177-hidden-supervisor");
        let logs = root.join("coordination/runtime/logs");
        let supervisors = root.join("coordination/runtime/supervisors");
        let bin = root.join("bin");
        let ready = root.join("ready");
        fs::create_dir_all(&logs).unwrap();
        fs::create_dir_all(&supervisors).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let program = bin.join("opencode");
        install_provider_harness(&program);
        let log_path = logs.join("wake-opencode.jsonl");
        fs::write(&log_path, "").unwrap();
        let token = "abcdef0123456789abcdef0123456789";
        let secret = "prompt-must-never-appear-in-process-argv";
        let ack_path = supervisors.join(format!("{token}.ack.json"));
        let status_path = supervisors.join(format!("{token}.status.json"));
        let spec = serde_json::json!({
            "version": 1,
            "protocolRevision": 3,
            "wakeId": "019fc177-0000-4000-8000-000000000001",
            "runtimeLimit": {
                "requestedReviewDeadlineSecs": null,
                "effectiveSecs": 21600,
                "source": "ordinary-default"
            },
            "token": token,
            "argv": provider_argv(&program, Some(secret)),
            "cwd": root.clone(),
            "logPath": log_path.clone(),
            "ackPath": ack_path.clone(),
            "statusPath": status_path.clone(),
            "policy": {
                "naturalExitGraceMs": 3000,
                "termGraceMs": 2000,
                "pollIntervalMs": 20
            },
            "baseline": []
        });
        let mut supervisor_command = fixture_orch_command(&[]);
        supervisor_command
            .args(["--root", root.to_str().unwrap(), "__wake-supervise"])
            .env(PROVIDER_MODE_ENV, "natural")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        configure_isolated_session(&mut supervisor_command);
        let mut child = supervisor_command.spawn().unwrap();
        let control = child.stdin.take().unwrap();
        let mut guard = HiddenSupervisorTestGuard {
            child: Some(child),
            control: Some(control),
        };
        let mut framed = serde_json::to_vec(&spec).unwrap();
        framed.push(b'\n');
        guard.control.as_mut().unwrap().write_all(&framed).unwrap();
        guard.control.as_mut().unwrap().flush().unwrap();

        let provider_ready = ready.join("provider-ready");
        let deadline = Instant::now() + Duration::from_secs(5);
        while (!(ack_path.exists() && provider_ready.exists())) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            ack_path.exists(),
            "hidden entry must ack without CURRENT-ROUND"
        );
        assert!(
            provider_ready.exists(),
            "test-only provider must reach its natural-exit barrier"
        );
        assert_eq!(
            fs::metadata(&ack_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let offer: serde_json::Value =
            serde_json::from_slice(&fs::read(&ack_path).unwrap()).unwrap();
        assert_eq!(offer["protocolRevision"], 3);
        let provider_pgid = u32::try_from(offer["providerPid"].as_u64().unwrap()).unwrap();
        let ps = Command::new("/bin/ps")
            .args([
                "-ww",
                "-p",
                &guard.child.as_ref().unwrap().id().to_string(),
                "-o",
                "command=",
            ])
            .output();
        match ps {
            Ok(ps) => {
                let command_line = String::from_utf8_lossy(&ps.stdout);
                let expected_program = fixture_orch_command(&[]).get_program().to_owned();
                orch_host::wake::validate_hidden_supervisor_command_line_for_test(
                    &command_line,
                    expected_program.to_str().unwrap(),
                    root.to_str().unwrap(),
                )
                .unwrap_or_else(|diagnostic| panic!("{diagnostic}"));

                let exact =
                    command_line.trim_matches(|character: char| character.is_ascii_whitespace());
                for leaked in [
                    program.to_string_lossy().into_owned(),
                    secret.to_string(),
                    "--exact".to_string(),
                    PROVIDER_TEST.to_string(),
                ] {
                    let observed = format!("{exact} {leaked}");
                    assert!(
                        orch_host::wake::validate_hidden_supervisor_command_line_for_test(
                            &observed,
                            expected_program.to_str().unwrap(),
                            root.to_str().unwrap(),
                        )
                        .is_err(),
                        "extra provider value must fail the exact supervisor command: {leaked:?}"
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                // Some test sandboxes deny even exact-PID ps. Exercise the
                // production parser in a second real CLI process instead;
                // closed stdin makes the hidden supervisor exit immediately.
                let parsed = fixture_orch_command(&[])
                    .args(["--root", root.to_str().unwrap(), "__wake-supervise"])
                    .stdin(Stdio::null())
                    .output()
                    .expect("launch hidden supervisor parser probe");
                assert!(!parsed.status.success());
                assert!(
                    !String::from_utf8_lossy(&parsed.stderr).contains("unexpected argument"),
                    "hidden supervisor command was rejected by clap: {}",
                    String::from_utf8_lossy(&parsed.stderr)
                );
            }
            Err(error) => panic!("exact-PID ps failed: {error}"),
        }

        fs::remove_file(&ack_path).unwrap();
        guard
            .control
            .as_mut()
            .unwrap()
            .write_all(format!("ACCEPT {token}\n").as_bytes())
            .unwrap();
        guard.control.as_mut().unwrap().flush().unwrap();
        guard.control.take();

        let accepted_deadline = Instant::now() + Duration::from_secs(2);
        while !ack_path.exists() && Instant::now() < accepted_deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(ack_path.exists(), "hidden entry must confirm ACCEPTED");
        let accepted: serde_json::Value =
            serde_json::from_slice(&fs::read(&ack_path).unwrap()).unwrap();
        assert_eq!(accepted["token"], token);
        assert_eq!(accepted["protocolRevision"], 3);
        assert_eq!(accepted["phase"], "ACCEPTED");
        assert_eq!(accepted["providerPid"], accepted["pgid"]);
        fs::remove_file(&ack_path).unwrap();
        fs::write(ready.join("exit-now"), b"exit\n").unwrap();

        let output = guard.wait_output();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let status: serde_json::Value =
            serde_json::from_slice(&fs::read(&status_path).unwrap()).unwrap();
        assert_eq!(status["terminalSeen"], true);
        assert_eq!(status["exitedNaturally"], true);
        assert_observed_receipted_managed_cleanup(&status);
        assert_unique_exact_terminal_record(&log_path);
        assert!(!exact_group_alive(provider_pgid));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn b177_accept_refuses_a_provider_exec_between_offer_and_accept() {
        let root = orch_host::util::test_scratch_dir("b177-accept-exec-race");
        let logs = root.join("coordination/runtime/logs");
        let supervisors = root.join("coordination/runtime/supervisors");
        let bin = root.join("bin");
        let ready = root.join("ready");
        fs::create_dir_all(&logs).unwrap();
        fs::create_dir_all(&supervisors).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let program = bin.join("opencode");
        install_provider_harness(&program);
        let log_path = logs.join("wake-opencode.jsonl");
        fs::write(&log_path, "").unwrap();
        let token = "fedcba9876543210fedcba9876543210";
        let ack_path = supervisors.join(format!("{token}.ack.json"));
        let status_path = supervisors.join(format!("{token}.status.json"));
        let spec = serde_json::json!({
            "version": 1,
            "protocolRevision": 3,
            "wakeId": "019fc177-0000-4000-8000-000000000002",
            "runtimeLimit": {
                "requestedReviewDeadlineSecs": null,
                "effectiveSecs": 21600,
                "source": "ordinary-default"
            },
            "token": token,
            "argv": provider_argv(&program, None),
            "cwd": root.clone(),
            "logPath": log_path,
            "ackPath": ack_path.clone(),
            "statusPath": status_path.clone(),
            "policy": {
                "naturalExitGraceMs": 3000,
                "termGraceMs": 2000,
                "pollIntervalMs": 20
            },
            "baseline": []
        });
        let mut supervisor_command = fixture_orch_command(&[]);
        supervisor_command
            .args(["--root", root.to_str().unwrap(), "__wake-supervise"])
            .env(PROVIDER_MODE_ENV, "exec-after-offer")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        configure_isolated_session(&mut supervisor_command);
        let mut child = supervisor_command.spawn().unwrap();
        let control = child.stdin.take().unwrap();
        let mut guard = HiddenSupervisorTestGuard {
            child: Some(child),
            control: Some(control),
        };
        let mut framed = serde_json::to_vec(&spec).unwrap();
        framed.push(b'\n');
        guard.control.as_mut().unwrap().write_all(&framed).unwrap();
        guard.control.as_mut().unwrap().flush().unwrap();

        let offer_deadline = Instant::now() + Duration::from_secs(5);
        while (!(ack_path.is_file() && ready.join("provider-ready").is_file()))
            && Instant::now() < offer_deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(ack_path.is_file(), "supervisor must publish OFFER");
        assert!(
            ready.join("provider-ready").is_file(),
            "provider fixture must reach its exec barrier"
        );
        let offer: serde_json::Value =
            serde_json::from_slice(&fs::read(&ack_path).unwrap()).unwrap();
        assert_eq!(offer["protocolRevision"], 3);
        let provider_pgid = u32::try_from(offer["providerPid"].as_u64().unwrap()).unwrap();

        fs::write(ready.join("exec-now"), b"exec\n").unwrap();
        let replacement_deadline = Instant::now() + Duration::from_secs(5);
        while !ready.join("replacement-ready").is_file() && Instant::now() < replacement_deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            ready.join("replacement-ready").is_file(),
            "replacement marker must prove exec completed"
        );
        fs::remove_file(&ack_path).unwrap();
        guard
            .control
            .as_mut()
            .unwrap()
            .write_all(format!("ACCEPT {token}\n").as_bytes())
            .unwrap();
        guard.control.as_mut().unwrap().flush().unwrap();
        guard.control.take();

        let cleanup_started = Instant::now();
        let output = guard.wait_output();
        assert!(
            cleanup_started.elapsed() < Duration::from_secs(10),
            "rejected same-epoch exec must be terminated by bounded cleanup, not natural sleep exit"
        );
        assert!(!output.status.success());
        let status: serde_json::Value =
            serde_json::from_slice(&fs::read(&status_path).unwrap()).unwrap();
        assert!(status["error"]
            .as_str()
            .is_some_and(|error| error.contains("changed between OFFER and ACCEPT")));
        assert_eq!(status["signals"], serde_json::json!(["TERM"]));
        assert_observed_receipted_managed_cleanup(&status);
        assert!(
            !ack_path.exists(),
            "a changed provider must never receive ACCEPTED"
        );
        assert!(!exact_group_alive(provider_pgid));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn b177_offer_rejects_eof_bad_token_and_timeout_without_orphaning_provider() {
        for (index, mode) in ["eof", "bad-token", "timeout"].into_iter().enumerate() {
            let root = orch_host::util::test_scratch_dir(&format!("b177-ack-{mode}"));
            let logs = root.join("coordination/runtime/logs");
            let supervisors = root.join("coordination/runtime/supervisors");
            let bin = root.join("bin");
            fs::create_dir_all(&logs).unwrap();
            fs::create_dir_all(&supervisors).unwrap();
            fs::create_dir_all(&bin).unwrap();
            let program = bin.join("opencode");
            install_provider_harness(&program);
            let log_path = logs.join("wake-opencode.jsonl");
            fs::write(&log_path, "").unwrap();
            let token = format!("{:032x}", index + 1);
            let wake_id = format!("019fc177-0000-4000-8000-{:012x}", index + 3);
            let ack_path = supervisors.join(format!("{token}.ack.json"));
            let status_path = supervisors.join(format!("{token}.status.json"));
            let spec = serde_json::json!({
                "version": 1,
                "protocolRevision": 3,
                "wakeId": wake_id,
                "runtimeLimit": {
                    "requestedReviewDeadlineSecs": null,
                    "effectiveSecs": 21600,
                    "source": "ordinary-default"
                },
                "token": token,
                "argv": provider_argv(&program, None),
                "cwd": root.clone(),
                "logPath": log_path,
                "ackPath": ack_path.clone(),
                "statusPath": status_path.clone(),
                "policy": {
                    "naturalExitGraceMs": 3000,
                    "termGraceMs": 2000,
                    "pollIntervalMs": 20
                },
                "baseline": []
            });
            let mut supervisor_command = fixture_orch_command(&[]);
            supervisor_command
                .args(["--root", root.to_str().unwrap(), "__wake-supervise"])
                .env(PROVIDER_MODE_ENV, "stubborn")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            configure_isolated_session(&mut supervisor_command);
            let mut child = supervisor_command.spawn().unwrap();
            let control = child.stdin.take().unwrap();
            let mut guard = HiddenSupervisorTestGuard {
                child: Some(child),
                control: Some(control),
            };
            let mut framed = serde_json::to_vec(&spec).unwrap();
            framed.push(b'\n');
            guard.control.as_mut().unwrap().write_all(&framed).unwrap();
            guard.control.as_mut().unwrap().flush().unwrap();

            let deadline = Instant::now() + Duration::from_secs(2);
            while !ack_path.is_file() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            let ack: serde_json::Value =
                serde_json::from_slice(&fs::read(&ack_path).unwrap()).unwrap();
            assert_eq!(ack["protocolRevision"], 3);
            let provider_pgid = u32::try_from(ack["providerPid"].as_u64().unwrap()).unwrap();
            match mode {
                "eof" => {
                    guard.control.take();
                }
                "bad-token" => {
                    fs::remove_file(&ack_path).unwrap();
                    guard
                        .control
                        .as_mut()
                        .unwrap()
                        .write_all(b"ACCEPT 00000000000000000000000000000000\n")
                        .unwrap();
                    guard.control.as_mut().unwrap().flush().unwrap();
                    guard.control.take();
                }
                "timeout" => {
                    fs::remove_file(&ack_path).unwrap();
                }
                _ => unreachable!(),
            }

            let output = guard.wait_output();
            guard.control.take();
            let status: serde_json::Value =
                serde_json::from_slice(&fs::read(&status_path).unwrap()).unwrap();
            assert!(!output.status.success(), "mode={mode}");
            assert_observed_receipted_managed_cleanup(&status);
            assert!(status["error"].is_string(), "mode={mode}");
            assert!(!exact_group_alive(provider_pgid), "mode={mode}");
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn b177_wake_fixture_is_not_a_cli_command_and_cannot_write_a_marker() {
        let root = orch_host::util::test_scratch_dir("b177-no-production-fixture");
        let marker = root.join("must-not-exist");
        let parsed = fixture_orch_command(&[])
            .args([
                "--root",
                root.to_str().unwrap(),
                "__wake-fixture",
                "provider-natural",
                "--ready-path",
                marker.to_str().unwrap(),
            ])
            .output()
            .expect("launch production CLI parser probe");
        assert!(
            !parsed.status.success(),
            "production CLI accepted __wake-fixture"
        );
        assert!(
            !marker.exists(),
            "rejected fixture syntax unexpectedly wrote a marker"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
