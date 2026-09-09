//! B262 frozen contract · an attempt terminal releases its review slots, and a
//! second review request for the same slot supersedes the first instead of
//! colliding with it.
//!
//! ## The defect (H148)
//!
//! `legacy::agent_inflight_load_from_events` keeps two projections:
//! `implementations` and `reviews`.  The four attempt terminals —
//! `AttemptBlocked` / `AttemptCrashed` / `AttemptTimedOut` / `AttemptFailed` —
//! only do `implementations.remove(...)`.  **Nothing ever removes a review slot
//! except a substantive `ReviewDelivered`.**  So every review that was requested
//! on an attempt that later died holds its slot forever.
//!
//! r69's closing measurement of unbalanced `(task, role)` slots:
//!
//! ```text
//! B246/secondary ×1   B252/secondary ×1   B253/primary ×2   B253/secondary ×2
//! ```
//!
//! `executor-opencode` has quota 12 and survived.  **`executor-claw` has quota 1**
//! — in r70 it was raised to 4, and this round schedules 3 of them.  One leak
//! and the primary seat is gone for the rest of the round, with no mechanical
//! way to get it back.
//!
//! The same unbalanced slots are also what strangled disk reclamation: r70's
//! pre-round cleanup measured 44 `WorkspaceLeased` against 4 unbalanced ones,
//! and `sweep_targets` pinned every one of their targets as `active_preserved`
//! forever.  A review slot is not bookkeeping; it is capacity **and** storage.
//!
//! ## The second half: a re-request collides with its own predecessor
//!
//! When a slot must be re-reviewed (r69 needed it twice — the planner's review
//! brief had listed 7 of 15 `requiredEvidence` items), the second
//! `orch wake --review-for` lands in a half state: `WorkspaceLeased` /
//! `WakeIssued` / `ReviewRequested` all append, the session really starts and
//! really produces frames — and then the backend receipt reconciliation refuses
//! it with
//!
//! ```text
//! conflicting ReviewRequested exists for wakeId=…
//! ```
//!
//! and writes `ActionRejected(exitCode=2)`.  The ledger says rejected while the
//! session is demonstrably running.  Cause: two `ReviewRequested` now exist for
//! the same `(task, attempt, role, agent)` and the receipt guard matched the
//! wrong one.
//!
//! ## What this contract freezes
//!
//! A1  All four attempt terminals release the review slots of that attempt.
//! A2  A terminal for a *different* attempt does not release this one.
//! A3  `ReviewDelivered` keeps releasing the slot exactly as before.
//! A4  `current_review_request` names the one live request for a slot, latest
//!     wins, so receipt reconciliation has an unambiguous target.
//! A5  Reachability (M4): the receipt guard consults it.
//! A6  Anti-laundering: the collision rejection still exists — this card makes
//!     the guard address the right request, it does not delete the guard.
//!
//! Iron rule 10: after relocation this target is byte-frozen.
//!
//! M1: revert the terminals to `implementations`-only -> A1 red.
//! M2: release every task's review slots on any terminal -> A2 red.
//! M3: make `current_review_request` return the first instead of the latest
//!     -> A4 red.
//! M4: keep the receipt guard scanning all requests -> A5 red.
//! M5: delete the collision rejection -> A6 red.

#![allow(dead_code)]
// 冻结种子内的窗口辅助函数按契约成组保留；未被本轮某条断言用到的
// 不得删除——删掉会让后续 M 变异无处落脚，也会让契约看起来比实际更窄。

use orch_core::EventRecord;
use orch_host::legacy::{agent_inflight_load_from_events, LoadKind};

// Compile-red until the live-request projection exists.
use orch_host::wake::current_review_request;

const LOAD_AUDIT: &str = include_str!("../src/legacy.rs");
const WAKE: &str = include_str!("../src/wake.rs");
fn load_audit_bytes() -> &'static [u8] {
    LOAD_AUDIT.split_once("mod load_audit {").unwrap().1
        .split_once("/// Read-only occupancy facts and historical admission checks").unwrap().0.as_bytes()
}
const WAKE_BYTES: &[u8] = WAKE.as_bytes();

const MISSING: usize = usize::MAX;
const ITEM_END: &[u8] = b"\n}\n";
const GUARD_FN: &[u8] = b"\nfn review_request_matches(";

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

// A1 source companion: exactly one `reviews.remove(` exists on the signed
// baseline (the ReviewDelivered arm). The terminals must add their own.
#[test]
fn frozen_source_contract_01() {
    assert!(
    count(load_audit_bytes(), b"reviews.remove(") >= 2,
    "B262: the attempt terminals must release review slots, not only implementations"
);
}
#[test]
fn frozen_source_contract_02() {
    assert!(
    count(load_audit_bytes(), b"\"AttemptBlocked\" | \"AttemptCrashed\"") == 1,
    "B262: the four-terminal arm stays a single arm — do not fan it out to dodge the contract"
);
}

// A5/A6 — the receipt guard addresses the current request, and still rejects.
#[test]
fn frozen_source_contract_03() {
    assert!(
    count(WAKE_BYTES, GUARD_FN) == 1,
    "B262: the receipt guard must be a unique definition"
);
}

fn guard_at() -> usize { find(WAKE_BYTES, GUARD_FN) }
fn guard_end() -> usize { find_from(WAKE_BYTES, ITEM_END, guard_at()) }

