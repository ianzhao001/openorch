//! B134 seeded-red contract.
//!
//! Negative mutations that must turn the named case red:
//! M1. Drop the observing agent from the monitor dedup key so two agents'
//!     alarms collapse into one.
//! M2. Silently append planner events onto a stale lease round instead of
//!     refusing the cross-round write.
//! M3. Trust pre-closure termination evidence without the in-closure
//!     no-sleep recheck before committing the reassignment to the ledger.

use orch_host::serve::{monitor_dedup_key, planner_append_round};
use orch_host::tierf::termination_commit_gate;

#[test]
fn monitor_dedup_key_binds_agent() {
    // M1: the same task+attempt observed via two agents must not dedup
    // against each other.
    let a = monitor_dedup_key("B134", "B134-A0001", "executor-desktop");
    let b = monitor_dedup_key("B134", "B134-A0001", "executor-claw");
    assert_ne!(a, b);
    for part in ["B134", "B134-A0001", "executor-desktop"] {
        assert!(a.contains(part), "key must bind {part}");
    }
}

#[test]
fn planner_append_round_refuses_cross_round() {
    // Matching rounds pass through untouched.
    assert_eq!(planner_append_round("r49", "r49").unwrap(), "r49");
    // M2: a stale persisted lease round must never be written to silently.
    assert!(planner_append_round("r48", "r49").is_err());
    assert!(planner_append_round("r49", "r48").is_err());
}

#[test]
fn termination_commit_gate_requires_fresh_recheck() {
    // The only legal commit shape: evidence says terminated AND the
    // in-closure recheck confirms the group is not alive.
    assert!(termination_commit_gate(true, Some(false)).is_ok());

    // M3: no recheck, a revived group, or unterminated evidence all refuse.
    assert!(termination_commit_gate(true, None).is_err());
    assert!(termination_commit_gate(true, Some(true)).is_err());
    assert!(termination_commit_gate(false, Some(false)).is_err());
}
