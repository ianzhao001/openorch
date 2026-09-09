//! ═══ 红种子契约 · B89（verifier verdict 末行精确解析）═══
//! 落位: orch/crates/orch-host/tests/verdict_last_line.rs（逐字节复制）
//! 预期红（redForm: compile）：`verify::parse_verdict_stream` 尚不存在 → error[E0432]。
//!
//! 负向变异下界：
//! M1 扫到较早 result 的 PASS 就接受 → only_last_result_event_counts 红；
//! M2 contains 子串接受 → substring_is_not_a_verdict 红；
//! M3 任意行接受、非最后非空行 → verdict_must_be_last_nonempty_line 红；
//! M4 同一最终 result 有多个裁决仍接受 → conflicting_or_duplicate_verdicts_are_rejected 红；
//! M5 run_verify 继续使用旧内联 contains parser → verifier 代码审查 FAIL。

use orch_host::verify::parse_verdict_stream;

fn event(kind: &str, result: serde_json::Value) -> String {
    serde_json::json!({"type": kind, "result": result}).to_string()
}

#[test]
fn only_last_result_event_counts() {
    let stream = format!(
        "{}\n{}\n",
        event("result", serde_json::json!("VERDICT: PASS")),
        event(
            "result",
            serde_json::json!("final narrative without verdict")
        ),
    );
    assert!(parse_verdict_stream(&stream).is_err());
}

#[test]
fn exact_last_nonempty_line_is_accepted() {
    for verdict in ["PASS", "FAIL", "BLOCKED"] {
        let stream = event(
            "result",
            serde_json::json!(format!("checked facts\n\nVERDICT: {verdict}\n")),
        );
        assert_eq!(parse_verdict_stream(&stream).unwrap(), verdict);
    }
}

#[test]
fn substring_is_not_a_verdict() {
    for bad in [
        "prefix VERDICT: PASS",
        "VERDICT: PASS suffix",
        "VERDICT: PASS.",
        "verdict: PASS",
        "VERDICT: MAYBE",
    ] {
        assert!(
            parse_verdict_stream(&event("result", serde_json::json!(bad))).is_err(),
            "{bad}"
        );
    }
}

#[test]
fn verdict_must_be_last_nonempty_line() {
    let stream = event(
        "result",
        serde_json::json!("VERDICT: PASS\ntrailing narrative"),
    );
    assert!(parse_verdict_stream(&stream).is_err());
}

#[test]
fn conflicting_or_duplicate_verdicts_are_rejected() {
    for bad in [
        "VERDICT: PASS\nVERDICT: FAIL",
        "VERDICT: PASS\nVERDICT: PASS",
    ] {
        assert!(
            parse_verdict_stream(&event("result", serde_json::json!(bad))).is_err(),
            "{bad}"
        );
    }
}

#[test]
fn later_non_result_events_do_not_replace_last_result() {
    let stream = format!(
        "{}\n{}\n",
        event("result", serde_json::json!("notes\nVERDICT: BLOCKED")),
        event("assistant", serde_json::json!("ignored")),
    );
    assert_eq!(parse_verdict_stream(&stream).unwrap(), "BLOCKED");
}

#[test]
fn non_string_last_result_is_protocol_error() {
    let stream = format!(
        "{}\n{}\n",
        event("result", serde_json::json!("VERDICT: PASS")),
        event("result", serde_json::json!({"verdict": "FAIL"})),
    );
    assert!(parse_verdict_stream(&stream).is_err());
}
