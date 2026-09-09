//! B145 seeded-red contract.
//!
//! Negative mutations that must turn the named case red:
//! M1. Accept an archived verdict whose (revision, digest) pair appears
//!     nowhere in the round's ledger validations (forgery).
//! M2. Reject a ledger-recorded pair because a re-canonicalization of the
//!     historical ROUND-IR bytes yields a different digest (the schema
//!     evolution false positive that blocked the r50 close).
//! M3. Accept a matching revision with a different digest.

use orch_host::round::archived_binding_ok;

fn v(items: &[(u32, &str)]) -> Vec<(u32, String)> {
    items.iter().map(|(r, d)| (*r, d.to_string())).collect()
}

#[test]
fn ledger_recorded_pair_is_the_truth() {
    let ledger = v(&[(1, "aaaa"), (2, "bbbb"), (3, "cccc")]);
    // M2: the pair recorded in the ledger binds, no matter what a newer
    // binary would recompute from the historical IR bytes.
    assert!(archived_binding_ok(1, "aaaa", &ledger));
    assert!(archived_binding_ok(3, "cccc", &ledger));
}

#[test]
fn unrecorded_pairs_are_forgeries() {
    let ledger = v(&[(1, "aaaa"), (2, "bbbb")]);
    // M1: a pair absent from the ledger never binds.
    assert!(!archived_binding_ok(4, "dddd", &ledger));
    // M3: revision alone is not identity — the digest must match too.
    assert!(!archived_binding_ok(1, "zzzz", &ledger));
    assert!(!archived_binding_ok(2, "aaaa", &ledger));
}
