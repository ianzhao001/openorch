//! B263 frozen contract · storage reclamation never loses the progress it
//! already made, never lets one refusal abort the sweep, and never decides what
//! to keep by looking at a wall clock.
//!
//! ## Measured, not theorised (r70 pre-round cleanup, 2026-08-11)
//!
//! The round-close path really does run `sweep_test_scratch_root`,
//! `sweep_trial_cache`, `reclaim_task_sites` and `sweep_targets_for_round`
//! (`orch-cli/src/main.rs::prepare_round_close_cleanup`).  It still released
//! nothing across all of r69.  Five independent causes were measured:
//!
//! 1. **Unbalanced leases pin targets forever.** 44 `WorkspaceLeased` in r69,
//!    **4 unbalanced** (`B246-secondary-opencode-g02/g03`,
//!    `B248-secondary-opencode-g01`, `B251-implement-desktop-g01`);
//!    `sweep_targets` marks each `active_preserved` for ever.  Fixing the leak
//!    itself is B262's job; this card must stop the *reclaimer* from being the
//!    thing that turns one leak into permanent accumulation.
//! 2. **A 24-hour wall clock fights the close it runs in.** Round close ran at
//!    18:59:10Z; the two `B248` review targets were **~1 hour old**, `B253`'s
//!    **13.6 hours** — all younger than the TTL, so even the properly retired
//!    ones were preserved.  A round's own review targets are *always* younger
//!    than 24h when that round closes.  **This is a design-level leak, true
//!    every single round.**
//! 3. **One stale receipt silently stops the whole GC subsystem.**
//!    `close.rs::trigger_site_gc` runs
//!    `reconcile_pending_backend_receipts(root, &round)?` **before**
//!    `reap_released_sites` — so a single receipt mismatch aborts with `?` and
//!    reaping never runs at all; the error is then swallowed by `eprintln!`.
//!    Measured directly: `orch sites gc --round r69` exits 2 with
//!    `conflicting ReviewRequested exists for wakeId=019ff02f-…`, and that wake
//!    belongs to `B246-A0004 primary executor-claw`, a review that **completed
//!    and was already `SiteRetired` at 10:48:25**.  A false positive on a
//!    finished, retired review had silently disabled site GC for a whole round.
//! 4. **A refusal is recorded nowhere a human will look.** The `B253` review
//!    site still held a reviewer's un-reverted negative mutation
//!    (`M orch/crates/orch-host/src/fusion.rs`, staged); `reap_released_sites`
//!    correctly refused to tear it down, wrote `phase: "planned"` into a
//!    journal file, and emitted no event and no close-summary line.
//! 5. **Partial success is reported as total failure.**
//!    `orch sites sweep-scratch --ttl-hours 24` run four times in a row moved
//!    the entry count **3874 → 2043 → 1885 → 1469 → 1185 → 1053**, but the first
//!    three runs exited 2 printing one line, `Directory not empty`, and **no
//!    removed list and no freed bytes**.  The caller could not tell that 2800
//!    entries had already been reclaimed.  Root cause:
//!    `util::sweep_test_scratch_root` returns `io::Result<Vec<PathBuf>>` and
//!    `return`s on the first error, discarding everything it had already done.
//!
//! ## Deliberately NOT frozen here
//!
//! The macOS `.DS_Store` / `ENOTEMPTY` race that produced those errors is real
//! (Finder recreates the file during traversal; a single retry cleared it every
//! time in the measured runs) but it is **not** made part of this frozen oracle:
//! a contract that depends on Spotlight's timing would be flaky by
//! construction.  The bounded retry is required by the card and proven in
//! `requiredEvidence`; the oracle below only freezes deterministic properties.
//!
//! ## What this contract proves
//!
//! A1  A scratch sweep that hits an un-removable entry still reports every entry
//!     it did remove, and does not fail the command.
//! A2  It only fails when it reclaimed nothing at all.
//! A3  Reclamation decides by lease disposition, not by a wall clock: the
//!     round-close path no longer passes a 24-hour TTL.
//! A4  A receipt reconciliation failure cannot prevent reaping.
//! A5  A refusal is structured, addressable evidence — not a journal file nobody
//!     reads.
//! A6  Anti-laundering: refusing to tear down a dirty site is still a refusal;
//!     this card must not "fix" reclamation by deleting reviewers' work.
//!
//! Iron rule 10: after relocation this target is byte-frozen.
//!
//! M1: `return Err` on the first un-removable entry -> A1/A2 red.
//! M2: restore the 24-hour TTL at round close -> A3 red.
//! M3: put the receipt reconcile back in front of reaping with `?` -> A4 red.
//! M4: drop the structured refusal record -> A5 red.
//! M5: make reclamation force-remove a dirty site -> A6 red.

