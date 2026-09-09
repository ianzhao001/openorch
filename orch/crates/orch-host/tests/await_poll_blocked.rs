//! ═══ 红种子契约 · B77（Phase D/D5 · await-report 侦测 BLOCKED 报告，杜绝静默阻塞）═══
//! 落位: orch/crates/orch-host/tests/await_poll_blocked.rs（逐字节复制）
//! 预期红（redForm: compile）：`tierf::{await_poll, AwaitPoll}` 与 `AwaitOutcome::Blocked`
//!   尚不存在 → error[E0432]/E0433。
//!
//! 背景（本轮实证 bad-case · r38/B74）：执行者遇不可解障碍时按协议写 `<task>-BLOCKED.md`
//! 并回等待；但 `run_await` 只轮询 `<task>-REPORT.md` + liveness。于是 BLOCKED 是**静默态**：
//! 无 REPORT → 一直轮询；执行者已 ack 处工作相位 → liveness 30min 不判滞。结果一个**立即可处置
//! 的诚实阻塞**最长静默 ~30min（B74 实测靠人工发现）。本棒让 `await_report` **同样双根轮询
//! `-BLOCKED.md`**，一旦出现即以独立归宿 `AwaitOutcome::Blocked` 提前返回、CLI 给独立退出码，
//! 让 planner 秒级获知。
//!
//! 关键取舍：REPORT 优先于 BLOCKED——若执行者同 tick 既写 REPORT 又留旧 BLOCKED，REPORT
//! （完成）胜过 BLOCKED（阻塞），避免旧 BLOCKED 遮蔽新完成。判定核抽为**纯函数**，不碰文件系统
//! （本 crate 无 tempfile dev-dep），文件存在性探测仍留在 run_await 的 `.is_file()` 薄壳里。
//!
//! 目标契约（改 orch_host::tierf，勿动 lib.rs、不新增依赖）：
//!   (1) `AwaitOutcome` 新增变体 `Blocked { report_rel: String }`（其余变体不变）。
//!   (2) 新纯枚举与判定：
//!         pub enum AwaitPoll { ReportFound, Blocked, Waiting }
//!         pub fn await_poll(report_found: bool, blocked_found: bool) -> AwaitPoll
//!       语义：report_found ⇒ ReportFound（**REPORT 优先**，即使 blocked_found 也为真）；
//!             否则 blocked_found ⇒ Blocked；两者皆假 ⇒ Waiting。
//!   （wiring：run_await 循环用双根 `.is_file()` 算出 report_found/blocked_found → await_poll →
//!     Blocked 时返回 `AwaitOutcome::Blocked{report_rel}`；CLI 映射独立退出码 6。此为 REPORT 说明，
//!     纯函数契约以下列断言为准。）
//!
//! 负向变异下界（转绿后逐条自证）：
//!   M1 BLOCKED 优先于 REPORT（反了优先级）⇒ report_wins_when_both 红；
//!   M2 忽略 blocked_found（无 REPORT 时恒 Waiting）⇒ blocked_only_is_blocked 红；
//!   M3 两者皆假不返回 Waiting ⇒ neither_is_waiting 红。

use orch_host::tierf::{await_poll, AwaitPoll};

#[test]
fn report_found_is_report() {
    assert_eq!(await_poll(true, false), AwaitPoll::ReportFound);
}

// REPORT 优先：既有 REPORT 又有旧 BLOCKED → 判 ReportFound（完成胜过阻塞）
#[test]
fn report_wins_when_both() {
    assert_eq!(await_poll(true, true), AwaitPoll::ReportFound);
}

#[test]
fn blocked_only_is_blocked() {
    assert_eq!(await_poll(false, true), AwaitPoll::Blocked);
}

#[test]
fn neither_is_waiting() {
    assert_eq!(await_poll(false, false), AwaitPoll::Waiting);
}
