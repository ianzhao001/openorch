//! B241 / H125 冻结契约测试。**落位后逐字节冻结，永不可改。**
//!
//! 钉死的不变量：进程组隔离（`setpgid` 而非 `setsid`）的组里，fork 窗口内产生的子进程
//! 必须能凭**父子系谱**被判为「被 owned 树包含」；同时 supervisor 的收敛必须有界，
//! 且**安全面一分不放宽**。
//!
//! 背景（r66 实测）：`wake.rs:4578` 的 `isolated_group_leader` 要求 `leader.sid == pgid`
//! （即组长 setsid 过），于是 `.process_group(0)` 创建的组锚点恒假，
//! fork 窗口子永远落到 `pgid {} contains unowned pid {}`；该 ambiguity 进入
//! `converge_residual_tree`（`wake.rs:5454`）一个**没有 deadline** 的 `loop`
//! ⇒ 永不收敛 = 门挂死，且永不自愈。r66 约 2/3 的门因此被打断。
//!
//! 负向变异（每条注入后，其点名的用例必须转红；撤销后复绿）：
//! M1. 把 `ContainedByLineage` 那一支删掉（回到只认会话隔离）
//!     → `fork_window_child_of_an_owned_leader_is_contained_by_lineage` 红
//! M2. 把「pgid 相等」本身当作包含依据（**这是被明确否决的错误论证**：`setpgid` 可迁移）
//!     → `a_process_that_merely_shares_the_pgid_is_never_contained` 红
//! M3. 把系谱判定放宽成「ppid 非零即可」（不要求 ppid ∈ owned）
//!     → `a_process_whose_parent_is_not_owned_is_never_contained` 红
//! M4. 让 uid 或 sid 不匹配的成员也算被包含（放宽既有会话分支的强度）
//!     → `session_containment_still_requires_matching_sid_and_uid` 红
//! M5. 让 owned 集合里的直接成员被误判成 Unowned（破坏最基本的一支）
//!     → `a_directly_owned_member_is_owned_direct` 红
//! M6. 去掉收敛预算（回到无界自旋：`convergence_budget_exhausted` 恒 false）
//!     → `convergence_budget_is_bounded` 红
//! M7. 让预算在 0 elapsed 就判耗尽（把有界做成「一上来就放弃」）
//!     → `convergence_budget_does_not_trip_immediately` 红
//! M8. 放弃消息里不点名 pgid，或漏掉某个未决 pid
//!     → `giveup_message_names_the_pgid_and_every_unresolved_member` 红
//!
//! **没有种子护栏的一条（卡面 §3 已披露）**：「超界放弃时不投任何信号」——
//! `ManagedGroupControl` 是私有 trait，集成测试无法实现，该条只能靠实弹与读码审查兜住。

use std::collections::BTreeSet;
use std::time::Duration;

use orch_host::wake::{
    classify_group_member, convergence_budget_exhausted, convergence_giveup_message,
    MemberContainment,
};

/// 组长：pid=pgid=100，会话仍是父会话（sid=7，**不等于 pgid**）——
/// 这正是 `.process_group(0)`（setpgid 不 setsid）造出来的形态。
const LEADER_PID: u32 = 100;
const LEADER_SID_SETPGID_ONLY: u32 = 7;
const UID: u32 = 501;

/// 会话隔离形态：组长自己 setsid 过，sid == pid == pgid。
const ISOLATED_LEADER_PID: u32 = 200;

fn owned(pids: &[u32]) -> BTreeSet<u32> {
    pids.iter().copied().collect()
}

#[test]
fn a_directly_owned_member_is_owned_direct() {
    // M5：最基本的一支——成员自己就在 owned 里。
    let set = owned(&[LEADER_PID, 101]);
    let got = classify_group_member(
        101,
        LEADER_PID,
        LEADER_PID,
        LEADER_SID_SETPGID_ONLY,
        UID,
        LEADER_PID,
        LEADER_SID_SETPGID_ONLY,
        UID,
        &set,
    );
    assert_eq!(
        got,
        MemberContainment::OwnedDirect,
        "owned 集合里的成员必须判 OwnedDirect"
    );
}

#[test]
fn fork_window_child_of_an_owned_leader_is_contained_by_lineage() {
    // M1：本卡的核心。组长只 setpgid 未 setsid（sid != pgid），
    // 子进程在 KILL 投递窗内被 fork 出来、还来不及进 owned；
    // 但它的 ppid 就是 owned 的组长 ⇒ 必须凭系谱判为被包含。
    let set = owned(&[LEADER_PID]);
    let got = classify_group_member(
        /* member_pid  */ 101,
        /* member_ppid */ LEADER_PID,
        /* member_pgid */ LEADER_PID,
        /* member_sid  */ LEADER_SID_SETPGID_ONLY,
        /* member_uid  */ UID,
        /* leader_pid  */ LEADER_PID,
        /* leader_sid  */ LEADER_SID_SETPGID_ONLY,
        /* leader_uid  */ UID,
        &set,
    );
    assert_eq!(
        got,
        MemberContainment::ContainedByLineage,
        "fork 窗口子的 ppid 是 owned 组长 ⇒ 必须判 ContainedByLineage，\
         而不是 Unowned（后者会让所有权证明永不收敛 = 门挂死）"
    );
}

