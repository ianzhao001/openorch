//! B110 CLI 集成测试：`orch handshake` 消费本次 wake 返回的精确 logPath 窗口。
//!
//! - 咬合（窗口内 provider 活动）→ exit 0；
//! - spawn 成功但无 provider 活动 → 轮询至超时 exit 3（Delivered ≠ Engaged）；
//! - --no-wake / POKE 备用道 → 显式 Pending 立即 exit 3，绝不读 legacy 文件。
//!
//! 不引入新依赖：直接启动 Cargo 为本次 integration target 构建的 `orch`；
//! scratch 落在本 worktree `orch/target/test-tmp`；fake provider 用 /bin/echo，
//! 绝不调用真实模型 CLI。

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const AGENT: &str = "executor-opencode";

fn fixture_orch_command(extras: &[(&str, &str)]) -> Command {
    let mut command = Command::new(support::orch_bin());
    support::configure_fixture_git_env(&mut command, extras);
    command
}

fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{}-{seq}", std::process::id())
}

fn temp_root(name: &str) -> PathBuf {
    let orch_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR 应形如 <worktree>/orch/crates/orch-cli");
    let dir = orch_dir
        .join("target")
        .join("test-tmp")
        .join(format!("b110-handshake-{name}-{}", unique_suffix()));
    fs::create_dir_all(&dir).expect("创建测试 scratch 目录失败");
    dir
}

fn setup_round(root: &Path, round: &str) {
    for path in [
        "coordination/runtime",
        "coordination/modes",
        "coordination/rounds/r47/tasks",
    ] {
        fs::create_dir_all(root.join(path)).unwrap();
    }
    fs::write(
        root.join("coordination/runtime/CURRENT-ROUND"),
        format!("{round}\n"),
    )
    .unwrap();
    fs::write(
        root.join("coordination/modes/test.yaml"),
        r#"agents:
  executor: {adapter: test, tier: none}
  verifier: {adapter: root-manual, tier: none}
hitl: {mergeGate: auto}
verification: {mode: root-manual-fixed-head}
liveness: {monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}
scheduling:
  allowedAgents: [executor-desktop, executor-claw, executor-opencode]
  capacities:
    executor-desktop: {agent: 2, quota: 2, roles: [implement]}
    executor-claw: {agent: 1, quota: 1, roles: [primary-review]}
    executor-opencode: {agent: 3, quota: 3, roles: [implement, secondary-review]}
budgets: {round: {wallMinutes: 60, maxModelWakes: 20}}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
"#,
    )
    .unwrap();
    fs::write(
        root.join("coordination/PROJECT-BINDING.yaml"),
        "scope: {protectedPaths: [\"coordination/**\"]}\ngit: {pushPolicy: forbidden}\ncommands:\n  testFast: {argv: [\"sh\", \"-c\", \"exit 0\"], timeoutSeconds: 30}\n",
    )
    .unwrap();
    fs::write(
        root.join(format!("coordination/rounds/{round}/tasks/BCLI.md")),
        format!("---\ntaskId: BCLI\nround: {round}\nagent: executor-opencode\nseedProtocol: pure-spec\nwriteSet: [fixture.txt]\nfrozenPaths: [coordination/**]\ngates: {{fast: [testFast]}}\nbudgets: {{wallMinutes: 10}}\nrequiredReviews:\n  - {{role: primary, agent: executor-claw}}\nrequiredEvidence: [cli]\n---\nfixture\n"),
    )
    .unwrap();
    orch_host::plan::run_plan(root).unwrap();
    orch_host::round::run_sign_off(root, Some("CLI fixture")).unwrap();
}

fn write_agent(root: &Path, agent: &str, injectable: bool, argv: &[&str]) {
    fs::create_dir_all(root.join("coordination")).unwrap();
    let value = serde_json::json!({
        "agents": {
            agent: {
                "injectable": injectable,
                "sessionId": if injectable { "fake-session-1" } else { "" },
                "wake": {"argv": argv},
                "pokeHint": "请处理 {round}"
            }
        }
    });
    fs::write(
        root.join("coordination/agents.yaml"),
        serde_json::to_string_pretty(&value).unwrap(),
    )
    .unwrap();
}

fn run_orch(root: &Path, args: &[&str]) -> std::process::Output {
    fixture_orch_command(&[])
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("启动 orch 失败")
}

/// 枚举 runtime/logs 下本次注入的 per-attempt 日志（wake-<agent>-*.jsonl）。
fn attempt_logs(root: &Path, agent: &str) -> Vec<PathBuf> {
    let dir = root.join("coordination/runtime/logs");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(&format!("wake-{agent}-")) && n.ends_with(".jsonl"))
                .unwrap_or(false)
        })
        .collect()
}

