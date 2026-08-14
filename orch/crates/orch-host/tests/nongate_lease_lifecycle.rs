//! B225 seeded-red contract: 非门机械建场端到端——租约有终点、席位永隔离（H110）。
//!
//! **v2（r65 重铸）**：v1（a6c246a0…）的 `event_with_id` 给包括 `RoundClosed` 退役锚点在内的
//! 所有事件无条件写 `taskId`，与 B224 已冻结的 `retirement_anchor_matches` 反伪造契约
//! （只接受 `task_id.is_none()` 的 RoundClosed）机械互斥 ⇒ 执行者 r64 诚实 BLOCKED。
//! v2 增加 `round_scoped_event`（不写 taskId），`RoundClosed` 锚点改走它——**不放宽 B224**。
//! （r64 曾铸 706547c4… 的 v2，本体佚失于收轮清理，r65 按 force-policy-answers §1 的
//! 逐字语义重铸；差异披露在卡面 §3。）
//!
//! Expected red: compile. `NONGATE_REVIEW_ROLE` 在 B225 之前不存在（该角色字符串目前是
//! 散落的魔法串；本卡把它收编为 wake 许可链的单一事实源）。
//! 依赖 B224 的 `SITE_RETIRED_EVENT_KIND` 与 fold 第三释放臂（B224 已于 r64 Recorded）。
//!
//! 事实基线（本卡卡面已订正 backlog H110 的过时描述）：`SiteRole::Nongate` 的
//! parse/建场/租约/续会话链在 r63/B207（d565aa1）已落地；本卡补的是许可、通道、
//! 路径与生命周期终点的**接线**，不是造门。
//!
//! Negative mutations that must turn the named case red:
//! M1. round close 清扫不覆盖 nongate role（终点缺失 ⇒ 非门租约永久 Active）
//!     -> `nongate_lease_releases_via_close_sweep_site_retired` 红。
//! M2. 放宽「非门永不满足正式审查席位」
//!     -> `nongate_never_satisfies_a_formal_review_slot` 红。
//! M3. 许可链绕开集中角色常量、another 魔法串漂移
//!     -> `nongate_review_role_string_is_the_single_source` 红。
//! M4. 退役锚点改回 task-scoped（`RoundClosed` 带 taskId）
//!     -> `nongate_lease_releases_via_close_sweep_site_retired` 红（B224 反伪造契约拒收）。

use orch_core::EventRecord;
use orch_host::sites::{site_identity, LeaseState, SiteRole, SITE_RETIRED_EVENT_KIND};
use orch_host::wake::NONGATE_REVIEW_ROLE;

const ROUND: &str = "r64";
const TASK: &str = "B903";
const ATTEMPT: &str = "B903-A0001";
const HEAD: &str = "0123456789012345678901234567890123456789";

fn event_with_id(event_id: &str, kind: &str, payload: serde_json::Value) -> EventRecord {
    serde_json::from_value(serde_json::json!({
        "eventId": event_id,
        "ts": "2026-08-04T00:00:00Z",
        "actor": "runtime:orch",
        "type": kind,
        "round": ROUND,
        "taskId": TASK,
        "payload": payload,
    }))
    .expect("构造事件失败")
}

/// v2：round 级事件**不写 taskId**——`RoundClosed` 退役锚点必须是 round-scoped，
/// 这是 B224 `retirement_anchor_matches` 的反伪造契约（task-scoped 的 RoundClosed 拒收）。
fn round_scoped_event(event_id: &str, kind: &str, payload: serde_json::Value) -> EventRecord {
    serde_json::from_value(serde_json::json!({
        "eventId": event_id,
        "ts": "2026-08-04T00:00:00Z",
        "actor": "runtime:orch",
        "type": kind,
        "round": ROUND,
        "payload": payload,
    }))
    .expect("构造事件失败")
}

fn nongate_lease(generation: u32) -> EventRecord {
    event_with_id(
        &format!("EV-LEASE-NG-{generation}"),
        "WorkspaceLeased",
        serde_json::json!({
            "siteId": format!("{TASK}-nongate-executor-pi-g{generation:02}"),
            "generation": generation,
            "attemptId": ATTEMPT,
            "role": "nongate",
            "agent": "executor-pi",
            "reviewedHead": HEAD,
            "paths": {
                "worktree": format!(".worktrees/{TASK}-nongate-executor-pi-g{generation:02}"),
                "target": format!("orch/target/review-{ATTEMPT}-nongate-executor-pi-g{generation:02}"),
            },
        }),
    )
}

#[test]
fn nongate_lease_releases_via_close_sweep_site_retired() {
    // M1：收轮清扫为非门现场落 SiteRetired（trigger=round-close，锚定 RoundClosed）
    //     ⇒ fold 判 Released。非门没有 verdict/delivery 事件，这是它唯一的机械终点。
    // M4：锚点必须 round-scoped（v1 在此写了 taskId，被 B224 反伪造契约拒收——即执行者
    //     BLOCKED 的直接原因；v2 修正为 round_scoped_event）。
    let events = vec![
        nongate_lease(1),
        round_scoped_event("EV-RC-1", "RoundClosed", serde_json::json!({"forced": false})),
        event_with_id(
            "EV-RETIRE-NG-1",
            SITE_RETIRED_EVENT_KIND,
            serde_json::json!({
                "siteId": format!("{TASK}-nongate-executor-pi-g01"),
                "generation": 1,
                "taskId": TASK,
                "attemptId": ATTEMPT,
                "role": "nongate",
                "agent": "executor-pi",
                "trigger": "round-close",
                "retireEventId": "EV-RC-1",
            }),
        ),
    ];
    let state = LeaseState::of(
        &events,
        &site_identity(TASK, SiteRole::Nongate, "executor-pi"),
        1,
    );
    assert!(
        matches!(state, LeaseState::Released { .. }),
        "close 清扫的 SiteRetired 必须能释放非门租约，得到 {state:?}"
    );
}

#[test]
fn nongate_without_endpoint_stays_active_forever() {
    // 终点缺失 ⇒ 永久 Active（fail-closed 方向）；这就是「机械建场先于终点事件」
    // 会制造的状态——本测试同时钉住 B225 依赖 B224 终点事件的排序理由。
    let events = vec![nongate_lease(1)];
    assert!(
        matches!(
            LeaseState::of(&events, &site_identity(TASK, SiteRole::Nongate, "executor-pi"), 1),
            LeaseState::Active { .. }
        ),
        "无终点事件的非门租约必须判活跃"
    );
}

#[test]
fn nongate_never_satisfies_a_formal_review_slot() {
    // M2：既有契约不回退（B207 落地）。
    assert!(!SiteRole::Nongate.satisfies_formal_review_slot());
    assert!(SiteRole::Primary.satisfies_formal_review_slot());
    assert!(SiteRole::Secondary.satisfies_formal_review_slot());
}

#[test]
fn nongate_review_role_string_is_the_single_source() {
    // M3：许可链（scheduling capacities roles）消费的角色词收编为常量。
    assert_eq!(NONGATE_REVIEW_ROLE, "nongate-review");
}
