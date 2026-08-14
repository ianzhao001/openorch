//! B255 frozen contract · the H56 trial gate runs in the exclusive lane.
//!
//! r69 evidence: `serve::tests::production_entries_visit_all_six_unattended_stations`
//! is the only full-workspace red that has blocked this round twice, at two
//! different stages of the same sample:
//!   * B254-A0002 (`serve.rs` run_dispatch edge) — `typed continuation backend
//!     receipt timed out`, 707/1/7, logs `085a0f6a…` / `56c71c27…`;
//!   * the rev15 owner baseline and the H142 landing gate (ACK observer edge) —
//!     `ACK pipeline did not converge within B203 budget`,
//!     `stage=AckPresentUnclaimed ticksElapsed=5 tickBudget=4`.
//!
//! Both assertions are wall-clock budgets: `wake::review_probe_timeout()` is
//! five seconds under `cfg(test)` (three hundred in production), and the B203
//! eight-second/four-tick bound reaches `ack_convergence_verdict` only after the
//! `cfg(test)` helpers `elapsed_reconcile_ticks` / `exact_tick_budget` convert a
//! measured `Instant::elapsed()` into ticks.  The product predicate
//! `ack_convergence_verdict` is deliberately wall-clock free and is not what
//! fails.  The same bytes pass ten isolated samples and four-way concurrent
//! pressure; only the 217-binary default lane is red.  A test that spawns real
//! processes and asserts real elapsed time cannot be validly measured while
//! oversubscribed.
//!
//! This repo already built the lane for exactly that class.  Four
//! process-global cases live there today — the two `gate::tests::registry_reap_*`
//! cases and `four_signal_topology_cells_pass_ten_consecutive_runs` are the very
//! tests that blocked B246 and the r68 planner baseline.  This contract moves
//! the H56 trial gate into the same lane and **freezes everything else**: ten
//! samples, the eight-second bound, and the tick source stay byte-identical, so
//! the move cannot be used to launder the gate.
//!
//! The compile-time sentinel is deliberate.  The workspace's own H56 flake can
//! fail before an assertion-red integration target is executed (H113 truncated
//! B254's first oracle exactly that way).  `E0080` makes this contract the first
//! stable red without adding a production API or weakening a gate.
//!
//! Iron rule 10: after relocation this target is byte-frozen.
//!
//! M1: drop the `#[ignore = "testExclusive:…"]` marker from the H56 test
//!     (leaving the runner line) -> sentinel red.
//! M2: keep the marker but attach it to a different function, or insert another
//!     `fn` between the marker and the H56 test -> sentinel red (adjacency).
//! M3: shrink the sample count or the eight-second bound while moving the lane
//!     -> sentinel red (anti-laundering).

const SERVE: &str = include_str!("../src/serve.rs");
const RUNNER: &str = include_str!("../../../scripts/test-exclusive.sh");

const MISSING: usize = usize::MAX;

const MARKER: &str =
    "#[ignore = \"testExclusive:serve::tests::production_entries_visit_all_six_unattended_stations\"]";
const H56_FN: &str = "fn production_entries_visit_all_six_unattended_stations";
const SAMPLES: &str = "const B203_TRIAL_GATE_SAMPLES: usize = 10;";
const BOUND: &str = "let ack_tick_budget = exact_tick_budget(Duration::from_secs(8), ack_tick);";
const TICK_SOURCE: &str =
    "let ticks_elapsed = elapsed_reconcile_ticks(started.elapsed(), ack_tick);";

const fn find_from(haystack: &[u8], needle: &[u8], start: usize) -> usize {
    if start > haystack.len() || needle.len() > haystack.len() - start {
        return MISSING;
    }
    if needle.is_empty() {
        return start;
    }
    let mut cursor = start;
    while cursor + needle.len() <= haystack.len() {
        let mut offset = 0;
        while offset < needle.len() && haystack[cursor + offset] == needle[offset] {
            offset += 1;
        }
        if offset == needle.len() {
            return cursor;
        }
        cursor += 1;
    }
    MISSING
}

const fn find(haystack: &[u8], needle: &[u8]) -> usize {
    find_from(haystack, needle, 0)
}

const fn count(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut total = 0;
    let mut cursor = 0;
    loop {
        let hit = find_from(haystack, needle, cursor);
        if hit == MISSING {
            return total;
        }
        total += 1;
        cursor = hit + needle.len();
    }
}

