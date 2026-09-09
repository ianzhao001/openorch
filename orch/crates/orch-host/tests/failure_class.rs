//! ═══ 红种子契约 · B92（typed FailureClass + AdapterOutcome）═══
//! 落位: orch/crates/orch-host/tests/failure_class.rs（逐字节复制）
//! 预期红（redForm: compile）：failure 契约符号尚不存在 → error[E0432]。
//!
//! 负向变异下界：
//! M1 把 quota phrase 当普通 429 → quota_exhaustion_wins_over_rate_limit 红；
//! M2 429/503 不可重试 → transient_provider_failures_are_retryable 红；
//! M3 auth/permission 可重试 → auth_and_permission_fail_closed 红；
//! M4 timeout flag 被日志覆盖 → timeout_is_structural 红；
//! M5 protocol error 被 exit 0 吞掉 → protocol_error_beats_success_exit 红；
//! M6 未知错误默认重试 → unknown_is_non_retryable 红。

use orch_host::failure::{
    classify_adapter_outcome, AdapterOutcome, FailureClass, FailureEvidence,
};

fn evidence<'a>(
    exit_code: Option<i32>,
    timed_out: bool,
    http_status: Option<u16>,
    protocol_error: Option<&'a str>,
    provider_error: Option<&'a str>,
) -> FailureEvidence<'a> {
    FailureEvidence {
        exit_code,
        timed_out,
        http_status,
        protocol_error,
        provider_error,
    }
}

fn failure(outcome: AdapterOutcome) -> (FailureClass, bool) {
    match outcome {
        AdapterOutcome::Failure { class, retryable } => (class, retryable),
        AdapterOutcome::Success => panic!("expected failure"),
    }
}

#[test]
fn clean_exit_is_success() {
    assert_eq!(
        classify_adapter_outcome(&evidence(Some(0), false, None, None, None)),
        AdapterOutcome::Success
    );
}

#[test]
fn quota_exhaustion_wins_over_rate_limit() {
    assert_eq!(
        failure(classify_adapter_outcome(&evidence(
            Some(1),
            false,
            None,
            None,
            Some("quota exceeded / credits exhausted")
        ))),
        (FailureClass::QuotaExhausted, false)
    );
}

#[test]
fn transient_provider_failures_are_retryable() {
    assert_eq!(
        failure(classify_adapter_outcome(&evidence(
            Some(1),
            false,
            Some(429),
            None,
            None
        ))),
        (FailureClass::RateLimited, true)
    );
    assert_eq!(
        failure(classify_adapter_outcome(&evidence(
            Some(1),
            false,
            Some(503),
            None,
            None
        ))),
        (FailureClass::ServiceUnavailable, true)
    );
}

#[test]
fn auth_and_permission_fail_closed() {
    assert_eq!(
        failure(classify_adapter_outcome(&evidence(
            Some(1),
            false,
            None,
            None,
            Some("authentication failed: invalid api key")
        ))),
        (FailureClass::Authentication, false)
    );
    assert_eq!(
        failure(classify_adapter_outcome(&evidence(
            Some(1),
            false,
            None,
            None,
            Some("permission denied by sandbox")
        ))),
        (FailureClass::Permission, false)
    );
}

#[test]
fn timeout_is_structural() {
    assert_eq!(
        failure(classify_adapter_outcome(&evidence(
            None,
            true,
            Some(503),
            None,
            None
        ))),
        (FailureClass::Timeout, true)
    );
}

#[test]
fn protocol_error_beats_success_exit() {
    assert_eq!(
        failure(classify_adapter_outcome(&evidence(
            Some(0),
            false,
            None,
            Some("missing REPORT"),
            None
        ))),
        (FailureClass::Protocol, false)
    );
}

#[test]
fn unknown_is_non_retryable() {
    assert_eq!(
        failure(classify_adapter_outcome(&evidence(
            Some(7),
            false,
            None,
            None,
            Some("something novel")
        ))),
        (FailureClass::Unknown, false)
    );
}
