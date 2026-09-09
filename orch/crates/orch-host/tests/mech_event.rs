//! ═══ 红种子契约 · B19 ═══（落位: orch/crates/orch-host/tests/mech_event.rs，逐字节复制）
//! 预期红（redForm: compile）：mech::failure_event 尚不存在。
//! 变异清单（E9 下界）: M1 kind 写错（非 MechCheckFailed）→ ① 红；M2 payload 丢 reason → ② 红；M3 stage 丢失 → ② 红
use orch_host::mech;

#[test]
fn failure_event_shape() {
    // ① kind/actor/taskId 形状
    let ev = mech::failure_event("B9", "r13", "domain", "文件域越界: x.rs");
    assert_eq!(ev.kind, "MechCheckFailed");
    assert_eq!(ev.actor, "runtime:orch");
    assert_eq!(ev.task_id.as_deref(), Some("B9"));
    assert_eq!(ev.round.as_deref(), Some("r13"));
}

#[test]
fn failure_event_payload_carries_stage_and_reason() {
    // ② payload {stage, reason} 齐全
    let ev = mech::failure_event("B9", "r13", "red-replay-claim", "报数造假");
    let p = ev.payload.expect("payload");
    assert_eq!(p["stage"], "red-replay-claim");
    assert_eq!(p["reason"], "报数造假");
}

#[test]
fn event_id_is_ulid_like() {
    // ③ eventId 26 位 ULID（复用 ledger::event 通道，非手写 evt- 前缀）
    let ev = mech::failure_event("B9", "r13", "domain", "x");
    assert_eq!(ev.event_id.len(), 26);
}
