//! B153 seeded-red contract (H19 + H21 residue).
//!
//! Negative mutations that must turn the named case red:
//! M1. Leave a conflict-failed merge (refs unmoved) without a barrier-closing
//!     escalation — that is the r51/B147 deadlock that froze a whole round.
//! M2. Let the recovery entry clear a barrier while main has already moved
//!     *and* the merge's absence cannot be proven (the merge really happened;
//!     clearing would hide a real merge).
//! M3. Report only the barrier's owner in a rejection, leaving the operator
//!     blind to which event was actually refused.

use orch_host::close::{barrier_recovery_plan, merge_failure_disposition, BarrierRecovery,
                       MergeFailure};
use orch_host::ledger::rejection_message;

#[test]
fn conflict_failures_must_close_the_barrier() {
    // refs unmoved + merge failed => a conflict-shaped escalation is the only
    // way the barrier ever closes again.
    assert_eq!(
        merge_failure_disposition(false, false),
        MergeFailure::ConflictEscalation
    );
    // M1: refs moved is the pre-existing boundary-violation path, unchanged.
    assert_eq!(
        merge_failure_disposition(true, false),
        MergeFailure::BoundaryViolation
    );
    // A valid main merge is never a failure disposition at all.
    assert_eq!(merge_failure_disposition(true, true), MergeFailure::None);
}

#[test]
fn recovery_only_clears_a_barrier_that_never_merged() {
    // Legal shape: barrier active, main still at the verdict-bound sha.
    assert_eq!(
        barrier_recovery_plan(true, false, true, false).unwrap(),
        BarrierRecovery::ClearAndAllowRedispatch
    );
    // M2: main advanced without a proof of absence => the merge may have
    // really happened; refuse.
    assert!(barrier_recovery_plan(true, true, true, false).is_err());
    // H97/H98: main advanced through a coordination-only commit, and the task
    // branch is provably absent from main's history => recovery must proceed,
    // otherwise the barrier freezes the round with no CLI arm able to clear it.
    assert_eq!(
        barrier_recovery_plan(true, true, true, true).unwrap(),
        BarrierRecovery::ClearAndAllowRedispatch
    );
    // No barrier => nothing to recover.
    assert!(barrier_recovery_plan(false, false, true, false).is_err());
    // Verdict no longer valid => refuse rather than resurrect a dead chain.
    // The absence proof does not soften this arm.
    assert!(barrier_recovery_plan(true, false, false, false).is_err());
    assert!(barrier_recovery_plan(true, false, false, true).is_err());
}

#[test]
fn rejections_name_both_the_refused_event_and_the_barrier() {
    // M3: the r51 misdiagnosis came from a message that named only the
    // barrier owner, so the planner read it as "B147's own barrier".
    let msg = rejection_message("VerdictIssued", "B155", "B147", "r51");
    assert!(msg.contains("VerdictIssued"));
    assert!(msg.contains("B155"), "refused event's task must appear");
    assert!(msg.contains("B147"), "barrier owner must appear");
    assert!(msg.contains("r51"));
}
