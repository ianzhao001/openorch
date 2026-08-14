//! B260 frozen contract · the review deadline is derived from the work being
//! asked for, the attach path stops being封顶 at half an hour, and a
//! deadline death says how far the session actually got.
//!
//! ## The defect (H147)
//!
//! `orch wake --review-for` defaults the review deadline to **1800s**
//! (`orch-cli/src/main.rs`, `resolve_review_request`), and the attach path
//! refuses anything outside `1..=1800` (`wake.rs`,
//! `run_managed_wake_attach_inner`) — so even an explicit override cannot exceed
//! half an hour.  Meanwhile the ordinary managed-wake ceiling is
//! `MANAGED_WAKE_MAX_RUNTIME_SECS = 21600`.  The two limits disagree by 12×.
//!
//! r69 measured every opencode review request in the round.  Nine succeeded, in
//! **15.7 – 31.5 minutes**, median **26.4**.  The default deadline is **30.0**.
//! Five of the nine successes land in the 25–35 minute band.
//!
//! ⇒ The default was set **at the median of the work's own duration
//! distribution**.  Half of every run was a coin flip.  That is why the failures
//! looked random.
//!
//! The clearest casualty is B246-A0003: session first frame 07:06:59Z, last
//! frame 07:36:17Z — **29.3 minutes alive against a 30.0 minute budget** — and
//! the last frame said:
//!
//! ```text
//! Worktree clean. Now let me update todos and write the review.
//! ```
//!
//! Every check done, the worktree already restored, cut down one second before
//! putting pen to paper.  The planner then read `outcome=StoppedByHardDeadline`
//! as "the channel is exhausted" and terminated an attempt whose primary had
//! already passed and whose three gates were green.  **A wrong attribution burned
//! a whole attempt cycle.**
//!
//! ## What this contract freezes
//!
//! A1  A pure, table-testable derivation: role × declared workload -> seconds.
//! A2  It never returns less than the old fixed default, and never more than the
//!     managed ceiling.  (This card may only ever *lengthen* a deadline.)
//! A3  The attach path is bounded by the same ceiling as every other managed
//!     wake — the 1800 cap is gone.
//! A4  Reachability (M4): the CLI actually derives the default instead of
//!     hardcoding 1800.
//! A5  A deadline death reports how far the session got: the ledger payload
//!     carries the real gap between the last observed frame and the kill, plus a
//!     summary of that frame, so attribution never again requires opening raw
//!     provider logs.
//! A6  Anti-drift: the new diagnostics go into the `ManagedWakeTerminated`
//!     ledger payload (`status.`-derived), **not** into the canonical
//!     `ManagedWakeAttachState` evidence set (`facts.`-derived) whose SHA-256 is
//!     a durable identity.
//!
//! Iron rule 10: after relocation this target is byte-frozen.
//!
//! M1: return a constant from the derivation -> A1 red (the table is not flat).
//! M2: let the derivation dip below 1800 or exceed the ceiling -> A2 red.
//! M3: restore the `1..=1800` attach bound -> A3 red.
//! M4: keep `deadline_secs.unwrap_or(1800)` in the CLI -> A4 red.
//! M5: put the new fields into the canonical evidence set -> A6 red (it would
//!     silently change every future attach-state digest).

#![allow(dead_code)]
// 冻结种子内的窗口辅助函数按契约成组保留；未被本轮某条断言用到的
// 不得删除——删掉会让后续 M 变异无处落脚，也会让契约看起来比实际更窄。

use orch_host::wake::MANAGED_WAKE_MAX_RUNTIME_SECS;

// Compile-red until the derivation exists.
use orch_host::wake::{default_review_deadline_secs, review_deadline_secs};

const WAKE: &str = include_str!("../src/wake.rs");
const CLI: &str = include_str!("../../orch-cli/src/main.rs");
const WAKE_BYTES: &[u8] = WAKE.as_bytes();
const CLI_BYTES: &[u8] = CLI.as_bytes();

const MISSING: usize = usize::MAX;
const ITEM_END: &[u8] = b"\n}\n";
const ATTACH_FN: &[u8] = b"\nfn run_managed_wake_attach_inner(";
const RESOLVE_FN: &[u8] = b"\nfn resolve_review_request(";