#[test]
fn frozen_source_contract_04() {
    assert!(
    guard_end() != MISSING,
    "B262: the receipt guard must terminate at a column-0 closing brace"
);
}
#[test]
fn frozen_source_contract_05() {
    assert!(
    count_between(WAKE_BYTES, b"current_review_request(", guard_at(), guard_end()) >= 1,
    "B262: the receipt guard must compare against the current request for the slot"
);
}
#[test]
fn frozen_source_contract_06() {
    assert!(
    count(WAKE_BYTES, b"conflicting ReviewRequested exists for wakeId=") == 1,
    "B262: the collision rejection survives — this card re-aims it, it does not delete it"
);
}

// ------------------------------------------------------- runtime companion ---

const ROUND: &str = "r90";
const TASK: &str = "B900";
const ATTEMPT: &str = "B900-A0001";
const OTHER_ATTEMPT: &str = "B900-A0002";
const AGENT: &str = "executor-claw";
const ROLE: &str = "primary";

fn record(kind: &str, event_id: &str, ts: &str, payload: serde_json::Value) -> EventRecord {
    EventRecord {
        event_id: event_id.to_string(),
        ts: ts.to_string(),
        actor: "runtime:orch".to_string(),
        kind: kind.to_string(),
        task_id: Some(TASK.to_string()),
        round: Some(ROUND.to_string()),
        payload: Some(payload),
        extra: serde_json::Map::new(),
    }
}

fn review_requested(event_id: &str, ts: &str, attempt: &str, wake_id: &str) -> EventRecord {
    record(
        "ReviewRequested",
        event_id,
        ts,
        serde_json::json!({
            "agent": AGENT,
            "attemptId": attempt,
            "continuationId": format!("review:{ROUND}:{TASK}:{attempt}:{ROLE}:{AGENT}"),
            "deadlineSecs": 1800,
            "requestedAt": ts,
            "role": ROLE,
            "wakeId": wake_id,
        }),
    )
}

fn attempt_terminal(kind: &str, attempt: &str) -> EventRecord {
    record(
        kind,
        "01B262TERMINAL000000000001",
        "2026-08-11T12:00:00Z",
        serde_json::json!({
            "actionId": "attempt-blocked",
            "agent": "executor-desktop",
            "attemptId": attempt,
            "attemptNo": 1,
        }),
    )
}

fn review_slots(events: &[EventRecord]) -> usize {
    agent_inflight_load_from_events(events, ROUND)
        .expect("load projection")
        .get(AGENT)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.kind == LoadKind::Review)
                .count()
        })
        .unwrap_or(0)
}

/// A1/A2/A3 — the slot accounting.
#[test]
fn an_attempt_terminal_releases_the_review_slots_of_that_attempt() {
    let requested = review_requested(
        "01B262REVIEWREQUESTED000001",
        "2026-08-11T10:00:00Z",
        ATTEMPT,
        "019ff030-1ea2-492a-b012-f27562e2d7a0",
    );

    assert_eq!(
        review_slots(std::slice::from_ref(&requested)),
        1,
        "an outstanding review request occupies one slot"
    );

    // A1 — all four terminals release it. r69 leaked five slots because none did.
    for kind in [
        "AttemptBlocked",
        "AttemptCrashed",
        "AttemptTimedOut",
        "AttemptFailed",
    ] {
        let events = vec![requested.clone(), attempt_terminal(kind, ATTEMPT)];
        assert_eq!(
            review_slots(&events),
            0,
            "{kind} on the reviewed attempt must release its review slot"
        );
    }

    // A2 — a terminal for a different attempt is not a release.
    let unrelated = vec![
        requested.clone(),
        attempt_terminal("AttemptBlocked", OTHER_ATTEMPT),
    ];
    assert_eq!(
        review_slots(&unrelated),
        1,
        "a terminal for another attempt must not release this slot"
    );
}

/// A4 — the current request for a slot is the latest one, so a re-review has an
/// unambiguous target instead of colliding with its own predecessor.
#[test]
fn a_second_review_request_supersedes_the_first_for_that_slot() {
    let first = review_requested(
        "01B262REVIEWREQUESTED000001",
        "2026-08-11T10:00:00Z",
        ATTEMPT,
        "019ff030-1ea2-492a-b012-f27562e2d7a0",
    );
    let second = review_requested(
        "01B262REVIEWREQUESTED000002",
        "2026-08-11T11:00:00Z",
        ATTEMPT,
        "019ff050-3573-4c69-a95e-1a020a820ab2",
    );
    let events = vec![first.clone(), second.clone()];

    let current = current_review_request(&events, ROUND, TASK, ROLE)
        .expect("a slot with two requests still has exactly one current request");
    assert_eq!(
        current.event_id, second.event_id,
        "the later request supersedes the earlier one; the guard must not match the stale one"
    );

    // One request is trivially its own current request.
    let single = vec![first.clone()];
    assert_eq!(
        current_review_request(&single, ROUND, TASK, ROLE)
            .expect("one request is current")
            .event_id,
        first.event_id
    );

    // An empty slot has none — the guard must not invent one.
    assert!(
        current_review_request(&[], ROUND, TASK, ROLE).is_none(),
        "no request means no current request"
    );

    // Two requests still occupy exactly one slot: a supersede is not a leak.
    assert_eq!(
        review_slots(&events),
        1,
        "superseding must not double-book the seat"
    );
}
