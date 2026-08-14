//! ═══ 红种子契约 · B43 ═══（落位: orch/crates/orch-host/tests/serve_round_closed.rs，逐字节复制）
//! 预期红（redForm: compile）：derive_tick_inputs 现为 4 参（task_states/new_inbox/dead_or_stalled/pending），
//!   本种子按新契约以 5 参调用（末位 round_closed: bool）→ error[E0061] this function takes 4 arguments
//!   but 5 arguments were supplied，文件级编译红。
//! 背景：design/11 §6 backlog① all_recorded 去抖——RoundClosed 后当前轮任务仍全 recorded、pending 又被
//!   planner 事件清空，serve_tick 会对已收轮**重复注入** AllRecorded（r24 收轮后实测触发一次）。修法：
//!   derive_tick_inputs 加 round_closed 入参，已关轮时抑制 all_recorded（`compute_tick_inputs` 从当前轮
//!   账本探 RoundClosed 事件传入，属 IO 胶水由卡+verifier 核）。backlog② inbox 优先级接线（compute_tick_inputs
//!   list_pending→list_pending_by_priority）亦属 IO 胶水，同由卡+verifier 核，不在本纯函数种子内。
//! 变异清单（E9 下界，负向自证逐条注入应各自变红）：
//!   M1 round_closed=true 不抑制(照旧算 all_recorded) → `closed_round_suppresses_all_recorded` 红；
//!   M2 round_closed=false 误抑制(去抖过头误伤正常收轮) → `open_round_still_signals_all_recorded` 红。
use orch_host::serve::{self, InjectReason};

#[test]
fn closed_round_suppresses_all_recorded() {
    // 全 recorded 但轮已 RoundClosed → all_recorded=false，decide 不再注入 AllRecorded
    let inputs = serve::derive_tick_inputs(
        &[("B42".to_string(), "recorded".to_string())],
        vec![],
        vec![],
        vec![],
        true, // round_closed
    );
    assert!(!inputs.all_recorded, "已收轮应抑制 all_recorded");
    let injections = serve::decide_injections(&inputs);
    assert!(
        injections.iter().all(|i| i.reason != InjectReason::AllRecorded),
        "已收轮不得再注入 AllRecorded"
    );
}

#[test]
fn open_round_still_signals_all_recorded() {
    // 未关轮全 recorded → all_recorded=true（回归：去抖不误伤正常收轮注入）
    let inputs = serve::derive_tick_inputs(
        &[("B42".to_string(), "recorded".to_string())],
        vec![],
        vec![],
        vec![],
        false, // round_closed
    );
    assert!(inputs.all_recorded, "未收轮全 recorded 应触发 all_recorded");
    let injections = serve::decide_injections(&inputs);
    assert!(
        injections.iter().any(|i| i.reason == InjectReason::AllRecorded),
        "未收轮全 recorded 应注入 AllRecorded"
    );
}
