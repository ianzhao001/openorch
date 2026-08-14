//! B140 seeded-red contract.
//!
//! Negative mutations that must turn the named case red:
//! M1. Leave a timed-out (or failed) previous attempt outside the
//!     blocked-successor archive path so its same-agent successor deadlocks
//!     on missing termination evidence (the production B137 bug).
//! M2. Let a crashed previous attempt take the probe-only path, bypassing
//!     the confirmed-dead termination-evidence arm.
//! M3. Treat an unknown terminal kind as eligible instead of failing closed.

use orch_host::tierf::successor_archive_applies;

#[test]
fn non_dead_terminals_take_the_probe_path() {
    // M1: every honest non-dead terminal leaves the same safety question —
    // is the old session really gone and the log quiet? — answered by the
    // B139 probe plan, not by termination evidence it can never produce.
    assert!(successor_archive_applies("AttemptBlocked"));
    assert!(successor_archive_applies("AttemptTimedOut"));
    assert!(successor_archive_applies("AttemptFailed"));
}

#[test]
fn crashed_terminal_keeps_the_confirmed_dead_arm() {
    // M2: a crash is a death claim; it must keep flowing through the
    // kind_is_dead termination-evidence machinery, never the probe shortcut.
    assert!(!successor_archive_applies("AttemptCrashed"));
}

#[test]
fn unknown_terminal_kinds_fail_closed() {
    // M3: no default eligibility for kinds this table has never modeled.
    assert!(!successor_archive_applies("VerdictIssued"));
    assert!(!successor_archive_applies("SomethingNew"));
    assert!(!successor_archive_applies(""));
}