#![allow(dead_code)]
// 冻结种子内的窗口辅助函数按契约成组保留；未被本轮某条断言用到的
// 不得删除——删掉会让后续 M 变异无处落脚，也会让契约看起来比实际更窄。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

// Compile-red until reclamation reports instead of losing what it did.
use orch_host::sites::{reap_released_sites_reported, SiteReapReport};
use orch_host::util::{sweep_test_scratch_root, ScratchSweepReport};

const CLOSE: &str = include_str!("../src/close.rs");
const CLI: &str = include_str!("../../orch-cli/src/main.rs");
const SITES: &str = include_str!("../src/sites.rs");
const CLOSE_BYTES: &[u8] = CLOSE.as_bytes();
const CLI_BYTES: &[u8] = CLI.as_bytes();
const SITES_BYTES: &[u8] = SITES.as_bytes();

const MISSING: usize = usize::MAX;
const ITEM_END: &[u8] = b"\n}\n";
const TRIGGER_FN: &[u8] = b"\nfn trigger_site_gc(";
const CLOSE_CLEANUP_FN: &[u8] = b"\nfn prepare_round_close_cleanup(";

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
    count(CLOSE_BYTES, TRIGGER_FN) == 1 && count(CLI_BYTES, CLOSE_CLEANUP_FN) == 1,
    "B263: both anchors must be unique definitions"
);
}

fn trigger_at() -> usize { find(CLOSE_BYTES, TRIGGER_FN) }
fn trigger_end() -> usize { find_from(CLOSE_BYTES, ITEM_END, trigger_at()) }
fn cleanup_at() -> usize { find(CLI_BYTES, CLOSE_CLEANUP_FN) }
fn cleanup_end() -> usize { find_from(CLI_BYTES, ITEM_END, cleanup_at()) }

#[test]
fn frozen_source_contract_02() {
    assert!(
    trigger_end() != MISSING && cleanup_end() != MISSING,
    "B263: both anchored items must terminate at a column-0 closing brace"
);
}

// ------------------------------ A3 · disposition, not a wall clock ---

#[test]
fn frozen_source_contract_03() {
    assert!(
    count_between(
        CLI_BYTES,
        b"Duration::from_secs(24 * 60 * 60)",
        cleanup_at(),
        cleanup_end()
    ) == 0,
    "B263: a round's own review targets are always younger than 24h at its close — \
     reclamation must not decide by wall clock"
);
}

// ------------------------------ A4 · a stale receipt cannot stop reaping ---

#[test]
fn frozen_source_contract_04() {
    assert!(
    count_between(
        CLOSE_BYTES,
        b"reconcile_pending_backend_receipts(root, &round)?",
        trigger_at(),
        trigger_end()
    ) == 0,
    "B263: receipt reconciliation must not be able to abort site reaping with `?`"
);
}
#[test]
fn frozen_source_contract_05() {
    assert!(
    count_between(CLOSE_BYTES, b"reap_released_sites", trigger_at(), trigger_end()) >= 1,
    "B263: the record trigger must still reap"
);
}

