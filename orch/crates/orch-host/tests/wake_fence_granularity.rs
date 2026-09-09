//! B200 · continuation fence 的**粒度**与**可重试性**（H72 P0 + H67）。
//!
//! 两条都在 r60 造成了真实损失，且同源——`identity_fence_decision` 只按 agent 聚合活跃
//! continuation，且对「后端从未接受」的 wake 没有重发路径：
//!
//! - **H72（P0）**：B192 的 claw 主审注入时 SmartClaw 上游限流，后端在 acceptance **之前**
//!   报错，会话从未启动；但 `WakeIssued` + `ReviewRequested` 已落账。此后同文重发被判
//!   `Idempotent{backend_accepted:false}` 并**重读同一份日志永久重放该错误**，改文重发被
//!   `request digest changed` 拒。**上游一次限流 = 该审查席位在本 attempt 内被永久钉死**，
//!   后端恢复也进不来。最终只能改卡换审查席位，而改卡又打断了 verdict 谱系 → r60 forced 收轮。
//! - **H67**：连派四卡时只有第一张拿到 wake，其余三张各留一个活跃 continuation，
//!   此后任何 wake 都撞 `ambiguous multiple active continuation owners`。
//!   mode 声明 `executor-desktop` 容量 4，**实际有效并发退化为 1**。
//!
//! 首红形态：**compile**。下面导入的两项在 `orch_host::wake` 中尚不存在，
//! rustc 报 `error[E0432]: unresolved imports`。
//! 不得以建同名空壳、改本文件、或把该 test 排除出门的方式伪造红绿。

use orch_host::wake::{wake_fence_decision, ActiveWake, WakeFenceOutcome, WakeIntent};

const DIGEST_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DIGEST_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn review_continuation(task: &str, attempt: &str, role: &str, agent: &str) -> String {
    format!("review:r61:{task}:{attempt}:{role}:{agent}")
}

fn impl_continuation(task: &str, attempt: &str, agent: &str) -> String {
    format!("implementation:r61:{task}:{attempt}:{agent}")
}

fn wake(continuation: &str, agent: &str, accepted: bool, rejected: bool) -> ActiveWake {
    ActiveWake::new(agent, continuation, "wake-1", DIGEST_A, accepted, rejected)
        .expect("fixture ActiveWake 必须可构造")
}

/// **H72 的核心**：后端在 acceptance 前明确报错（`backend_accepted=false` 且已落
/// `ActionRejected` ⇒ `wrapper_exited=true`）⇒ 可证明**无外部副作用**⇒ 必须允许重发。
#[test]
fn a_rejected_never_accepted_wake_is_retryable() {
    let agent = "executor-claw";
    let cont = review_continuation("B192", "B192-A0001", "primary", agent);
    let active = vec![wake(&cont, agent, /* accepted */ false, /* rejected */ true)];

    let outcome = wake_fence_decision(
        &active,
        agent,
        &WakeIntent::continuation(&cont).expect("intent"),
        DIGEST_A,
        1,
    )
    .expect("同 continuation 同摘要不应被拒");

    assert_eq!(
        outcome,
        WakeFenceOutcome::Spawn,
        "后端从未接受且已明确拒绝 ⇒ 必须可重发，不得永久重放那次失败"
    );
}

/// 反向铁律：后端**已接受**（会话真的起来了）时，同文重发仍必须幂等，
/// 绝不能因为本卡就变成「每次都重开一个会话」。
#[test]
fn an_accepted_wake_stays_idempotent() {
    let agent = "executor-opencode";
    let cont = review_continuation("B192", "B192-A0001", "secondary", agent);
    let active = vec![wake(&cont, agent, /* accepted */ true, /* rejected */ false)];

    let outcome = wake_fence_decision(
        &active,
        agent,
        &WakeIntent::continuation(&cont).expect("intent"),
        DIGEST_A,
        1,
    )
    .expect("同 continuation 同摘要不应被拒");

    assert!(
        matches!(outcome, WakeFenceOutcome::Idempotent { .. }),
        "后端已接受时必须保持幂等，实际 {outcome:?}"
    );
}

