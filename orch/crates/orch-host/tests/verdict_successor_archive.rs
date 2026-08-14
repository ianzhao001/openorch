//! r58/B188 frozen seed: payload-aware verdict successor archive routing.
//!
//! Red form: compile. Before B188, `successor_archive_event_applies` does not
//! exist. The implementation must make the production archive selector call
//! this same function; adding a test-only lookalike does not satisfy the card.

use orch_host::tierf::{successor_archive_applies, successor_archive_event_applies};
use serde_json::json;

#[test]
fn fail_and_blocked_verdicts_require_the_successor_archive_probe() {
    assert!(successor_archive_event_applies(
        "VerdictIssued",
        Some(&json!({"attemptId": "B178-A0001", "verdict": "FAIL"})),
    ));
    assert!(successor_archive_event_applies(
        "VerdictIssued",
        Some(&json!({"attemptId": "B178-A0001", "verdict": "BLOCKED"})),
    ));
}

#[test]
fn pass_unknown_and_missing_verdicts_never_take_the_archive_path() {
    for payload in [
        Some(json!({"verdict": "PASS"})),
        Some(json!({"verdict": "UNKNOWN"})),
        Some(json!({"other": true})),
        None,
    ] {
        assert!(!successor_archive_event_applies(
            "VerdictIssued",
            payload.as_ref(),
        ));
    }
    assert!(!successor_archive_event_applies(
        "AttemptCrashed",
        Some(&json!({"verdict": "FAIL"})),
    ));
}

#[test]
fn legacy_kind_only_contract_remains_frozen() {
    assert!(successor_archive_applies("AttemptBlocked"));
    assert!(successor_archive_applies("AttemptTimedOut"));
    assert!(successor_archive_applies("AttemptFailed"));
    assert!(!successor_archive_applies("VerdictIssued"));
}
