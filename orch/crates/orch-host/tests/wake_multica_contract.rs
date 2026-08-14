//! B195 · `coordination/scripts/wake-multica.sh` 的退出码契约（planner 应急直修
//! `0128c7b` 的转正回归）。
//!
//! 该脚本是三笔 planner 直修中**唯一没有任何 evidence 文件**的一笔：当时只留了 diff 与
//! BOARD 记述，没有机械可回归的留证。本文件就是补上的那份留证——把「消除确定性假成功」
//! 这条声称，变成会随全量门一起跑的判据。
//!
//! 首红形态：**compile**。本文件顶部声明的 `support_multica` 模块尚不存在
//! （目标文件 `orch/crates/orch-host/tests/support_multica/mod.rs`），rustc 报
//! `error[E0583]: file not found for module `support_multica``。
//! 不得以建空壳、改本文件、或把本测试排除出门的方式伪造红绿。
//!
//! **脚本本体在 `coordination/**` 冻结区内，本卡一个字节都不改**：这是审计，不是重写。
//! scratch 一律落在本 worktree 的 `orch/target/test-tmp`。

mod support_multica;

use std::path::{Path, PathBuf};

use support_multica::{run_wake_multica, FakeMultica, MulticaScript};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("CARGO_MANIFEST_DIR 上溯三级应为仓根")
        .to_path_buf()
}

fn script_path() -> PathBuf {
    repo_root().join("coordination/scripts/wake-multica.sh")
}

/// exit 0 只能由 **exact terminal** 换来：解析成功且含顶层 `payloads` 键的 JSON 帧。
#[test]
fn terminal_frame_is_the_only_success() {
    let fake = FakeMultica::spawn("terminal", MulticaScript::TerminalFrame);
    let out = run_wake_multica(&script_path(), fake.sock_path(), "hello", "30");

    assert_eq!(out.code, 0, "观测到终帧必须 exit 0；stderr={}", out.stderr);
    assert!(
        out.stdout.contains("\"payloads\""),
        "终帧必须逐行透传到 stdout: {}",
        out.stdout
    );
    assert!(
        out.stderr.contains("terminal=yes"),
        "诊断必须走 stderr 且自报 terminal=yes: {}",
        out.stderr
    );
}

/// **这是加固的核心**：服务端一连上就关、零帧 —— 旧行为 exit 0（确定性假成功），
/// 新契约必须是 70。B178 期间的静默失败正是这个形态。
#[test]
fn zero_frame_eof_is_seventy_not_success() {
    let fake = FakeMultica::spawn("zero-frame", MulticaScript::ZeroFrameEof);
    let out = run_wake_multica(&script_path(), fake.sock_path(), "hello", "30");

    assert_eq!(
        out.code, 70,
        "零帧 EOF 必须 exit 70，绝不能是 0（这正是被修掉的假成功）；stderr={}",
        out.stderr
    );
    assert!(
        out.stderr.contains("frames=0") && out.stderr.contains("terminal=no"),
        "诊断必须说明零帧且无终帧: {}",
        out.stderr
    );
}

/// 有帧但流被截断（无终帧即 EOF）：71，与零帧区分开。
#[test]
fn frames_without_terminal_are_seventy_one() {
    let fake = FakeMultica::spawn("truncated", MulticaScript::FramesThenEof);
    let out = run_wake_multica(&script_path(), fake.sock_path(), "hello", "30");

    assert_eq!(out.code, 71, "有帧无终帧必须 exit 71；stderr={}", out.stderr);
    assert!(
        !out.stdout.trim().is_empty(),
        "被截断前收到的帧仍必须已经透传出去"
    );
}

/// 超时且无终帧：72。超时与 EOF 必须可区分，否则上游无法归因。
#[test]
fn timeout_without_terminal_is_seventy_two() {
    let fake = FakeMultica::spawn("silent", MulticaScript::Silent);
    let out = run_wake_multica(&script_path(), fake.sock_path(), "hello", "1");

    assert_eq!(out.code, 72, "超时无终帧必须 exit 72；stderr={}", out.stderr);
    assert!(
        out.stderr.contains("timeout") || out.stderr.contains("超时"),
        "诊断必须点明超时: {}",
        out.stderr
    );
}

