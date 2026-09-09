//! B141 seeded-red contract.
//!
//! Negative mutations that must turn the named case red:
//! M1. Give an unknown complexity value a default tier instead of failing
//!     closed.
//! M2. Let a dependency cycle (or an edge onto an unknown card) pass the
//!     ordering check.
//! M3. Skip the capability check so a card lands on an agent whose mode
//!     roles cannot satisfy it.

use std::collections::BTreeMap;

use orch_host::card::{complexity_tier, ComplexityTier};
use orch_host::plan::{capability_supported, card_dependency_order};

fn deps(edges: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
    edges
        .iter()
        .map(|(k, vs)| (k.to_string(), vs.iter().map(|v| v.to_string()).collect()))
        .collect()
}

#[test]
fn complexity_is_a_closed_enum() {
    assert_eq!(complexity_tier("complex").unwrap(), ComplexityTier::Complex);
    assert_eq!(complexity_tier("medium").unwrap(), ComplexityTier::Medium);
    assert_eq!(complexity_tier("light").unwrap(), ComplexityTier::Light);
    // M1: no default tier for unmodeled values.
    assert!(complexity_tier("hard").is_err());
    assert!(complexity_tier("").is_err());
}

#[test]
fn dependency_order_rejects_cycles_and_unknown_targets() {
    let ok = card_dependency_order(&deps(&[
        ("B142", &[]),
        ("B143", &["B142"]),
        ("B144", &[]),
    ]))
    .unwrap();
    let pos = |t: &str| ok.iter().position(|x| x == t).unwrap();
    assert!(pos("B142") < pos("B143"));
    // M2: a cycle must be rejected, not silently linearized.
    assert!(card_dependency_order(&deps(&[("A", &["B"]), ("B", &["A"])])).is_err());
    // An edge onto a card that does not exist in this round is refused.
    assert!(card_dependency_order(&deps(&[("A", &["GHOST"])])).is_err());
}

#[test]
fn capabilities_must_be_backed_by_agent_roles() {
    let roles = vec!["implement".to_string(), "primary-review".to_string()];
    assert!(capability_supported(&["implement".to_string()], &roles).is_ok());
    assert!(capability_supported(&[], &roles).is_ok());
    // M3: a required capability outside the agent's mode roles refuses.
    assert!(capability_supported(&["takeover".to_string()], &roles).is_err());
}
