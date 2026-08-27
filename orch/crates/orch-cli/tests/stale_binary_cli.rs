//! B169/H35 real-CLI coverage for the compiled build stamp and stale guard.
//!
//! The launched binary is the exact executable Cargo built for this integration
//! target, including when `CARGO_TARGET_DIR` isolates the build.
//!
//! B287 keeps the stale-command classification checks independent from the
//! repository's live round state. An unusable round must still reject
//! `snapshot`, but that product-level rejection is distinct from the stale
//! binary guard allowing the read-only command to reach round validation.

mod stale_snapshot_local_fixture_support;
mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use stale_snapshot_local_fixture_support::prepare_local_valid_materialized_round;

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

fn commit_tracked_scratch_state(repo: &mut StaleRepo, message: &str) {
    git_ok(&repo.root, &["add", "-u"]);
    let tree = git_stdout(&repo.root, &["write-tree"]);
    let commit = support::fixture_git_command(&repo.root)
        .args(["commit-tree", &tree, "-p", &repo.main_sha, "-m", message])
        .env("GIT_AUTHOR_NAME", "B287 Test")
        .env("GIT_AUTHOR_EMAIL", "b287@example.invalid")
        .env("GIT_COMMITTER_NAME", "B287 Test")
        .env("GIT_COMMITTER_EMAIL", "b287@example.invalid")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("启动 scratch git commit-tree 失败");
    assert!(
        commit.status.success(),
        "scratch git commit-tree 失败: {}",
        String::from_utf8_lossy(&commit.stderr)
    );
    repo.main_sha = String::from_utf8(commit.stdout)
        .expect("scratch commit-tree stdout 非 UTF-8")
        .trim()
        .to_owned();
    git_ok(
        &repo.root,
        &["update-ref", "refs/heads/main", &repo.main_sha],
    );
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

fn prepare_stale_snapshot_probe(
    name: &str,
    committed_build_registry_drift: bool,
) -> Option<(StaleRepo, String)> {
    let mut repo = stale_repo_fixture(name, true)?;
    if committed_build_registry_drift {
        drift_agent_registry(&repo, "# B287 committed build-tree registry drift\n");
        commit_tracked_scratch_state(&mut repo, "B287 committed registry drift fixture");
    }
    let current_round = prepare_local_valid_materialized_round(&repo.root)
        .expect("materialized build tree 必须能在 scratch 内准备有效轮态");
    assert_ne!(current_round, "r58", "fixture 不得退回已关闭的历史轮");
    inject_hostile_inflight_round_state(&repo, &current_round);
    repair_materialized_round_contract(&repo, &current_round);
    Some((repo, current_round))
}

fn inject_hostile_inflight_round_state(repo: &StaleRepo, round: &str) {
    let events_path = repo
        .root
        .join("coordination/rounds")
        .join(round)
        .join("events.jsonl");
    let mut events = fs::read_to_string(&events_path).expect("读取 scratch round ledger 失败");
    if !events.ends_with('\n') {
        events.push('\n');
    }
    let probe = serde_json::json!({
        "eventId": "01M0ZZZZZZZZZZZZZZZZZZZZZZ",
        "ts": "2026-08-20T00:00:00Z",
        "actor": "runtime:orch",
        "type": "DispatchIssued",
        "taskId": "B287-PROBE",
        "round": round,
        "payload": {
            "agent": "executor-desktop",
            "attemptId": "B287-PROBE-A0001",
            "attemptNo": 1,
            "goPath": format!(
                "coordination/rounds/{round}/dispatch/executor-desktop/GO-B287-PROBE-A0001.md"
            ),
            "baseSha": repo.main_sha.as_str(),
        }
    });
    events.push_str(&serde_json::to_string(&probe).expect("序列化 hostile inflight probe 失败"));
    events.push('\n');
    fs::write(&events_path, events).expect("注入 hostile inflight scratch event 失败");
}

fn remove_round_events(repo: &StaleRepo, round: &str, event_types: &[&str]) -> usize {
    let events_path = repo
        .root
        .join("coordination/rounds")
        .join(round)
        .join("events.jsonl");
    let events = fs::read_to_string(&events_path).expect("读取 scratch round ledger 失败");
    let mut removed = 0usize;
    let kept = events
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter(|line| {
            let event: serde_json::Value =
                serde_json::from_str(line).expect("scratch round ledger 必须是合法 JSONL");
            let should_remove = event
                .get("type")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|event_type| event_types.contains(&event_type));
            removed += usize::from(should_remove);
            !should_remove
        })
        .collect::<Vec<_>>();
    fs::write(&events_path, format!("{}\n", kept.join("\n")))
        .expect("写回 scratch round ledger 失败");
    removed
}

