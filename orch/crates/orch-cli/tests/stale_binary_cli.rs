//! B169/H35 real-CLI coverage for the compiled build stamp and stale guard.
//!
//! The launched binary is the exact executable Cargo built for this integration
//! target, including when `CARGO_TARGET_DIR` isolates the build.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

struct StaleRepo {
    root: PathBuf,
    build_sha: String,
    main_sha: String,
}

fn fixture_orch_command(extras: &[(&str, &str)]) -> Command {
    let mut command = Command::new(support::orch_bin());
    support::configure_fixture_git_env(&mut command, extras);
    command
}

fn orch_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR 应形如 <worktree>/orch/crates/orch-cli")
        .to_path_buf()
}

fn repo_root() -> PathBuf {
    orch_dir()
        .parent()
        .expect("orch/ 应位于 worktree 根")
        .to_path_buf()
}

fn unique_scratch_path(name: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let parent = orch_dir().join("target").join("test-tmp");
    fs::create_dir_all(&parent).expect("创建 test-tmp 失败");
    parent.join(format!("b169-stale-{name}-{}-{seq}", std::process::id()))
}

fn git(root: &Path, args: &[&str]) -> Output {
    support::fixture_git_command(root)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("启动 git {args:?} 失败: {error}"))
}

fn git_ok(root: &Path, args: &[&str]) -> Output {
    let output = git(root, args);
    assert!(
        output.status.success(),
        "git {args:?} 失败: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git_stdout(root: &Path, args: &[&str]) -> String {
    String::from_utf8(git_ok(root, args).stdout)
        .expect("git stdout 非 UTF-8")
        .trim()
        .to_owned()
}

fn stale_repo_fixture(name: &str, materialize_build_tree: bool) -> Option<StaleRepo> {
    // Source archives intentionally have no stamp. The host-level contract
    // covers that graceful-degradation path; a git-backed test run exercises
    // the full stale CLI path.
    let build_sha = option_env!("ORCH_BUILD_GIT_SHA")?.trim().to_owned();
    if build_sha.is_empty() {
        return None;
    }
    let root = unique_scratch_path(name);
    let clone = support::fixture_git_command(&repo_root())
        .args(["clone", "--no-checkout", "--quiet"])
        .arg(repo_root())
        .arg(&root)
        .output()
        .expect("启动 git clone 失败");
    assert!(
        clone.status.success(),
        "git clone 失败: {}",
        String::from_utf8_lossy(&clone.stderr)
    );

    // Create a tiny tree whose parent is the binary's build commit. The child
    // therefore advances main and changes `orch/**`, while the working tree
    // remains suitable for a clean `orch init` integration test.
    if materialize_build_tree {
        git_ok(&root, &["read-tree", &build_sha]);
    } else {
        git_ok(&root, &["read-tree", "--empty"]);
    }
    fs::create_dir_all(root.join("orch")).expect("创建 marker 目录失败");
    fs::write(root.join("orch/stale-marker"), "compiled input changed\n").expect("写 marker 失败");
    git_ok(&root, &["add", "orch/stale-marker"]);
    let tree = git_stdout(&root, &["write-tree"]);
    let commit = support::fixture_git_command(&root)
        .args(["commit-tree", &tree, "-p", &build_sha])
        .env("GIT_AUTHOR_NAME", "B169 Test")
        .env("GIT_AUTHOR_EMAIL", "b169@example.invalid")
        .env("GIT_COMMITTER_NAME", "B169 Test")
        .env("GIT_COMMITTER_EMAIL", "b169@example.invalid")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("启动 git commit-tree 失败");
    assert!(
        commit.status.success(),
        "git commit-tree 失败: {}",
        String::from_utf8_lossy(&commit.stderr)
    );
    let main_sha = String::from_utf8(commit.stdout)
        .expect("commit-tree stdout 非 UTF-8")
        .trim()
        .to_owned();
    git_ok(&root, &["update-ref", "refs/heads/main", &main_sha]);
    if materialize_build_tree {
        git_ok(&root, &["read-tree", "--reset", "-u", &main_sha]);
    }

    Some(StaleRepo {
        root,
        build_sha,
        main_sha,
    })
}

fn stale_repo(name: &str) -> Option<StaleRepo> {
    stale_repo_fixture(name, false)
}

fn run_orch(root: &Path, args: &[&str]) -> Output {
    fixture_orch_command(&[])
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("启动 orch 失败")
}

fn current_valid_materialized_round(root: &Path) -> String {
    let rounds = fs::read_dir(root.join("coordination/rounds"))
        .expect("materialized build tree 缺 coordination/rounds");
    let mut candidates = Vec::new();

    for entry in rounds {
        let entry = entry.expect("读取 materialized round 目录失败");
        if !entry.file_type().expect("读取 round 类型失败").is_dir() {
            continue;
        }
        let Some(round) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(number) = round
            .strip_prefix('r')
            .and_then(|value| value.parse::<u64>().ok())
        else {
            continue;
        };
        let round_root = entry.path();
        if !round_root.join("ROUND-IR.yaml").is_file() {
            continue;
        }
        let Ok(events) = fs::read_to_string(round_root.join("events.jsonl")) else {
            continue;
        };
        let mut production_validated = false;
        let mut closed = false;
        for line in events.lines().filter(|line| !line.trim().is_empty()) {
            let event: serde_json::Value =
                serde_json::from_str(line).expect("materialized ledger 必须是合法 JSONL");
            match event.get("type").and_then(serde_json::Value::as_str) {
                Some("TaskValidated") => {
                    let digest = event
                        .pointer("/payload/validationDigest")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    production_validated |= event.get("actor").and_then(serde_json::Value::as_str)
                        == Some("runtime:orch")
                        && digest.len() == 64
                        && digest.bytes().all(|byte| byte.is_ascii_hexdigit());
                }
                Some("RoundClosed") => closed = true,
                _ => {}
            }
        }
        if production_validated && !closed {
            candidates.push((number, round));
        }
    }

    candidates.sort();
    candidates
        .pop()
        .map(|(_, round)| round)
        .expect("materialized build tree 中必须存在 production-validated 未关闭轮")
}

fn assert_stale_read_allowed(repo: &StaleRepo, output: &Output, command: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "陈旧二进制必须允许只读 {command}: {stderr}"
    );
    assert!(
        stderr.contains("read-only command is allowed")
            && stderr.contains(&repo.build_sha)
            && stderr.contains(&repo.main_sha),
        "只读 {command} 放行必须留证并点名两 SHA: {stderr}"
    );
}

