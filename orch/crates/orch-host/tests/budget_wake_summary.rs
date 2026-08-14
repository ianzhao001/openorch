//! ═══ 红种子契约 · B57（唤醒预算摘要纯原语）═══
//! 落位: orch/crates/orch-host/tests/budget_wake_summary.rs（逐字节复制）
//! 预期红（redForm: compile）：下列 API 尚不存在 → error[E0432]/error[E0425]。
//!
//! 背景：B53 已把「模型唤醒计数 + 阈值」机器化（`count_model_wakes` /
//! `model_wake_permitted`），供预算门用。但缺一个**面向报表/状态**的纯摘要原语，
//! 把 spent/max 折叠成人可读、机器可比的预算余量结构（cost/status 报表将来可用）。
//! 本棒是该 additive 纯函数——不改任何既有函数/行为、不新增依赖。语义与既有
//! `model_wake_permitted`（`spent < max` 放行、`spent >= max` 阻断）保持一致。
//!
//! 目标契约（落在 orch_host::budget，模块已 pub 导出，勿动 lib.rs）：
//!  - pub struct WakeBudgetSummary {
//!        pub spent: u64,
//!        pub max: Option<u64>,
//!        pub remaining: Option<u64>,
//!        pub exhausted: bool,
//!    }  派生 Debug + PartialEq。
//!  - pub fn wake_budget_summary(spent: u64, max: Option<u64>) -> WakeBudgetSummary
//!      · remaining = max.map(|m| m.saturating_sub(spent))（无上限 ⇒ None）；
//!      · exhausted = max 存在且 spent >= max（与 model_wake_permitted 取反一致）；
//!      · 无上限（max=None）永不 exhausted、remaining=None。
//!
//! 负向变异下界（转绿后逐条自证）：
//!  M1 remaining 用普通减法（spent>max 时 underflow/panic 或回绕）而非 saturating_sub
//!       ⇒ remaining_saturates_at_zero 红；
//!  M2 exhausted 用 `>` 而非 `>=`（spent==max 漏判耗尽）
//!       ⇒ exhausted_is_inclusive_at_max 红；
//!  M3 无上限时误判 exhausted 或给出 Some(remaining)
//!       ⇒ unlimited_budget_never_exhausted 红。

use orch_host::budget::{wake_budget_summary, WakeBudgetSummary};

#[test]
fn summary_with_remaining_headroom() {
    assert_eq!(
        wake_budget_summary(3, Some(10)),
        WakeBudgetSummary {
            spent: 3,
            max: Some(10),
            remaining: Some(7),
            exhausted: false,
        }
    );
}

#[test]
fn exhausted_is_inclusive_at_max() {
    // 恰好达到上限 ⇒ 耗尽、余量 0
    assert_eq!(
        wake_budget_summary(10, Some(10)),
        WakeBudgetSummary {
            spent: 10,
            max: Some(10),
            remaining: Some(0),
            exhausted: true,
        }
    );
}

#[test]
fn remaining_saturates_at_zero() {
    // 超支不 underflow：remaining 饱和到 0、耗尽为真
    assert_eq!(
        wake_budget_summary(12, Some(10)),
        WakeBudgetSummary {
            spent: 12,
            max: Some(10),
            remaining: Some(0),
            exhausted: true,
        }
    );
}

#[test]
fn unlimited_budget_never_exhausted() {
    assert_eq!(
        wake_budget_summary(999, None),
        WakeBudgetSummary {
            spent: 999,
            max: None,
            remaining: None,
            exhausted: false,
        }
    );
}
