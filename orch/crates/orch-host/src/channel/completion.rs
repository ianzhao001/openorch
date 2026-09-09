//! Execution reasons and conservative, driver-framed failure diagnostics.

use crate::failure::{classify_adapter_outcome, AdapterOutcome, FailureClass, FailureEvidence};
use crate::harness::HarnessId;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The runtime event that ended synchronous observation, independent of exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChannelExitReason {
    /// The child was reaped with an actual OS exit status.
    Exited,
    /// The configured total hard deadline won and initiated owned cleanup.
    HardDeadline,
    /// Observation was incomplete or failed; no death or cancellation is implied.
    ObservationFailed,
}

/// A bounded, redacted diagnostic from a runtime fact or a trusted driver frame.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DriverFailure {
    #[serde(skip)]
    class: FailureClass,
    class_name: &'static str,
    source: String,
    summary: String,
    redacted_frame_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reported_http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reported_error_type: Option<String>,
}

impl DriverFailure {
    /// Stable failure spelling suitable for the manifest's failureClass field.
    pub fn class_name(&self) -> &'static str { self.class_name }

    /// Typed class, retaining Unknown when the available evidence is insufficient.
    pub fn class(&self) -> FailureClass { self.class }

    /// Bounded redacted explanation; the original output is not replaced by it.
    pub fn summary(&self) -> &str { &self.summary }

    fn new(class: FailureClass, source: String, message: &str, frame: Option<&str>) -> Self {
        let redacted = crate::redact::redact_full(message);
        Self {
            class,
            class_name: class.as_str(),
            source,
            summary: redacted.chars().take(512).collect(),
            reported_http_status: None,
            reported_error_type: None,
            redacted_frame_sha256: frame.map(|line| {
                hex::encode(Sha256::digest(crate::redact::redact_full(line).as_bytes()))
            }),
        }
    }
}

fn error_message(error: &Value) -> Option<String> {
    error.pointer("/data/message").or_else(|| error.get("message"))
        .and_then(Value::as_str).map(str::to_string)
        .or_else(|| error.as_str().map(str::to_string))
}

fn http_status(error: &Value) -> Option<u16> {
    ["/statusCode", "/status", "/data/statusCode", "/data/status"]
        .into_iter().find_map(|path| error.pointer(path)?.as_u64()?.try_into().ok())
        .filter(|status| (100..=599).contains(status))
}

fn provider_failure(driver: HarnessId, error: &Value, line: &str) -> DriverFailure {
    let mut message = error_message(error).unwrap_or_else(|| "driver error without a message".into());
    let mut status = http_status(error);
    let mut provider_type = error.get("type").or_else(|| error.get("name"))
        .and_then(Value::as_str).map(str::to_string);
    // Some drivers wrap the provider's typed error JSON in their own error message.
    // This decode happens only inside an already-selected top-level driver error.
    if let Ok(inner) = serde_json::from_str::<Value>(&message) {
        if matches!(inner.get("type").and_then(Value::as_str),
            Some("api_error" | "authentication_error" | "permission_error" | "rate_limit_error" | "overloaded_error")) {
            status = http_status(&inner).or(status);
            provider_type = inner.get("type").and_then(Value::as_str).map(str::to_string);
            if let Some(inner_message) = inner.get("message").and_then(Value::as_str) {
                message = inner_message.to_string();
            }
        }
    }
    let lower = message.to_ascii_lowercase();
    let class = if status == Some(401) || provider_type.as_deref() == Some("authentication_error") {
        FailureClass::Authentication
    } else if status == Some(403) || provider_type.as_deref() == Some("permission_error") {
        FailureClass::Permission
    } else if provider_type.as_deref() == Some("rate_limit_error") {
        FailureClass::RateLimited
    } else if provider_type.as_deref() == Some("overloaded_error") {
        FailureClass::ServiceUnavailable
    } else {
        match classify_adapter_outcome(&FailureEvidence {
            exit_code: Some(1), timed_out: false, http_status: status,
            protocol_error: None, provider_error: Some(&message),
        }) {
            AdapterOutcome::Failure { class: FailureClass::Unknown, .. }
                if lower.contains("upstream response stream failed") => FailureClass::UpstreamStream,
            AdapterOutcome::Failure { class, .. } => class,
            AdapterOutcome::Success => FailureClass::Unknown,
        }
    };
    let mut failure = DriverFailure::new(class, format!("{}:error-frame", driver.as_str()), &message, Some(line));
    // These are reported evidence, not an attribution to the driver, gateway or
    // model. The invocation separately records its requested/effective tuple.
    failure.reported_http_status = status;
    failure.reported_error_type = provider_type.map(|value|
        crate::redact::redact_full(&value).chars().take(128).collect());
    failure
}