// ------------------------------ A6 · a dirty site is still refused ---

#[test]
fn frozen_source_contract_06() {
    assert!(
    count(
        SITES_BYTES,
        "现场含 tracked/staged 修改；拒绝自动拆除".as_bytes()
    ) == 1,
    "B263: reclamation must keep refusing to tear down a site that still holds a \
     reviewer's uncommitted work"
);
}

// ------------------------------------------------------- runtime companion ---

static SEQ: AtomicU64 = AtomicU64::new(0);

fn scratch_root(name: &str) -> PathBuf {
    let orch_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("CARGO_MANIFEST_DIR 上溯两级应为 orch 工作区根");
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    orch_root.join("target/test-tmp").join(format!(
        "b263-{name}-{}-{seq}",
        std::process::id()
    ))
}

/// Make one entry deterministically un-removable: a directory holding a file,
/// with the directory itself stripped of write permission.  Unlinking the child
/// then fails with EACCES on every POSIX host, with no timing involved.
fn make_undeletable(parent: &Path, name: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = parent.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("pinned"), b"pinned\n").unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
    dir
}

fn restore(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
}

/// A1/A2 — the sweep keeps what it achieved.
///
/// r69's four consecutive `sweep-scratch` runs each reclaimed hundreds of
/// entries and each reported nothing but `Directory not empty`.  That is the
/// exact shape this test forbids.
#[test]
fn a_scratch_sweep_reports_everything_it_removed_even_when_one_entry_refuses() {
    let root = scratch_root("partial");
    fs::create_dir_all(&root).unwrap();
    for name in ["removable-a", "removable-b"] {
        fs::create_dir_all(root.join(name)).unwrap();
        fs::write(root.join(name).join("f"), b"x").unwrap();
    }
    let pinned = make_undeletable(&root, "pinned-c");

    let report: ScratchSweepReport =
        sweep_test_scratch_root(&root, Duration::ZERO, &[]).expect(
            "a sweep that reclaimed real entries must not present itself as a total failure",
        );

    assert_eq!(
        report.removed.len(),
        2,
        "both removable entries must be reported as removed: {:?}",
        report.removed
    );
    assert_eq!(
        report.failed.len(),
        1,
        "the refusal must be reported, with its path: {:?}",
        report.failed
    );
    assert!(
        report.failed[0].0.ends_with("pinned-c"),
        "the failure must name the entry it could not remove: {:?}",
        report.failed
    );
    assert!(!root.join("removable-a").exists());
    assert!(!root.join("removable-b").exists());

    // A2 — and when it reclaims nothing at all, that is a real failure.
    let barren = scratch_root("barren");
    fs::create_dir_all(&barren).unwrap();
    let only = make_undeletable(&barren, "pinned-only");
    let nothing = sweep_test_scratch_root(&barren, Duration::ZERO, &[]);
    match nothing {
        Err(_) => {}
        Ok(report) => assert!(
            report.removed.is_empty() && !report.failed.is_empty(),
            "a sweep that reclaimed nothing must not look like a success: {report:?}"
        ),
    }

    restore(&pinned);
    restore(&only);
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&barren);
}

/// A5 — a refusal is addressable evidence, not a journal file nobody reads.
#[test]
fn a_reap_refusal_is_structured_evidence() {
    let report: SiteReapReport = reap_released_sites_reported(Path::new("/nonexistent-b263"), "r90")
        .unwrap_or_else(|error| panic!("a missing root is not a reason to lose the report: {error:#}"));
    assert!(report.reaped.is_empty());
    assert!(
        report.refused.is_empty(),
        "nothing to refuse when there is nothing to reap: {:?}",
        report.refused
    );
    // The report must be able to *carry* a refusal with its reason; a bare count
    // is what made r69's `phase: \"planned\"` journal useless.
    let _: &Vec<(String, String)> = &report.refused;
}