const fn count_between(haystack: &[u8], needle: &[u8], start: usize, end: usize) -> usize {
    if start == MISSING || end == MISSING || start >= end || end > haystack.len() {
        return 0;
    }
    let mut total = 0;
    let mut cursor = start;
    loop {
        let hit = find_from(haystack, needle, cursor);
        if hit == MISSING || hit >= end {
            return total;
        }
        total += 1;
        cursor = hit + needle.len();
    }
}

const SERVE_BYTES: &[u8] = SERVE.as_bytes();
const RUNNER_BYTES: &[u8] = RUNNER.as_bytes();

const MARKER_AT: usize = find(SERVE_BYTES, MARKER.as_bytes());
const H56_FN_AT: usize = find(SERVE_BYTES, H56_FN.as_bytes());

// The anchor must be unique: a non-unique `.find` target is exactly how B251's
// frozen oracle became unsatisfiable on legitimate production bytes (H139).
const _: () = assert!(
    count(SERVE_BYTES, H56_FN.as_bytes()) == 1,
    "B255: the H56 trial-gate function name must occur exactly once in serve.rs"
);

// A1 — the marker exists and precedes the H56 test.
const _: () = assert!(
    MARKER_AT != MISSING && H56_FN_AT != MISSING && MARKER_AT < H56_FN_AT,
    "B255: H56 trial gate must carry #[ignore = \"testExclusive:…\"] before its fn"
);

// A2 — adjacency: nothing but the `#[test]` attribute and whitespace may sit
// between the marker and the H56 test.  Without this, the marker could be
// parked on some other function and the contract would be hollow — the exact
// failure mode that made B254-A0001 a formal FAIL (frozen window covered
// helper definitions instead of the real callsite).
const _: () = assert!(
    count_between(SERVE_BYTES, b"fn ", MARKER_AT, H56_FN_AT) == 0,
    "B255: no other fn may sit between the exclusive marker and the H56 trial gate"
);
// `saturating_add` keeps the unfixed tree's `MARKER_AT == usize::MAX` from
// overflowing in const evaluation, so A1 stays the first and only sentinel red.
const _: () = assert!(
    count_between(
        SERVE_BYTES,
        b"#[ignore",
        MARKER_AT.saturating_add(1),
        H56_FN_AT
    ) == 0,
    "B255: exactly one ignore marker may bind the H56 trial gate"
);

// A3 — the runner drives it exactly once.
const _: () = assert!(
    count(
        RUNNER_BYTES,
        b"production_entries_visit_all_six_unattended_stations"
    ) == 1,
    "B255: test-exclusive.sh must run the H56 trial gate exactly once"
);

// A4 — anti-laundering: the lane move may not shrink the signed budgets.
const _: () = assert!(
    count(SERVE_BYTES, SAMPLES.as_bytes()) == 1,
    "B255: the H56 trial gate must keep exactly ten B203 samples"
);
const _: () = assert!(
    count(SERVE_BYTES, BOUND.as_bytes()) == 1,
    "B255: the B203 eight-second/four-tick bound must stay byte-identical"
);
const _: () = assert!(
    count(SERVE_BYTES, TICK_SOURCE.as_bytes()) == 1,
    "B255: the ACK observer must keep its existing tick source"
);

/// Runtime companion: the three independently maintained artifacts must agree.
/// The compile-time sentinel above proves the H56 binding; this proves the move
/// did not desynchronise the lane's own closed-set contract.
#[test]
fn h56_trial_gate_is_a_member_of_the_exclusive_lane() {
    let marker_count = SERVE.matches(MARKER).count();
    assert_eq!(marker_count, 1, "exactly one H56 exclusive marker in serve.rs");

    let runner_line = RUNNER
        .lines()
        .filter(|line| line.contains("production_entries_visit_all_six_unattended_stations"))
        .collect::<Vec<_>>();
    assert_eq!(runner_line.len(), 1, "one runner call: {runner_line:?}");
    let line = runner_line[0];
    assert!(
        line.starts_with("run_exact orch-host --lib _ "),
        "H56 must be driven through the lib run_exact form: {line}"
    );
    assert!(
        line.ends_with("serve::tests::production_entries_visit_all_six_unattended_stations"),
        "runner must name the exact H56 test path: {line}"
    );

    // The marker payload and the runner argument must be the same test path.
    let marker_path = MARKER
        .trim_start_matches("#[ignore = \"testExclusive:")
        .trim_end_matches("\"]");
    assert!(
        line.ends_with(marker_path),
        "marker path and runner argument must match: {marker_path} vs {line}"
    );
}
