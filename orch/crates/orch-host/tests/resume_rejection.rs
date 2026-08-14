//! B138 seeded-red contract.
//!
//! Negative mutations that must turn the named case red:
//! M1. Ledger-reject a drift (or an already-rejected) failure again on the
//!     resume path, double-appending ActionRejected.
//! M2. Shrink the protocol-effect enumeration scan back to close.rs-only so
//!     new unleased entrances stop being caught.
//! M3. Shrink the lifecycle-append capability scan back to close.rs-only.

use orch_host::tierf::resume_failure_disposition;

fn enumeration_source(name: &str) -> String {
    let path = format!("{}/tests/{}", env!("CARGO_MANIFEST_DIR"), name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

#[test]
fn resume_failures_share_the_dispatch_disposition_table() {
    // Plain failures are ledger-rejected exactly once.
    assert_eq!(resume_failure_disposition(false, false), "reject-and-ledger");
    // M1: drift and pre-rejected failures pass through untouched.
    assert_eq!(resume_failure_disposition(true, false), "passthrough");
    assert_eq!(resume_failure_disposition(false, true), "passthrough");
    assert_eq!(resume_failure_disposition(true, true), "passthrough");
}

#[test]
fn protocol_enumeration_scans_all_leased_surfaces() {
    // M2: B134 wired runtask/serve into the protocol lease; the meta-test
    // must scan those surfaces, not just close.rs.
    let src = enumeration_source("protocol_effect_enumeration.rs");
    for surface in ["runtask.rs", "serve.rs"] {
        assert!(src.contains(surface), "enumeration must scan {surface}");
    }
}

#[test]
fn capability_gate_scans_all_lifecycle_append_surfaces() {
    // M3: lifecycle-append authority must be checked wherever appends live.
    let src = enumeration_source("ledger_capability_gate.rs");
    for surface in ["tierf.rs", "serve.rs"] {
        assert!(src.contains(surface), "capability gate must scan {surface}");
    }
}
