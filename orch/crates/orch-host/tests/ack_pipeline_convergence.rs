//! B252 冻结契约 · ACK 流水线的有界收敛与阶段命名（H56/B203 unattended 家族）
//!
//! 立卡事实（`coordination/rounds/r69/planning/b251-owner-baseline.md`）：
//! `cargo test --workspace` 下 `serve::tests::production_entries_visit_all_six_unattended_stations`
//! 稳定复现红。`.ack` 已存在、夹具自己打印了「.ack 观察到」，但 `DispatchAcked` 在
//! `run_await` 的 21s 内一次都没落成 durable 事件（和解节拍 2s ⇒ ~10 次机会全落空）。
//! 现有断言只会说「did not observe exact DispatchAcked within 8s」——**它说不出卡在哪一段**，
//! 因此既无法归因，也无法证明修好了。
//!
//! 已否掉的假说（不要重走，实验见上述文档 §7）：
//!   E1 纯 CPU 饿死（14 spinner / 12 核）→ 绿；
//!   E2 同进程兄弟干扰（`-p orch-host --lib` 全量 701 测试）→ 绿；
//!   H56 夹具是私有仓（`serve.rs` 的 `git init -q`），**不撞** B249 的 canonical
//!   `orch-worktree-init.lock`，与 B251 不同根因。
//!   ⇒ 复现的必要条件是**跨二进制并发**；具体是哪个机器级共享资源**尚未隔离**，
//!   本契约刻意不预设根因。
//!
//! 本契约只钉两件可判定的事，**不钉根因**：
//!   ① ACK 流水线的**阶段可命名**——给定 durable 事实即可判出停在哪一段；
//!   ② 收敛判据以**和解节拍数**为单位、**有界**，且超界时必须带上阶段与已耗节拍。
//! 墙钟只允许出现在把「真实经过时间」换算成节拍数的一处入口，判据本身不得读时钟。
//!
//! 铁律 10：本文件落位后逐字节冻结。补充用例请放 writeSet 内其他落点。
//!
//! 负向变异靶（M1–M3，REPORT 必须逐条自证）：
//!   M1 把 `AckPresentUnclaimed` 的判据改成「只看 DispatchAcked 缺席」而忽略 ack 是否存在
//!      ⇒ `go_not_consumed_is_not_reported_as_unclaimed` 必须红。
//!   M2 把超界判据从 `>` 改成 `>=`（或反之）⇒ `budget_boundary_is_exact` 必须红。
//!   M3 让超界 verdict 丢掉 `stage` 或 `ticks_elapsed`（填默认值）
//!      ⇒ `exceeded_budget_names_the_stalled_stage` 必须红。

use orch_host::serve::{
    ack_convergence_verdict, classify_ack_pipeline, AckPipelineStage, AckSnapshot,
};

/// 构造一个只含所需 durable 事实的快照。生产实现必须让本结构可由
/// 「ack 文件是否存在 + 该 attempt 的事件集合」直接得出，不得依赖进程内状态。
fn snapshot(ack_present: bool, acked: bool, started: bool) -> AckSnapshot {
    AckSnapshot {
        ack_present,
        dispatch_acked: acked,
        attempt_started: started,
    }
}

#[test]
fn stages_are_total_over_the_four_durable_combinations() {
    // GO 尚未被消费
    assert_eq!(
        classify_ack_pipeline(snapshot(false, false, false)),
        AckPipelineStage::GoNotConsumed
    );
    // .ack 已在盘上，但 durable 事件一条都没落 —— 这正是 r69 基线红的形态
    assert_eq!(
        classify_ack_pipeline(snapshot(true, false, false)),
        AckPipelineStage::AckPresentUnclaimed
    );
    // 已落 DispatchAcked，AttemptStarted 尚未补齐（partial legacy）
    assert_eq!(
        classify_ack_pipeline(snapshot(true, true, false)),
        AckPipelineStage::Acked
    );
    // 完整
    assert_eq!(
        classify_ack_pipeline(snapshot(true, true, true)),
        AckPipelineStage::AckedAndStarted
    );
}

#[test]
fn go_not_consumed_is_not_reported_as_unclaimed() {
    // M1 靶：ack 不在盘上时，绝不能报成「已 ack 但没 claim」——那会把
    // 「执行者还没消费 GO」误判成「运行时没收割 ack」，归因直接反向。
    let stage = classify_ack_pipeline(snapshot(false, false, false));
    assert_ne!(stage, AckPipelineStage::AckPresentUnclaimed);
    assert_eq!(stage, AckPipelineStage::GoNotConsumed);
}

