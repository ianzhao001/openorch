//! B158 seeded-red contract (H25: capacity and roles enforced at the real entry).
//!
//! Negative mutations that must turn the named case red:
//! M1. Release an implementation slot on `ReportObserved`. The executor is
//!     still occupied through the remediation phase — this is the exact bug
//!     the planner-side stopgap shipped with, and the reason claw showed 0/1
//!     while it was actively fixing a card.
//! M2. Count only implementations. Under the strict role table claw never
//!     implements, so a review-only agent would appear permanently idle and
//!     its cap of 1 would mean nothing.
//! M3. Treat an unreadable ledger, a missing capacity entry, or a missing role
//!     as "go ahead". Every one of those must fail closed.

use orch_host::legacy::{agent_inflight_load, capacity_admits, role_admits, LoadKind};

fn dispatch(task: &str, attempt: &str, agent: &str) -> String {
    format!(
        r#"{{"eventId":"d-{task}","ts":"2026-07-28T00:00:00Z","type":"DispatchIssued","actor":"runtime:orch","taskId":"{task}","round":"r53","payload":{{"attemptId":"{attempt}","agent":"{agent}"}}}}"#
    )
}

fn plain(kind: &str, task: &str, attempt: &str, agent: &str) -> String {
    format!(
        r#"{{"eventId":"{kind}-{task}","ts":"2026-07-28T00:05:00Z","type":"{kind}","actor":"runtime:orch","taskId":"{task}","round":"r53","payload":{{"attemptId":"{attempt}","agent":"{agent}"}}}}"#
    )
}

fn review(task: &str, attempt: &str, role: &str, agent: &str) -> String {
    format!(
        r#"{{"eventId":"rr-{task}-{agent}","ts":"2026-07-28T00:02:00Z","type":"ReviewRequested","actor":"runtime:orch","taskId":"{task}","round":"r53","payload":{{"attemptId":"{attempt}","role":"{role}","agent":"{agent}","deadlineSecs":900,"requestedAt":"2026-07-28T00:02:00Z"}}}}"#
    )
}

#[test]
fn remediation_phase_still_occupies_the_slot() {
    // M1: DispatchIssued -> ReportObserved is NOT a release. Only a terminal
    // event frees the agent.
    let ledger = [
        dispatch("B900", "B900-A0001", "executor-desktop"),
        plain("ReportObserved", "B900", "B900-A0001", "executor-desktop"),
    ]
    .join("\n");
    let load = agent_inflight_load(&ledger, "r53").expect("readable ledger");
    let items = load.get("executor-desktop").expect("agent present");
    assert_eq!(items.len(), 1, "a delivered REPORT does not free the executor");
    assert_eq!(items[0].kind, LoadKind::Implementation);
    assert_eq!(items[0].task_id, "B900");

    for terminal in [
        "TaskRecorded",
        "AttemptBlocked",
        "AttemptCrashed",
        "AttemptTimedOut",
        "AttemptFailed",
    ] {
        let closed = format!(
            "{ledger}\n{}",
            plain(terminal, "B900", "B900-A0001", "executor-desktop")
        );
        assert!(
            agent_inflight_load(&closed, "r53")
                .expect("readable")
                .get("executor-desktop")
                .map_or(true, Vec::is_empty),
            "{terminal} must free the slot"
        );
    }
}

#[test]
fn review_load_counts_against_the_same_cap() {
    // M2: a review-only agent must be visible to the gate, otherwise its cap
    // of 1 is decorative — the exact failure the user caught in r52.
    let ledger = review("B900", "B900-A0001", "primary", "executor-claw");
    let load = agent_inflight_load(&ledger, "r53").expect("readable");
    let items = load.get("executor-claw").expect("reviewer is loaded");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind, LoadKind::Review);

    // cap = 1 and one review in flight -> the second injection is refused,
    // and the refusal names who is holding the slot.
    let err = capacity_admits(&load, "executor-claw", 1).unwrap_err();
    assert!(err.contains("B900"), "refusal must list the occupying work");

    // A free agent is admitted.
    assert!(capacity_admits(&load, "executor-opencode", 1).is_ok());
}

#[test]
fn roles_and_bad_input_fail_closed() {
    // M3: roles are checked at injection time, not only on the card.
    let reviewer_only = ["primary-review".to_string(), "secondary-review".to_string()];
    let implementer_only = ["implement".to_string()];

    assert!(role_admits(&implementer_only, "implement").is_ok());
    assert!(role_admits(&reviewer_only, "primary-review").is_ok());
    assert!(
        role_admits(&reviewer_only, "implement").is_err(),
        "a review-only agent must not be handed an implementation"
    );
    assert!(
        role_admits(&implementer_only, "primary-review").is_err(),
        "an implement-only agent must not be handed a review"
    );
    assert!(role_admits(&[], "implement").is_err(), "no roles => refuse");

    // Unreadable ledger must never look like "nobody is busy".
    assert!(agent_inflight_load("{not json", "r53").is_err());

    // Capacity 0 or an unknown agent is a refusal, not a free pass.
    let empty = agent_inflight_load("", "r53").expect("empty ledger is readable");
    assert!(capacity_admits(&empty, "executor-claw", 0).is_err());
}
