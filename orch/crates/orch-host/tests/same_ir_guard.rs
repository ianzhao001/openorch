//! B154 seeded-red contract (H20).
//!
//! Negative mutations that must turn the named case red:
//! M1. Let two cards that both write an IR-structure file schedule
//!     concurrently without a declared dependsOn (the B147xB150 conflict).
//! M2. Treat a non-IR shared file as if it needed the same serialization
//!     (over-blocking: ordinary shared files are the scheduler's job, not
//!     this guard's).
//! M3. Accept a one-directional edge as insufficient — a declared dependsOn
//!     in either direction serializes the pair and must be allowed.

use orch_host::plan::same_ir_file_conflict;

fn v(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

const PLAN: &str = "orch/crates/orch-host/src/plan.rs";
const CARD: &str = "orch/crates/orch-host/src/card.rs";
const OTHER: &str = "orch/crates/orch-host/src/serve.rs";

#[test]
fn undeclared_same_ir_file_pairs_are_rejected() {
    // M1: both write plan.rs, neither depends on the other -> refuse, naming
    // the file and both tasks.
    let err = same_ir_file_conflict(("B147", &v(&[PLAN])), ("B150", &v(&[PLAN, OTHER])), false)
        .unwrap_err();
    assert!(err.contains("plan.rs"));
    assert!(err.contains("B147") && err.contains("B150"));
    // card.rs counts too.
    assert!(same_ir_file_conflict(("A", &v(&[CARD])), ("B", &v(&[CARD])), false).is_err());
}

#[test]
fn ordinary_shared_files_are_not_this_guards_business() {
    // M2: serve.rs overlap is a scheduler/writeSet concern, not an IR-shape
    // serialization concern.
    assert!(same_ir_file_conflict(("A", &v(&[OTHER])), ("B", &v(&[OTHER])), false).is_ok());
    // Disjoint sets are fine regardless.
    assert!(same_ir_file_conflict(("A", &v(&[PLAN])), ("B", &v(&[OTHER])), false).is_ok());
}

#[test]
fn a_declared_dependency_serializes_the_pair() {
    // M3: with dependsOn declared (either direction), the pair is serialized
    // by the DAG and the guard must let it through.
    assert!(same_ir_file_conflict(("A", &v(&[PLAN])), ("B", &v(&[PLAN])), true).is_ok());
    assert!(same_ir_file_conflict(("A", &v(&[CARD])), ("B", &v(&[CARD, OTHER])), true).is_ok());
}
