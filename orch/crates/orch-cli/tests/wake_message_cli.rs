//! B109 CLI 集成测试：`orch wake`/`orch nudge` 富输入三来源、互斥冲突、
//! UTF-8 字节数、默认提示、非零退出。
//!
//! 不引入新依赖：直接用 `std::process::Command` 启动 Cargo 为本次
//! integration target 构建的 `orch`；临时根目录落在本 worktree
//! `orch/target/test-tmp`（遵守协议 scratch 约束，不污染工作树）。
//!
//! fake agent 的 wake.argv 设为 `["/bin/echo", "{message}"]`：/bin/echo 立即退出，
//! spawn 成功后 WakeIssued/NudgeIssued 仍被落账（payload 携带 messageSource/messageBytes），
//! 绝不调用真实模型 CLI。

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;

const AGENT: &str = "executor-opencode";

fn fixture_orch_command(extras: &[(&str, &str)]) -> Command {
    let mut command = Command::new(support::orch_bin());
    support::configure_fixture_git_env(&mut command, extras);
    command
}

/// 临时根目录：`<worktree>/orch/target/test-tmp/b109-cli-<唯一>`。
fn temp_root(name: &str) -> PathBuf {
    let orch_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR 应形如 <worktree>/orch/crates/orch-cli");
    let dir = orch_dir
        .join("target")
        .join("test-tmp")
        .join(format!("b109-cli-{name}-{}", unique_suffix()));
    fs::create_dir_all(&dir).expect("创建测试 scratch 目录失败");
    dir
}

/// 进程内唯一后缀：`{pid}-{seq}`。
/// 唯一性 = 进程号 + 模块级 `AtomicU64.fetch_add` 单调序号（B108 契约先例，
/// 见 orch-host util::unique_scratch_name）；不读时钟——毫秒时间戳+pid 在同
/// 进程并发/同毫秒场景会撞名（attempt 1 假红根因之一）。
fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{}-{seq}", std::process::id())
}

fn setup_round(root: &Path, round: &str) {
    for path in [
        "coordination/runtime",
        "coordination/modes",
        "coordination/rounds/r1/tasks",
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
        root.join(format!("coordination/rounds/{round}/tasks/B109.md")),
        format!("---\ntaskId: B109\nround: {round}\nagent: executor-opencode\nseedProtocol: pure-spec\nwriteSet: [fixture.txt]\nfrozenPaths: [coordination/**]\ngates: {{fast: [testFast]}}\nbudgets: {{wallMinutes: 10}}\nrequiredReviews:\n  - {{role: primary, agent: executor-claw}}\nrequiredEvidence: [cli]\n---\nfixture\n"),
    )
    .unwrap();
    orch_host::plan::run_plan(root).unwrap();
    orch_host::round::run_sign_off(root, Some("CLI fixture")).unwrap();
}

/// B110：nudge 必须绑定显式 task 的真实 current attempt。测试夹具写入一条
/// 完整 modern DispatchIssued，避免恢复 B109 时代“按 agent 猜最近任务”的旧语义。
fn setup_current_dispatch(root: &Path, round: &str, task: &str, agent: &str) {
    let event = orch_host::ledger::event(
        "DispatchIssued",
        "runtime:orch",
        Some(task),
        Some(round),
        json!({
            "agent": agent,
            "attemptId": format!("{task}-A0001"),
            "attemptNo": 1,
            "baseSha": "test-base-sha",
            "goPath": format!(
                "coordination/rounds/{round}/dispatch/{agent}/GO-{task}-A0001.md"
            ),
            "method": "test"
        }),
    );
    orch_host::ledger::append(root, round, &[event]).unwrap();
}

/// 写一个 fake injectable agent，wake.argv 用 /bin/echo 立即退出。
fn write_fake_agent(root: &Path, agent: &str) {
    fs::create_dir_all(root.join("coordination")).unwrap();
    let value = json!({
        "agents": {
            agent: {
                "injectable": true,
                "sessionId": "fake-session-1",
                "wake": {
                    "argv": ["/bin/echo", "{message}"]
                },
                "pokeHint": ""
            }
        }
    });
    fs::write(
        root.join("coordination/agents.yaml"),
        serde_json::to_string_pretty(&value).unwrap(),
    )
    .unwrap();
}

