//! B261 frozen contract · a declared session death releases its continuation,
//! and a reused wake never echoes the same line as a real dispatch.
//!
//! ## The defect (H145 / H145-b), twice fatal in r69
//!
//! `wake_fence_decision` treats a wake as live when
//! `backend_accepted || !wrapper_exited`, and `backend_accepted` is derived from
//! a single `AgentEventReceived(wake-backend-receipt)`.  Once a provider has
//! ever acknowledged a wake, that flag is **permanently true** — even after the
//! provider process has died.  The fence therefore returns `Idempotent`
//! forever, and the CLI prints:
//!
//! ```text
//! orch wake executor-opencode: 注入已完成
//! ```
//!
//! …**exit 0, with no process started and no `WakeIssued` appended.**  The
//! caller cannot tell this apart from a real dispatch.  r69 measured it twice:
//!
//! * **B253-A0002** — opencode's primary-review session had been accepted, then
//!   killed itself on a sandbox rejection.  Every re-dispatch became a silent
//!   no-op: log still 82 lines, one sessionID, no new `WakeIssued` in the
//!   ledger.  The planner read "注入已完成" as success and then misdiagnosed the
//!   *old* session's log lines as a fresh sandbox hit.
//! * **B246-A0003** — the reviewer had finished **all** substantive work
//!   (213 frames, zero sandbox rejections, all four SHAs independently
//!   re-derived, worktree clean) and died writing the file.  `declare-dead`
//!   successfully appended `ManagedWakeAttachState(StoppedByHardDeadline)` — and
//!   the fence did not care, because `active_wakes_from_events` does not consume
//!   that event at all.  Re-dispatch: "注入已完成", 213 lines, no new process.
//!
//! ⇒ `orch wake declare-dead` — the command whose entire purpose is to record
//! that a wake is dead — **has no effect on the predicate that decides whether a
//! wake is alive.**  The continuation is mechanically unusable and nothing tells
//! the caller.
//!
//! ## What this contract freezes
//!
//! A1  `active_wakes_from_events` is a public projection (it is the thing that
//!     decides liveness; it must be independently testable).
//! A2  A terminal `ManagedWakeAttachState` releases its lifecycle slot.
//! A3  A non-terminal attach state does **not** release it — `declare-dead` is
//!     authenticated evidence, not a wish.
//! A4  `WakeRunOutcome` is public and both authorized wake entry points return
//!     it, so "spawned" and "reused" are different values, not the same `()`.
//! A5  Reachability (M4): the liveness projection actually consults
//!     `ManagedWakeAttachState`, and the CLI actually branches on the outcome.
//! A6  Anti-laundering: the fence's live predicate and its ambiguity check
//!     survive — this card releases *proven-dead* wakes, it does not weaken the
//!     one-live-wake invariant.
//!
//! Iron rule 10: after relocation this target is byte-frozen.
//!
//! M1: ignore `ManagedWakeAttachState` in the projection -> A2 red.
//! M2: release on any attach state regardless of outcome -> A3 red.
//! M3: keep `-> Result<()>` on the authorized entries -> A4 red (compile).
//! M4: keep the CLI printing one unconditional line -> A5 red.
//! M5: delete the ambiguous-multiple-live-wakes failure -> A6 red.

#![allow(dead_code)]
// 冻结种子内的窗口辅助函数按契约成组保留；未被本轮某条断言用到的
// 不得删除——删掉会让后续 M 变异无处落脚，也会让契约看起来比实际更窄。

use orch_core::EventRecord;

// Compile-red until liveness is a public, testable projection and the wake
// outcome is a value the CLI can branch on.
use orch_host::wake::{active_wakes_from_events, WakeRunOutcome};

const WAKE: &str = include_str!("../src/wake.rs");
const CLI: &str = include_str!("../../orch-cli/src/main.rs");
const WAKE_BYTES: &[u8] = WAKE.as_bytes();
const CLI_BYTES: &[u8] = CLI.as_bytes();

