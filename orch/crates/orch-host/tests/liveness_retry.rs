//! ═══ 红种子契约 · B76（Phase D/D2 · liveness-dead 有界指数退避重试 + 双派 guard）═══
//! 落位: orch/crates/orch-host/tests/liveness_retry.rs（逐字节复制）
//! 预期红（redForm: compile）：`serve::{decide_liveness_retry, RetryDecision, count_dead_attempts}`
//!   尚不存在 → error[E0432]。
//!
//! 背景（Phase D · D2）：liveness-dead ≠ 断言/代码失败——它常是**瞬时**的（B70 实证：同一 session
//! 后来自愈）。现 Rust 侧无任何执行者自动重试：判死即 terminal（CLI exit3/wave blocker），"改派上限
//! →BLOCKED" 全在 planner 协议文档。本棒把"无 provider 报错的通道未咬合"做**有界指数退避自动重试**，
//! 只有断言/代码失败才交改派/BLOCKED——镜像既有 `decide_planner_liveness`（serve.rs）的重试计数模式。
//!
//! ⚠️ 安全 guard（D4 已大幅减少假死，此为兜底）：只有**确无 worktree/活动痕迹**（真没起步）才自动
//! 重派；若已有活动＝执行者可能还活着，**不得**自动重派（否则双重派发一个其实在环的执行者）。
//!
//! 目标契约（落在 orch_host::serve，模块已 pub 导出，勿动 lib.rs、不新增依赖）：
//!   (1) #[derive(Debug, PartialEq)] pub enum RetryDecision { RetryAfter(u64 /*secs*/), GiveUp }
//!   (2) pub fn decide_liveness_retry(dead_attempts: u8, max_retries: u8, base_secs: u64, had_activity: bool) -> RetryDecision
//!         · had_activity 为真 ⇒ GiveUp（双派 guard，优先于一切）；
//!         · 否则 dead_attempts < max_retries ⇒ RetryAfter(base_secs << dead_attempts)（指数：base*2^n）；
//!         · 否则（dead_attempts >= max_retries）⇒ GiveUp。
//!   (3) pub fn count_dead_attempts(events: &[orch_core::EventRecord], task: &str) -> u8
//!         逐条数 kind=="AttemptCrashed" ∧ taskId==task。
//!
//! 负向变异下界（转绿后逐条自证）：
//!   M1 线性非指数（base*dead_attempts）⇒ backoff_is_exponential 红；
//!   M2 边界 off-by-one（<= max_retries 才 GiveUp）⇒ gives_up_at_max 红；
//!   M3 忽略 had_activity guard（有活动仍重派）⇒ had_activity_gives_up 红；
//!   M4 count 不按 task 过滤 ⇒ count_filters_by_task 红。

use orch_core::EventRecord;
use orch_host::serve::{count_dead_attempts, decide_liveness_retry, RetryDecision};

fn crashed(task: &str) -> EventRecord {
    EventRecord {
        event_id: "id".to_string(),
        ts: "2026-07-25T00:00:00Z".to_string(),
        actor: "runtime:orch".to_string(),
        kind: "AttemptCrashed".to_string(),
        task_id: Some(task.to_string()),
        round: Some("r38".to_string()),
        payload: None,
        extra: serde_json::Map::new(),
    }
}
fn other(kind: &str, task: &str) -> EventRecord {
    EventRecord {
        event_id: "id".to_string(),
        ts: "2026-07-25T00:00:00Z".to_string(),
        actor: "runtime:orch".to_string(),
        kind: kind.to_string(),
        task_id: Some(task.to_string()),
        round: Some("r38".to_string()),
        payload: None,
        extra: serde_json::Map::new(),
    }
}

// 指数退避：base=30 → 30/60/120（30<<0/1/2）
#[test]
fn backoff_is_exponential() {
    assert_eq!(decide_liveness_retry(0, 3, 30, false), RetryDecision::RetryAfter(30));
    assert_eq!(decide_liveness_retry(1, 3, 30, false), RetryDecision::RetryAfter(60));
    assert_eq!(decide_liveness_retry(2, 3, 30, false), RetryDecision::RetryAfter(120));
}

// 达到 max_retries 即 GiveUp（3 >= 3）
#[test]
fn gives_up_at_max() {
    assert_eq!(decide_liveness_retry(3, 3, 30, false), RetryDecision::GiveUp);
}

// 双派 guard：有活动痕迹一律 GiveUp（即便次数未到）
#[test]
fn had_activity_gives_up() {
    assert_eq!(decide_liveness_retry(0, 3, 30, true), RetryDecision::GiveUp);
}

// count 只数本 task 的 AttemptCrashed
#[test]
fn count_filters_by_task() {
    let events = [
        crashed("B76"),
        other("DispatchIssued", "B76"),
        crashed("B99"),
        crashed("B76"),
    ];
    assert_eq!(count_dead_attempts(&events, "B76"), 2);
    assert_eq!(count_dead_attempts(&events, "B99"), 1);
    assert_eq!(count_dead_attempts(&[], "B76"), 0);
}
