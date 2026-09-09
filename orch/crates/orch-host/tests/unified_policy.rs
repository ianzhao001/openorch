//! B150 seeded-red contract.
//!
//! Negative mutations that must turn the named case red:
//! M1. Fall back to filename-order mode selection when no explicit modeRef
//!     is bound.
//! M2. Keep the hardcoded hierarchy-order assertion so a reordered (but
//!     structurally valid) signed hierarchy is rejected.
//! M3. Tolerate duplicate or unregistered members in the signed hierarchy.

use orch_host::plan::{hierarchy_structurally_valid, mode_ref_binding};

fn v(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn mode_selection_requires_explicit_binding() {
    let available = v(&["relay-selfhost", "aaa-experimental"]);
    assert_eq!(
        mode_ref_binding(Some("relay-selfhost"), &available).unwrap(),
        "relay-selfhost"
    );
    // Unknown ref refuses.
    assert!(mode_ref_binding(Some("ghost"), &available).is_err());
    // M1: absence of a binding is an error, never a filename-order guess.
    assert!(mode_ref_binding(None, &available).is_err());
}

#[test]
fn hierarchy_is_structure_checked_not_order_pinned() {
    let registered = v(&["executor-desktop", "executor-claw", "executor-opencode"]);
    // M2: any order of registered, deduped members is valid.
    assert!(hierarchy_structurally_valid(
        &v(&["executor-desktop", "executor-claw", "executor-opencode"]),
        &registered
    )
    .is_ok());
    assert!(hierarchy_structurally_valid(
        &v(&["executor-opencode", "executor-desktop"]),
        &registered
    )
    .is_ok());
    // M3: duplicates, unknown members, or an empty hierarchy refuse.
    assert!(hierarchy_structurally_valid(
        &v(&["executor-desktop", "executor-desktop"]),
        &registered
    )
    .is_err());
    assert!(hierarchy_structurally_valid(&v(&["agy"]), &registered).is_err());
    assert!(hierarchy_structurally_valid(&[], &registered).is_err());
}