#[test]
fn handshake_engaged_succeeds_via_exact_log_path() {
    let root = temp_root("engaged");
    setup_round(&root, "r47");
    write_agent(
        &root,
        AGENT,
        true,
        &["/bin/echo", "{\"type\":\"turn.started\"}"],
    );

    let output = run_orch(&root, &["handshake", AGENT, "--timeout-secs", "10"]);
    assert!(
        output.status.success(),
        "咬合应成功: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("握手成功"), "stdout: {stdout}");

    // 本次注入的 per-attempt 日志存在且含咬合标记（handshake 判的正是它）。
    let logs = attempt_logs(&root, AGENT);
    assert_eq!(logs.len(), 1, "应恰好一份 per-attempt 日志");
    let content = fs::read_to_string(&logs[0]).unwrap();
    assert!(content.contains("turn.started"));
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn handshake_captures_a_larger_end_on_the_next_observation() {
    // T5 CLI production path：首轮 capture 时 provider 尚未写 marker；下一轮重新
    // capture 更大 end 后成功。若把 spawn 时水位永久冻结，本测试会超时。
    let root = temp_root("delayed-engaged");
    setup_round(&root, "r47");
    write_agent(
        &root,
        AGENT,
        true,
        &[
            "/bin/sh",
            "-c",
            "sleep 1; printf '{\"type\":\"turn.started\"}\\n'",
        ],
    );

    let output = run_orch(&root, &["handshake", AGENT, "--timeout-secs", "6"]);
    assert!(
        output.status.success(),
        "延迟 marker 应在后续 observation 咬合: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("握手成功"));
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn handshake_spawn_without_engagement_times_out() {
    let root = temp_root("no-mark");
    setup_round(&root, "r47");
    write_agent(
        &root,
        AGENT,
        true,
        &["/bin/echo", "spawn ok but no provider mark"],
    );

    let output = run_orch(&root, &["handshake", AGENT, "--timeout-secs", "2"]);
    assert_eq!(output.status.code(), Some(3), "未咬合应 exit 3");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("握手失败"), "stdout: {stdout}");
    // spawn 成功（日志有输出）≠ 已咬合：日志里只有 echo 噪声。
    let logs = attempt_logs(&root, AGENT);
    assert_eq!(logs.len(), 1);
    let content = fs::read_to_string(&logs[0]).unwrap();
    assert!(content.contains("no provider mark"));
    assert!(!content.contains("turn.started"));
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn handshake_no_wake_is_explicitly_pending() {
    let root = temp_root("no-wake");
    setup_round(&root, "r47");
    write_agent(
        &root,
        AGENT,
        true,
        &["/bin/echo", "{\"type\":\"turn.started\"}"],
    );

    let output = run_orch(
        &root,
        &["handshake", AGENT, "--no-wake", "--timeout-secs", "30"],
    );
    assert_eq!(output.status.code(), Some(3), "无注入应显式 Pending exit 3");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("待投递") && stdout.contains("pending"),
        "stdout 须明示 Pending: {stdout}"
    );
    // --no-wake 不注入：不得产生 per-attempt 日志，也不得有 legacy 探活文件可读。
    assert!(
        attempt_logs(&root, AGENT).is_empty(),
        "--no-wake 不得产生注入日志"
    );
    assert!(!root
        .join(format!("coordination/runtime/logs/wake-{AGENT}.log"))
        .exists());
    assert!(
        root.join(format!("coordination/runtime/POKE-{AGENT}.txt"))
            .exists(),
        "POKE 备用信标必须落盘"
    );
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn handshake_poke_fallback_is_explicitly_pending() {
    let root = temp_root("poke");
    setup_round(&root, "r47");
    write_agent(&root, AGENT, false, &[]);

    let output = run_orch(&root, &["handshake", AGENT, "--timeout-secs", "30"]);
    assert_eq!(output.status.code(), Some(3));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("待投递") && stdout.contains("pending"),
        "POKE 备用道须明示 Pending: {stdout}"
    );
    assert!(attempt_logs(&root, AGENT).is_empty());
    assert!(root
        .join(format!("coordination/runtime/POKE-{AGENT}.txt"))
        .exists());
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn nudge_cli_requires_explicit_task_before_any_side_effect() {
    let root = temp_root("nudge-task-required");
    setup_round(&root, "r47");
    write_agent(&root, AGENT, true, &["/bin/echo", "turn.started"]);

    let output = run_orch(&root, &["nudge", AGENT, "--no-wake"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--task"),
        "stderr 须说明缺 --task: {stderr}"
    );
    assert!(!root
        .join(format!("coordination/runtime/POKE-{AGENT}.txt"))
        .exists());
    assert!(attempt_logs(&root, AGENT).is_empty());
    fs::remove_dir_all(&root).unwrap();
}