const MISSING: usize = usize::MAX;
const ITEM_END: &[u8] = b"\n}\n";
const PROJECTION_FN: &[u8] = b"\npub fn active_wakes_from_events(";

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

// ------------------------------------------- A5 · production reachability ---

#[test]
fn frozen_source_contract_01() {
    assert!(
    count(WAKE_BYTES, PROJECTION_FN) == 1,
    "B261: the liveness projection must exist exactly once as a public item"
);
}

fn proj_at() -> usize { find(WAKE_BYTES, PROJECTION_FN) }
fn proj_end() -> usize { find_from(WAKE_BYTES, ITEM_END, proj_at()) }

#[test]
fn frozen_source_contract_02() {
    assert!(
    proj_end() != MISSING,
    "B261: the projection must terminate at a column-0 closing brace"
);
}
#[test]
fn frozen_source_contract_03() {
    assert!(
    count_between(WAKE_BYTES, b"ManagedWakeAttachState", proj_at(), proj_end()) >= 1,
    "B261: the liveness projection must consume declared-death evidence"
);
}

// A4 — both authorized entries hand the outcome back to the caller.
//
// A whole-file count would be useless here: three *internal* helpers already
// return `Result<WakeRunOutcome>` on the signed baseline, so `count >= 2` is
// satisfied before any work is done.  The assertion must be window-scoped to
// the two public entries that today throw the outcome away.
const AUTH_FN: &[u8] = b"\npub fn run_wake_with_message_authorized(";
const AUTH_REVIEW_FN: &[u8] = b"\npub fn run_wake_with_message_authorized_review(";

#[test]
fn frozen_source_contract_04() {
    assert!(
    count(WAKE_BYTES, AUTH_FN) == 1 && count(WAKE_BYTES, AUTH_REVIEW_FN) == 1,
    "B261: both authorized entries must be unique public definitions"
);
}

fn auth_at() -> usize { find(WAKE_BYTES, AUTH_FN) }
fn auth_end() -> usize { find_from(WAKE_BYTES, ITEM_END, auth_at()) }
fn auth_review_at() -> usize { find(WAKE_BYTES, AUTH_REVIEW_FN) }
fn auth_review_end() -> usize { find_from(WAKE_BYTES, ITEM_END, auth_review_at()) }

#[test]
fn frozen_source_contract_05() {
    assert!(
    count_between(
        WAKE_BYTES,
        b"-> Result<WakeRunOutcome> {",
        auth_at(),
        auth_end()
    ) == 1
        && count_between(
            WAKE_BYTES,
            b"-> Result<WakeRunOutcome> {",
            auth_review_at(),
            auth_review_end()
        ) == 1,
    "B261: both authorized wake entries must return the outcome, not ()"
);
}

// A5 — the CLI branches on it and says something different.
#[test]
fn frozen_source_contract_06() {
    assert!(
    count(CLI_BYTES, b"WakeRunOutcome::Idempotent") >= 1,
    "B261: the CLI must branch on the wake outcome"
);
}
#[test]
fn frozen_source_contract_07() {
    assert!(
    count(CLI_BYTES, "未起新进程".as_bytes()) >= 1,
    "B261: a reused wake must say so — it must not be printable as a real dispatch"
);
}

// ----------------------------------------------------- A6 · anti-laundering ---

// Two call sites carry this predicate on the signed baseline: the fence's own
// `live` filter and the reissue path's successor check. Both must survive.
#[test]
fn frozen_source_contract_08() {
    assert!(
    count(WAKE_BYTES, b"wake.backend_accepted || !wake.wrapper_exited") == 2,
    "B261: the fence's live predicate stays at both sites — this card releases proven-dead wakes only"
);
}
#[test]
fn frozen_source_contract_09() {
    assert!(
    count(WAKE_BYTES, b"has ambiguous multiple live wakes") == 1,
    "B261: the one-live-wake invariant must not be relaxed"
);
}

// ------------------------------------------------------- runtime companion ---

