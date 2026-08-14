//! B142 seeded-red contract.
//!
//! Negative mutations that must turn the named case red:
//! M1. Derive the escalation chain from anything but the signed hierarchy
//!     order (reorder, wrap around, or fall back to a hardcoded list for an
//!     agent missing from the hierarchy).
//! M2. Fire the stall escalation when the multiplier is zero (the feature
//!     must be off unless the round explicitly opts in).
//! M3. Report an acknowledged GO as overdue, authorizing a duplicate wake
//!     (the double-session hazard this round was built to kill).

use orch_host::scheduler::escalation_chain_from_hierarchy;
use orch_host::serve::stall_escalation_due;
use orch_host::tierf::go_ack_overdue;

fn h(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn escalation_chain_is_data_driven_by_signed_hierarchy() {
    let hierarchy = h(&["executor-desktop", "executor-claw", "executor-opencode"]);
    assert_eq!(
        escalation_chain_from_hierarchy(&hierarchy, "executor-desktop").unwrap(),
        hierarchy
    );
    assert_eq!(
        escalation_chain_from_hierarchy(&hierarchy, "executor-claw").unwrap(),
        h(&["executor-claw", "executor-opencode"])
    );
    // A different signed order is honored verbatim — no hardcoded fallback.
    let reordered = h(&["executor-opencode", "executor-desktop"]);
    assert_eq!(
        escalation_chain_from_hierarchy(&reordered, "executor-opencode").unwrap(),
        reordered
    );
    // M1: unknown agents fail closed instead of borrowing a default chain.
    assert!(escalation_chain_from_hierarchy(&hierarchy, "agy").is_err());
}

#[test]
fn stall_escalation_requires_explicit_opt_in() {
    // Multiplier N: escalate only after stall exceeds N x wall budget.
    assert!(stall_escalation_due(7_201, 60, 2));
    assert!(!stall_escalation_due(7_199, 60, 2));
    // M2: zero multiplier means the feature is off, no matter how long.
    assert!(!stall_escalation_due(u64::MAX, 60, 0));
}

#[test]
fn acked_go_is_never_overdue() {
    // Unclaimed GO past the ack window is overdue (re-wake territory).
    assert!(go_ack_overdue(1_000, 1_200, 120, false));
    assert!(!go_ack_overdue(1_000, 1_100, 120, false));
    // M3: an acknowledged GO must never authorize another wake.
    assert!(!go_ack_overdue(1_000, 999_999, 120, true));
}
