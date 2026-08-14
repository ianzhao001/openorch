//! ═══ 红种子契约 · B54（Committee 预设首切片）═══
//! 落位: orch/crates/orch-host/tests/preset_committee.rs（逐字节复制）
//! 预期红（redForm: compile）：下列 API 尚不存在 → error[E0433]/error[E0425]。
//!
//! 背景：Parallel 预设首切片（B49 plan_waves）已落地并行分波纯函数；Committee
//! 预设长期顺延。本棒是 Committee 首切片——两个纯函数 + 两个类型，机器化
//! 「委员会扇出计划」与「法定票数裁决」，不接线 daemon、不改任何既有行为。
//!
//! 目标契约（全部落在 orch_host::preset，模块已 pub 导出，勿动 lib.rs）：
//!  - pub struct CommitteePlan { pub members: Vec<String>, pub quorum: usize }
//!      派生 Debug + PartialEq。
//!  - pub enum CommitteeOutcome { Approved, Rejected }
//!      派生 Debug + PartialEq。
//!  - pub fn plan_committee(agents: &[String], quorum: usize)
//!        -> Result<CommitteePlan, String>
//!      · members = agents 去重后按「首次出现」顺序保留；
//!      · quorum 必须落在 1..=members.len()（去重后成员数）内，否则 Err（文案含
//!        实际 quorum 与允许上界，便于 planner 定位）；
//!      · 合法时 CommitteePlan{ members, quorum }。
//!  - pub fn committee_verdict(votes: &[bool], quorum: usize) -> CommitteeOutcome
//!      · true（赞成）票数 >= quorum ⇒ Approved；否则 Rejected；
//!      · 阈值是「达到即通过」（>=，不是 >）。
//!
//! 负向变异下界（转绿后逐条自证）：
//!  M1 plan_committee 不校验 quorum（接受 0 或 > 成员数）
//!       ⇒ plan_committee_validates_quorum_bounds 红；
//!  M2 members 不去重或不保序
//!       ⇒ plan_committee_dedups_preserving_first_seen_order 红；
//!  M3 committee_verdict 用 > 代 >=，或统计错票
//!       ⇒ committee_verdict_uses_inclusive_quorum_threshold 红。

use orch_host::preset::{committee_verdict, plan_committee, CommitteeOutcome, CommitteePlan};

fn agents(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|s| s.to_string()).collect()
}

#[test]
fn plan_committee_validates_quorum_bounds() {
    // quorum = 0 非法
    assert!(plan_committee(&agents(&["a", "b", "c"]), 0).is_err());
    // quorum 超过成员数非法
    assert!(plan_committee(&agents(&["a", "b", "c"]), 4).is_err());
    // 边界内合法：下界 1 与上界 = 成员数
    assert!(plan_committee(&agents(&["a", "b", "c"]), 1).is_ok());
    assert!(plan_committee(&agents(&["a", "b", "c"]), 3).is_ok());
}

#[test]
fn plan_committee_dedups_preserving_first_seen_order() {
    let plan = plan_committee(&agents(&["a", "b", "a", "c", "b"]), 2).unwrap();
    assert_eq!(
        plan,
        CommitteePlan {
            members: agents(&["a", "b", "c"]),
            quorum: 2,
        }
    );
    // 去重后成员数是 quorum 上界的判据：去重后 3 个成员，quorum 3 合法、4 非法
    assert!(plan_committee(&agents(&["a", "b", "a", "c", "b"]), 3).is_ok());
    assert!(plan_committee(&agents(&["a", "b", "a", "c", "b"]), 4).is_err());
}

#[test]
fn committee_verdict_uses_inclusive_quorum_threshold() {
    // 2 赞成 / quorum 2 ⇒ 达到即通过
    assert_eq!(
        committee_verdict(&[true, true, false], 2),
        CommitteeOutcome::Approved
    );
    // 2 赞成 / quorum 3 ⇒ 不足
    assert_eq!(
        committee_verdict(&[true, true, false], 3),
        CommitteeOutcome::Rejected
    );
    // 单票场景
    assert_eq!(committee_verdict(&[true], 1), CommitteeOutcome::Approved);
    assert_eq!(committee_verdict(&[false], 1), CommitteeOutcome::Rejected);
}
