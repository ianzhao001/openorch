//! B178 seeded-red contract (await-report watcher scope + wake coalescing).
//!
//! Production incident: await-report watched the whole .worktrees tree recursively
//! and used an unbounded channel. Cargo writes below orch/target therefore woke the
//! loop once per event; each wake reparsed the complete round ledger, and queued
//! events kept the process hot after Cargo became quiet.
//!
//! Negative mutations that must turn the named case red:
//! M1. Widen either watch root to .worktrees, or mark a root recursive.
//!     exact_task_report_roots_are_non_recursive must fail.
//! M2. Replace the capacity-one nonblocking coalescer with an unbounded queue.
//!     a_relevant_burst_has_exactly_one_pending_wake must fail.
//! M3. Enqueue before filtering, or accept Cargo target / another task's paths.
//!     one_hundred_thousand_target_events_schedule_zero_wakes must fail.
//! M4. Create the future worktree path, or install roots only once at startup.
//!     a_late_worktree_parent_becomes_watchable_without_precreation must fail.
//! M5. Remove or change the two-second reconciliation fallback.
//!     reconciliation_tick_remains_two_seconds must fail.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::TryRecvError;
use std::time::Duration;

use orch_host::tierf::{
    await_report_event_is_relevant, await_report_pending_watch_roots,
    await_report_watch_plan, await_report_wake_channel, AwaitReportWatchNotice,
    AWAIT_REPORT_RECONCILE_TICK,
};

fn evidence_paths(root: &Path, round: &str, task: &str) -> BTreeSet<PathBuf> {
    let report_rel = format!("coordination/rounds/{round}/reports/{task}-REPORT.md");
    let blocked_rel = format!("coordination/rounds/{round}/reports/{task}-BLOCKED.md");
    [
        root.join(&report_rel),
        root.join(".worktrees").join(task).join(&report_rel),
        root.join(&blocked_rel),
        root.join(".worktrees").join(task).join(&blocked_rel),
    ]
    .into_iter()
    .collect()
}

fn watch_dirs(root: &Path, round: &str, task: &str) -> BTreeSet<PathBuf> {
    let rel = format!("coordination/rounds/{round}/reports");
    [
        root.join(&rel),
        root.join(".worktrees").join(task).join(&rel),
    ]
    .into_iter()
    .collect()
}

#[test]
fn exact_task_report_roots_are_non_recursive() {
    let root = Path::new("/repo");
    let plan = await_report_watch_plan(root, "r58", "B178");

    assert_eq!(
        plan.evidence_paths.iter().cloned().collect::<BTreeSet<_>>(),
        evidence_paths(root, "r58", "B178"),
        "REPORT and BLOCKED must both be observed in the main root and task worktree"
    );
    assert_eq!(plan.watch_roots.len(), 2);
    assert_eq!(
        plan.watch_roots
            .iter()
            .map(|watch| watch.path.clone())
            .collect::<BTreeSet<_>>(),
        watch_dirs(root, "r58", "B178")
    );
    assert!(
        plan.watch_roots.iter().all(|watch| !watch.recursive),
        "both exact report parents must use RecursiveMode::NonRecursive"
    );
    assert!(
        plan.watch_roots
            .iter()
            .all(|watch| watch.path != root.join(".worktrees")),
        "the global .worktrees root is never a watch target"
    );
}

#[test]
fn terminal_evidence_rename_rescan_and_error_wake_but_noise_does_not() {
    let root = Path::new("/repo");
    let plan = await_report_watch_plan(root, "r58", "B178");

    for evidence in &plan.evidence_paths {
        assert!(await_report_event_is_relevant(
            &plan,
            AwaitReportWatchNotice::Paths(std::slice::from_ref(evidence)),
        ));
    }

    let report = root.join("coordination/rounds/r58/reports/B178-REPORT.md");
    let temporary = report.with_extension("md.tmp");
    assert!(
        await_report_event_is_relevant(
            &plan,
            AwaitReportWatchNotice::Paths(&[temporary, report]),
        ),
        "atomic rename with the target path must wake"
    );

    for watch in &plan.watch_roots {
        assert!(
            await_report_event_is_relevant(
                &plan,
                AwaitReportWatchNotice::Paths(std::slice::from_ref(&watch.path)),
            ),
            "a backend-coalesced parent-directory event must wake"
        );
    }
    assert!(await_report_event_is_relevant(
        &plan,
        AwaitReportWatchNotice::Rescan,
    ));
    assert!(await_report_event_is_relevant(
        &plan,
        AwaitReportWatchNotice::WatcherError,
    ));

    let other_task =
        root.join("coordination/rounds/r58/reports/B999-REPORT.md");
    assert!(!await_report_event_is_relevant(
        &plan,
        AwaitReportWatchNotice::Paths(&[other_task]),
    ));
    assert!(!await_report_event_is_relevant(
        &plan,
        AwaitReportWatchNotice::Paths(&[]),
    ));
}

#[test]
fn one_hundred_thousand_target_events_schedule_zero_wakes() {
    let root = Path::new("/repo");
    let plan = await_report_watch_plan(root, "r58", "B178");
    let cargo_noise =
        root.join(".worktrees/B178/orch/target/debug/incremental/noise/object.o");

    let accepted = (0..100_000)
        .filter(|_| {
            await_report_event_is_relevant(
                &plan,
                AwaitReportWatchNotice::Paths(std::slice::from_ref(&cargo_noise)),
            )
        })
        .count();

    assert_eq!(
        accepted, 0,
        "Cargo target churn must schedule zero ledger-rescan wakes"
    );
}

#[test]
fn a_relevant_burst_has_exactly_one_pending_wake() {
    let (sender, receiver) = await_report_wake_channel();

    let newly_queued = (0..100_000).filter(|_| sender.signal()).count();
    assert_eq!(
        newly_queued, 1,
        "a burst must coalesce into one capacity-one pending wake"
    );
    assert!(matches!(receiver.try_recv(), Ok(())));
    assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));

    assert!(sender.signal(), "draining re-arms exactly one wake");
    assert!(
        !sender.signal(),
        "a second undrained signal must be nonblocking and coalesced"
    );
    assert!(matches!(receiver.try_recv(), Ok(())));
    assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn a_late_worktree_parent_becomes_watchable_without_precreation() {
    let root = Path::new("/repo");
    let plan = await_report_watch_plan(root, "r58", "B178");
    let expected = watch_dirs(root, "r58", "B178");
    let main_dir = root.join("coordination/rounds/r58/reports");
    let worktree_dir =
        root.join(".worktrees/B178/coordination/rounds/r58/reports");

    let mut present = BTreeSet::from([main_dir.clone()]);
    let mut watched = BTreeSet::new();
    let first =
        await_report_pending_watch_roots(&plan, &watched, |path| present.contains(path));
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].path, main_dir);
    assert!(!first[0].recursive);
    assert!(
        !present.contains(&worktree_dir),
        "planning and probing must not create a fake worktree path"
    );

    watched.insert(main_dir);
    present.insert(worktree_dir.clone());
    let second =
        await_report_pending_watch_roots(&plan, &watched, |path| present.contains(path));
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].path, worktree_dir);
    assert!(!second[0].recursive);

    watched.extend(expected);
    assert!(
        await_report_pending_watch_roots(&plan, &watched, |_| true).is_empty(),
        "already installed roots must not be registered again"
    );
}

#[test]
fn reconciliation_tick_remains_two_seconds() {
    assert_eq!(
        AWAIT_REPORT_RECONCILE_TICK,
        Duration::from_secs(2),
        "stat reconciliation is the correctness guarantee when notify loses events"
    );
}
