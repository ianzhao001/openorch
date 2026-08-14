//! ═══ 红种子契约 · B42 ═══（落位: orch/crates/orch-host/tests/serve_dead_or_stalled.rs，逐字节复制）
//! 预期红（redForm: compile）：serve::dead_or_stalled_tasks 尚不存在（E0425），文件级编译红。
//! 背景：design/11 §3/§6 backlog——daemon serve_tick 判「执行者判死/判滞」注入主控，但 B39 的
//!   compute_tick_inputs 把 dead_or_stalled 传空(serve.rs:135 `Vec::new()`)，此路永不触发。
//!   本棒补一个纯函数：从账本 EscalationRaised(payload.stage=liveness-dead|liveness-stalled) 信号
//!   判出当前判死/判滞的任务列表，并接入 compute_tick_inputs。信号即 tierf.rs:180/199 落的账
//!   (复用既有 liveness judge 落账，勿重造 liveness 判定)；ResumeIssued/NudgeIssued/ReportObserved/
//!   TaskRecorded 视为处置/恢复，清除对应任务的判死态(镜像 serve::pending_reasons 的 issue/clear 语义)。
//! 变异清单（E9 下界，负向自证逐条注入应各自变红）：
//!   M1 不看 stage(把所有 EscalationRaised 都算) → `non_liveness_escalation_is_ignored` 红；
//!   M2 忽略恢复事件(不清除) → `resume_or_report_clears_flag` 红；
//!   M3 只认 dead 丢 stalled → `stalled_stage_is_flagged` 红；
//!   M4 不去重/不保序 → `dedup_and_preserve_first_seen_order` 红。
use orch_host::ledger::event;
use orch_host::serve;
use serde_json::json;

fn esc(task: &str, stage: &str) -> orch_core::EventRecord {
    event("EscalationRaised", "runtime:orch", Some(task), Some("r24"), json!({"stage": stage}))
}

#[test]
fn dead_stage_is_flagged() {
    let events = vec![esc("B36", "liveness-dead")];
    assert_eq!(serve::dead_or_stalled_tasks(&events), vec!["B36".to_string()]);
}

#[test]
fn stalled_stage_is_flagged() {
    // M3：liveness-stalled 与 liveness-dead 同等入列
    let events = vec![esc("B40", "liveness-stalled")];
    assert_eq!(serve::dead_or_stalled_tasks(&events), vec!["B40".to_string()]);
}

#[test]
fn non_liveness_escalation_is_ignored() {
    // M1：其它 EscalationRaised(in-flight TTL / oracle 等)不是 liveness 信号，不得入列
    let events = vec![
        esc("B10", "in-flight-ttl"),
        event("EscalationRaised", "runtime:orch", Some("B11"), Some("r24"), json!({"reason": "no stage"})),
    ];
    assert!(serve::dead_or_stalled_tasks(&events).is_empty());
}

#[test]
fn resume_or_report_clears_flag() {
    // M2：判死后被 RESUME / 执行者带 REPORT 回归 → 清除，不再判死
    let resumed = vec![
        esc("B36", "liveness-dead"),
        event("ResumeIssued", "runtime:orch", Some("B36"), Some("r24"), json!({})),
    ];
    assert!(serve::dead_or_stalled_tasks(&resumed).is_empty());

    let reported = vec![
        esc("B40", "liveness-stalled"),
        event("ReportObserved", "runtime:orch", Some("B40"), Some("r24"), json!({})),
    ];
    assert!(serve::dead_or_stalled_tasks(&reported).is_empty());
}

#[test]
fn reflag_after_recovery_when_it_dies_again() {
    // 恢复后再次判死应重新入列(逐事件推进语义，非一锤定音)
    let events = vec![
        esc("B36", "liveness-dead"),
        event("ResumeIssued", "runtime:orch", Some("B36"), Some("r24"), json!({})),
        esc("B36", "liveness-dead"),
    ];
    assert_eq!(serve::dead_or_stalled_tasks(&events), vec!["B36".to_string()]);
}

#[test]
fn dedup_and_preserve_first_seen_order() {
    // M4：同一任务多次判死只列一次；多任务按首次判死顺序列出
    let events = vec![
        esc("B36", "liveness-dead"),
        esc("B40", "liveness-stalled"),
        esc("B36", "liveness-dead"),
    ];
    assert_eq!(
        serve::dead_or_stalled_tasks(&events),
        vec!["B36".to_string(), "B40".to_string()]
    );
}