/// 读 NudgeIssued/WakeIssued 的 payload（账本最后一行匹配事件类型）。
fn ledger_events(root: &Path, round: &str) -> Vec<serde_json::Value> {
    let text = fs::read_to_string(root.join(format!("coordination/rounds/{round}/events.jsonl")))
        .expect("读 events.jsonl 失败");
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("事件行非合法 JSON"))
        .collect()
}

/// 取账本中指定事件类型的最后一条 payload。
/// 注意：EventRecord 序列化后事件类型字段名是 `type`（`kind` 经 serde rename），
/// 读 `kind` 永远落空（attempt 1 假红根因之一）。
fn last_payload<'a>(events: &'a [serde_json::Value], kind: &str) -> Option<&'a serde_json::Value> {
    events
        .iter()
        .rev()
        .find(|ev| ev.get("type").and_then(|k| k.as_str()) == Some(kind))
        .and_then(|ev| ev.get("payload"))
}

fn run_orch(root: &Path, args: &[&str]) -> std::process::Output {
    fixture_orch_command(&[])
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("启动 orch 失败")
}

fn run_orch_stdin(root: &Path, args: &[&str], stdin: &str) -> std::process::Output {
    use std::io::Write;
    let mut child = fixture_orch_command(&[])
        .arg("--root")
        .arg(root)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("启动 orch 失败");
    if let Some(mut child_stdin) = child.stdin.take() {
        child_stdin
            .write_all(stdin.as_bytes())
            .expect("写 stdin 失败");
    }
    child.wait_with_output().expect("等待 orch 退出失败")
}

