//! B230 seeded-red contract: `ReportCollect` 在 `Executing` 阶段失去持有者后必须存在
//! 机械恢复路径（H117，P0 恢复面；用户 2026-08-05 裁定修法 ①+③「作废重来」）。
//!
//! Expected red: compile。`orch_host::tierf::plan_expired_executing_recovery` 与
//! `ExpiredExecutingRecovery` 在 B230 之前不存在——本卡交付的恢复决策函数即编译红锚定。
//!
//! 契约背景（r64 实测，force-policy-answers-r64.md §1-2，账本有 ActionRejected 为证）：
//! 阶段序列 `Claimed → Executing → Executed → Completed` 中唯 `Executing` 无出口：
//! 续收过期后 bail（tierf.rs:1475-1477）、`--new-attempt` 只看 phase（attempt.rs:1354-1360）、
//! in-lock release 要求原持有者（tierf.rs:1685-1687）。而 fold 层**已允许** Executing→Released
//! （attempt.rs:4106-4115 前驱表），`Claimed` 过期恢复（tierf.rs:1479-1517）已示范
//! 「用锚点自身 owner/generation 写 Released + 同事务新代 reclaim」在生产中合法运行。
//!
//! 本种子钉的是**纯决策面**（不产事件、不落账、不碰文件系统）：
//! 给定最新 durable 锚点与当前时刻，产出「作废重来」计划——
//! release 沿用锚点 owner/generation（Claimed 过期分支同构），标 `outcomeUnknown`（门跑到
//! 哪一步不明，绝不洗白），reclaim 铸全新 owner/generation 与未来租约（新代重跑门）。
//! 生产接线（tierf.rs Executing 分支在租约过期时改走本函数并同事务落账
//! `ReportCollectReleased{outcomeUnknown:true}` + 新代 `ReportCollectClaimed`）由卡面
//! requiredEvidence 钉住，不在本种子内断言。
//!
//! Negative mutations that must turn the named case red:
//! M1. release 用调用者新造 owner/generation（伪造持有者即可释放他人 Executing）
//!     -> `expired_executing_yields_anchor_scoped_release_with_outcome_unknown` 红。
//! M2. `outcome_unknown` 恒 false（把「结果不明」洗成正常释放）
//!     -> `expired_executing_yields_anchor_scoped_release_with_outcome_unknown` 红。
//! M3. 租约存续也放行作废（fail-closed 被拆掉，活着的持有者被顶）
//!     -> `live_lease_refuses_recovery` 红。
//! M4. reclaim 复用锚点 owner/generation（新代不新，门结果与旧代混淆）
//!     -> `reclaim_is_a_fresh_generation_with_future_lease` 红。
//! M5. 非 `Executing` 锚点也放行（本函数越权处理其他阶段）
//!     -> `non_executing_anchor_is_rejected` 红。

use std::time::Duration;

use orch_core::EventRecord;
use orch_host::tierf::{plan_expired_executing_recovery, ExpiredExecutingRecovery};

const ROUND: &str = "r65";
const TASK: &str = "B904";
const ANCHOR_OWNER: &str = "01JEXAMPLEOWNER0000000000A";
const ANCHOR_GENERATION: &str = "01JEXAMPLEGENERATION00000A";
const LEASE_UNTIL: &str = "2026-01-01T00:00:00Z";

fn collect_anchor(kind: &str) -> EventRecord {
    serde_json::from_value(serde_json::json!({
        "eventId": "EV-COLLECT-ANCHOR-1",
        "ts": "2025-12-31T23:00:00Z",
        "actor": "runtime:orch",
        "type": kind,
        "round": ROUND,
        "taskId": TASK,
        "payload": {
            "actionId": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            "agent": "executor-desktop",
            "attemptId": "B904-A0001",
            "attemptNo": 1,
            "owner": ANCHOR_OWNER,
            "leaseGeneration": ANCHOR_GENERATION,
            "leaseUntil": LEASE_UNTIL,
        },
    }))
    .expect("构造锚点事件失败")
}

fn lease_deadline() -> std::time::SystemTime {
    humantime::parse_rfc3339(LEASE_UNTIL).expect("解析 leaseUntil 失败")
}

#[test]
fn expired_executing_yields_anchor_scoped_release_with_outcome_unknown() {
    // M1/M2：release 必须沿用锚点自身 owner/generation（tierf.rs:1479-1517 的 Claimed
    // 过期先例同构——只有这样 fold 的 owner 等值校验才承认这次释放），且必须标
    // outcomeUnknown（门跑到哪一步不明，不得洗白成正常释放）。
    let anchor = collect_anchor("ReportCollectExecuting");
    let now = lease_deadline() + Duration::from_secs(1);
    let plan: ExpiredExecutingRecovery =
        plan_expired_executing_recovery(&anchor, now).expect("过期 Executing 必须可恢复");
    assert_eq!(plan.release_owner, ANCHOR_OWNER, "release 必须沿用锚点 owner");
    assert_eq!(
        plan.release_generation, ANCHOR_GENERATION,
        "release 必须沿用锚点 generation"
    );
    assert!(plan.outcome_unknown, "过期作废必须标 outcomeUnknown（不得洗白门结果）");
}

#[test]
fn reclaim_is_a_fresh_generation_with_future_lease() {
    // M4：新代必须真的新——owner/generation 都不得复用锚点值，租约终点在 now 之后
    // （代价是新代重跑门，远优于永久死锁）。
    let anchor = collect_anchor("ReportCollectExecuting");
    let now = lease_deadline() + Duration::from_secs(1);
    let plan = plan_expired_executing_recovery(&anchor, now).expect("过期 Executing 必须可恢复");
    assert!(!plan.reclaim_owner.is_empty(), "reclaim owner 不得为空");
    assert!(!plan.reclaim_generation.is_empty(), "reclaim generation 不得为空");
    assert_ne!(plan.reclaim_owner, ANCHOR_OWNER, "reclaim owner 必须是全新铸造");
    assert_ne!(
        plan.reclaim_generation, ANCHOR_GENERATION,
        "reclaim generation 必须是全新一代"
    );
    let reclaim_until = humantime::parse_rfc3339(&plan.reclaim_lease_until)
        .expect("reclaim_lease_until 必须是 RFC3339");
    assert!(reclaim_until > now, "新代租约终点必须在当前时刻之后");
}

#[test]
fn live_lease_refuses_recovery() {
    // M3：租约存续 ⇒ 拒绝作废（Busy 语义保持，fail-closed 不放松）。
    let anchor = collect_anchor("ReportCollectExecuting");
    let now = lease_deadline() - Duration::from_secs(60);
    assert!(
        plan_expired_executing_recovery(&anchor, now).is_err(),
        "租约存续期间必须拒绝作废重来"
    );
}

#[test]
fn non_executing_anchor_is_rejected() {
    // M5：本函数只服务 Executing 死锁面；Claimed 过期有自己的恢复路径（tierf.rs:1479-1517），
    // 不得被本函数越权接管。
    let anchor = collect_anchor("ReportCollectClaimed");
    let now = lease_deadline() + Duration::from_secs(1);
    assert!(
        plan_expired_executing_recovery(&anchor, now).is_err(),
        "非 Executing 锚点必须拒绝"
    );
}