/// 未接受**但也未被拒**（还在途中）时不得重发——那会造成双开。
#[test]
fn a_pending_wake_is_not_retryable() {
    let agent = "executor-claw";
    let cont = review_continuation("B193", "B193-A0001", "primary", agent);
    let active = vec![wake(&cont, agent, /* accepted */ false, /* rejected */ false)];

    let outcome = wake_fence_decision(
        &active,
        agent,
        &WakeIntent::continuation(&cont).expect("intent"),
        DIGEST_A,
        1,
    )
    .expect("同 continuation 同摘要不应被拒");

    assert_ne!(
        outcome,
        WakeFenceOutcome::Spawn,
        "receipt 尚未到达 ≠ 后端已拒绝；此时重发会双开"
    );
}

/// **H67 的核心**：歧义判据必须按 `(agent, taskId, attemptId)` 定位，
/// 而不是「该 agent 有多于一个活跃 continuation 就拒」。
#[test]
fn multiple_continuations_do_not_block_an_exactly_addressed_one() {
    let agent = "executor-desktop";
    let target = impl_continuation("B198", "B198-A0001", agent);
    let active = vec![
        wake(&impl_continuation("B199", "B199-A0001", agent), agent, true, false),
        wake(&target, agent, true, false),
        wake(&impl_continuation("B200", "B200-A0001", agent), agent, true, false),
    ];

    // 容量 4：三个在飞 continuation 是合法的，且意图精确指名了其中一个。
    let outcome = wake_fence_decision(
        &active,
        agent,
        &WakeIntent::continuation(&target).expect("intent"),
        DIGEST_A,
        4,
    )
    .expect("容量足够且意图精确时不得因「多个活跃 continuation」而拒");

    assert!(
        matches!(outcome, WakeFenceOutcome::Idempotent { .. }),
        "精确指名已接受的 continuation 时应判幂等，实际 {outcome:?}"
    );
}

/// 容量仍然是硬约束：在飞数已达容量时，**新** continuation 必须被拒。
#[test]
fn capacity_is_still_enforced_for_a_new_continuation() {
    let agent = "executor-desktop";
    let active = vec![
        wake(&impl_continuation("B198", "B198-A0001", agent), agent, true, false),
        wake(&impl_continuation("B199", "B199-A0001", agent), agent, true, false),
    ];
    let fresh = impl_continuation("B200", "B200-A0001", agent);

    let refused = wake_fence_decision(
        &active,
        agent,
        &WakeIntent::continuation(&fresh).expect("intent"),
        DIGEST_A,
        2,
    );
    assert!(
        refused.is_err(),
        "在飞数已达容量 2 时不得再开第三个 continuation，实际 {refused:?}"
    );
}

/// 摘要变化仍必须被拒——本卡不放宽「同一 continuation 的请求不可变」这条。
#[test]
fn a_changed_request_digest_is_still_refused() {
    let agent = "executor-opencode";
    let cont = review_continuation("B192", "B192-A0001", "secondary", agent);
    let active = vec![wake(&cont, agent, true, false)];

    let refused = wake_fence_decision(
        &active,
        agent,
        &WakeIntent::continuation(&cont).expect("intent"),
        DIGEST_B,
        1,
    );
    assert!(
        refused.is_err(),
        "同 continuation 改请求摘要必须被拒，实际 {refused:?}"
    );
}

/// 无作用域意图（unscoped）在有活跃 continuation 时仍必须被拒。
#[test]
fn an_unscoped_wake_is_still_refused_when_something_is_active() {
    let agent = "executor-claw";
    let cont = review_continuation("B192", "B192-A0001", "primary", agent);
    let active = vec![wake(&cont, agent, true, false)];

    let refused = wake_fence_decision(&active, agent, &WakeIntent::unscoped(), DIGEST_A, 4);
    assert!(
        refused.is_err(),
        "unscoped wake 在有活跃 continuation 时必须被拒，实际 {refused:?}"
    );
}
