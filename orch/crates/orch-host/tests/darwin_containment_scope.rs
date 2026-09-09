//! r58/B189 frozen seed: managed wake containment claims must be capability-typed.
//!
//! Red form: compile. Before B189 these public types/functions do not exist.
//! The production supervisor status path must consume the same capability and
//! claim functions; a test-only lookalike does not satisfy this contract.

use orch_host::wake::{
    managed_containment_capability_for_platform, managed_containment_claim,
    ManagedContainmentCapability, ManagedContainmentClaim,
};

#[test]
fn darwin_never_claims_fork_complete_from_receipt_census() {
    let capability = managed_containment_capability_for_platform("macos");
    assert_eq!(
        capability,
        ManagedContainmentCapability::ObservedReceipted
    );
    assert_eq!(
        managed_containment_claim(capability, true),
        ManagedContainmentClaim::ManagedScopeTerminated
    );
    assert!(!managed_containment_claim(capability, true).fork_complete());
}

#[test]
fn incomplete_managed_scope_is_never_upgraded_by_capability_labels() {
    for capability in [
        ManagedContainmentCapability::ObservedReceipted,
        ManagedContainmentCapability::ForkComplete,
    ] {
        assert_eq!(
            managed_containment_claim(capability, false),
            ManagedContainmentClaim::ContainmentUnproven
        );
    }
}

#[test]
fn fork_complete_claim_requires_an_explicit_capability() {
    assert_eq!(
        managed_containment_claim(ManagedContainmentCapability::ForkComplete, true),
        ManagedContainmentClaim::ForkCompleteTerminated
    );
    assert!(managed_containment_claim(
        ManagedContainmentCapability::ForkComplete,
        true
    )
    .fork_complete());
    assert_eq!(
        managed_containment_capability_for_platform("linux"),
        ManagedContainmentCapability::ObservedReceipted
    );
    assert_eq!(
        managed_containment_capability_for_platform("unknown"),
        ManagedContainmentCapability::ObservedReceipted
    );
}