fn remove_round_event(repo: &StaleRepo, round: &str, event_type: &str) {
    assert!(
        remove_round_events(repo, round, &[event_type]) > 0,
        "scratch round 必须至少有一条 {event_type}"
    );
}

fn reset_round_for_contract_repair(repo: &StaleRepo, round: &str) {
    let events_path = repo
        .root
        .join("coordination/rounds")
        .join(round)
        .join("events.jsonl");
    let events = fs::read_to_string(&events_path).expect("读取 scratch round ledger 失败");
    let mut removed_task_scoped = 0usize;
    let kept = events
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter(|line| {
            let event: serde_json::Value =
                serde_json::from_str(line).expect("scratch round ledger 必须是合法 JSONL");
            let task_scoped = event
                .get("taskId")
                .and_then(serde_json::Value::as_str)
                .is_some();
            removed_task_scoped += usize::from(task_scoped);
            let inherited_authorization = matches!(
                event.get("type").and_then(serde_json::Value::as_str),
                Some(
                    "TaskValidated"
                        | "PlanSignedOff"
                        | "AgentPinAmended"
                        | "RuntimePolicyActivated"
                        | "RuntimePolicyDeactivated"
                )
            );
            !task_scoped && !inherited_authorization
        })
        .collect::<Vec<_>>();
    fs::write(&events_path, format!("{}\n", kept.join("\n")))
        .expect("写回 scratch contract-repair ledger 失败");
    assert!(
        removed_task_scoped > 0,
        "materialized round 必须带至少一条 hostile task-scoped runtime event"
    );
    let repaired = fs::read_to_string(&events_path).expect("重读 scratch repair ledger 失败");
    assert!(
        repaired
            .lines()
            .filter(|line| !line.trim().is_empty())
            .all(|line| {
                let event = serde_json::from_str::<serde_json::Value>(line)
                    .expect("repair ledger 必须保持合法 JSONL");
                event.get("taskId").is_none()
                    && !matches!(
                        event.get("type").and_then(serde_json::Value::as_str),
                        Some(
                            "TaskValidated"
                                | "PlanSignedOff"
                                | "AgentPinAmended"
                                | "RuntimePolicyActivated"
                                | "RuntimePolicyDeactivated"
                        )
                    )
            }),
        "contract repair 前置必须清空 inherited task lifecycle 与 round-scoped authorization"
    );
}

fn drift_agent_registry(repo: &StaleRepo, marker: &str) {
    let registry_path = repo.root.join("coordination/agents.yaml");
    let mut registry =
        fs::read_to_string(&registry_path).expect("读取 scratch agent registry 失败");
    registry.push('\n');
    registry.push_str(marker);
    fs::write(&registry_path, registry).expect("写 scratch agent registry drift 失败");
}

fn repair_materialized_round_contract(repo: &StaleRepo, round: &str) {
    // A materialized template may itself have been built in T1 or T2. Drop
    // inherited authorization/delta facts, then let the production planner
    // bind the exact scratch bytes and sign that freshly validated revision.
    // This makes the control state local to the fixture instead of assuming
    // the build commit happened to carry a usable live round.
    reset_round_for_contract_repair(repo, round);

    let planned = run_orch(&repo.root, &["--allow-stale-binary", "plan"]);
    assert!(
        planned.status.success(),
        "B287 scratch plan 必须修复继承轮态；stdout={}; stderr={}",
        String::from_utf8_lossy(&planned.stdout),
        String::from_utf8_lossy(&planned.stderr)
    );
    let signed = run_orch(
        &repo.root,
        &[
            "--allow-stale-binary",
            "round",
            "sign-off",
            "--note",
            "B287 scratch self-consistent round",
        ],
    );
    assert!(
        signed.status.success(),
        "B287 scratch sign-off 必须绑定新鲜 IR；stdout={}; stderr={}",
        String::from_utf8_lossy(&signed.stdout),
        String::from_utf8_lossy(&signed.stderr)
    );

    let control = run_orch(&repo.root, &["snapshot"]);
    assert_stale_read_allowed(repo, &control, "snapshot self-consistent control");
}

