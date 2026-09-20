//! B363 frozen structured-diagnostic contract. Compile-red before the shared
//! diagnostic API exists; production fixtures cover observation and Fusion use.
use orch_host::consult::{ChannelDiagnostic, DiagnosticCode};

fn full() -> ChannelDiagnostic {
    ChannelDiagnostic {
        code: DiagnosticCode::ProviderFailure,
        failure_class: Some("protocol".into()),
        stage: Some("completed".into()),
        reason: Some("bounded reason".into()),
        duration_secs: Some(7),
        deadline_secs: Some(900),
        exit_code: Some(1),
        terminal_status: Some("failed".into()),
        observed_model: Some("opaque-model".into()),
        stdout_overflow: Some(false),
        stderr_overflow: Some(false),
    }
}

#[test]
fn wire_shape_is_additive_and_camel_case() {
    let value = serde_json::to_value(full()).unwrap();
    assert_eq!(value["code"], "provider_failure");
    assert_eq!(value["failureClass"], "protocol");
    assert_eq!(value["deadlineSecs"], 900);
    assert_eq!(value["terminalStatus"], "failed");
}

#[test]
fn capture_evidence_missing_has_a_distinct_code() {
    let mut diagnostic = full();
    diagnostic.code = DiagnosticCode::CaptureEvidenceMissing;
    diagnostic.failure_class = None;
    diagnostic.reason = None;
    let value = serde_json::to_value(diagnostic).unwrap();
    assert_eq!(value["code"], "capture_evidence_missing");
    assert!(value.get("failureClass").is_none());
}

#[test]
fn diagnostic_reason_is_sanitized_and_bounded() {
    let diagnostic = ChannelDiagnostic::provider_failure(
        Some("protocol"),
        Some("\u{1b}[31mfailed\u{1b}[0m\nAPI_KEY=secret"),
        None,
    );
    let reason = diagnostic.reason.unwrap();
    assert_eq!(reason, "[redacted]");
    assert!(reason.len() <= 2048);
}

#[test]
fn absent_optional_facts_are_not_invented() {
    let diagnostic = ChannelDiagnostic::capture_evidence_missing();
    let value = serde_json::to_value(diagnostic).unwrap();
    for key in ["failureClass", "reason", "exitCode", "observedModel"] {
        assert!(value.get(key).is_none(), "{key}");
    }
}

#[test]
fn overflow_and_deadline_facts_remain_independent() {
    let value = serde_json::to_value(full()).unwrap();
    assert_eq!(value["stdoutOverflow"], false);
    assert_eq!(value["stderrOverflow"], false);
    assert_eq!(value["durationSecs"], 7);
    assert_eq!(value["deadlineSecs"], 900);
}
