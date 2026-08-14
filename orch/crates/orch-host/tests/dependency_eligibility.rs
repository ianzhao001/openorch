//! B147 seeded-red contract.
//!
//! Negative mutations that must turn the named case red:
//! M1. Let a task with unmet dependsOn slip past the blocker (schedule it
//!     before its prerequisite is Recorded).
//! M2. Silently downgrade a successor to a candidate missing a required
//!     capability.
//! M3. Wrap the eligibility chain around (or downgrade) instead of failing
//!     with eligible-chain-exhausted.

use orch_host::plan::task_dependency_blockers;
use orch_host::scheduler::{next_eligible, successor_eligible};

fn v(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn unmet_dependencies_block_scheduling() {
    // M1: B depends on A; A not Recorded yet -> A is a blocker.
    assert_eq!(
        task_dependency_blockers(&v(&["A", "C"]), &v(&["C"])),
        v(&["A"])
    );
    // All prerequisites Recorded -> no blockers.
    assert!(task_dependency_blockers(&v(&["A"]), &v(&["A", "C"])).is_empty());
    assert!(task_dependency_blockers(&[], &[]).is_empty());
}

#[test]
fn successors_must_cover_required_capabilities() {
    let roles = v(&["implement", "critical-implement"]);
    assert!(successor_eligible(&v(&["critical-implement"]), &roles).is_ok());
    assert!(successor_eligible(&[], &roles).is_ok());
    // M2: a missing capability refuses loudly, naming the gap.
    let err = successor_eligible(&v(&["critical-implement"]), &v(&["implement"]))
        .unwrap_err();
    assert!(err.contains("critical-implement"));
}

#[test]
fn exhausted_chains_escalate_never_downgrade() {
    let chain = v(&["a", "b", "c"]);
    // First candidate passing every filter wins, in signed order.
    assert_eq!(
        next_eligible(&chain, &v(&["a"]), &[], &[]).unwrap(),
        "b"
    );
    // Ineligible (capability-failed) candidates are skipped, not downgraded.
    assert_eq!(
        next_eligible(&chain, &[], &v(&["b"]), &v(&["a"])).unwrap(),
        "c"
    );
    // M3: everything excluded -> Err mentioning eligible-chain-exhausted.
    let err = next_eligible(&chain, &v(&["a"]), &v(&["b"]), &v(&["c"])).unwrap_err();
    assert!(err.contains("eligible-chain-exhausted"));
}
