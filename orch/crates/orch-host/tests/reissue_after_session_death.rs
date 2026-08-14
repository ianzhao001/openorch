//! B246 · 审查 wake 的**重投出口**（H132②）——「已接受但会话已死」是 B200 没覆盖的第五态。
//!
//! # 为什么现在才需要这条路径
//!
//! r61/B200 已经交付了 continuation fence 的粒度与可重试性，它的冻结种子
//! `wake_fence_granularity.rs` 钉死了四条基线：
//!
//! | 后端状态 | `wake_fence_decision` 判定 | 语义 |
//! |---|---|---|
//! | 未接受 + 已明确拒绝 | `Spawn` | 可证明无外部副作用 ⇒ 可重发（H72） |
//! | **已接受** | `Idempotent` | 会话真的起来了 ⇒ 幂等，绝不双开 |
//! | 未接受 + 未拒绝（在途） | 非 `Spawn` | receipt 未到 ≠ 已拒绝，重发会双开 |
//! | 多 continuation | 按 (agent,task,attempt) 精确定位 | H67 |
//!
//! **第五态没人管**：wake 曾被后端接受（`backend_accepted = true`），但该 managed 会话
//! **随后终止了**（截断 / 被 cancel / 自然退出）。此时 `wake_fence_decision` 只能答
//! `Idempotent`——因为按它拿到的五个参数，这和"会话正活着"完全无法区分。
//! 调用方拿到 `Idempotent` 且 backend receipt 已 reconcile 后直接 `return Ok(())`：
//! **无 spawn、无事件、无 `ActionRejected`、CLI 照常报成功**。审查席就此静默钉死。
//!
//! # 本卡的解法（以及明确不做的事）
//!
//! 仓里**已经有**表达"会话已死"的类型：`SessionDeathEvidence::from_terminal_facts()`
//! 只在 `managed_scope_terminated == true` 且分类不是 `OperationalError` 时才构造得出，
//! 并且它自带一个 `exempts_request_digest()` 钩子——**该钩子的生产调用者至今为零**
//! （r61/B192 造好了证据类型，却从没把它接进任何判定）。本卡就是接这条线。
//!
//! **不改 `wake_fence_decision`**：它的签名与四条基线由 B200 冻结种子逐字钉死，
//! 动它 = 冻结种子转红 = 整棒作废（铁律 10）。重投判定是**调用方层的新函数**，
//! 它在需要时调用既有 fence，但不改变 fence 自己的任何答案。
//!
//! **不做 H115 的 nudge 半片**：H115 的实撞是 nudge 打一个**活着的** BLOCKED 会话，
//! 那个场景**永远构造不出** `SessionDeathEvidence`（前提是 scope 已终止）。
//! 那半片需要 `--supersede`（新 CLI + 新事件 + 作废旧 continuation 语义），是另一张卡。
//! **本卡不得声称关闭 H115。**

use orch_host::wake::{
    reissue_decision, ActiveWake, ManagedWakeTerminationFacts, ReissueOutcome,
    SessionDeathEvidence, WakeIntent,
};

const DIGEST_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DIGEST_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn review_continuation(task: &str, attempt: &str, role: &str, agent: &str) -> String {
    format!("review:r68:{task}:{attempt}:{role}:{agent}")
}

fn wake(continuation: &str, agent: &str, wake_id: &str, accepted: bool) -> ActiveWake {
    ActiveWake::new(agent, continuation, wake_id, DIGEST_A, accepted, false)
        .expect("fixture ActiveWake 必须可构造")
}

/// 真实的终态事实：managed scope 已终止且自然退出 ⇒ 证据可构造。
fn dead(wake_id: &str, agent: &str) -> SessionDeathEvidence {
    let facts = ManagedWakeTerminationFacts {
        wake_id: wake_id.to_string(),
        agent: agent.to_string(),
        completion_reason: Some("terminal".to_string()),
        terminal_seen: true,
        exited_naturally: true,
        hard_deadline_reached: false,
        cancel_request_id: None,
        signals: Vec::new(),
        managed_scope_terminated: true,
        log_bytes_read: 4096,
    };
    SessionDeathEvidence::from_terminal_facts(&facts)
        .expect("managedScopeTerminated=true 且非 OperationalError ⇒ 证据必须构造得出")
}

/// **本卡的核心**：已接受但会话已终止 ⇒ 必须给出重投出口，而不是静默幂等。
#[test]
fn an_accepted_wake_whose_session_died_is_reissuable() {
    let agent = "executor-claw";
    let cont = review_continuation("B246", "B246-A0001", "primary", agent);
    let active = vec![wake(&cont, agent, "wake-dead-1", /* accepted */ true)];

    let outcome = reissue_decision(
        &active,
        agent,
        &WakeIntent::continuation(&cont).expect("intent"),
        DIGEST_A,
        1,
        Some(&dead("wake-dead-1", agent)),
    )
    .expect("同 continuation 同摘要 + 死亡证据不应被拒");

    assert_eq!(
        outcome,
        ReissueOutcome::Reissue {
            superseded_wake_id: "wake-dead-1".to_string()
        },
        "已接受但 scope 已终止 ⇒ 必须可重投，且必须点名被取代的 wakeId"
    );
}