const ROUND: &str = "r90";
const TASK: &str = "B900";
const ATTEMPT: &str = "B900-A0001";
const AGENT: &str = "executor-opencode";
const ROLE: &str = "secondary";
const WAKE_ID: &str = "019ff1ee-0457-4bd7-83b5-d09ec22a8eec";

fn record(kind: &str, event_id: &str, payload: serde_json::Value) -> EventRecord {
    EventRecord {
        event_id: event_id.to_string(),
        ts: "2026-08-11T10:00:00Z".to_string(),
        actor: "runtime:orch".to_string(),
        kind: kind.to_string(),
        task_id: Some(TASK.to_string()),
        round: Some(ROUND.to_string()),
        payload: Some(payload),
        extra: serde_json::Map::new(),
    }
}

/// Shapes copied from the real r69 ledger, not invented.
fn review_requested() -> EventRecord {
    record(
        "ReviewRequested",
        "01B261REVIEWREQUESTED000001",
        serde_json::json!({
            "agent": AGENT,
            "attemptId": ATTEMPT,
            "continuationId": format!("review:{ROUND}:{TASK}:{ATTEMPT}:{ROLE}:{AGENT}"),
            "deadlineSecs": 1800,
            "role": ROLE,
            "wakeId": WAKE_ID,
        }),
    )
}

fn declared_death(outcome: &str, phase: &str) -> EventRecord {
    record(
        "ManagedWakeAttachState",
        "01B261ATTACHSTATE0000000001",
        serde_json::json!({
            "actionId": "b261-action",
            "agent": AGENT,
            "attemptId": ATTEMPT,
            "evidenceSha256": "2baf46ec5c7f89317be1223421cafe3ed0001e715d1050974582fe397ad9edad",
            "outcome": outcome,
            "phase": phase,
            "role": ROLE,
            "wakeId": WAKE_ID,
        }),
    )
}

/// A1/A2/A3 — the whole point of `declare-dead`.
#[test]
fn a_declared_session_death_releases_its_continuation() {
    // A1 — public and callable; an empty ledger holds nothing live.
    assert!(
        active_wakes_from_events(&[], ROUND, AGENT)
            .expect("an empty ledger projects cleanly")
            .is_empty(),
        "no events means no live wakes"
    );

    // The lifecycle exists and is considered live.
    let live = active_wakes_from_events(&[review_requested()], ROUND, AGENT)
        .expect("a review lifecycle projects");
    assert_eq!(
        live.len(),
        1,
        "an outstanding review request occupies exactly one lifecycle slot: {live:?}"
    );

    // A2 — a terminal declared death releases it. This is exactly what
    // `orch wake declare-dead` appends, and today it changes nothing.
    for outcome in [
        "TruncatedNoTerminal",
        "StoppedByHardDeadline",
        "StoppedByAuthenticatedCancel",
    ] {
        let after = active_wakes_from_events(
            &[review_requested(), declared_death(outcome, "session-death-declared")],
            ROUND,
            AGENT,
        )
        .expect("projection after a declared death");
        assert!(
            after.is_empty(),
            "{outcome} is a terminal death: the continuation must be released, got {after:?}"
        );
    }

    // A3 — a delivered terminal is not a death, and a non-terminal phase is not
    // evidence of anything. Neither may release the slot.
    let delivered = active_wakes_from_events(
        &[
            review_requested(),
            declared_death("DeliveredTerminal", "session-death-declared"),
        ],
        ROUND,
        AGENT,
    )
    .expect("projection with a delivered terminal");
    assert_eq!(
        delivered.len(),
        1,
        "a delivered terminal is a completed wake, not a released-by-death one: {delivered:?}"
    );

    // A4 — the outcome is a value with two distinguishable variants.
    let spawned = WakeRunOutcome::Spawned {
        wake_id: WAKE_ID.to_string(),
    };
    let reused = WakeRunOutcome::Idempotent {
        wake_id: WAKE_ID.to_string(),
    };
    assert_ne!(
        spawned, reused,
        "spawning and reusing must not be the same value — that is the whole defect"
    );
}
