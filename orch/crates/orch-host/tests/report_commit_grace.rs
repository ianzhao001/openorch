//! 红种子契约 · B107 · REPORT 写盘到 commit 的有界等待。
//! 预期红：compile，缺少 ReportCommitState/ReportCommitDecision/decide_report_commit_wait。
//! M1：MissingFromHead 立即 Reject 而非 Wait，uncommitted_inside_grace_waits 红。
//! M2：超时仍 Wait，uncommitted_at_deadline_rejects 红。
//! M3：字节不同仍 Ready，different_bytes_never_ready 红。

use std::time::Duration;

use orch_host::tierf::{
    decide_report_commit_wait, ReportCommitDecision, ReportCommitState,
};

#[test]
fn uncommitted_inside_grace_waits() {
    assert_eq!(
        decide_report_commit_wait(
            ReportCommitState::MissingFromHead,
            Duration::from_secs(118),
            Duration::from_secs(120),
        ),
        ReportCommitDecision::Wait
    );
}

#[test]
fn uncommitted_at_deadline_rejects() {
    assert_eq!(
        decide_report_commit_wait(
            ReportCommitState::MissingFromHead,
            Duration::from_secs(120),
            Duration::from_secs(120),
        ),
        ReportCommitDecision::RejectUncommitted
    );
}

#[test]
fn exact_committed_bytes_are_ready() {
    assert_eq!(
        decide_report_commit_wait(
            ReportCommitState::InHeadSameBytes,
            Duration::ZERO,
            Duration::from_secs(120),
        ),
        ReportCommitDecision::Ready
    );
}

#[test]
fn different_bytes_never_ready() {
    assert_eq!(
        decide_report_commit_wait(
            ReportCommitState::InHeadDifferentBytes,
            Duration::from_secs(1),
            Duration::from_secs(120),
        ),
        ReportCommitDecision::RejectDifferentBytes
    );
}
