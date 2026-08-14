//! B152 seeded-red contract (H22).
//!
//! Negative mutations that must turn the named case red:
//! M1. Call a ledger shorter than the WAL "consistent" — that is exactly the
//!     silent destruction (git reset --hard) this card exists to detect.
//! M2. Accept a ledger line that the WAL never saw (hand-forged event).
//! M3. Drop the missing-count / recovery hint from the diagnosis so an
//!     operator cannot tell how much was lost.

use orch_host::ledger::{reconcile_wal, WalVerdict};

fn lines(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn ledger_equal_to_or_prefix_of_wal_is_the_only_healthy_shape() {
    // Exactly equal: healthy.
    assert_eq!(
        reconcile_wal(&lines(&["a", "b"]), &lines(&["a", "b"])),
        WalVerdict::Consistent
    );
    // WAL ahead by an in-flight append (ledger written first, WAL flush lags
    // by at most the current append) is tolerated only in the ledger->WAL
    // direction, never the reverse.
    assert_eq!(
        reconcile_wal(&lines(&["a"]), &lines(&["a"])),
        WalVerdict::Consistent
    );
}

#[test]
fn ledger_shorter_than_wal_is_destruction_with_a_count() {
    // M1 + M3: a truncated ledger must be reported, and the report must carry
    // how many events vanished so the operator can judge the blast radius.
    match reconcile_wal(&lines(&["a"]), &lines(&["a", "b", "c"])) {
        WalVerdict::LedgerTruncated { missing } => assert_eq!(missing, 2),
        other => panic!("expected LedgerTruncated, got {other:?}"),
    }
    match reconcile_wal(&[], &lines(&["a"])) {
        WalVerdict::LedgerTruncated { missing } => assert_eq!(missing, 1),
        other => panic!("expected LedgerTruncated, got {other:?}"),
    }
}

#[test]
fn ledger_lines_absent_from_wal_are_forgeries() {
    // M2: a line the runtime never appended cannot appear in the ledger.
    match reconcile_wal(&lines(&["a", "X"]), &lines(&["a", "b"])) {
        WalVerdict::LedgerDiverged { at } => assert_eq!(at, 1),
        other => panic!("expected LedgerDiverged, got {other:?}"),
    }
    // Extra tail with no WAL backing is divergence too, not "ahead".
    match reconcile_wal(&lines(&["a", "b"]), &lines(&["a"])) {
        WalVerdict::LedgerDiverged { at } => assert_eq!(at, 1),
        other => panic!("expected LedgerDiverged, got {other:?}"),
    }
}
