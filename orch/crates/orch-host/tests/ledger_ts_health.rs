//! ═══ 红种子契约 · B25 ═══（落位: orch/crates/orch-host/tests/ledger_ts_health.rs，逐字节复制）
//! 预期红（redForm: compile）：ledger::ts_health / TsHealth 尚不存在（E0425，文件级编译红）。
//! 背景：B23 REPORT §6 自省②——run_tick 对「好行坏 ts」按 now 保守处理，TTL 保护静默失效
//!   且无告警。本棒补健康度探针：合法 JSON 行里 ts 缺失/不可解析(RFC3339) 的计数+行号，
//!   供 doctor/status 接线曝光（坏 JSON 行归既有坏行检查，不归本探针）。
//! 变异清单（E9 下界）: M1 bad_ts 恒 0 → ② 红；M2 坏 JSON 行也计入 bad_ts → ③ 红；M3 行号按 0 起算 → ② 红（bad_lines 断言）
use orch_host::ledger;

const GOOD1: &str = r#"{"eventId":"e1","ts":"2026-07-23T03:00:00Z","actor":"a","type":"RoundOpened","round":"rX","payload":{}}"#;
const GOOD2: &str = r#"{"eventId":"e2","ts":"2026-07-23T03:01:00Z","actor":"a","type":"PlanSignedOff","round":"rX","payload":{}}"#;

#[test]
fn all_good_is_zero() {
    // ① 全好账：total=2，bad_ts=0
    let h = ledger::ts_health(&format!("{GOOD1}\n{GOOD2}\n"));
    assert_eq!(h.total, 2);
    assert_eq!(h.bad_ts, 0);
    assert!(h.bad_lines.is_empty());
}

#[test]
fn unparseable_ts_counted_with_line_no() {
    // ② 好行坏 ts：计数 1，行号（1 起算）= 2
    let bad = r#"{"eventId":"e2","ts":"not-a-time","actor":"a","type":"X","round":"rX","payload":{}}"#;
    let h = ledger::ts_health(&format!("{GOOD1}\n{bad}\n"));
    assert_eq!(h.bad_ts, 1);
    assert_eq!(h.bad_lines, vec![2]);
}

#[test]
fn broken_json_not_counted_here() {
    // ③ 坏 JSON 行不归本探针（坏行检查已覆盖）：total 只数合法 JSON 行，bad_ts 不受扰
    let h = ledger::ts_health(&format!("{GOOD1}\n{{not-json\n"));
    assert_eq!(h.total, 1);
    assert_eq!(h.bad_ts, 0);
}

#[test]
fn missing_ts_field_is_bad() {
    // ④ 合法 JSON 但缺 ts 字段 → 计入 bad_ts
    let nots = r#"{"eventId":"e3","actor":"a","type":"X","round":"rX","payload":{}}"#;
    let h = ledger::ts_health(&format!("{nots}\n"));
    assert_eq!(h.total, 1);
    assert_eq!(h.bad_ts, 1);
}
