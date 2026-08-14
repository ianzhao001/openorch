//! ═══ 红种子契约 · B75（Phase D/D1 · pre-dispatch 握手：wake 日志咬合探测）═══
//! 落位: orch/crates/orch-host/tests/wake_engaged.rs（逐字节复制）
//! 预期红（redForm: compile）：`wake::wake_engaged` 尚不存在 → error[E0432]。
//!
//! 背景（Phase D · D1）：现 `orch dispatch` 是"发了再判死"——写 GO→wake→靠 liveness 事后判死，
//! 一个真死的通道要耗一个 attempt 才暴露。我们的执行者被 `codex exec resume {message}` 直接唤醒，
//! **不跑 wait-dispatch.sh**，故不产生 "waiting" 心跳；真正证明"会话已 resume/咬合"的信号 =
//! **wake 日志里的 `thread.started`/`turn.started`**（正是 B70/B73 我手动盯的探活信号）。本棒把该
//! 手动探活编码成纯判据 `wake_engaged`，供新 `orch handshake <agent>` 前置门用：dispatch 前先
//! 轻量 probe wake + 轮询 wake 日志，咬合才正式 dispatch，把"通道死"挡在开销前（不耗 attempt）。
//!
//! 目标契约（落在 orch_host::wake，模块已 pub 导出，勿动 lib.rs、不新增依赖）：
//!   - pub fn wake_engaged(log: &str) -> bool
//!       真 ⟺ 日志含会话咬合标记 `thread.started` 或 `turn.started`（子串命中即可，容纳 JSON 行
//!       形态如 `{"type":"thread.started",...}`）；仅有 ERROR/技能加载失败等噪声、或空日志 ⇒ 假。
//!
//! 负向变异下界（转绿后逐条自证）：
//!   M1 任意非空日志即判咬合（忽略标记）⇒ only_noise_not_engaged 红；
//!   M2 只认 thread.started 漏认 turn.started ⇒ turn_started_is_engaged 红；
//!   M3 空日志判咬合 ⇒ empty_not_engaged 红。

use orch_host::wake::wake_engaged;

// codex 首个 wake 事件：thread.started
#[test]
fn thread_started_is_engaged() {
    let log = r#"ERROR failed to load skill /x/SKILL.md
{"type":"thread.started","thread_id":"019f8e57"}
{"type":"turn.started"}"#;
    assert!(wake_engaged(log), "含 thread.started 应判咬合");
}

// 仅 turn.started 也算咬合（会话已开始出话）
#[test]
fn turn_started_is_engaged() {
    let log = "{\"type\":\"turn.started\"}\n";
    assert!(wake_engaged(log), "含 turn.started 应判咬合");
}

// 只有噪声（技能加载失败/一般 ERROR）不算咬合
#[test]
fn only_noise_not_engaged() {
    let log = "ERROR failed to load skill /a/SKILL.md: missing YAML frontmatter\nERROR another line";
    assert!(!wake_engaged(log), "仅噪声/ERROR 不得判咬合");
}

// 空日志不算咬合
#[test]
fn empty_not_engaged() {
    assert!(!wake_engaged(""), "空日志不得判咬合");
}