#[test]
fn a_process_that_merely_shares_the_pgid_is_never_contained() {
    // M2：**被明确否决的错误论证**。setpgid(2) 可以把任意进程迁进本组，
    // 所以「pgid 相等」本身绝不构成包含依据。
    // 该成员 pgid 与组长一致、sid/uid 也一致，但 ppid 不在 owned 里。
    let set = owned(&[LEADER_PID]);
    let got = classify_group_member(
        /* member_pid  */ 999,
        /* member_ppid */ 1, // 被 init 收养 / 或本就与 owned 树无关
        /* member_pgid */ LEADER_PID,
        /* member_sid  */ LEADER_SID_SETPGID_ONLY,
        /* member_uid  */ UID,
        /* leader_pid  */ LEADER_PID,
        /* leader_sid  */ LEADER_SID_SETPGID_ONLY,
        /* leader_uid  */ UID,
        &set,
    );
    assert_eq!(
        got,
        MemberContainment::Unowned,
        "仅仅 pgid 相同不构成包含——setpgid 可迁移，安全面不得因此放宽"
    );
}

#[test]
fn a_process_whose_parent_is_not_owned_is_never_contained() {
    // M3：系谱必须要求 ppid ∈ owned，而不是「ppid 非零」。
    let set = owned(&[LEADER_PID]);
    let got = classify_group_member(
        /* member_pid  */ 777,
        /* member_ppid */ 555, // 非零，但不在 owned 里
        /* member_pgid */ LEADER_PID,
        /* member_sid  */ LEADER_SID_SETPGID_ONLY,
        /* member_uid  */ UID,
        /* leader_pid  */ LEADER_PID,
        /* leader_sid  */ LEADER_SID_SETPGID_ONLY,
        /* leader_uid  */ UID,
        &set,
    );
    assert_eq!(
        got,
        MemberContainment::Unowned,
        "父进程不在 owned 里就不构成系谱包含"
    );
}

#[test]
fn session_containment_still_requires_matching_sid_and_uid() {
    // M4：既有的会话隔离分支不得被放宽。组长 setsid 过（sid == pid == pgid），
    // 成员 sid 匹配且 uid 匹配 ⇒ 被会话包含；uid 一旦不匹配 ⇒ 立刻不包含。
    let set = owned(&[ISOLATED_LEADER_PID]);

    let contained = classify_group_member(
        201,
        1, // 注意：即便 ppid 已被 init 收养，会话论证仍独立成立
        ISOLATED_LEADER_PID,
        ISOLATED_LEADER_PID,
        UID,
        ISOLATED_LEADER_PID,
        ISOLATED_LEADER_PID,
        UID,
        &set,
    );
    assert_eq!(
        contained,
        MemberContainment::ContainedBySession,
        "会话隔离组内 sid/uid 双匹配的成员仍应判 ContainedBySession"
    );

    let foreign_uid = classify_group_member(
        202,
        1,
        ISOLATED_LEADER_PID,
        ISOLATED_LEADER_PID,
        UID + 1, // uid 不匹配
        ISOLATED_LEADER_PID,
        ISOLATED_LEADER_PID,
        UID,
        &set,
    );
    assert_eq!(
        foreign_uid,
        MemberContainment::Unowned,
        "uid 不匹配必须打破会话包含——既有安全强度不得被放宽"
    );
}

#[test]
fn convergence_budget_is_bounded() {
    // M6：收敛必须有界。存在某个有限 elapsed 使预算判为耗尽——
    // 这正是「无界自旋 = 永不自愈的挂死」的反面。
    assert!(
        convergence_budget_exhausted(Duration::from_secs(3_600)),
        "收敛预算必须有上界：跑满一小时仍不判耗尽就是无界自旋"
    );
}

#[test]
fn convergence_budget_does_not_trip_immediately() {
    // M7：有界不能做成「一上来就放弃」——那会把正常的短暂 ambiguity 变成误报。
    assert!(
        !convergence_budget_exhausted(Duration::ZERO),
        "elapsed=0 时不得判耗尽"
    );
    assert!(
        !convergence_budget_exhausted(Duration::from_millis(250)),
        "一个 250ms 重试周期内不得判耗尽"
    );
}

#[test]
fn giveup_message_names_the_pgid_and_every_unresolved_member() {
    // M8：放弃必须是**可判别**的——不是静默 break，也不是笼统一句话。
    let message = convergence_giveup_message(4321, &[4322, 4399]);
    assert!(
        message.contains("4321"),
        "放弃消息必须点名 pgid；实得：{message}"
    );
    for pid in ["4322", "4399"] {
        assert!(
            message.contains(pid),
            "放弃消息必须逐个点名未决成员 {pid}；实得：{message}"
        );
    }
}
