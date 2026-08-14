//! B139 seeded-red contract.
//!
//! Negative mutations that must turn the named case red:
//! M1. Skip the archive decision while the previous attempt's wake log is
//!     still advancing (a possibly-live backend writes into the worktree).
//! M2. Allow the successor to proceed while the previous session's durable
//!     signal still says alive.
//! M3. Skip the WIP archive for a dirty previous worktree (silently dropping
//!     uncommitted work instead of preserving it).

use orch_host::tierf::{blocked_successor_archive_plan, BlockedSuccessorArchive};

#[test]
fn clean_and_quiet_previous_skips_archive() {
    // Nothing uncommitted, session gone (or honestly unknown), quiet log:
    // there is nothing to archive and no termination evidence is needed.
    assert_eq!(
        blocked_successor_archive_plan(true, Some(false), false).unwrap(),
        BlockedSuccessorArchive::SkipClean
    );
    assert_eq!(
        blocked_successor_archive_plan(true, None, false).unwrap(),
        BlockedSuccessorArchive::SkipClean
    );
}

#[test]
fn advancing_log_or_live_session_refuses() {
    // M1: an advancing wake log blocks the successor no matter what.
    assert!(blocked_successor_archive_plan(true, Some(false), true).is_err());
    assert!(blocked_successor_archive_plan(false, Some(false), true).is_err());
    // M2: a durable-alive previous session blocks the successor.
    assert!(blocked_successor_archive_plan(true, Some(true), false).is_err());
    assert!(blocked_successor_archive_plan(false, Some(true), false).is_err());
}

#[test]
fn dirty_previous_archives_with_probe_evidence() {
    // M3: uncommitted work must be preserved, not skipped.
    assert_eq!(
        blocked_successor_archive_plan(false, Some(false), false).unwrap(),
        BlockedSuccessorArchive::ArchiveWip
    );
    assert_eq!(
        blocked_successor_archive_plan(false, None, false).unwrap(),
        BlockedSuccessorArchive::ArchiveWip
    );
}