/// 仅仅出现 `payloads` 字样、但不是合法 JSON 帧的残余缓冲，**不构成** exact terminal。
/// 这条防的是「用 substring 冒充终帧」——加固前后最容易退化回去的一处。
#[test]
fn payloads_substring_that_is_not_json_is_not_terminal() {
    let fake = FakeMultica::spawn("bogus", MulticaScript::MalformedPayloadsText);
    let out = run_wake_multica(&script_path(), fake.sock_path(), "hello", "30");

    assert_ne!(
        out.code, 0,
        "不可解析的 payloads 字样不得被当作终帧：{} / {}",
        out.stdout, out.stderr
    );
    assert!(
        out.code == 70 || out.code == 71,
        "应落在无终帧的 EOF 分支（70/71），实际 {}",
        out.code
    );
}

/// 终帧可能不带换行就直接关连接：残余缓冲清算路径必须仍认它，且只认解析成功的。
#[test]
fn terminal_frame_without_trailing_newline_still_succeeds() {
    let fake = FakeMultica::spawn("no-newline", MulticaScript::TerminalFrameNoNewline);
    let out = run_wake_multica(&script_path(), fake.sock_path(), "hello", "30");

    assert_eq!(
        out.code, 0,
        "无换行的合法终帧必须仍算 exact terminal；stderr={}",
        out.stderr
    );
    assert!(out.stdout.contains("\"payloads\""), "终帧仍须透传");
}

/// 缺参与连接失败的既有退出码沿用，不得被加固改掉。
#[test]
fn legacy_exit_codes_are_preserved() {
    let missing = run_wake_multica(&script_path(), Path::new("/nonexistent.sock"), "", "5");
    assert_eq!(missing.code, 64, "缺 message 必须沿用 exit 64");

    let unreachable = run_wake_multica(
        &script_path(),
        Path::new("/nonexistent/definitely-not-here.sock"),
        "hello",
        "5",
    );
    assert_eq!(unreachable.code, 3, "连接失败必须沿用 exit 3");
}

/// stdout 逐行透传格式不变（`watch-sessions.py` 依赖它）：
/// 诊断只能走 stderr，stdout 上不得混入 `[wake-multica]` 前缀行。
#[test]
fn stdout_stays_pure_stream_for_watch_sessions() {
    let fake = FakeMultica::spawn("stream", MulticaScript::TerminalFrame);
    let out = run_wake_multica(&script_path(), fake.sock_path(), "hello", "30");

    for line in out.stdout.lines().filter(|line| !line.trim().is_empty()) {
        assert!(
            !line.starts_with("[wake-multica]"),
            "诊断混进了 stdout，会破坏 watch-sessions.py 的解析: {line}"
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(line).is_ok(),
            "stdout 每一非空行都应是服务端原样回传的 JSON 帧: {line}"
        );
    }
    assert!(
        out.stderr.contains("[wake-multica]"),
        "诊断必须出现在 stderr: {}",
        out.stderr
    );
}

/// 请求侧不变量：发出的必须是单行 NDJSON `run` 请求，且 `cwd` 绑定到 git common dir 的父目录。
#[test]
fn request_is_single_line_run_with_repo_bound_cwd() {
    let fake = FakeMultica::spawn("request", MulticaScript::TerminalFrame);
    let out = run_wake_multica(&script_path(), fake.sock_path(), "审查请求正文", "30");
    assert_eq!(out.code, 0, "前置：本用例应走成功路径");

    let request = fake.request().expect("服务端必须收到请求");
    assert_eq!(request["type"], "run", "请求类型必须是 run");
    assert_eq!(
        request["prompt"], "审查请求正文",
        "prompt 必须逐字透传（含非 ASCII）"
    );
    assert!(
        request["sessionId"].as_str().is_some_and(|id| !id.is_empty()),
        "sessionId 必须非空: {request}"
    );
    let cwd = request["cwd"].as_str().expect("cwd 必须是字符串");
    assert!(
        Path::new(cwd).join(".git").exists() || Path::new(cwd).join("HEAD").exists(),
        "cwd 必须绑定到调用处所属的仓，实际 {cwd}"
    );
}