/// **不得静默**：没有死亡证据时必须**响亮拒绝**，而不是 `Ok(())` 装作已注入。
/// 这一条正是 H132② 的伤害面——CLI 报成功而账本一片空白。
#[test]
fn an_accepted_wake_without_death_evidence_is_refused_loudly() {
    let agent = "executor-opencode";
    let cont = review_continuation("B246", "B246-A0001", "secondary", agent);
    let active = vec![wake(&cont, agent, "wake-live-1", /* accepted */ true)];

    let outcome = reissue_decision(
        &active,
        agent,
        &WakeIntent::continuation(&cont).expect("intent"),
        DIGEST_A,
        1,
        None,
    )
    .expect("缺证据是业务拒绝，不是入参错误");

    match outcome {
        ReissueOutcome::Refused { ref reason } => {
            assert!(
                !reason.trim().is_empty(),
                "拒绝必须带非空理由，planner 要据它决策"
            );
        }
        other => panic!("活会话不得被重投，实际 {other:?}"),
    }
}

/// 证据必须**精确绑定到被重投的那个 wake**；张冠李戴不授权任何副作用。
#[test]
fn death_evidence_for_another_wake_does_not_authorize_reissue() {
    let agent = "executor-zcode";
    let cont = review_continuation("B246", "B246-A0001", "secondary", agent);
    let active = vec![wake(&cont, agent, "wake-target", /* accepted */ true)];

    let outcome = reissue_decision(
        &active,
        agent,
        &WakeIntent::continuation(&cont).expect("intent"),
        DIGEST_A,
        1,
        Some(&dead("wake-someone-else", agent)),
    )
    .expect("身份不匹配是业务拒绝");

    assert!(
        matches!(outcome, ReissueOutcome::Refused { .. }),
        "死亡证据的 wakeId 与目标不符时不得重投，实际 {outcome:?}"
    );
}

/// **把 B192 造好却从未接线的钩子真正接上**：会话已死时，改文重投不再被
/// `request digest changed` 拦住——这正是 `exempts_request_digest()` 的设计意图。
/// 注意语义边界：豁免**只在有死亡证据时**成立，活会话改文仍必须被拒（下一条）。
#[test]
fn a_dead_session_exempts_the_request_digest() {
    let agent = "executor-claw";
    let cont = review_continuation("B246", "B246-A0002", "primary", agent);
    let active = vec![wake(&cont, agent, "wake-dead-2", /* accepted */ true)];
    let evidence = dead("wake-dead-2", agent);
    assert!(
        evidence.exempts_request_digest(),
        "证据类型自述可豁免摘要——本卡要让这个自述真正被生产路径消费"
    );

    let outcome = reissue_decision(
        &active,
        agent,
        &WakeIntent::continuation(&cont).expect("intent"),
        DIGEST_B, // 与在飞 wake 的 DIGEST_A 不同
        1,
        Some(&evidence),
    )
    .expect("死亡证据在场时改文不应被摘要 fence 拒");

    assert_eq!(
        outcome,
        ReissueOutcome::Reissue {
            superseded_wake_id: "wake-dead-2".to_string()
        },
        "会话已死 ⇒ 改文重投合法（B192 钩子的既定语义）"
    );
}

/// 反向铁律：**活**会话改文仍必须被拒。豁免的前提是"已死"，不是"想换材料"。
/// 若这条转绿，说明实现把 fence 整个放宽了，等于给 planner 开了中途换审查材料的后门。
#[test]
fn a_live_session_still_refuses_a_changed_digest() {
    let agent = "executor-opencode";
    let cont = review_continuation("B246", "B246-A0002", "secondary", agent);
    let active = vec![wake(&cont, agent, "wake-live-2", /* accepted */ true)];

    let outcome = reissue_decision(
        &active,
        agent,
        &WakeIntent::continuation(&cont).expect("intent"),
        DIGEST_B,
        1,
        None,
    );

    match outcome {
        Ok(ReissueOutcome::Refused { .. }) | Err(_) => {}
        Ok(other) => panic!("活会话改文必须被拒，实际 {other:?}"),
    }
}

/// B200 的第三条基线在新路径下必须原样成立：在途（未接受未拒绝）永不可重投，
/// 即便调用方错误地塞进一份别处来的死亡证据也不行——在途意味着外部副作用未知。
#[test]
fn a_pending_wake_is_never_reissuable() {
    let agent = "executor-claw";
    let cont = review_continuation("B246", "B246-A0003", "primary", agent);
    let active = vec![wake(&cont, agent, "wake-pending", /* accepted */ false)];

    let outcome = reissue_decision(
        &active,
        agent,
        &WakeIntent::continuation(&cont).expect("intent"),
        DIGEST_A,
        1,
        Some(&dead("wake-pending", agent)),
    )
    .expect("在途是业务拒绝");

    assert!(
        matches!(outcome, ReissueOutcome::Refused { .. }),
        "receipt 未到达时外部副作用未知，任何证据都不得授权重投，实际 {outcome:?}"
    );
}