/// M1/CLI 显式文本来源：--message foo → messageSource=explicit + 正确字节数。
#[test]
fn cli_wake_explicit_message_records_source_and_bytes() {
    let root = temp_root("wake-explicit");
    setup_round(&root, "r1");
    write_fake_agent(&root, AGENT);

    let out = run_orch(&root, &["wake", AGENT, "--message", "hello"]);
    assert!(
        out.status.success(),
        "wake 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let events = ledger_events(&root, "r1");
    let payload = last_payload(&events, "WakeIssued").expect("缺 WakeIssued");
    assert_eq!(
        payload.get("messageSource").and_then(|v| v.as_str()),
        Some("explicit"),
        "显式来源应落 explicit: {payload}"
    );
    assert_eq!(
        payload.get("messageBytes").and_then(|v| v.as_u64()),
        Some(5),
        "hello = 5 字节: {payload}"
    );
    assert_eq!(payload.get("agent").and_then(|v| v.as_str()), Some(AGENT));
}

/// M1/CLI 文件来源：--message-file <path> → messageSource=file。
#[test]
fn cli_wake_file_message_records_file_source() {
    let root = temp_root("wake-file");
    setup_round(&root, "r1");
    write_fake_agent(&root, AGENT);
    let msg_file = root.join("msg.txt");
    fs::write(&msg_file, "from file 中文").unwrap();

    let out = run_orch(
        &root,
        &["wake", AGENT, "--message-file", msg_file.to_str().unwrap()],
    );
    assert!(
        out.status.success(),
        "wake 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let events = ledger_events(&root, "r1");
    let payload = last_payload(&events, "WakeIssued").expect("缺 WakeIssued");
    assert_eq!(
        payload.get("messageSource").and_then(|v| v.as_str()),
        Some("file"),
        "文件来源应落 file: {payload}"
    );
    assert_eq!(
        payload.get("messageBytes").and_then(|v| v.as_u64()),
        Some("from file 中文".as_bytes().len() as u64),
        "UTF-8 字节数（非字符数）: {payload}"
    );
}

/// M1/CLI stdin 来源：--message - → messageSource=stdin。
#[test]
fn cli_wake_stdin_message_records_stdin_source() {
    let root = temp_root("wake-stdin");
    setup_round(&root, "r1");
    write_fake_agent(&root, AGENT);

    let out = run_orch_stdin(
        &root,
        &["wake", AGENT, "--message", "-"],
        "piped from stdin",
    );
    assert!(
        out.status.success(),
        "wake 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let events = ledger_events(&root, "r1");
    let payload = last_payload(&events, "WakeIssued").expect("缺 WakeIssued");
    assert_eq!(
        payload.get("messageSource").and_then(|v| v.as_str()),
        Some("stdin"),
        "stdin 来源应落 stdin: {payload}"
    );
    assert_eq!(
        payload.get("messageBytes").and_then(|v| v.as_u64()),
        Some("piped from stdin".as_bytes().len() as u64),
        "stdin 内容字节数: {payload}"
    );
}

#[test]
fn cli_wake_stdin_rechecks_active_round_after_blocking_read() {
    use std::io::Write;
    use std::time::Duration;

    let root = temp_root("wake-stdin-round-drift");
    setup_round(&root, "r1");
    let sentinel = root.join("WAKE-SPAWNED");
    let registry = json!({
        "agents": {
            AGENT: {
                "injectable": true,
                "sessionId": "fake-session-1",
                "wake": {"argv": ["/bin/sh", "-c", format!("touch {}", sentinel.display())]},
                "pokeHint": ""
            }
        }
    });
    fs::write(
        root.join("coordination/agents.yaml"),
        serde_json::to_vec_pretty(&registry).unwrap(),
    )
    .unwrap();
    let ledger = root.join("coordination/rounds/r1/events.jsonl");
    let before = fs::read(&ledger).unwrap();

    let mut child = fixture_orch_command(&[])
        .arg("--root")
        .arg(&root)
        .args(["wake", AGENT, "--message", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    fs::create_dir_all(root.join("coordination/rounds/r2")).unwrap();
    fs::write(root.join("coordination/rounds/r2/events.jsonl"), "").unwrap();
    fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r2\n").unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"release blocked stdin")
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(!output.status.success());
    assert!(!sentinel.exists());
    assert_eq!(fs::read(&ledger).unwrap(), before);
    assert!(last_payload(&ledger_events(&root, "r2"), "WakeIssued").is_none());
}

/// M1/CLI 默认来源：省略所有 --message* → messageSource=default + stdout 提示。
#[test]
fn cli_wake_default_message_records_default_source_and_announces() {
    let root = temp_root("wake-default");
    setup_round(&root, "r1");
    write_fake_agent(&root, AGENT);

    let out = run_orch(&root, &["wake", AGENT]);
    assert!(
        out.status.success(),
        "wake 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("default"),
        "stdout 应明示 default 来源: {stdout}"
    );
    let events = ledger_events(&root, "r1");
    let payload = last_payload(&events, "WakeIssued").expect("缺 WakeIssued");
    assert_eq!(
        payload.get("messageSource").and_then(|v| v.as_str()),
        Some("default"),
        "默认来源应落 default: {payload}"
    );
    // 默认文本 "orch: 有新信号,请检查你的 dispatch 信道并按协议处理" 字节数
    let default = "orch: 有新信号,请检查你的 dispatch 信道并按协议处理";
    assert_eq!(
        payload.get("messageBytes").and_then(|v| v.as_u64()),
        Some(default.as_bytes().len() as u64),
        "默认文本字节数: {payload}"
    );
}

/// M2/CLI 来源互斥冲突：--message foo --message-file bar → 非零退出（不落账）。
#[test]
fn cli_wake_conflicting_sources_exit_nonzero() {
    let root = temp_root("wake-conflict");
    setup_round(&root, "r1");
    write_fake_agent(&root, AGENT);
    let msg_file = root.join("msg.txt");
    fs::write(&msg_file, "file").unwrap();

    let out = run_orch(
        &root,
        &[
            "wake",
            AGENT,
            "--message",
            "inline",
            "--message-file",
            msg_file.to_str().unwrap(),
        ],
    );
    assert!(
        !out.status.success(),
        "来源冲突应非零退出，实际 status: {:?}，stdout: {}，stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // 冲突在解析期捕获：不应写任何 WakeIssued
    let events = ledger_events(&root, "r1");
    assert!(
        last_payload(&events, "WakeIssued").is_none(),
        "冲突应不落账 WakeIssued，但找到: {:?}",
        last_payload(&events, "WakeIssued")
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("互斥"), "stderr 应说明互斥冲突: {stderr}");
}

/// M3/CLI UTF-8 字节数：--message 中文 → bytes=6（非 char 数 2）。
#[test]
fn cli_wake_utf8_message_uses_byte_length() {
    let root = temp_root("wake-utf8");
    setup_round(&root, "r1");
    write_fake_agent(&root, AGENT);

    let out = run_orch(&root, &["wake", AGENT, "--message", "中文"]);
    assert!(
        out.status.success(),
        "wake 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let events = ledger_events(&root, "r1");
    let payload = last_payload(&events, "WakeIssued").expect("缺 WakeIssued");
    assert_eq!(
        payload.get("messageBytes").and_then(|v| v.as_u64()),
        Some(6),
        "'中文' = 6 UTF-8 字节（非 2 char）: {payload}"
    );
    assert_eq!(
        payload.get("messageSource").and_then(|v| v.as_str()),
        Some("explicit")
    );
}

/// 非法 UTF-8 文件 → 非零退出（不落账）。
#[test]
fn cli_wake_invalid_utf8_file_exits_nonzero() {
    let root = temp_root("wake-bad-utf8");
    setup_round(&root, "r1");
    write_fake_agent(&root, AGENT);
    let msg_file = root.join("bad.txt");
    // 非法 UTF-8 序列（0xFF 不是合法起始字节）
    fs::write(&msg_file, b"\xFF\xFE\x00bad").unwrap();

    let out = run_orch(
        &root,
        &["wake", AGENT, "--message-file", msg_file.to_str().unwrap()],
    );
    assert!(
        !out.status.success(),
        "非法 UTF-8 应非零退出，实际 status: {:?}",
        out.status.code()
    );
    let events = ledger_events(&root, "r1");
    assert!(
        last_payload(&events, "WakeIssued").is_none(),
        "非法 UTF-8 不应落账 WakeIssued"
    );
}

/// 读文件失败（路径不存在）→ 非零退出。
#[test]
fn cli_wake_missing_file_exits_nonzero() {
    let root = temp_root("wake-missing-file");
    setup_round(&root, "r1");
    write_fake_agent(&root, AGENT);
    let missing = root.join("does-not-exist.txt");

    let out = run_orch(
        &root,
        &["wake", AGENT, "--message-file", missing.to_str().unwrap()],
    );
    assert!(
        !out.status.success(),
        "文件不存在应非零退出，实际 status: {:?}",
        out.status.code()
    );
    let events = ledger_events(&root, "r1");
    assert!(last_payload(&events, "WakeIssued").is_none());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--message-file"),
        "stderr 应指向 --message-file 失败原因: {stderr}"
    );
}

/// 空文件是显式空消息：messageSource=file、messageBytes=0（不回退 default）。
#[test]
fn cli_wake_empty_file_is_explicit_empty_not_default() {
    let root = temp_root("wake-empty-file");
    setup_round(&root, "r1");
    write_fake_agent(&root, AGENT);
    let msg_file = root.join("empty.txt");
    fs::write(&msg_file, "").unwrap();

    let out = run_orch(
        &root,
        &["wake", AGENT, "--message-file", msg_file.to_str().unwrap()],
    );
    assert!(
        out.status.success(),
        "wake 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let events = ledger_events(&root, "r1");
    let payload = last_payload(&events, "WakeIssued").expect("缺 WakeIssued");
    assert_eq!(
        payload.get("messageSource").and_then(|v| v.as_str()),
        Some("file"),
        "空文件是显式空消息（file），不回退 default: {payload}"
    );
    assert_eq!(
        payload.get("messageBytes").and_then(|v| v.as_u64()),
        Some(0),
        "空文件 messageBytes=0: {payload}"
    );
}

/// nudge 同解析器：--message-file 来源落 NudgeIssued messageSource=file。
#[test]
fn cli_nudge_file_message_records_file_source() {
    let root = temp_root("nudge-file");
    setup_round(&root, "r1");
    write_fake_agent(&root, AGENT);
    setup_current_dispatch(&root, "r1", "B109", AGENT);
    let msg_file = root.join("msg.txt");
    fs::write(&msg_file, "nudge from file").unwrap();

    let out = run_orch(
        &root,
        &[
            "nudge",
            AGENT,
            "--task",
            "B109",
            "--message-file",
            msg_file.to_str().unwrap(),
            "--force",
        ],
    );
    assert!(
        out.status.success(),
        "nudge 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let events = ledger_events(&root, "r1");
    let payload = last_payload(&events, "NudgeIssued").expect("缺 NudgeIssued");
    assert_eq!(
        payload.get("messageSource").and_then(|v| v.as_str()),
        Some("file"),
        "nudge 文件来源应落 file: {payload}"
    );
    assert_eq!(
        payload.get("messageBytes").and_then(|v| v.as_u64()),
        Some("nudge from file".as_bytes().len() as u64),
        "nudge 文件内容字节数: {payload}"
    );
}

/// nudge 默认来源：省略 --message* → messageSource=default。
#[test]
fn cli_nudge_default_message_records_default_source() {
    let root = temp_root("nudge-default");
    setup_round(&root, "r1");
    write_fake_agent(&root, AGENT);
    setup_current_dispatch(&root, "r1", "B109", AGENT);

    let out = run_orch(&root, &["nudge", AGENT, "--task", "B109", "--force"]);
    assert!(
        out.status.success(),
        "nudge 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let events = ledger_events(&root, "r1");
    let payload = last_payload(&events, "NudgeIssued").expect("缺 NudgeIssued");
    assert_eq!(
        payload.get("messageSource").and_then(|v| v.as_str()),
        Some("default"),
        "nudge 默认来源应落 default: {payload}"
    );
    let default = "请检查当前任务进度：完成则写 REPORT；被阻塞则写 BLOCKED 说明卡点。";
    assert_eq!(
        payload.get("messageBytes").and_then(|v| v.as_u64()),
        Some(default.as_bytes().len() as u64),
        "nudge 默认文本字节数: {payload}"
    );
}

/// B110：缺 --task 必须由 CLI fail-closed，且在账本、NUDGE/POKE 文件和
/// provider wake 日志上都保持零副作用。
#[test]
fn cli_nudge_without_task_is_nonzero_and_side_effect_free() {
    let root = temp_root("nudge-missing-task");
    setup_round(&root, "r1");
    write_fake_agent(&root, AGENT);
    let ledger = root.join("coordination/rounds/r1/events.jsonl");
    let before = fs::read(&ledger).unwrap();

    let out = run_orch(&root, &["nudge", AGENT, "--force"]);
    assert!(!out.status.success(), "缺 --task 不得成功");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--task <TASK>"),
        "stderr 应明确缺少 --task: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fs::read(&ledger).unwrap(), before, "缺 --task 不得落账");
    assert!(!root
        .join(format!("coordination/rounds/r1/dispatch/{AGENT}/NUDGE.md"))
        .exists());
    assert!(!root
        .join(format!("coordination/runtime/POKE-{AGENT}.txt"))
        .exists());
    let logs = root.join("coordination/runtime/logs");
    assert!(
        !logs.exists() || fs::read_dir(logs).unwrap().next().is_none(),
        "缺 --task 不得 spawn provider wake"
    );
}

/// nudge 来源互斥冲突 → 非零退出（不落账）。
#[test]
fn cli_nudge_conflicting_sources_exit_nonzero() {
    let root = temp_root("nudge-conflict");
    setup_round(&root, "r1");
    write_fake_agent(&root, AGENT);
    let msg_file = root.join("msg.txt");
    fs::write(&msg_file, "file").unwrap();

    let out = run_orch(
        &root,
        &[
            "nudge",
            AGENT,
            "--message",
            "inline",
            "--message-file",
            msg_file.to_str().unwrap(),
            "--force",
        ],
    );
    assert!(
        !out.status.success(),
        "nudge 来源冲突应非零退出，实际 status: {:?}",
        out.status.code()
    );
    let events = ledger_events(&root, "r1");
    assert!(
        last_payload(&events, "NudgeIssued").is_none(),
        "nudge 冲突不应落账 NudgeIssued"
    );
}
