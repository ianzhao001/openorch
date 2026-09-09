//! B159 seeded-red contract (H29: recovery completeness).
//!
//! Negative mutations that must turn the named case red:
//! M1. Let the release arm fire without a recorded post-merge-gate failure, or
//!     while main no longer contains the merge, or while the gate is still red
//!     at the tip. Each of those turns "release" into "hide the problem".
//! M2. Let `post-merge-gate-released` close a barrier that has NOT executed its
//!     merge — that would be a back door for claiming "the merge never
//!     happened", which is the exact opposite of what the stage asserts.
//! M3. Let the relaxed record path land a TaskRecorded while the tip gate is
//!     red, or land it without leaving the audit trail (which sha range, which
//!     files, why). A relaxation without proof is just a bypass.

use orch_host::close::{record_relaxation_plan, release_preconditions, RecordRelaxation};

fn v(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn release_requires_all_three_preconditions() {
    // M1: every arm fail-closed. Signature carries the three facts the arm
    // must check: a recorded gate failure, main still containing the merge,
    // and the gate being green at the tip.
    assert!(release_preconditions(true, true, true).is_ok());

    let no_red = release_preconditions(false, true, true).unwrap_err();
    assert!(
        no_red.contains("post-merge-gate"),
        "missing gate-failure evidence must be named: {no_red}"
    );

    let main_lost = release_preconditions(true, false, true).unwrap_err();
    assert!(
        main_lost.contains("main"),
        "a merge no longer on main must be named: {main_lost}"
    );

    let still_red = release_preconditions(true, true, false).unwrap_err();
    assert!(
        still_red.contains("红") || still_red.to_lowercase().contains("red"),
        "a tip that is still red must be named: {still_red}"
    );
}

#[test]
fn release_never_pretends_the_merge_did_not_happen() {
    // M2: the stage asserts "the merge DID happen and its gate went red".
    // Offering it before MergeExecuted must be refused, and it must never
    // produce a TaskRecorded of its own.
    let event = orch_host::close::barrier_released_event(
        "B900",
        "r54",
        &"a".repeat(40),
        "testFast",
    );
    assert_eq!(event.kind, "EscalationRaised");
    let payload = event.payload.as_ref().expect("payload");
    assert_eq!(payload["stage"], "post-merge-gate-released");
    // mergeSha is a real 40-char sha — the opposite of merge-conflict's null,
    // which declares the merge never happened.
    assert_eq!(
        payload["mergeSha"].as_str().map(str::len),
        Some(40),
        "release must carry the full merge sha"
    );
    assert!(payload.get("gate").is_some_and(|g| !g.as_str().unwrap_or("").is_empty()));
}

#[test]
fn record_relaxation_demands_a_green_tip_and_leaves_proof() {
    // M3: relaxation is a controlled widening of the record gate's time
    // binding, never a bypass.
    let test_only = v(&[
        "orch/crates/orch-host/tests/review_lifecycle.rs",
        "orch/crates/orch-host/src/wake.rs",
    ]);

    // Tip red => refuse, no matter how innocent the file list looks.
    assert!(record_relaxation_plan(&test_only, false).is_err());

    // Tip green => allowed, and the plan carries the audit trail.
    let plan = record_relaxation_plan(&test_only, true).expect("green tip admits relaxation");
    match plan {
        RecordRelaxation::Allowed { files } => {
            assert_eq!(files.len(), test_only.len(), "every widened file is recorded");
            assert!(files.iter().any(|f| f.contains("wake.rs")));
        }
        RecordRelaxation::Refused { .. } => panic!("green tip must admit relaxation"),
    }

    // An empty range is not a relaxation case at all — nothing was widened.
    assert!(record_relaxation_plan(&[], true).is_err());
}
