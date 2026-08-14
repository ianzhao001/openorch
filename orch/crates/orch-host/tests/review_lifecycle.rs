//! B157 seeded-red contract (review requests become durable facts).
//!
//! Negative mutations that must turn the named case red:
//! M1. Emit the review request without landing it in the ledger, or land it
//!     but let the command succeed when the append fails. "Request sent" and
//!     "request recorded" are two halves of one production action.
//! M2. Derive pending expectations from `ReportObserved` x requiredReviews
//!     instead of from the request itself — that fabricates requests that were
//!     never issued, and still shows nothing when the request truly is silent.
//! M3. Drop the agent dimension from review identity, so a re-review by a
//!     different agent is closed by the previous agent's delivery.

use orch_host::wake::{review_request_event, ReviewRequest};
use orch_host::serve::{pending_review_expectations, ExpectationState};

fn request(task: &str, attempt: &str, role: &str, agent: &str) -> ReviewRequest {
    ReviewRequest {
        task_id: task.to_string(),
        attempt_id: attempt.to_string(),
        role: role.to_string(),
        agent: agent.to_string(),
        deadline_secs: 900,
    }
}

fn delivery(task: &str, attempt: &str, role: &str, agent: &str, body_len: usize) -> String {
    format!(
        r#"{{"eventId":"d-{task}-{agent}","ts":"2026-07-28T00:10:00Z","type":"ReviewDelivered","actor":"runtime:orch","taskId":"{task}","round":"r53","payload":{{"attemptId":"{attempt}","role":"{role}","agent":"{agent}","bodyLen":{body_len}}}}}"#
    )
}

#[test]
fn a_review_request_carries_full_identity_and_is_durable() {
    // M1: the event the real wake entry must append, with every field a later
    // query needs to say "who owes what, since when".
    let event = review_request_event(&request("B900", "B900-A0001", "primary", "executor-claw"))
        .expect("request is representable as a canonical event");
    assert_eq!(event.kind, "ReviewRequested");
    assert_eq!(event.task_id.as_deref(), Some("B900"));
    let payload = event.payload.as_ref().expect("payload");
    for key in ["attemptId", "role", "agent", "deadlineSecs", "requestedAt"] {
        assert!(payload.get(key).is_some(), "payload must carry {key}");
    }
    assert_eq!(payload["agent"], "executor-claw");
    assert_eq!(payload["role"], "primary");
}

#[test]
fn pending_projects_from_the_request_not_from_the_report() {
    // M2: one real request, zero monitor ticks -> a plain query sees it.
    let ledger = review_request_event(&request("B900", "B900-A0001", "primary", "executor-claw"))
        .expect("event")
        .to_jsonl();
    let pending = pending_review_expectations(&ledger, "r53", 0).expect("readable ledger");
    assert_eq!(pending.len(), 1, "a durable request is visible without a tick");
    assert_eq!(pending[0].state, ExpectationState::Waiting);
    assert_eq!(pending[0].agent, "executor-claw");

    // A REPORT alone must NOT manufacture an expectation: no request was ever
    // issued, so the ledger must not claim one was.
    let report_only = r#"{"eventId":"r1","ts":"2026-07-28T00:00:00Z","type":"ReportObserved","actor":"runtime:orch","taskId":"B900","round":"r53","payload":{"attemptId":"B900-A0001"}}"#;
    assert!(
        pending_review_expectations(report_only, "r53", 0)
            .expect("readable")
            .is_empty(),
        "expectations must come from requests, never be back-derived from a REPORT"
    );

    // deadline=0 disables expiry; idle == deadline is still Waiting.
    let ledger_zero = review_request_event(&ReviewRequest {
        deadline_secs: 0,
        ..request("B901", "B901-A0001", "primary", "executor-claw")
    })
    .expect("event")
    .to_jsonl();
    assert_eq!(
        pending_review_expectations(&ledger_zero, "r53", 86_400).expect("readable")[0].state,
        ExpectationState::Waiting
    );
    assert_eq!(
        pending_review_expectations(&ledger, "r53", 900).expect("readable")[0].state,
        ExpectationState::Waiting,
        "idle == deadline is not yet overdue"
    );
}

#[test]
fn review_identity_keeps_the_agent_dimension() {
    // M3: same task, same role, different reviewer -> the old delivery must
    // not close the new expectation.
    let ledger = [
        review_request_event(&request("B900", "B900-A0001", "primary", "executor-claw"))
            .expect("event")
            .to_jsonl(),
        delivery("B900", "B900-A0001", "primary", "executor-claw", 4096),
        review_request_event(&request("B900", "B900-A0001", "primary", "executor-opencode"))
            .expect("event")
            .to_jsonl(),
    ]
    .join("\n");
    let pending = pending_review_expectations(&ledger, "r53", 0).expect("readable");
    assert_eq!(pending.len(), 1, "the re-review is still owed");
    assert_eq!(pending[0].agent, "executor-opencode");

    // An empty-bodied review is not a delivery.
    let shell = [
        review_request_event(&request("B902", "B902-A0001", "primary", "executor-claw"))
            .expect("event")
            .to_jsonl(),
        delivery("B902", "B902-A0001", "primary", "executor-claw", 0),
    ]
    .join("\n");
    assert_eq!(
        pending_review_expectations(&shell, "r53", 0)
            .expect("readable")
            .len(),
        1,
        "a frontmatter-only review does not discharge the expectation"
    );
}
