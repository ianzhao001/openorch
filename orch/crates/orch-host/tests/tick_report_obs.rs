//! ═══ 红种子契约 · B46 ═══（落位: orch/crates/orch-host/tests/tick_report_obs.rs，逐字节复制）
//! 预期红（redForm: compile）：tick_report 现为 2 参（mechanical/injections），本种子按新契约以
//!   3 参调用（末位 pending_inbox: usize）→ error[E0061] this function takes 2 arguments but 3
//!   arguments were supplied，文件级编译红。
//! 背景：daemon 无人当班后，tick_report 是唯一观测窗口，但现在只报 mechanical/injected 两侧，
//!   看不到 inbox 积压——安静 tick 与「积压但去抖抑制」不可区分。本棒给 TickReport 增
//!   pending_inbox 字段（compute 侧已有 list_pending_by_priority 结果可传），is_idle 语义收紧：
//!   有积压即非 idle。run_daemon 打印行带上 pending 数是 IO 胶水，由卡+verifier 核。
//! 变异清单（E9 下界，负向自证逐条注入应各自变红）：
//!   M1 pending_inbox 不入结构体(丢弃入参) → `tick_report_carries_pending_inbox_count` 红；
//!   M2 is_idle 忽略 pending_inbox → `idle_requires_no_pending_inbox` 红。
use orch_host::serve;

#[test]
fn tick_report_carries_pending_inbox_count() {
    let report = serve::tick_report(&[], &[], 3);
    assert_eq!(report.pending_inbox, 3, "pending_inbox 计数应原样入报告");
}

#[test]
fn idle_requires_no_pending_inbox() {
    assert!(serve::tick_report(&[], &[], 0).is_idle(), "无积压+无动作=idle");
    assert!(
        !serve::tick_report(&[], &[], 2).is_idle(),
        "inbox 有积压即非 idle（安静 tick 与积压可区分）"
    );
}