#[test]
fn durable_acked_without_ack_file_still_counts_as_acked() {
    // ack 文件可被现场清理/Drop 删除，但 durable 事件一旦落账即为终局事实。
    // 判据必须以账本为准，不得因为文件消失而回退到未 claim。
    assert_eq!(
        classify_ack_pipeline(snapshot(false, true, false)),
        AckPipelineStage::Acked
    );
    assert_eq!(
        classify_ack_pipeline(snapshot(false, true, true)),
        AckPipelineStage::AckedAndStarted
    );
}

#[test]
fn converged_stages_are_exactly_acked_and_started() {
    // 收敛 = 已 claim。未 claim 的两态都不算收敛，无论花了多少节拍。
    for ticks in [0_u32, 1, 7, 4096] {
        assert!(!ack_convergence_verdict(AckPipelineStage::GoNotConsumed, ticks, 4).converged);
        assert!(
            !ack_convergence_verdict(AckPipelineStage::AckPresentUnclaimed, ticks, 4).converged
        );
        assert!(ack_convergence_verdict(AckPipelineStage::Acked, ticks, 4).converged);
        assert!(ack_convergence_verdict(AckPipelineStage::AckedAndStarted, ticks, 4).converged);
    }
}

#[test]
fn budget_boundary_is_exact() {
    // M2 靶：恰好用满预算不算超界；多一个节拍才算。边界必须是精确的 `>`，
    // 否则「预算 N」的语义会在实现与卡面之间漂移一个节拍。
    let at_budget = ack_convergence_verdict(AckPipelineStage::AckPresentUnclaimed, 4, 4);
    assert!(!at_budget.exceeded, "ticks == budget 不得判超界");
    let over = ack_convergence_verdict(AckPipelineStage::AckPresentUnclaimed, 5, 4);
    assert!(over.exceeded, "ticks > budget 必须判超界");
}

#[test]
fn a_converged_pipeline_never_reports_exceeded() {
    // 已收敛就不该再谈超界——否则收尾竞态会把成功报成失败。
    let late = ack_convergence_verdict(AckPipelineStage::AckedAndStarted, 9_999, 4);
    assert!(late.converged);
    assert!(!late.exceeded);
}

#[test]
fn exceeded_budget_names_the_stalled_stage() {
    // M3 靶：超界 verdict 必须自带「停在哪一段」与「已耗多少节拍」。
    // r69 基线红之所以无法归因，正是因为旧断言只有一句 8s 超时。
    let verdict = ack_convergence_verdict(AckPipelineStage::AckPresentUnclaimed, 11, 4);
    assert!(verdict.exceeded);
    assert!(!verdict.converged);
    assert_eq!(verdict.stage, AckPipelineStage::AckPresentUnclaimed);
    assert_eq!(verdict.ticks_elapsed, 11);
    assert_eq!(verdict.tick_budget, 4);
}

#[test]
fn zero_budget_is_rejected_not_silently_treated_as_unbounded() {
    // 预算 0 是配置错误。它必须立刻超界（fail-closed），
    // 绝不能被当成「无限等待」——无界等待正是 H137 那类挂死的来源。
    let verdict = ack_convergence_verdict(AckPipelineStage::AckPresentUnclaimed, 0, 0);
    assert!(verdict.exceeded, "预算 0 必须 fail-closed");
    assert!(!verdict.converged);
}

#[test]
fn the_verdict_is_a_pure_function_of_its_inputs() {
    // 同输入必同输出：判据不得读时钟、不得读全局状态。
    // 这是本卡能在跨二进制并发下稳定的前提。
    let a = ack_convergence_verdict(AckPipelineStage::AckPresentUnclaimed, 6, 4);
    let b = ack_convergence_verdict(AckPipelineStage::AckPresentUnclaimed, 6, 4);
    assert_eq!(a.converged, b.converged);
    assert_eq!(a.exceeded, b.exceeded);
    assert_eq!(a.stage, b.stage);
    assert_eq!(a.ticks_elapsed, b.ticks_elapsed);
    assert_eq!(a.tick_budget, b.tick_budget);
}
