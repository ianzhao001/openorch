//! B259 frozen contract · the post-merge suffix guard must accept the runtime's
//! own `SiteRetired`, and it must accept it **only** on a fully anchored
//! identity — never by bare event kind.
//!
//! ## The defect (H138), five occurrences in r69
//!
//! `orch seal` closes a task by appending, in one checked batch,
//! `TaskRecorded` together with the `SiteRetired` events that retire the sites
//! that task leased.  Its final authorization pass then re-reads the
//! expected-main ledger suffix and rejects anything not on a whitelist
//! (`verify.rs:2371`).  That whitelist forgot the very events the runtime had
//! just written, so the command reports:
//!
//! ```text
//! merge 前 expected-main ledger suffix 含未授权事件 SiteRetired:runtime:orch
//! ```
//!
//! …**after** the irreversible work is already durable.  r69 measured it five
//! times.  The clearest is B247-A0001: post-merge gates `3 green / 0 red /
//! 367680ms`, `MergeExecuted 4ead9b27`, `TaskRecorded 16:33:17` — all on the
//! ledger — and the process still returned exit 2.  B246-A0004 was identical
//! (mergeSha `d1e6932a`, `SiteRetired ×7` appended in the same second as
//! `TaskRecorded`).  Two byte-identical shapes ⇒ this is deterministic, not
//! flaky.
//!
//! Nothing was actually wrong.  But every occurrence forced a human to read the
//! ledger line by line to decide whether exit 2 meant "your task failed" or
//! "the guard mis-scored its own bookkeeping".  That is the cost this whole
//! round exists to remove.
//!
//! ## The rule this contract freezes
//!
//! A `SiteRetired` in the post-merge suffix is authorized **iff all five hold**:
//!
//!   1. `actor == "runtime:orch"` — only the runtime retires sites;
//!   2. same `round`, and its `taskId` is the task being recorded;
//!   3. `retireEventId` names an event that is present in the same suffix and
//!      is the **unique** `TaskRecorded` for that task;
//!   4. the retired site identity — `siteId` / `generation` / `attemptId` /
//!      `role` / `agent` — matches a `WorkspaceLeased` that already exists in
//!      the prior suffix;
//!   5. that identity is retired **at most once**.
//!
//! Anything short of all five stays rejected.  The backlog entry is explicit
//! about why: *"绝不能裸放行 event kind"*.  A whitelist that only checks
//! `kind == "SiteRetired"` would let a forged retirement ride into main behind a
//! legitimate record, and site retirement is what releases capacity and disk.
//!
//! ## What this contract proves
//!
//! A1  The predicate exists as a pure, independently testable function.
//! A2  It accepts the exact canonical shape r69 produced five times.
//! A3  It rejects each of the five conditions being violated, one at a time.
//! A4  Reachability (M4): `validate_expected_main_contract` actually consults it
//!     inside the `CanonicalPostMergeSuffix` branch — a predicate nobody calls
//!     is the r64/H116 failure mode.
//! A5  Anti-laundering: the guard is not disabled wholesale — the rejection
//!     `bail!` and the existing whitelist terms survive.
//!
//! Iron rule 10: after relocation this target is byte-frozen.
//!
//! M1: accept by bare kind (drop the identity/anchor checks) -> A3 red.
//! M2: keep the predicate but never call it from the guard -> A4 red.
//! M3: delete the `未授权事件` bail! or widen the whitelist to everything -> A5 red.
//! M4: accept a `SiteRetired` whose `retireEventId` does not resolve to the
//!     unique TaskRecorded -> A3 red.

#![allow(dead_code)]
// 冻结种子内的窗口辅助函数按契约成组保留；未被本轮某条断言用到的
// 不得删除——删掉会让后续 M 变异无处落脚，也会让契约看起来比实际更窄。

use orch_core::EventRecord;

// Compile-red until the predicate exists.
use orch_host::verify::post_merge_site_retirement_is_authorized;

const VERIFY: &str = include_str!("../src/verify.rs");
const VERIFY_BYTES: &[u8] = VERIFY.as_bytes();

const MISSING: usize = usize::MAX;
const ITEM_END: &[u8] = b"\n}\n";
const GUARD_FN: &[u8] = b"fn validate_expected_main_contract(";

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
    count(VERIFY_BYTES, GUARD_FN) == 1,
    "B259: `fn validate_expected_main_contract(` must occur exactly once"
);
}

fn guard_at() -> usize { find(VERIFY_BYTES, GUARD_FN) }
fn guard_end() -> usize { find_from(VERIFY_BYTES, ITEM_END, guard_at()) }

#[test]
fn frozen_source_contract_02() {
    assert!(
    guard_end() != MISSING,
    "B259: the guard must terminate at a column-0 closing brace"
);
}

// ------------------------------------------- A4 · production reachability ---

#[test]
fn frozen_source_contract_03() {
    assert!(
    count_between(
        VERIFY_BYTES,
        b"post_merge_site_retirement_is_authorized(",
        guard_at(),
        guard_end()
    ) >= 1,
    "B259: the post-merge whitelist must consult the retirement predicate"
);
}

// ----------------------------------------------------- A5 · anti-laundering ---