fn find_from(haystack: &[u8], needle: &[u8], start: usize) -> usize {
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

fn find(haystack: &[u8], needle: &[u8]) -> usize {
    find_from(haystack, needle, 0)
}

fn count(haystack: &[u8], needle: &[u8]) -> usize {
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

fn count_between(haystack: &[u8], needle: &[u8], start: usize, end: usize) -> usize {
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

#[test]
fn frozen_source_contract_01() {
    assert!(
    count(WAKE_BYTES, ATTACH_FN) == 1 && count(CLI_BYTES, RESOLVE_FN) == 1,
    "B260: both anchors must be unique definitions, not call sites"
);
}

fn attach_at() -> usize { find(WAKE_BYTES, ATTACH_FN) }
fn attach_end() -> usize { find_from(WAKE_BYTES, ITEM_END, attach_at()) }
fn resolve_at() -> usize { find(CLI_BYTES, RESOLVE_FN) }
fn resolve_end() -> usize { find_from(CLI_BYTES, ITEM_END, resolve_at()) }

#[test]
fn frozen_source_contract_02() {
    assert!(
    attach_end() != MISSING && resolve_end() != MISSING,
    "B260: both anchored items must terminate at a column-0 closing brace"
);
}

// ------------------------------------- A3 · the attach cap is gone ---

#[test]
fn frozen_source_contract_03() {
    assert!(
    count(WAKE_BYTES, b"(1..=1800)") == 0,
    "B260: the attach path must not be capped at 1800 while ordinary managed wakes get 21600"
);
}
#[test]
fn frozen_source_contract_04() {
    assert!(
    count_between(
        WAKE_BYTES,
        b"MANAGED_WAKE_MAX_RUNTIME_SECS",
        attach_at(),
        attach_end()
    ) >= 1,
    "B260: attach must be bounded by the same ceiling as every other managed wake"
);
}

// ------------------------------------------- A4 · the CLI derives it ---

#[test]
fn frozen_source_contract_05() {
    assert!(
    count_between(
        CLI_BYTES,
        b"deadline_secs.unwrap_or(1800)",
        resolve_at(),
        resolve_end()
    ) == 0,
    "B260: the review request must stop hardcoding a half-hour default"
);
}
#[test]
fn frozen_source_contract_06() {
    assert!(
    count_between(
        CLI_BYTES,
        b"default_review_deadline_secs(",
        resolve_at(),
        resolve_end()
    ) == 1,
    "B260: the review request must derive its default from the declared workload"
);
}

// ---------------------------------- A5/A6 · the death says how far it got ---

#[test]
fn frozen_source_contract_07() {
    assert!(
    count(WAKE_BYTES, b"\"lastFrameAgeSecs\": status.") >= 1
        && count(WAKE_BYTES, b"\"lastFrameSummary\": status.") >= 1,
    "B260: the ManagedWakeTerminated payload must carry the real last-frame gap and summary"
);
}
#[test]
fn frozen_source_contract_08() {
    assert!(
    count(WAKE_BYTES, b"\"lastFrameAgeSecs\": facts.") == 0
        && count(WAKE_BYTES, b"\"lastFrameSummary\": facts.") == 0,
    "B260: the canonical attach-state evidence set is a durable digest — do not extend it"
);
}
#[test]
fn frozen_source_contract_09() {
    assert!(
    count(WAKE_BYTES, b"\"logBytesRead\": facts.log_bytes_read") == 1,
    "B260: the canonical evidence set keeps its existing shape"
);
}

// ------------------------------------------------------- runtime companion ---

/// A1/A2 — the derivation is a real function of role and declared workload, and
/// it is monotone, bounded, and never shorter than the default it replaces.
#[test]
fn the_review_deadline_is_derived_from_the_work_and_never_shrinks() {
    // A1 — not flat in either argument.
    let primary_light = review_deadline_secs("primary", 1);
    let primary_heavy = review_deadline_secs("primary", 15);
    let secondary_heavy = review_deadline_secs("secondary", 15);
    assert!(
        primary_heavy > primary_light,
        "more declared evidence must buy more time: {primary_light} vs {primary_heavy}"
    );
    assert!(
        primary_heavy > secondary_heavy,
        "primary does the mutation self-proof; it must not get less than secondary: \
         {primary_heavy} vs {secondary_heavy}"
    );

    // A2 — bounded on both sides. The lower bound is the old fixed default:
    // this card may only ever lengthen a deadline, never shorten one.
    for role in ["primary", "secondary", "nongate"] {
        for items in [0usize, 1, 7, 15, 64, 4096] {
            let secs = review_deadline_secs(role, items);
            assert!(
                (1800..=MANAGED_WAKE_MAX_RUNTIME_SECS).contains(&secs),
                "{role}/{items} produced {secs}, outside [1800, {MANAGED_WAKE_MAX_RUNTIME_SECS}]"
            );
        }
        // Monotone, and saturating rather than overflowing at absurd inputs.
        let mut previous = 0;
        for items in [0usize, 1, 2, 8, 32, 128, 512, 4096] {
            let secs = review_deadline_secs(role, items);
            assert!(secs >= previous, "{role} must be monotone in workload");
            previous = secs;
        }
        assert_eq!(
            review_deadline_secs(role, usize::MAX),
            MANAGED_WAKE_MAX_RUNTIME_SECS,
            "an absurd workload must saturate at the ceiling, not wrap"
        );
    }

    // The observed r69 distribution is the reason this card exists: nine
    // successful opencode reviews took 15.7-31.5 minutes. A seven-item card must
    // buy comfortably more than the 31.5-minute worst case that still succeeded.
    let realistic = review_deadline_secs("secondary", 7);
    assert!(
        realistic > 31 * 60 + 30,
        "a seven-item secondary review must outlast r69's slowest success (31.5min); got {realistic}s"
    );

    // A4 companion: the IR-reading wrapper is a real public entry point.
    let _: fn(&std::path::Path, &str, &str) -> anyhow::Result<u64> = default_review_deadline_secs;
}
