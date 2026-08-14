//! B254 frozen contract · durable ACK bootstrap precedes optional await setup.
//!
//! r69/B253 pre-dispatch owner baseline at exact main
//! `403c6ed46b2eb3c2e8f1d3df15c25a06ce43daeb` reproduced H56 as the only
//! full-workspace failure: the ACK file existed while the durable pipeline was
//! still `AckPresentUnclaimed` at tick 5 of a four-tick budget.  The same bytes
//! passed ten isolated samples.  B252 moved the canonical claim ahead of
//! liveness, but optional notify setup and a duplicate full active-card compile
//! still happen before that claim.
//!
//! This contract keeps B203's public eight-second/four-tick bound and the H56
//! ordinary test lane unchanged.  It requires the first exact ledger/dispatch/
//! GO/ACK reconciliation and canonical durable claim to finish before optional
//! watcher filesystem setup (`create_dir_all` / `is_dir` / `watch`), receive,
//! sleep, or liveness edges can consume the signed budget.  The retained entry
//! guard must still fail closed through the critical-drift path, and the claim
//! itself keeps its independent write-time rebind.
//!
//! The compile-time sentinel is deliberate.  Before exact plan sign-off the
//! workspace's own unsigned-IR fixture may fail before an assertion-red
//! integration target is executed.  E0080 makes this source-order contract the
//! first stable red without adding a production API or weakening a gate.
//!
//! Iron rule 10: after relocation this target is byte-frozen.
//!
//! M1: move notify/mkdir/watch before the canonical claim -> sentinel/test red.
//! M2: restore the duplicate unlocked entry guard or drop critical wrapping -> red.
//! M3: gate the claim on watcher/liveness availability -> existing tierf unit red.

use std::{fs, path::Path};

const SOURCE: &str = include_str!("../src/tierf.rs");
const MISSING: usize = usize::MAX;

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

const fn count_between(haystack: &[u8], needle: &[u8], start: usize, end: usize) -> usize {
    if start == MISSING || end == MISSING || start >= end || end > haystack.len() {
        return 0;
    }
    let mut count = 0;
    let mut cursor = start;
    while cursor < end {
        let found = find_from(haystack, needle, cursor);
        if found == MISSING || found >= end {
            break;
        }
        count += 1;
        cursor = found + needle.len();
    }
    count
}

const fn ordered_between(
    haystack: &[u8],
    left: &[u8],
    right: &[u8],
    start: usize,
    end: usize,
) -> bool {
    if start == MISSING || end == MISSING || start >= end || end > haystack.len() {
        return false;
    }
    let left_position = find_from(haystack, left, start);
    let right_position = find_from(haystack, right, start);
    left_position != MISSING
        && right_position != MISSING
        && left_position < right_position
        && right_position < end
}

const SOURCE_BYTES: &[u8] = SOURCE.as_bytes();
const WRAPPER: usize = find_from(SOURCE_BYTES, b"pub fn run_await_with_hook(", 0);
const SECTION_END: usize = find_from(SOURCE_BYTES, b"pub struct NudgeOutcome", WRAPPER);
const CLAIM: usize = find_from(
    SOURCE_BYTES,
    b"ack_logged = claim_observed_dispatch_ack(",
    WRAPPER,
);
const CRITICAL_GUARD: usize = find_from(
    SOURCE_BYTES,
    b"critical_point(require_active_tierf_task(",
    WRAPPER,
);
const CONTRACT_IS_SATISFIED: bool = WRAPPER != MISSING
    && SECTION_END != MISSING
    && CLAIM != MISSING
    && WRAPPER < CLAIM
    && CLAIM < SECTION_END
    && count_between(
        SOURCE_BYTES,
        b"require_active_tierf_task(",
        WRAPPER,
        CLAIM,
    ) == 1
    && CRITICAL_GUARD != MISSING
    && CRITICAL_GUARD < CLAIM
    && ordered_between(
        SOURCE_BYTES,
        b"ack_logged = claim_observed_dispatch_ack(",
        b"runtime.install_watcher(",
        WRAPPER,
        SECTION_END,
    )
    && ordered_between(
        SOURCE_BYTES,
        b"ack_logged = claim_observed_dispatch_ack(",
        b"runtime.create_dir_all(",
        WRAPPER,
        SECTION_END,
    )
    && ordered_between(
        SOURCE_BYTES,
        b"ack_logged = claim_observed_dispatch_ack(",
        b"runtime.disable_watcher(",
        WRAPPER,
        SECTION_END,
    )
    && ordered_between(
        SOURCE_BYTES,
        b"ack_logged = claim_observed_dispatch_ack(",
        b"runtime.watcher_available()",
        WRAPPER,
        SECTION_END,
    )
    && ordered_between(
        SOURCE_BYTES,
        b"ack_logged = claim_observed_dispatch_ack(",
        b"runtime.watch(",
        WRAPPER,
        SECTION_END,
    )
    && ordered_between(
        SOURCE_BYTES,
        b"ack_logged = claim_observed_dispatch_ack(",
        b"hook(\"before-liveness-probe\")",
        WRAPPER,
        SECTION_END,
    )
    && ordered_between(
        SOURCE_BYTES,
        b"ack_logged = claim_observed_dispatch_ack(",
        b"runtime.recv_timeout(",
        WRAPPER,
        SECTION_END,
    )
    && ordered_between(
        SOURCE_BYTES,
        b"ack_logged = claim_observed_dispatch_ack(",
        b"runtime.sleep(",
        WRAPPER,
        SECTION_END,
    );

const _: () = assert!(
    CONTRACT_IS_SATISFIED,
    "B254: durable ACK bootstrap must precede optional await setup with one critical entry guard"
);

fn await_section(source: &str) -> &str {
    let start = source
        .find("pub fn run_await_with_hook(")
        .expect("B254: await wrapper disappeared");
    let tail = &source[start..];
    let end = tail
        .find("pub struct NudgeOutcome")
        .expect("B254: await section terminator disappeared");
    &tail[..end]
}

#[test]
fn durable_ack_bootstrap_precedes_optional_setup_with_one_critical_guard() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = fs::read_to_string(manifest.join("src/tierf.rs")).unwrap();
    let body = await_section(&source);
    let claim = body
        .find("ack_logged = claim_observed_dispatch_ack(")
        .expect("B254: canonical durable ACK bootstrap disappeared");

    for edge in [
        "runtime.install_watcher(",
        "runtime.create_dir_all(",
        "runtime.disable_watcher(",
        "runtime.watcher_available()",
        "runtime.watch(",
        "hook(\"before-liveness-probe\")",
        "runtime.recv_timeout(",
        "runtime.sleep(",
    ] {
        let position = body
            .find(edge)
            .unwrap_or_else(|| panic!("B254: optional await edge disappeared: {edge}"));
        assert!(
            claim < position,
            "B254: durable ACK bootstrap must precede optional await edge {edge}"
        );
    }

    let prefix = &body[..claim];
    assert_eq!(
        prefix.matches("require_active_tierf_task(").count(),
        1,
        "B254: duplicate pre-claim full IR compiles consume the signed ACK budget"
    );
    assert!(
        prefix.contains("critical_point(require_active_tierf_task("),
        "B254: the retained entry guard must preserve critical-drift passthrough"
    );
}
