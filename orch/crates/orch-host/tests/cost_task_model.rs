//! ═══ 红种子契约 · B52 ═══（落位: orch/crates/orch-host/tests/cost_task_model.rs，逐字节复制）
//! 预期红（redForm: compile）：`cost::task_model` 尚不存在 → error[E0425]，文件级编译红。
//! 背景：B45(r26) 已把 REPORT §0 的 EnvDecl 并入 ReportObserved payload（envDecl.model/depth/
//!   capture）。cost 报表要能按棒回答「这棒到底是谁跑的」——本棒实现账本折叠内核：
//!   `pub fn task_model(events: &[orch_core::EventRecord], task: &str) -> Option<String>`
//!   —— 该 task **最后一条** ReportObserved 的 payload.envDecl.model（返工重收以最新为准）；
//!   无 ReportObserved / 无 envDecl / model 空串 → None（老轮账本无 §0，不得虚构）。
//!   报表列接线（RoundCost/打印面加 model 列）由实现自行设计，种子只钉折叠内核。
//! 变异清单（E9 下界，负向自证逐条注入应各自变红）：
//!   M1 取首条而非末条 → `rework_latest_report_wins` 红；
//!   M2 无 envDecl 虚构默认值 → `missing_env_decl_is_none` 红；
//!   M3 不过滤 taskId → `other_task_report_ignored` 红；
//!   M4 空 model 当有效 → `empty_model_is_none` 红。
use orch_host::cost;
use orch_host::ledger::event;
use serde_json::json;

fn report(task: &str, model: &str) -> orch_core::EventRecord {
    event(
        "ReportObserved",
        "runtime:orch",
        Some(task),
        Some("r27"),
        json!({"reportPath": format!("reports/{task}-REPORT.md"),
               "envDecl": {"model": model, "depth": "high", "capture": "log"}}),
    )
}

#[test]
fn env_decl_model_is_extracted() {
    let events = vec![report("B52", "kimi-k3")];
    assert_eq!(cost::task_model(&events, "B52").as_deref(), Some("kimi-k3"));
}

#[test]
fn rework_latest_report_wins() {
    // M1：返工重收后以最新 ReportObserved 为准
    let events = vec![report("B52", "old-model"), report("B52", "kimi-k3")];
    assert_eq!(cost::task_model(&events, "B52").as_deref(), Some("kimi-k3"));
}

#[test]
fn missing_env_decl_is_none() {
    // M2：老轮账本无 §0 → None，不得虚构
    let events = vec![event(
        "ReportObserved",
        "runtime:orch",
        Some("B52"),
        Some("r27"),
        json!({"reportPath": "reports/B52-REPORT.md"}),
    )];
    assert!(cost::task_model(&events, "B52").is_none());
}

#[test]
fn other_task_report_ignored() {
    // M3：只认本 task
    let events = vec![report("B51", "glm-5.2")];
    assert!(cost::task_model(&events, "B52").is_none());
}

#[test]
fn empty_model_is_none() {
    // M4：空串不是模型身份
    let events = vec![report("B52", "")];
    assert!(cost::task_model(&events, "B52").is_none());
}
