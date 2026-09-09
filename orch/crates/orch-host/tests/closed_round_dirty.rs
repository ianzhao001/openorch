//! B171 seeded-red contract (H34: a closed round's artifacts are the binding
//! object of root verdicts, so any working-tree drift in them must be reported
//! by name — and the legitimate post-close writes must never be false-flagged).
//!
//! Negative mutations that must turn the named case red:
//! M1. Stay silent about a tampered closed-round review. r56 caught an r53-era
//!     session still rewriting its own r53 review file (+157/-77) five days
//!     after that round closed; one habitual `git add -A` would have committed
//!     the tampering into history, and review bytes are what root verdicts bind
//!     to (`committed_regular_blob_bytes`).
//! M2. Flag the writes that the protocol itself asks for after close —
//!     `reports/<agentId>-SUMMARY.md` and the dispatch `.ack` / `.seen` traces.
//!     A check that cries wolf on every normal round teaches the planner to
//!     ignore it, which is worse than not having it.
//! M3. Report the current, still-open round's artifacts as tampering, or repair
//!     anything automatically. The active round is supposed to be dirty, and a
//!     diagnostic that rewrites history is far more dangerous than the drift it
//!     was meant to surface.

use orch_host::closed_round_audit::{closed_round_drift, DriftInput};

fn input(closed: &[&str], dirty: &[&str]) -> DriftInput {
    DriftInput {
        closed_rounds: closed.iter().map(|r| r.to_string()).collect(),
        dirty_paths: dirty.iter().map(|p| p.to_string()).collect(),
    }
}

#[test]
fn a_tampered_closed_round_artifact_is_reported_by_name() {
    // M1: replay the exact r56 finding.
    let findings = closed_round_drift(&input(
        &["r53", "r55"],
        &["coordination/rounds/r53/reviews/B158-A0001-secondary-executor-opencode.md"],
    ));
    assert_eq!(findings.len(), 1, "the tampered file must be reported");
    let finding = &findings[0];
    assert_eq!(
        finding.path,
        "coordination/rounds/r53/reviews/B158-A0001-secondary-executor-opencode.md",
        "the report names the exact file, not just the round"
    );
    assert_eq!(finding.round, "r53");

    // Evidence and report artifacts of a closed round are equally binding.
    let more = closed_round_drift(&input(
        &["r55"],
        &[
            "coordination/rounds/r55/evidence/B162-main-ref-never-moves-backwards-uninvited.json",
            "coordination/rounds/r55/reports/B163-REPORT.md",
        ],
    ));
    assert_eq!(
        more.len(),
        2,
        "evidence/ and reports/ of a closed round are covered too"
    );
}

#[test]
fn legitimate_post_close_writes_are_not_flagged() {
    // M2: the protocol asks agents to write a SUMMARY during close, and the
    // dispatch channel leaves ack/seen traces. Neither is tampering.
    let benign = closed_round_drift(&input(
        &["r55", "r56"],
        &[
            "coordination/rounds/r55/reports/executor-desktop-SUMMARY.md",
            "coordination/rounds/r56/reports/executor-claw-SUMMARY.md",
            "coordination/rounds/r56/dispatch/executor-desktop/GO-B167-A0001.md.ack",
            "coordination/rounds/r56/dispatch/executor-desktop/NUDGE.md.seen",
        ],
    ));
    assert!(
        benign.is_empty(),
        "post-close summaries and dispatch traces must never be flagged: {benign:?}"
    );

    // A summary is only exempt by shape — a review with a similar name is not.
    let disguised = closed_round_drift(&input(
        &["r55"],
        &["coordination/rounds/r55/reviews/executor-desktop-SUMMARY.md"],
    ));
    assert_eq!(
        disguised.len(),
        1,
        "the exemption is scoped to reports/, not to any file named SUMMARY"
    );
}

#[test]
fn the_open_round_is_left_alone_and_nothing_is_repaired() {
    // M3a: the active round is supposed to have dirty artifacts — that is work
    // in progress, not tampering.
    let active = closed_round_drift(&input(
        &["r55"],
        &[
            "coordination/rounds/r57/reviews/B168-A0001-primary-executor-claw.md",
            "coordination/rounds/r57/evidence/B168-x.json",
        ],
    ));
    assert!(
        active.is_empty(),
        "an open round's artifacts are never tampering: {active:?}"
    );

    // Paths outside the round artifact tree are out of scope entirely.
    let elsewhere = closed_round_drift(&input(
        &["r55"],
        &[
            "coordination/BOARD.md",
            "coordination/CURRENT.md",
            "orch/crates/orch-host/src/plan.rs",
        ],
    ));
    assert!(elsewhere.is_empty(), "only round artifacts are in scope: {elsewhere:?}");

    // M3b: the finding carries a manual remediation hint and nothing else — the
    // check must never repair. `git checkout --` is forbidden in this repo, so
    // the hint has to be the surgical restore that r56 actually used.
    let finding = &closed_round_drift(&input(
        &["r53"],
        &["coordination/rounds/r53/reviews/B158-A0001-secondary-executor-opencode.md"],
    ))[0];
    assert!(
        finding.remediation.contains("git show HEAD:"),
        "the hint is a surgical restore, never `git checkout --`: {}",
        finding.remediation
    );
    assert!(
        !finding.remediation.contains("checkout --")
            && !finding.remediation.contains("reset --hard"),
        "the forbidden git commands must never be suggested: {}",
        finding.remediation
    );
}
