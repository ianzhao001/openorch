#![cfg(feature = "selfhost")]
//! B110 CLI 集成测试：`orch handshake` 消费本次 wake 返回的精确 logPath 窗口。
//!
//! - 咬合（窗口内 provider 活动）→ exit 0；
//! - spawn 成功但无 provider 活动 → 轮询至超时 exit 3（Delivered ≠ Engaged）；
//! - --no-wake / POKE 备用道 → 显式 Pending 立即 exit 3，绝不读 legacy 文件。
//!
//! 不引入新依赖：直接启动 Cargo 为本次 integration target 构建的 `orch`；
//! scratch 落在本 worktree `orch/target/test-tmp`；fake provider 用 /bin/echo，
//! 绝不调用真实模型 CLI。

mod support_legacy_plan;

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
    support_legacy_plan::materialize(root).unwrap();
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

fn assert_legacy_handshake_is_read_only(name: &str, injectable: bool, args: &[&str]) {
    let root = temp_root(name);
    setup_round(&root, "r47");
    write_agent(
        &root,
        AGENT,
        injectable,
        &["/bin/echo", "{\"type\":\"turn.started\"}"],
    );
    let ledger_path = root.join("coordination/rounds/r47/events.jsonl");
    let before = fs::read(&ledger_path).unwrap();
    let output = run_orch(&root, args);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("unrecognized subcommand")
    );
    assert_eq!(fs::read(&ledger_path).unwrap(), before);
    assert!(attempt_logs(&root, AGENT).is_empty());
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn retired_handshake_rejects_even_with_engaged_input() {
    assert_legacy_handshake_is_read_only("engaged", true, &["handshake", AGENT, "--timeout-secs", "10"]);
}

#[test]
fn retired_handshake_rejects_delayed_input() {
    assert_legacy_handshake_is_read_only("delayed-engaged", true, &["handshake", AGENT, "--timeout-secs", "6"]);
}

#[test]
fn retired_handshake_rejects_missing_engagement() {
    assert_legacy_handshake_is_read_only("no-mark", true, &["handshake", AGENT, "--timeout-secs", "2"]);
}

#[test]
fn retired_handshake_rejects_no_wake() {
    assert_legacy_handshake_is_read_only("no-wake", true, &["handshake", AGENT, "--no-wake", "--timeout-secs", "30"]);
}

#[test]
fn retired_handshake_rejects_poke_fallback() {
    assert_legacy_handshake_is_read_only("poke", false, &["handshake", AGENT, "--timeout-secs", "30"]);
}

#[test]
fn retired_nudge_cli_is_rejected_before_any_side_effect() {
    let root = temp_root("nudge-task-required");
    setup_round(&root, "r47");
    write_agent(&root, AGENT, true, &["/bin/echo", "turn.started"]);

    let output = run_orch(&root, &["nudge", AGENT, "--no-wake"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unrecognized subcommand"),
        "stderr 须说明命令已不可解析: {stderr}"
    );
    assert!(!root
        .join(format!("coordination/runtime/POKE-{AGENT}.txt"))
        .exists());
    assert!(attempt_logs(&root, AGENT).is_empty());
    fs::remove_dir_all(&root).unwrap();
}
