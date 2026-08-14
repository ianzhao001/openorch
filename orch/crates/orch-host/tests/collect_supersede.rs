//! ═══ 红种子契约 · B55（O12 残留：纠正性重派后旧收取失效判定）═══
//! 落位: orch/crates/orch-host/tests/collect_supersede.rs（逐字节复制）
//! 预期红（redForm: compile）：下列 API 尚不存在 → error[E0432]/error[E0425]。
//!
//! 背景：O12（m0/dryrun-errata.md）——纠正性重派（改派/re-dispatch）后，旧
//! `await-report` 收取进程仍持启动时旧快照，误判机检（B47/B53 两次踩坑）。B50 已
//! 修「收取时刻重读账本」的一半；残留一半=「重派后必须判定旧收取已失效并终止」。
//! 本棒把该判定机器化为**纯账本折叠**：给定事件序列，返回被后续重派取代（superseded）
//! 的 `DispatchIssued` 事实，供调用方据此终止对应旧收取进程。**不真正杀进程**
//! （杀进程属调用方职责、且会引入环境依赖），本切片只做纯决策函数。
//!
//! 目标契约（落在 orch_host::tierf，模块已 pub 导出，勿动 lib.rs）：
//!  - pub struct SupersededDispatch {
//!        pub task_id: String,
//!        pub dispatch_event_id: String,
//!        pub agent: Option<String>,
//!    }  派生 Debug + PartialEq。
//!  - pub fn superseded_dispatches(events: &[orch_core::EventRecord]) -> Vec<SupersededDispatch>
//!      · 按 task 分组所有 `DispatchIssued`；
//!      · 某 task 有 >= 2 条 `DispatchIssued`（重派）时，除**最后一条**外全部 superseded；
//!      · 单条或零条 dispatch 的 task 不产出；
//!      · 返回按事件在账本中出现的先后顺序；
//!      · agent 取该 `DispatchIssued.payload.agent`（缺失则 None）。
//!
//! 负向变异下界（转绿后逐条自证）：
//!  M1 只按「有 >=2 dispatch」而返回**全部**（含最后一条）
//!       ⇒ keeps_latest_dispatch_live 红；
//!  M2 不分 task（跨 task 混判取代）
//!       ⇒ supersession_is_per_task 红；
//!  M3 单条 dispatch 也判 superseded
//!       ⇒ single_dispatch_is_never_superseded 红。

use orch_core::EventRecord;
use orch_host::ledger::event;
use orch_host::tierf::{superseded_dispatches, SupersededDispatch};
use serde_json::json;

fn dispatch(task: &str, agent: &str) -> EventRecord {
    event(
        "DispatchIssued",
        "runtime:orch",
        Some(task),
        Some("r29"),
        json!({ "agent": agent, "baseSha": "deadbeef" }),
    )
}

#[test]
fn single_dispatch_is_never_superseded() {
    let events = vec![dispatch("B90", "executor-opencode")];
    assert!(superseded_dispatches(&events).is_empty());
}

#[test]
fn keeps_latest_dispatch_live() {
    let first = dispatch("B91", "executor-opencode");
    let second = dispatch("B91", "executor-desktop");
    let events = vec![first.clone(), second.clone()];

    let sup = superseded_dispatches(&events);
    assert_eq!(
        sup,
        vec![SupersededDispatch {
            task_id: "B91".to_string(),
            dispatch_event_id: first.event_id.clone(),
            agent: Some("executor-opencode".to_string()),
        }]
    );
    // 最后一条（second）必须仍活着 —— 不在 superseded 列表内
    assert!(sup.iter().all(|s| s.dispatch_event_id != second.event_id));
}

#[test]
fn supersession_is_per_task() {
    // B92 重派三次；B93 单派一次穿插其间。
    let d1 = dispatch("B92", "a");
    let other = dispatch("B93", "x");
    let d2 = dispatch("B92", "b");
    let d3 = dispatch("B92", "c");
    let events = vec![d1.clone(), other, d2.clone(), d3.clone()];

    let ids: Vec<String> = superseded_dispatches(&events)
        .into_iter()
        .map(|s| {
            assert_eq!(s.task_id, "B92"); // B93 单派不得入列
            s.dispatch_event_id
        })
        .collect();
    // 前两条 B92 dispatch 被取代，按账本顺序；d3（最后）保留。
    assert_eq!(ids, vec![d1.event_id.clone(), d2.event_id.clone()]);
}