fn assert_round_independent_stale_read(repo: &StaleRepo, scenario: &str) {
    let schema = run_orch(&repo.root, &["schema"]);
    assert_stale_read_allowed(repo, &schema, &format!("schema ({scenario})"));
}

fn assert_snapshot_reaches_round_validation(
    repo: &StaleRepo,
    scenario: &str,
    expected_round_error: &[&str],
) {
    let snapshot = run_orch(&repo.root, &["snapshot"]);
    let stderr = String::from_utf8_lossy(&snapshot.stderr);
    assert!(
        !snapshot.status.success(),
        "{scenario}: 不可用轮态下 snapshot 必须 fail-closed"
    );
    assert!(
        stderr.contains("read-only command is allowed")
            && stderr.contains(&repo.build_sha)
            && stderr.contains(&repo.main_sha),
        "{scenario}: 陈旧 guard 必须先把只读 snapshot 放行到轮态校验: {stderr}"
    );
    assert!(
        !stderr.contains("stale binary refused state-changing command"),
        "{scenario}: 只读 snapshot 不得被误分成写命令: {stderr}"
    );
    assert!(
        expected_round_error
            .iter()
            .any(|expected| stderr.contains(expected)),
        "{scenario}: snapshot 必须因轮态本身拒绝，而非其他原因: {stderr}"
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
    let Some((repo, current_round)) = prepare_stale_snapshot_probe("snapshot-unsigned", true)
    else {
        eprintln!("skip: build outside git has no ORCH_BUILD_GIT_SHA");
        return;
    };
    remove_round_event(&repo, &current_round, "PlanSignedOff");

    // T1: a stale read-only command remains usable while snapshot itself keeps
    // rejecting an unsigned round. The two decisions must not be conflated.
    assert_round_independent_stale_read(&repo, "unsigned round");
    assert_snapshot_reaches_round_validation(&repo, "unsigned round", &["尚未绑定 PlanSignedOff"]);

    let snapshot_path = repo.root.join("coordination/runtime/snapshot.json");
    let denied = run_orch(&repo.root, &["snapshot", "--write"]);
    assert_stale_write_rejected(&repo, &denied, "snapshot --write");
    assert!(!snapshot_path.exists(), "拒绝必须发生在 snapshot 写盘之前");

    let overridden = run_orch(&repo.root, &["--allow-stale-binary", "snapshot", "--write"]);
    let overridden_stderr = String::from_utf8_lossy(&overridden.stderr);
    assert!(
        !overridden.status.success()
            && overridden_stderr.contains("--allow-stale-binary")
            && overridden_stderr.contains("尚未绑定 PlanSignedOff"),
        "逃生舱只绕过 stale guard，不得绕过 unsigned round 校验: {overridden_stderr}"
    );
    assert!(!snapshot_path.exists(), "轮态校验失败时不得写 snapshot");
}

#[test]
fn stale_snapshot_stays_read_only_during_registry_drift() {
    let Some((mut repo, _current_round)) =
        prepare_stale_snapshot_probe("snapshot-registry-drift", false)
    else {
        eprintln!("skip: build outside git has no ORCH_BUILD_GIT_SHA");
        return;
    };
    drift_agent_registry(&repo, "# B287 committed T2 registry drift probe\n");
    commit_tracked_scratch_state(&mut repo, "B287 committed T2 registry drift");

    // T2: registry bytes have moved beyond the signed IR. This used to make
    // the cloned repository's live state decide whether the stale test passed.
    assert_round_independent_stale_read(&repo, "registry drift");
    assert_snapshot_reaches_round_validation(
        &repo,
        "registry drift",
        &[
            "ROUND-IR 与 mode/binding/cards 漂移",
            "agent registry",
            "agentRegistryDigest",
        ],
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