fn assert_stale_write_rejected(repo: &StaleRepo, output: &Output, command: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "陈旧二进制必须拒绝写入型 {command}"
    );
    assert!(
        stderr.contains("stale binary refused state-changing command")
            && stderr.contains(&repo.build_sha)
            && stderr.contains(&repo.main_sha),
        "写入型 {command} 拒绝必须留证并点名两 SHA: {stderr}"
    );
}

fn assert_stale_override_audited(repo: &StaleRepo, output: &Output, command: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "显式逃生舱应放行写入型 {command}: {stderr}"
    );
    assert!(
        stderr.contains("--allow-stale-binary")
            && stderr.contains(&repo.build_sha)
            && stderr.contains(&repo.main_sha),
        "{command} 逃生舱必须留证并点名两 SHA: {stderr}"
    );
}

#[test]
fn stale_state_change_is_rejected_and_override_is_audited() {
    let Some(repo) = stale_repo("mutation") else {
        eprintln!("skip: build outside git has no ORCH_BUILD_GIT_SHA");
        return;
    };

    let denied = run_orch(&repo.root, &["init"]);
    let denied_stderr = String::from_utf8_lossy(&denied.stderr);
    assert!(!denied.status.success(), "陈旧二进制不得静默执行 init");
    assert!(
        denied_stderr.contains("stale binary refused state-changing command"),
        "stderr 未点名陈旧拒绝: {denied_stderr}"
    );
    assert!(
        denied_stderr.contains(&repo.build_sha) && denied_stderr.contains(&repo.main_sha),
        "stderr 必须点名 build/main 两个 SHA: {denied_stderr}"
    );
    assert!(
        !repo.root.join("coordination").exists(),
        "拒绝必须发生在 init 写盘之前"
    );

    let allowed = run_orch(&repo.root, &["--allow-stale-binary", "init"]);
    let allowed_stderr = String::from_utf8_lossy(&allowed.stderr);
    assert!(
        allowed.status.success(),
        "显式逃生舱应放行 init: {allowed_stderr}"
    );
    assert!(
        allowed_stderr.contains("--allow-stale-binary")
            && allowed_stderr.contains(&repo.build_sha)
            && allowed_stderr.contains(&repo.main_sha),
        "逃生舱必须留证并点名两 SHA: {allowed_stderr}"
    );
    assert!(repo.root.join("coordination").is_dir());
}

#[test]
fn stale_read_only_doctor_remains_available_and_warns() {
    let Some(repo) = stale_repo("doctor") else {
        eprintln!("skip: build outside git has no ORCH_BUILD_GIT_SHA");
        return;
    };
    let initialized = run_orch(&repo.root, &["--allow-stale-binary", "init"]);
    assert!(
        initialized.status.success(),
        "doctor fixture init 失败: {}",
        String::from_utf8_lossy(&initialized.stderr)
    );

    let doctor = run_orch(&repo.root, &["doctor"]);
    let stdout = String::from_utf8_lossy(&doctor.stdout);
    let stderr = String::from_utf8_lossy(&doctor.stderr);
    assert!(
        doctor.status.success(),
        "只读 doctor 必须保持可用；stdout={stdout}; stderr={stderr}"
    );
    assert!(
        stderr.contains("read-only command is allowed")
            && stderr.contains(&repo.build_sha)
            && stderr.contains(&repo.main_sha),
        "只读放行应明确留证: {stderr}"
    );
    assert!(
        stdout.contains("二进制构建印记") && stdout.contains("陈旧"),
        "doctor 应追加黄色重建提示: {stdout}"
    );
}

