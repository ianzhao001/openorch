//! ═══ 红种子契约 · B50 ═══（落位: orch/crates/orch-host/tests/dispatch_refresh.rs，逐字节复制）
//! 预期红（redForm: compile）：`tierf::latest_dispatch_facts` 尚不存在 → error[E0425]/E0433，
//!   文件级编译红。
//! 背景：O12（r26/B47 事故）——纠正性重派后，长命 await-report 进程仍持有启动时读到的旧
//!   DispatchIssued 基线（62ce02a），对按新基线（1a34655）开分支的执行者误判机检 FAIL。
//!   「真值在账本，不在内存」被跨等待的内存快照破坏。本棒两件事：
//!   ① 纯函数抽取：`pub fn latest_dispatch_facts(events: &[orch_core::EventRecord], task: &str)
//!      -> Option<serde_json::Value>`——返回该 task **最后一条** DispatchIssued 的 payload（clone）；
//!   ② 接线：await-report 在 REPORT 观测成功后、机检开始前，**重读账本**并用本函数刷新
//!      baseSha/agent/goPath 三事实（替换启动时快照）。
//! 变异清单（E9 下界，负向自证逐条注入应各自变红）：
//!   M1 取首条而非末条 → `two_dispatches_latest_wins` 红；
//!   M2 无派发时返 Some/panic → `no_dispatch_returns_none` 红；
//!   M3 不过滤 taskId(拿别的任务) → `other_task_dispatch_ignored` 红；
//!   M4 不过滤 kind(拿非 DispatchIssued) → `non_dispatch_events_ignored` 红。
use orch_host::ledger::event;
use orch_host::tierf;
use serde_json::json;

fn dispatch(task: &str, base: &str) -> orch_core::EventRecord {
    event(
        "DispatchIssued",
        "runtime:orch",
        Some(task),
        Some("r27"),
        json!({"agent": "executor-x", "baseSha": base, "goPath": format!("go/{task}.md")}),
    )
}

#[test]
fn two_dispatches_latest_wins() {
    // M1：纠正性重派后必须以最后一条为准（B47 事故根因）
    let events = vec![dispatch("B47", "62ce02a"), dispatch("B47", "1a34655")];
    let p = tierf::latest_dispatch_facts(&events, "B47").expect("有派发应返 Some");
    assert_eq!(p.get("baseSha").and_then(|v| v.as_str()), Some("1a34655"));
}

#[test]
fn no_dispatch_returns_none() {
    // M2：从未派发 → None，不得虚构
    let events = vec![event("ReportObserved", "runtime:orch", Some("B47"), Some("r27"), json!({}))];
    assert!(tierf::latest_dispatch_facts(&events, "B47").is_none());
}

#[test]
fn other_task_dispatch_ignored() {
    // M3：只认本 task 的派发
    let events = vec![dispatch("B46", "aaaaaaa")];
    assert!(tierf::latest_dispatch_facts(&events, "B47").is_none());
}

#[test]
fn non_dispatch_events_ignored() {
    // M4：kind 必须是 DispatchIssued（同 task 其它事件带 baseSha 也不算）
    let events = vec![event(
        "NudgeIssued",
        "runtime:orch",
        Some("B47"),
        Some("r27"),
        json!({"baseSha": "bbbbbbb"}),
    )];
    assert!(tierf::latest_dispatch_facts(&events, "B47").is_none());
}
