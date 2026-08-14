//! ═══ 红种子契约 · B41 ═══（落位: orch/crates/orch-host/tests/serve_run.rs，逐字节复制）
//! 预期红（redForm: compile）：serve::{TickReport, tick_report} 尚不存在（E0432 + E0425），文件级编译红。
//! 背景：design/11 §3/§6——orch serve daemon 常驻循环,每 tick = runloop 机械分支(verify/merge/close)
//!   + serve_tick 判断分支(注入主控)。本棒补循环体 + orch serve CLI;种子钉「每 tick 观测汇总」纯内核
//!   (daemon 逐 tick 打印它,idle tick 可静默)。循环体/CLI 属 IO 胶水,由 verifier+门核验(B16 先例)。
//! 变异清单（E9 下界）:
//!   M1 mechanical 计数错(不等于机械动作数) → ① 红；M2 空 tick 不判 idle → ② 红；M3 injected reason 丢失/串位 → ③ 红
use orch_host::serve::{self, InjectReason, Injection};

#[test]
fn tick_report_counts_mechanical_and_lists_injection_reasons() {
    // ① 机械动作计数 + 注入 reason 按序列出
    let mechanical = vec!["Verify B41".to_string(), "Merge B40".to_string()];
    let injections = vec![
        Injection { reason: InjectReason::TaskFailed, message: "t".to_string() },
        Injection { reason: InjectReason::NewInstruction, message: "n".to_string() },
    ];
    let report = serve::tick_report(&mechanical, &injections, 0);
    assert_eq!(report.mechanical, 2);
    assert_eq!(
        report.injected,
        vec![InjectReason::TaskFailed, InjectReason::NewInstruction]
    );
    assert!(!report.is_idle());
}

#[test]
fn empty_tick_is_idle() {
    // ② 无机械动作且无注入 → idle(daemon 可静默不刷屏)
    let report = serve::tick_report(&[], &[], 0);
    assert_eq!(report.mechanical, 0);
    assert!(report.injected.is_empty());
    assert!(report.is_idle());
}

#[test]
fn injection_only_tick_is_not_idle() {
    // ③ 仅有注入(无机械)也非 idle;reason 如实反映
    let injections = vec![Injection { reason: InjectReason::AllRecorded, message: "a".to_string() }];
    let report = serve::tick_report(&[], &injections, 0);
    assert!(!report.is_idle());
    assert_eq!(report.injected, vec![InjectReason::AllRecorded]);
    assert_eq!(report.mechanical, 0);
}