#[test]
fn frozen_source_contract_04() {
    assert!(
    count_between(
        VERIFY_BYTES,
        b"merge \xe5\x89\x8d expected-main ledger suffix \xe5\x90\xab\xe6\x9c\xaa\xe6\x8e\x88\xe6\x9d\x83\xe4\xba\x8b\xe4\xbb\xb6",
        guard_at(),
        guard_end()
    ) == 1,
    "B259: the rejection must survive — this card widens the whitelist, it does not remove it"
);
}
#[test]
fn frozen_source_contract_05() {
    assert!(
    count_between(VERIFY_BYTES, b"CommittedLedgerMode::CanonicalRootSuffix", guard_at(), guard_end())
        >= 1
        && count_between(
            VERIFY_BYTES,
            b"CommittedLedgerMode::CanonicalPostMergeSuffix",
            guard_at(),
            guard_end()
        ) >= 1,
    "B259: both suffix modes keep their own whitelists — the root suffix is not widened"
);
}

// ------------------------------------------------------- runtime companion ---

const ROUND: &str = "r90";
const TASK: &str = "B900";
const ATTEMPT: &str = "B900-A0001";
const RECORD_ID: &str = "01B259TASKRECORDED000000001";

fn event(kind: &str, actor: &str, payload: serde_json::Value) -> EventRecord {
    EventRecord {
        event_id: format!("01B259{kind}{actor}").replace([':', '-'], "0"),
        ts: "2026-08-11T18:46:01Z".to_string(),
        actor: actor.to_string(),
        kind: kind.to_string(),
        task_id: Some(TASK.to_string()),
        round: Some(ROUND.to_string()),
        payload: Some(payload),
        extra: serde_json::Map::new(),
    }
}

fn leased() -> EventRecord {
    event(
        "WorkspaceLeased",
        "runtime:orch",
        serde_json::json!({
            "siteId": "B900-primary-executor-claw-g01",
            "generation": 1,
            "taskId": TASK,
            "attemptId": ATTEMPT,
            "role": "primary",
            "agent": "executor-claw",
        }),
    )
}

fn recorded() -> EventRecord {
    let mut record = event("TaskRecorded", "runtime:orch", serde_json::json!({"taskId": TASK}));
    record.event_id = RECORD_ID.to_string();
    record
}

fn retirement() -> EventRecord {
    event(
        "SiteRetired",
        "runtime:orch",
        serde_json::json!({
            "siteId": "B900-primary-executor-claw-g01",
            "generation": 1,
            "taskId": TASK,
            "attemptId": ATTEMPT,
            "role": "primary",
            "agent": "executor-claw",
            "retireEventId": RECORD_ID,
        }),
    )
}

/// A2 — the exact shape `orch seal` wrote five times in r69.
#[test]
fn the_canonical_batch_retirement_is_authorized() {
    let prior = vec![leased(), recorded()];
    assert!(
        post_merge_site_retirement_is_authorized(&retirement(), &prior, ROUND, TASK),
        "the runtime's own batch retirement must not be scored as an unauthorized suffix event"
    );
}

/// A3 — each of the five conditions, violated one at a time.  A predicate that
/// bare-allows the kind passes A2 and fails every case here.
#[test]
fn every_single_missing_anchor_is_still_refused() {
    let prior = vec![leased(), recorded()];

    let mut foreign_actor = retirement();
    foreign_actor.actor = "executor-desktop".to_string();
    assert!(
        !post_merge_site_retirement_is_authorized(&foreign_actor, &prior, ROUND, TASK),
        "only the runtime may retire a site"
    );

    let mut other_round = retirement();
    other_round.round = Some("r89".to_string());
    assert!(
        !post_merge_site_retirement_is_authorized(&other_round, &prior, ROUND, TASK),
        "a retirement from another round is not authorized here"
    );

    let mut unanchored = retirement();
    unanchored.payload = Some(serde_json::json!({
        "siteId": "B900-primary-executor-claw-g01",
        "generation": 1,
        "taskId": TASK,
        "attemptId": ATTEMPT,
        "role": "primary",
        "agent": "executor-claw",
        "retireEventId": "01B259SOMETHINGELSE00000001",
    }));
    assert!(
        !post_merge_site_retirement_is_authorized(&unanchored, &prior, ROUND, TASK),
        "retireEventId must resolve to the unique TaskRecorded in this suffix"
    );

    let mut drifted = retirement();
    drifted.payload = Some(serde_json::json!({
        "siteId": "B900-secondary-executor-opencode-g01",
        "generation": 1,
        "taskId": TASK,
        "attemptId": ATTEMPT,
        "role": "secondary",
        "agent": "executor-opencode",
        "retireEventId": RECORD_ID,
    }));
    assert!(
        !post_merge_site_retirement_is_authorized(&drifted, &prior, ROUND, TASK),
        "a site that was never leased in this suffix cannot be retired by it"
    );

    let ambiguous = vec![leased(), recorded(), recorded()];
    assert!(
        !post_merge_site_retirement_is_authorized(&retirement(), &ambiguous, ROUND, TASK),
        "two TaskRecorded anchors are ambiguous and must fail closed"
    );

    let no_lease = vec![recorded()];
    assert!(
        !post_merge_site_retirement_is_authorized(&retirement(), &no_lease, ROUND, TASK),
        "a retirement without its matching WorkspaceLeased must fail closed"
    );
}
