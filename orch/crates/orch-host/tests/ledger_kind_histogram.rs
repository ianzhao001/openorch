//! ═══ 红种子契约 · B61（账本事件类型直方图纯折叠）═══
//! 落位: orch/crates/orch-host/tests/ledger_kind_histogram.rs（逐字节复制）
//! 预期红（redForm: compile）：`kind_histogram` 尚不存在 → error[E0432]。
//!
//! 背景：`ledger` 已有 `ts_health`/`ts_health_line` 等只读探针，但缺一个「事件类型分布」
//! 折叠，供 doctor/status 展示 event mix。本棒 additive——不改 `event()`/`append()`/
//! 任何既有测试、不新增依赖。
//!
//! ⚠️ 确定性注意：`ledger::event()` 会读环境变量 `ORCH_PLANNER_WAKE_ID` 并附带预算
//! reservation（src/ledger.rs），故本种子**直接用结构体字面量构造 EventRecord**，不经
//! `event()`，保证纯函数无环境依赖。
//!
//! 目标契约（落在 orch_host::ledger，模块已 pub 导出，勿动 lib.rs）：
//!  - pub fn kind_histogram(events: &[orch_core::EventRecord])
//!        -> std::collections::BTreeMap<String, usize>
//!      · 按 `ev.kind`（即 JSON `type`）逐条计数；未知/任意类型字符串原样保留计入；
//!      · 空切片 ⇒ 空 map；BTreeMap ⇒ key 确定性有序。
//!
//! 负向变异下界（转绿后逐条自证）：
//!  M1 不按 kind 分组（按 actor / 恒定 key）⇒ counts_by_kind 红；
//!  M2 过滤掉未知类型 ⇒ unknown_kinds_counted_verbatim 红；
//!  M3 空切片不返回空 map（panic/默认插值）⇒ empty_is_empty 红。

use orch_core::EventRecord;
use orch_host::ledger::kind_histogram;

fn ev(kind: &str) -> EventRecord {
    EventRecord {
        event_id: "id".to_string(),
        ts: "2026-01-01T00:00:00Z".to_string(),
        actor: "runtime:orch".to_string(),
        kind: kind.to_string(),
        task_id: None,
        round: None,
        payload: None,
        extra: serde_json::Map::new(),
    }
}

#[test]
fn counts_by_kind() {
    let h = kind_histogram(&[
        ev("DispatchIssued"),
        ev("VerdictIssued"),
        ev("DispatchIssued"),
    ]);
    assert_eq!(h.get("DispatchIssued"), Some(&2));
    assert_eq!(h.get("VerdictIssued"), Some(&1));
    assert_eq!(h.len(), 2);
}

#[test]
fn unknown_kinds_counted_verbatim() {
    let h = kind_histogram(&[ev("TotallyMadeUpKind"), ev("TotallyMadeUpKind")]);
    assert_eq!(h.get("TotallyMadeUpKind"), Some(&2));
}

#[test]
fn empty_is_empty() {
    assert!(kind_histogram(&[]).is_empty());
}
