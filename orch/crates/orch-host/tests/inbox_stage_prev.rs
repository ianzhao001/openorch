//! ═══ 红种子契约 · B68（inbox 阶段回退纯函数 · 压测棒）═══
//! 落位: orch/crates/orch-host/tests/inbox_stage_prev.rs（逐字节复制）
//! 预期红（redForm: compile）：`inbox::prev_stage` 尚不存在 → error[E0432]。
//!
//! 契约（落在 orch_host::inbox，勿动 lib.rs）：
//!   - pub fn prev_stage(s: InboxStage) -> Option<InboxStage>
//!       既有 `next_stage` 的逆：Done→Some(Processing)，Processing→Some(Pending)，Pending→None。
//! 负向变异：M1 方向反了(用 next 语义) / M2 Pending 不返 None ⇒ 对应用例红。

use orch_host::inbox::{next_stage, prev_stage, InboxStage};

#[test]
fn prev_stage_is_inverse_of_next() {
    assert_eq!(prev_stage(InboxStage::Done), Some(InboxStage::Processing));
    assert_eq!(prev_stage(InboxStage::Processing), Some(InboxStage::Pending));
    assert_eq!(prev_stage(InboxStage::Pending), None);
}

#[test]
fn prev_then_next_round_trips() {
    let p = prev_stage(InboxStage::Done).unwrap();
    assert_eq!(next_stage(p), Some(InboxStage::Done));
}
