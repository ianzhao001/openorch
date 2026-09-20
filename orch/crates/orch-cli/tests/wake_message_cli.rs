#![cfg(feature = "selfhost")]
//! B109 CLI 集成测试：`orch wake`/`orch nudge` 富输入三来源、互斥冲突、
//! UTF-8 字节数、默认提示、非零退出。
//!
//! 不引入新依赖：直接用 `std::process::Command` 启动 Cargo 为本次
//! integration target 构建的 `orch`；临时根目录落在本 worktree
//! `orch/target/test-tmp`（遵守协议 scratch 约束，不污染工作树）。
//!
//! schema 3 成功型 wake fixture（B321 rev3）：初始化精确 fixture Git root
//! （`rev-parse --show-toplevel` == canonical root 且 `HEAD^{commit}` 可解析），
//! 再写 gitignored `.orch/harnesses.yaml`（code-owned `opencode` driver +
//! `cwdPolicy: project-root`，绝不声明 raw argv/env），fake provider 打印
//! opencode 形状 JSONL 后保持存活以完成 managed supervisor 握手。
//! WakeIssued 仍落 messageSource/messageBytes，绝不调用真实模型 CLI。
//! nudge/冲突输入/副作用前拒绝用例保留 tracked `agents.yaml` legacy 夹具。

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
        "coordination/rounds/r1/tasks",
    ] {
        fs::create_dir_all(root.join(path)).unwrap();
    }
    fs::write(
        root.join("coordination/runtime/CURRENT-ROUND"),
        format!("{round}\n"),
    )
    .unwrap();
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap();
    fs::copy(
        source_root.join("coordination/PROJECT-BINDING.yaml"),
        root.join("coordination/PROJECT-BINDING.yaml"),
    )
    .unwrap();
    fs::copy(source_root.join(".gitignore"), root.join(".gitignore")).unwrap();
    orch_host::ledger::append(
        root,
        round,
        &[orch_host::ledger::event(
            "RoundOpened",
            "runtime:orch",
            None,
            Some(round),
            json!({"purpose": "wake message fixture", "contractSchemaVersion": 3}),
        )],
    )
    .unwrap();
    fs::write(
        root.join(format!("coordination/rounds/{round}/tasks/B109.md")),
        format!("---\nschemaVersion: 3\ntaskId: B109\nround: {round}\nseedProtocol: verify-only\nredForm: assertion\ndependsOn: []\nentryPoints: [fixture.txt]\nseeds: []\nwriteSet: [fixture.txt]\nfrozenPaths: [coordination/rounds/**]\ngates: {{fast: [testFast, testExclusive, check, checkDefault, buildDefault, buildSelfhost]}}\nrequiredEvidence: [cli]\n---\nfixture\n"),
    )
    .unwrap();
    fs::write(root.join("fixture.txt"), "fixture\n").unwrap();
    orch_host::plan::run_plan(root).unwrap();
    orch_host::round::run_sign_off(root, Some("CLI fixture")).unwrap();
}

/// B110：nudge 必须绑定显式 task 的真实 current attempt。测试夹具写入一条
/// 完整 modern DispatchIssued，避免恢复 B109 时代“按 agent 猜最近任务”的旧语义。


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

