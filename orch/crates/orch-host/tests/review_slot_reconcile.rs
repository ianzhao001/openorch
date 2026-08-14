//! B173 seed (H44 缺陷 B) — 审查槽位的自愈式对账契约。
//!
//! 背景：`ReviewRequested` 占一个调度槽，只有 `ReviewDelivered` 释放；而
//! `ReviewDelivered` **只由 serve.rs 的 daemon 路径写**，手工 CLI 流程一条都没有
//! （r54/r55/r56 账本实证）。⇒ 槽位只增不减。r57 是 3 卡 × 2 角色 = 每 agent 恰好 3 条，
//! 正好顶满 capacity 3，补发被硬拒，占用者列表里赫然是三条早已交卷的审查。
//!
//! 本 seed 钉死对账的纯判据：**产物已存在 ⇒ 该槽应被释放；已释放的不重复释放。**
//! 判据必须是纯函数——产物存在性由入参传入，scheduler.rs 内不得读盘。
//!
//! M1：无视 `delivered` 集合 ⇒ `an_already_released_slot_is_not_released_twice` 红。
//! M2：只按 taskId 匹配、忽略 role/agent ⇒ `each_role_releases_independently` 红。
//! M3：对没有产物的请求也产出释放 ⇒ `a_pending_review_keeps_its_slot` 红。

use orch_host::scheduler::{reconcile_delivered_reviews, DeliveredReviewKey, ReviewDeliveryPlan};

/// r57 真实形态：同一张卡的两个角色分派给两个不同 agent。
fn requested() -> Vec<DeliveredReviewKey> {
    vec![
        DeliveredReviewKey {
            task_id: "B172".to_string(),
            attempt_id: "B172-A0001".to_string(),
            role: "primary".to_string(),
            agent: "executor-claw".to_string(),
        },
        DeliveredReviewKey {
            task_id: "B172".to_string(),
            attempt_id: "B172-A0001".to_string(),
            role: "secondary".to_string(),
            agent: "executor-opencode".to_string(),
        },
        DeliveredReviewKey {
            task_id: "B173".to_string(),
            attempt_id: "B173-A0001".to_string(),
            role: "primary".to_string(),
            agent: "executor-claw".to_string(),
        },
    ]
}

#[test]
fn a_review_with_an_artifact_releases_its_slot() {
    // 最基本的一格：产物已落盘 ⇒ 该槽应被释放。
    // 这正是 r57 撞墙时缺的那一步——三条早已交卷的审查一直占着槽。
    let present = vec![requested()[0].clone()];
    let plan = reconcile_delivered_reviews(&requested(), &present, &[]);
    assert_eq!(
        plan,
        ReviewDeliveryPlan {
            release: vec![requested()[0].clone()],
        },
        "产物已存在的审查必须被释放"
    );
}

#[test]
fn a_pending_review_keeps_its_slot() {
    // M3：产物尚未落盘的审查**仍在进行中**，槽位必须继续占着。
    // 若这里也释放，容量闸就形同虚设——它保护的正是「同一 agent 同时扛太多审查」。
    let plan = reconcile_delivered_reviews(&requested(), &[], &[]);
    assert_eq!(
        plan,
        ReviewDeliveryPlan { release: vec![] },
        "没有产物的审查必须继续占槽"
    );
}

#[test]
fn an_already_released_slot_is_not_released_twice() {
    // M1：幂等。`wake` 每次准入都会跑一遍对账，若不看已有的 ReviewDelivered，
    // 账本会被同一条释放事件反复污染。
    let present = vec![requested()[0].clone(), requested()[1].clone()];
    let already = vec![requested()[0].clone()];
    let plan = reconcile_delivered_reviews(&requested(), &present, &already);
    assert_eq!(
        plan,
        ReviewDeliveryPlan {
            release: vec![requested()[1].clone()],
        },
        "已有 ReviewDelivered 的不得重复产出"
    );
}

#[test]
fn each_role_releases_independently() {
    // M2：同一张卡的 primary 与 secondary 是**两个不同 agent 的两个槽**。
    // 只按 taskId 匹配会把另一个角色的槽一起释放掉，
    // 于是一个仍在跑的审查被判成已交卷——容量闸失去意义且掩盖真实进度。
    let present = vec![requested()[1].clone()];
    let plan = reconcile_delivered_reviews(&requested(), &present, &[]);
    assert_eq!(
        plan,
        ReviewDeliveryPlan {
            release: vec![requested()[1].clone()],
        },
        "释放必须按 (task, attempt, role, agent) 四元组精确匹配，不得只看 taskId"
    );
}

#[test]
fn an_unrequested_artifact_is_never_released() {
    // 防伪造：盘上出现一份从未被请求过的审查产物（例如手工放进去的、
    // 或上一轮残留的同名文件），不得凭空产出释放事件——
    // 账本只记录**发生过的请求**的终结，不为孤立文件背书。
    let stray = DeliveredReviewKey {
        task_id: "B999".to_string(),
        attempt_id: "B999-A0001".to_string(),
        role: "primary".to_string(),
        agent: "executor-claw".to_string(),
    };
    let plan = reconcile_delivered_reviews(&requested(), &[stray], &[]);
    assert_eq!(
        plan,
        ReviewDeliveryPlan { release: vec![] },
        "未被请求过的产物不得产出释放事件"
    );
}

#[test]
fn release_order_follows_request_order() {
    // 确定性：多条同时可释放时，输出顺序必须跟随请求顺序。
    // 不确定的顺序会让账本字节随运行而变，审查者无法逐字节复现。
    let present = vec![requested()[2].clone(), requested()[0].clone()];
    let plan = reconcile_delivered_reviews(&requested(), &present, &[]);
    assert_eq!(
        plan,
        ReviewDeliveryPlan {
            release: vec![requested()[0].clone(), requested()[2].clone()],
        },
        "释放顺序必须跟随请求顺序，保证账本字节确定"
    );
}
