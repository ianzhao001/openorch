//! B136 seeded-red contract.
//!
//! Negative mutations that must turn the named case red:
//! M1. Accept a card whose seed target (or any declared delivery point) is
//!     outside its writeSet instead of rejecting at plan time.
//! M2. Miss a frozen-glob prefix hit so a writeSet entry under a frozen tree
//!     slips through the disjointness check.
//! M3. Leave a newly added card out of the must-reverify list after replan.

use std::collections::BTreeMap;

use orch_host::plan::{
    seed_targets_covered_by_write_set, tasks_requiring_reverify, write_set_disjoint_from_frozen,
};

fn v(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn seed_targets_must_be_inside_write_set() {
    let write_set = v(&[
        "orch/crates/orch-host/src/serve.rs",
        "orch/crates/orch-host/tests/ambiguous_active.rs",
    ]);
    assert!(seed_targets_covered_by_write_set(
        &v(&["orch/crates/orch-host/tests/ambiguous_active.rs"]),
        &write_set
    )
    .is_ok());
    // M1: the B132-A0001 failure class — a delivery point the card never
    // authorized must be rejected before sign-off, not discovered mid-attempt.
    assert!(seed_targets_covered_by_write_set(
        &v(&["orch/crates/orch-cli/src/main.rs"]),
        &write_set
    )
    .is_err());
}

#[test]
fn write_set_must_not_touch_frozen_paths() {
    let frozen = v(&["orch/crates/orch-core/**", "orch/crates/orch-host/src/close.rs"]);
    assert!(write_set_disjoint_from_frozen(
        &v(&["orch/crates/orch-host/src/serve.rs"]),
        &frozen
    )
    .is_ok());
    // Exact hit refuses.
    assert!(write_set_disjoint_from_frozen(
        &v(&["orch/crates/orch-host/src/close.rs"]),
        &frozen
    )
    .is_err());
    // M2: a glob prefix hit refuses too.
    assert!(write_set_disjoint_from_frozen(
        &v(&["orch/crates/orch-core/src/lib.rs"]),
        &frozen
    )
    .is_err());
}

#[test]
fn changed_and_new_cards_require_reverify() {
    let mut old = BTreeMap::new();
    old.insert("B134".to_string(), "aaaa".to_string());
    old.insert("B135".to_string(), "bbbb".to_string());
    let mut new = BTreeMap::new();
    new.insert("B134".to_string(), "aaaa".to_string()); // unchanged
    new.insert("B135".to_string(), "cccc".to_string()); // byte change
    new.insert("B136".to_string(), "dddd".to_string()); // M3: newly added
    assert_eq!(
        tasks_requiring_reverify(&old, &new),
        vec!["B135".to_string(), "B136".to_string()]
    );
    // No drift, nothing to redo.
    assert!(tasks_requiring_reverify(&old, &old).is_empty());
}