/// B321 rev3：把 fixture root 初始化成精确 Git 仓库并提交全部已落盘的
/// tracked fixture 字节，使 unified channel 的 schema3 判定
/// （`rev-parse --show-toplevel` canonical == canonical root）与 consult
/// 固定 HEAD 核验（`HEAD^{commit}`）成立。走 `support::fixture_git_command`
/// 共享边界（`git -c core.fsmonitor=false -C <root>`）。
fn init_fixture_git(root: &Path) {
    let run = |args: &[&str]| {
        let output = support::fixture_git_command(root)
            .args(args)
            .output()
            .expect("启动 fixture git 失败");
        assert!(
            output.status.success(),
            "fixture git {args:?} 失败: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["add", "-A"]);
    run(&[
        "-c",
        "user.name=orch-test",
        "-c",
        "user.email=orch@test.invalid",
        "commit",
        "-qm",
        "fixture",
    ]);
}

/// B321 rev3：unified channel 只接受 gitignored `.orch/harnesses.yaml` 的
/// code-owned driver 配置。fake provider 输出 opencode 形状 JSONL 后保持
/// 存活（`/bin/sleep 8`），覆盖 supervisor 的 5 秒 OFFER 和 2 秒 ACCEPTED
/// 最长窗口，让 OFFER→ACCEPT→ACCEPTED 握手完成；绝不声明 raw argv/env，
/// 绝不调用真实模型 CLI。
/// 必须在 [init_fixture_git] 的 commit 之后调用：`.orch/harnesses.yaml`
/// 被 fixture `.gitignore` 覆盖，config 字节永不入 git。
fn write_fake_harness(root: &Path, alias: &str) {
    use std::os::unix::fs::PermissionsExt;
    let local = root.join(".orch");
    fs::create_dir_all(&local).unwrap();
    let executable = local.join("fake-provider.sh");
    fs::write(
        &executable,
        b"#!/bin/sh\nprintf '%s\\n' '{\"type\":\"step_start\",\"sessionID\":\"session-alpha\",\"part\":{\"type\":\"step-start\",\"modelID\":\"model-a\"}}'\nprintf '%s\\n' '{\"type\":\"text\",\"part\":{\"text\":\"done\"}}'\nprintf '%s\\n' '{\"type\":\"step_finish\",\"part\":{\"type\":\"step-finish\",\"reason\":\"stop\"}}'\n/bin/sleep 8\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&executable, permissions).unwrap();
    fs::write(
        local.join("harnesses.yaml"),
        format!(
            "version: 1\nharnesses:\n  {alias}:\n    driver: opencode\n    executable: {}\n    enabled: true\n    defaults: {{provider: local, model: model-a, effort: high}}\n    cwdPolicy: project-root\n",
            executable.display()
        ),
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
    init_fixture_git(&root);
    write_fake_harness(&root, AGENT);

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
    init_fixture_git(&root);
    write_fake_harness(&root, AGENT);
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
    init_fixture_git(&root);
    write_fake_harness(&root, AGENT);

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
    init_fixture_git(&root);
    write_fake_harness(&root, AGENT);

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
    // 默认文本 "orch: 请检查当前项目并汇报进展" 字节数
    let default = "orch: 请检查当前项目并汇报进展";
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
    init_fixture_git(&root);
    write_fake_harness(&root, AGENT);

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

/// 空文件是显式空消息（B321 rev4）：CLI 把文件字节原样解析为
/// `source=file / bytes=0 / text=""`（绝不因内容为空回落 default），
/// unified channel consult 把 prompt 直接绑定为该原始文本，而 B320 的
/// `prepare_invocation`（channel.rs）在任何 config resolve/render/spawn
/// 之前无条件拒绝空 prompt。因此显式空文件必须得到精确的 pre-spawn 拒绝：
/// exit 恰好为 2、stderr 含 `invocation prompt 不能为空`、账本恰好追加一条
/// operation=wake/exitCode=2 的 ActionRejected，零 WakeIssued、零 provider
/// marker、零其它副作用。该精确拒绝同时证明 empty 未回落 non-empty
/// default——若空内容被当成“无消息”而回落默认文本，非空默认消息会成功走
/// 完整 channel 并落账 `messageSource=default` 的 WakeIssued。
#[test]
fn cli_wake_empty_file_is_explicit_empty_not_default() {
    let root = temp_root("wake-empty-file");
    setup_round(&root, "r1");
    init_fixture_git(&root);
    write_fake_harness(&root, AGENT);
    let msg_file = root.join("empty.txt");
    fs::write(&msg_file, "").unwrap();
    let ledger_path = root.join("coordination/rounds/r1/events.jsonl");
    let ledger_before = fs::read(&ledger_path).unwrap();

    let out = run_orch(
        &root,
        &["wake", AGENT, "--message-file", msg_file.to_str().unwrap()],
    );

    // 精确 pre-spawn 拒绝：exit 恰好为 2，stderr 点名空 prompt 契约。
    assert_eq!(
        out.status.code(),
        Some(2),
        "显式空文件必须 exit 2（pre-spawn 拒绝），实际 status: {:?}，stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("invocation prompt 不能为空"),
        "stderr 应点名空 prompt 拒绝原因: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("messageSource"),
        "拒绝路径不得打印成功 wake 的 messageSource 行: {stdout}"
    );

    // 账本增量必须恰好是一条 ActionRejected：既有字节逐字保留（前缀相等），
    // 新增唯一事件 operation=wake、exitCode=2、reason 含空 prompt 原因，且
    // payload 绝不携带 messageSource/messageBytes。
    let ledger_after = fs::read(&ledger_path).unwrap();
    assert!(
        ledger_after.len() > ledger_before.len(),
        "拒绝必须落一条 durable ActionRejected"
    );
    assert_eq!(
        &ledger_after[..ledger_before.len()],
        &ledger_before[..],
        "既有账本字节不得被改写"
    );
    let delta = String::from_utf8(ledger_after[ledger_before.len()..].to_vec())
        .expect("新增账本行必须是合法 UTF-8");
    let mut new_lines = delta.lines().filter(|line| !line.trim().is_empty());
    let rejection_line = new_lines
        .next()
        .expect("拒绝必须恰好追加一条 ActionRejected 事件");
    assert!(
        new_lines.next().is_none(),
        "除该条 ActionRejected 外不得有其它账本副作用: {delta}"
    );
    let rejection: serde_json::Value =
        serde_json::from_str(rejection_line).expect("ActionRejected 行非合法 JSON");
    assert_eq!(
        rejection.get("type").and_then(|v| v.as_str()),
        Some("ActionRejected"),
        "唯一新增事件必须是 ActionRejected: {rejection}"
    );
    let payload = rejection.get("payload").expect("ActionRejected 缺 payload");
    assert_eq!(
        payload.get("operation").and_then(|v| v.as_str()),
        Some("wake"),
        "ActionRejected.operation 应为 wake: {payload}"
    );
    assert_eq!(
        payload.get("exitCode").and_then(|v| v.as_i64()),
        Some(2),
        "ActionRejected.exitCode 应为 2: {payload}"
    );
    let reason = payload.get("reason").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        reason.contains("invocation prompt 不能为空"),
        "ActionRejected.reason 应点名空 prompt: {reason}"
    );
    assert!(
        payload.get("messageSource").is_none() && payload.get("messageBytes").is_none(),
        "rejection payload 不得携带 messageSource/messageBytes: {payload}"
    );

    // 零 WakeIssued：显式空内容没有作为成功 wake 落账；也没有任何事件记录
    // messageSource=default——若空文件被当成“无消息”回落默认文本，非空默认
    // 消息会成功走完整 channel 并落 messageSource=default 的 WakeIssued。
    // 精确的空 prompt 拒绝证明空文件被当作显式空消息原样传入 channel。
    let events = ledger_events(&root, "r1");
    assert!(
        last_payload(&events, "WakeIssued").is_none(),
        "显式空文件不得落账 WakeIssued"
    );
    assert!(
        !events.iter().any(|event| event
            .get("payload")
            .and_then(|p| p.get("messageSource"))
            .and_then(|v| v.as_str())
            == Some("default")),
        "空文件不得回落 non-empty default（任何事件都不得带 messageSource=default）"
    );

    // 零 provider marker：拒绝发生在 prepare_invocation（任何 config
    // resolve/render/spawn 之前），fake provider 绝不能被 spawn——不得出现
    // wake log 文件。
    let logs = root.join("coordination/runtime/logs");
    assert!(
        !logs.exists() || fs::read_dir(&logs).unwrap().next().is_none(),
        "空 prompt 必须 pre-spawn 拒绝，不得留下 provider wake log"
    );
}