/// Classify only runtime stop facts and the selected driver's top-level error frames.
/// Plain text, quoted JSON and tool-result bodies never become provider evidence.
/// Direct executable exit 72 is ordinary status. The code-owned wrapper ABI can
/// independently report timeout; that does not become a local HardDeadline fact.
pub fn classify_driver_failure(
    driver: HarnessId,
    stdout: &str,
    stderr: &str,
    reason: ChannelExitReason,
    exit_code: Option<i32>,
) -> Option<DriverFailure> {
    match reason {
        ChannelExitReason::HardDeadline => return Some(DriverFailure::new(
            FailureClass::Timeout, "runtime:hard-deadline".into(),
            "configured hard deadline reached", None)),
        ChannelExitReason::ObservationFailed => return Some(DriverFailure::new(
            FailureClass::ObservationFailed, "runtime:observation".into(),
            "runtime observation failed; native termination is not established", None)),
        ChannelExitReason::Exited => {}
    }
    for line in stdout.lines() {
        let Ok(frame) = serde_json::from_str::<Value>(line) else { continue };
        let kind = frame.get("type").and_then(Value::as_str);
        let error = match driver {
            HarnessId::OpenCode | HarnessId::Mimo | HarnessId::Claude
                if kind == Some("error") => frame.get("error"),
            _ => None,
        };
        if let Some(error) = error.filter(|value| value.is_object() || value.is_string()) {
            // Tool execution errors do not report provider authentication or quota.
            if error.get("name").and_then(Value::as_str) == Some("ToolError") {
                return Some(DriverFailure::new(FailureClass::Protocol,
                    format!("{}:tool-error-frame", driver.as_str()), "driver reported a tool execution error", Some(line)));
            }
            return Some(provider_failure(driver, error, line));
        }
        if matches!(driver, HarnessId::Claude | HarnessId::Cursor | HarnessId::CodeBuddy)
            && kind == Some("result") && frame.get("is_error").and_then(Value::as_bool) == Some(true) {
            // A driver error-result can include arbitrary model text in result.
            // Do not scan that body to infer a provider error class.
            return Some(DriverFailure::new(FailureClass::Protocol,
                format!("{}:error-result", driver.as_str()), "driver reported an error result", Some(line)));
        }
    }
    let uses_wrapper = [crate::harness::DriverAction::Execute, crate::harness::DriverAction::Review,
        crate::harness::DriverAction::Consult].into_iter()
        .filter_map(|action| driver.driver_contract(action)).any(|contract| contract.wrapper.is_some());
    if uses_wrapper && crate::harness::WRAPPER_EXIT_CODES.iter()
        .any(|entry| Some(entry.code) == exit_code && entry.state == "timedOut") {
        return Some(DriverFailure::new(FailureClass::Timeout,
            format!("{}:wrapper-exit-status", driver.as_str()),
            "code-owned wrapper reported timeout; local deadline and native termination are separate facts", None));
    }
    if exit_code.is_some_and(|code| code != 0) && stdout.trim().is_empty() && !stderr.trim().is_empty() {
        // No stdout and an observed nonzero exit delimit a failed startup.
        // Preserve its diagnostic without scanning free text for auth/quota
        // or claiming that the named driver/provider/model caused the failure.
        return Some(DriverFailure::new(FailureClass::Protocol,
            format!("{}:startup-diagnostic", driver.as_str()), stderr, Some(stderr)));
    }
    (exit_code != Some(0)).then(|| DriverFailure::new(FailureClass::Unknown,
        "runtime:exit-status".into(), &format!("driver process exited with code {exit_code:?} without a trusted error frame"), None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn provider_frames_retain_precise_classes_and_unknown_stays_unknown() {
        for (error, expected) in [
            (json!({"message":"upstream response stream failed"}), "upstream-stream"),
            (json!({"message":"upstream response stream failed","statusCode":401}), "authentication"),
            (json!({"message":"quota exhausted"}), "quotaExhausted"),
            (json!({"message":"too many requests","statusCode":429}), "rateLimited"),
            (json!({"message":"unavailable","statusCode":503}), "serviceUnavailable"),
            (json!({"message":"forbidden","statusCode":403}), "permission"),
            (json!({"message":"opaque diagnostic"}), "unknown"),
        ] {
            let frame = json!({"type":"error","error":error}).to_string();
            let failure = classify_driver_failure(HarnessId::OpenCode,&frame,"",ChannelExitReason::Exited,Some(1)).unwrap();
            assert_eq!(failure.class_name(),expected);
            assert!(failure.redacted_frame_sha256.is_some());
        }
    }

    #[test]
    fn model_text_tool_echo_and_other_driver_grammars_do_not_classify_provider_errors() {
        let quoted = r#"{"type":"error","error":{"message":"unauthorized; quota; upstream response stream failed"}}"#;
        for frame in [
            json!({"type":"text","part":{"text":quoted}}).to_string(),
            json!({"type":"tool_result","output":quoted}).to_string(),
            json!({"type":"result","is_error":false,"result":quoted}).to_string(),
            format!("Example: {quoted}"),
        ] {
            assert!(classify_driver_failure(HarnessId::OpenCode,&frame,"unauthorized",ChannelExitReason::Exited,Some(0)).is_none());
        }
        for driver in [HarnessId::Pi,HarnessId::Dsh,HarnessId::ZCode,HarnessId::Agy,HarnessId::Dclaw,HarnessId::SmartClaw] {
            assert!(classify_driver_failure(driver,quoted,"",ChannelExitReason::Exited,Some(0)).is_none(),"{driver:?}");
        }
        let tool_error = json!({"type":"error","error":{"name":"ToolError","message":"unauthorized"}}).to_string();
        assert_eq!(classify_driver_failure(HarnessId::OpenCode,&tool_error,"",ChannelExitReason::Exited,Some(1)).unwrap().class_name(),"protocol");
        let result = json!({"type":"result","is_error":true,"result":quoted}).to_string();
        assert_eq!(classify_driver_failure(HarnessId::Claude,&result,"",ChannelExitReason::Exited,Some(1)).unwrap().class_name(),"protocol");
    }

    #[test]
    fn explicit_startup_diagnostic_does_not_override_a_successful_answer() {
        let diagnostic = "SqliteError: PRAGMA wal_checkpoint failed";
        assert_eq!(classify_driver_failure(HarnessId::OpenCode,"",diagnostic,ChannelExitReason::Exited,Some(1)).unwrap().class_name(),"protocol");
        assert!(classify_driver_failure(HarnessId::OpenCode,"answer",diagnostic,ChannelExitReason::Exited,Some(0)).is_none());
        for (driver, message) in [
            (HarnessId::OpenCode,"Error: Unexpected error\nFailed query: PRAGMA wal_checkpoint(PASSIVE)"),
            (HarnessId::Mimo,"Error: Unexpected error\nFailed query: PRAGMA wal_checkpoint(PASSIVE)"),
            (HarnessId::Cursor,"Error: Authentication required. Please run 'agent login' first"),
        ] {
            let failure = classify_driver_failure(driver,"",message,ChannelExitReason::Exited,Some(1)).unwrap();
            assert_eq!(failure.class_name(),"protocol");
            assert_eq!(failure.source,format!("{}:startup-diagnostic",driver.as_str()));
            assert!(failure.reported_http_status.is_none());
            assert_eq!(classify_driver_failure(driver,"tool output",message,ChannelExitReason::Exited,Some(1)).unwrap().class_name(),"unknown");
            assert_eq!(classify_driver_failure(driver,"",message,ChannelExitReason::Exited,None).unwrap().class_name(),"unknown");
        }
    }

    #[test]
    fn reported_gateway_evidence_retains_units_and_redacts_secrets_without_claiming_model_limits() {
        let frame = json!({"type":"error","error":{"type":"authentication_error",
            "statusCode":401,"message":"Authorization: Bearer sk-test_abcdefghijklmnopqrstuvwxyz0123456789"}}).to_string();
        let value = serde_json::to_value(classify_driver_failure(HarnessId::OpenCode,
            &frame,"",ChannelExitReason::Exited,Some(1)).unwrap()).unwrap();
        assert_eq!(value["reportedHttpStatus"],401);
        assert_eq!(value["reportedErrorType"],"authentication_error");
        assert_eq!(value["source"],"opencode:error-frame");
        assert!(!value.to_string().contains("abcdefghijklmnopqrstuvwxyz0123456789"));
        assert!(value.get("modelLimit").is_none());
        let timeout = serde_json::to_value(classify_driver_failure(HarnessId::OpenCode,
            &frame,"",ChannelExitReason::HardDeadline,None).unwrap()).unwrap();
        assert_eq!(timeout["source"],"runtime:hard-deadline");
        assert!(timeout.get("reportedHttpStatus").is_none());
    }

    #[test]
    fn reserved_wrapper_timeout_is_distinct_from_direct_exit_and_runtime_deadline() {
        for driver in [HarnessId::SmartClaw, HarnessId::Pi, HarnessId::Dsh, HarnessId::ZCode] {
            let failure = classify_driver_failure(driver,"","",ChannelExitReason::Exited,Some(72)).unwrap();
            assert_eq!(failure.class_name(),"timeout");
            assert_eq!(failure.source,format!("{}:wrapper-exit-status",driver.as_str()));
        }
        for driver in [HarnessId::OpenCode,HarnessId::Claude,HarnessId::Mimo] {
            let failure = classify_driver_failure(driver,"","",ChannelExitReason::Exited,Some(72)).unwrap();
            assert_eq!(failure.class_name(),"unknown");
            assert_eq!(failure.source,"runtime:exit-status");
        }
    }
}