#[test]
fn stale_ledger_recover_distinguishes_dry_run_from_apply() {
    let Some(repo) = stale_repo("ledger-recover") else {
        eprintln!("skip: build outside git has no ORCH_BUILD_GIT_SHA");
        return;
    };
    let round = "r-stale-ledger";
    let ledger_dir = repo.root.join("coordination/rounds").join(round);
    let wal_dir = repo.root.join("coordination/runtime/ledger-wal");
    fs::create_dir_all(&ledger_dir).expect("创建 ledger fixture 失败");
    fs::create_dir_all(&wal_dir).expect("创建 WAL fixture 失败");
    fs::write(ledger_dir.join("events.jsonl"), b"").expect("写 ledger fixture 失败");
    fs::write(wal_dir.join(format!("{round}.jsonl")), b"").expect("写 WAL fixture 失败");

    let dry_run = run_orch(&repo.root, &["ledger", "recover", "--round", round]);
    assert_stale_read_allowed(&repo, &dry_run, "ledger recover dry-run");
    assert!(
        String::from_utf8_lossy(&dry_run.stdout).contains("apply=false"),
        "dry-run 必须打印恢复计划: {}",
        String::from_utf8_lossy(&dry_run.stdout)
    );

    let denied = run_orch(
        &repo.root,
        &["ledger", "recover", "--round", round, "--apply"],
    );
    assert_stale_write_rejected(&repo, &denied, "ledger recover --apply");

    let allowed = run_orch(
        &repo.root,
        &[
            "--allow-stale-binary",
            "ledger",
            "recover",
            "--round",
            round,
            "--apply",
        ],
    );
    assert_stale_override_audited(&repo, &allowed, "ledger recover --apply");
}

#[test]
fn stale_snapshot_distinguishes_read_from_write() {
    let Some(repo) = stale_repo_fixture("snapshot", true) else {
        eprintln!("skip: build outside git has no ORCH_BUILD_GIT_SHA");
        return;
    };
    let runtime_dir = repo.root.join("coordination/runtime");
    fs::create_dir_all(&runtime_dir).expect("创建 snapshot runtime fixture 失败");
    let current_round = current_valid_materialized_round(&repo.root);
    assert_ne!(current_round, "r58", "fixture 不得退回已关闭的历史轮");
    fs::write(
        runtime_dir.join("CURRENT-ROUND"),
        format!("{current_round}\n"),
    )
    .expect("写 snapshot CURRENT-ROUND fixture 失败");
    let read = run_orch(&repo.root, &["snapshot"]);
    assert_stale_read_allowed(&repo, &read, "snapshot");

    fs::write(runtime_dir.join("CURRENT-ROUND"), "r58\n")
        .expect("写历史 snapshot CURRENT-ROUND fixture 失败");
    let historical = run_orch(&repo.root, &["snapshot"]);
    let historical_stderr = String::from_utf8_lossy(&historical.stderr);
    assert!(
        !historical.status.success(),
        "历史 r58 必须保持 IR 漂移拒绝"
    );
    // 归档卡可能引用后来已改名的模块，因此历史轮既可能在 IR 漂移校验处拒绝，
    // 也可能更早在 plan crosscheck 处拒绝。两者都必须响亮失败；绝不能为了让
    // 归档卡通过当前 HEAD 的校验而恢复已经删除的旧模块。
    assert!(
        historical_stderr.contains("ROUND-IR 与 mode/binding/cards 漂移")
            || historical_stderr.contains("plan crosscheck 违规清单"),
        "历史轮必须响亮拒绝: {historical_stderr}"
    );

    fs::write(
        runtime_dir.join("CURRENT-ROUND"),
        format!("{current_round}\n"),
    )
    .expect("恢复当前 snapshot CURRENT-ROUND fixture 失败");

    let snapshot_path = repo.root.join("coordination/runtime/snapshot.json");
    let denied = run_orch(&repo.root, &["snapshot", "--write"]);
    assert_stale_write_rejected(&repo, &denied, "snapshot --write");
    assert!(!snapshot_path.exists(), "拒绝必须发生在 snapshot 写盘之前");

    let allowed = run_orch(&repo.root, &["--allow-stale-binary", "snapshot", "--write"]);
    assert_stale_override_audited(&repo, &allowed, "snapshot --write");
    assert!(
        snapshot_path.is_file(),
        "逃生舱放行后 snapshot 必须真实写盘"
    );
}

#[test]
fn missing_git_metadata_degrades_with_an_explanation() {
    let root = unique_scratch_path("no-git");
    fs::create_dir_all(&root).expect("创建无 git fixture 失败");
    // test-tmp itself lives below the real worktree, so cap git discovery at
    // that parent to model a genuinely non-git project root.
    let output = fixture_orch_command(&[])
        .arg("--root")
        .arg(&root)
        .arg("schema")
        .env(
            "GIT_CEILING_DIRECTORIES",
            root.parent().expect("scratch path 应有 parent"),
        )
        .output()
        .expect("启动 orch 失败");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "无 git 时只读命令应放行: {stderr}");
    assert!(
        stderr.contains("binary staleness unknown") && stderr.contains("command allowed"),
        "降级放行必须说明原因: {stderr}"
    );
}
