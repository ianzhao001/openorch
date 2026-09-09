//! B264 frozen contract · a non-zero exit code says how far the command's
//! declared effect got — and the default for anything unclassified is
//! "I do not know", never "nothing happened".
//!
//! ## Why this card is the highest-leverage one in r70
//!
//! r69 ended with seven defects sharing one shape: **a failure signal that
//! disagrees with the actual state.**  The clearest single frame is B248's
//! close, which hit three guards in one command —
//! registry reap identity mismatch (H146), stale-binary refusal, and the merge
//! suffix guard (H138).  **All three behaved correctly. All three surfaced as
//! `exit 2`.**  Reading the exit code, the most basic discrimination tool there
//! is, had stopped working in this repository: every occurrence forced a human
//! to read the ledger line by line to decide whether `2` meant "your task
//! failed" or "the runtime mis-scored its own bookkeeping".
//!
//! r70's own pre-round cleanup produced three more instances in twenty minutes
//! (`orch sites gc` aborting the whole subsystem on one stale receipt;
//! `sweep-targets` reporting `incomplete` after deleting 4.2 GB;
//! `sweep-scratch` exiting 2 four times while reclaiming 2800 entries).
//!
//! ## The taxonomy (planner adjudication of the r70 fusion consult)
//!
//! Both independent consultants converged on the main dimension: **declared
//! effect progress**, judged against the ledger — not retryability, not fault,
//! not "does a human need to look".  They diverged on the code table and the
//! planner adjudicated in favour of the minimal-migration one, because it is the
//! only one that leaves every frozen contract byte-identical:
//!
//! | code | meaning | mechanical test |
//! |---:|---|---|
//! | 0 | EffectAchieved | the declared terminal effect is durable, no unexplained failure |
//! | 1 | EffectPartial | ≥1 unit achieved ∧ ≥1 failed/refused, **and the summary was printed** |
//! | 2 | Rejected (zero effect) | refused strictly before any effect, **and carrying an `ActionRejection`** |
//! | 5 | EffectUnknown | **the default for every unclassified error** |
//!
//! `3/4/6/7/64/70-72` stay exactly as they are: they leave through
//! `Ok(ExitCode::from(..))`, not the error path.
//!
//! The single most important property is the **default**.  Today every
//! unclassified `bail!` lands on `2` and thereby *claims* nothing happened.
//! After this card, claiming "nothing happened" requires evidence
//! (an `ActionRejection`), and everything else says "I do not know" — which is
//! the only honest answer a program can give about its own side effects.
//!
//! ## The discriminating pair
//!
//! Both of these exit `2` today.  They must diverge:
//!
//! * `orch --root <dir-with-no-round> status` — a plain `bail!` with no
//!   `ActionRejection` behind it, on a path that has already touched the
//!   filesystem. It must become **5**.
//! * `orch --root <nonexistent-path> status` — argument resolution failed before
//!   anything at all could happen. It stays **2**.
//!
//! An implementation that renames 2 to 5 everywhere fails the second.  One that
//! does nothing fails the first.  Neither can be satisfied by a comment.
//!
//! ## What this contract proves
//!
//! A1  `CliDisposition` exists with exactly the four codes above.
//! A2  The unclassified default really moved off 2, measured through the real
//!     binary — not through a unit test of a mapping table.
//! A3  A genuine zero-effect argument rejection still exits 2.
//! A4  The table is compiled into `orch guide`, so the contract ships with the
//!     tool instead of living in a document that can drift.
//! A5  Anti-laundering: the existing `ActionRejection` exit-code plumbing and its
//!     `1..=255` validation survive; production callers stop passing bare ints.
//!
//! Iron rule 10: after relocation this target is byte-frozen.
//!
//! M1: leave unclassified errors on 2 -> A2 red.
//! M2: renumber everything to 5 -> A3 red.
//! M3: ship the enum but never route the CLI through it -> A2 red.
//! M4: document the table only in the repo, not in `orch guide` -> A4 red.
//! M5: keep accepting a bare `i32` at production rejection sites -> A5 red.

#![allow(dead_code)]
// 冻结种子内的窗口辅助函数按契约成组保留；未被本轮某条断言用到的
// 不得删除——删掉会让后续 M 变异无处落脚，也会让契约看起来比实际更窄。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

// Compile-red until the taxonomy exists as a type.
use orch_host::failure::CliDisposition;

const FAILURE: &str = include_str!("../../orch-host/src/failure.rs");
const FAILURE_BYTES: &[u8] = FAILURE.as_bytes();

const MISSING: usize = usize::MAX;

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

// ----------------------------------------------------- A5 · anti-laundering ---

#[test]
fn frozen_source_contract_01() {
    assert!(
    count(
        FAILURE_BYTES,
        "ActionRejection.exit_code 必须在 1..=255（且必须与 CLI 实退一致）".as_bytes()
    ) == 1,
    "B264: the durable rejection's exit-code validation survives"
);
}
#[test]
fn frozen_source_contract_02() {
    assert!(
    count(FAILURE_BYTES, b"pub fn rejection_exit_code(") == 1,
    "B264: the existing extraction plumbing is reused, not replaced wholesale"
);
}
#[test]
fn frozen_source_contract_03() {
    assert!(
    count(FAILURE_BYTES, b"disposition: CliDisposition") >= 1,
    "B264: production rejection sites must classify by type, not by a bare integer"
);
}

// ------------------------------------------------------- runtime companion ---

static SEQ: AtomicU64 = AtomicU64::new(0);

fn scratch(name: &str) -> PathBuf {
    let orch_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("CARGO_MANIFEST_DIR 上溯两级应为 orch 工作区根");
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    orch_root.join("target/test-tmp").join(format!(
        "b264-{name}-{}-{seq}",
        std::process::id()
    ))
}

fn orch(args: &[&str]) -> (i32, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_orch"))
        .args(args)
        .output()
        .expect("run the real orch binary");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.code().unwrap_or(-1), text)
}

/// A1 — the four dispositions and their codes are one authoritative table.
#[test]
fn the_disposition_table_is_exactly_four_codes() {
    assert_eq!(CliDisposition::EffectAchieved.code(), 0);
    assert_eq!(CliDisposition::EffectPartial.code(), 1);
    assert_eq!(CliDisposition::Rejected.code(), 2);
    assert_eq!(CliDisposition::EffectUnknown.code(), 5);

    let all = CliDisposition::ALL;
    assert_eq!(all.len(), 4, "no fifth disposition may appear by accident");
    let mut codes = all.iter().map(|d| d.code()).collect::<Vec<_>>();
    codes.sort_unstable();
    assert_eq!(codes, vec![0, 1, 2, 5]);

    // Reserved ranges: shells own 126/127 and 128+N; sysexits owns 64..=78 and
    // wake-multica already uses 64/70/71/72.
    for disposition in all {
        let code = disposition.code();
        assert!(
            code < 64 || code > 78,
            "{disposition:?} collides with the sysexits range"
        );
        assert!(code < 126, "{disposition:?} collides with shell-reserved codes");
    }
}

/// A2/A3 — the discriminating pair, measured through the real binary.
///
/// Both exit 2 on the signed baseline. Exactly one of them must move.
#[test]
fn an_unclassified_error_stops_claiming_that_nothing_happened() {
    // A2 — a plain `bail!` on a path that has already touched the filesystem.
    let root = scratch("no-round");
    fs::create_dir_all(&root).expect("create an empty root");
    let (code, text) = orch(&["--root", &root.display().to_string(), "status"]);
    assert_eq!(
        code,
        CliDisposition::EffectUnknown.code() as i32,
        "an unclassified failure must say 'I do not know', not 'nothing happened': {text}"
    );
    let _ = fs::remove_dir_all(&root);

    // A3 — argument resolution failed before anything could happen at all. This
    // one is a genuine zero-effect rejection and must stay 2.
    let (code, text) = orch(&["--root", "/definitely/not/here/b264", "status"]);
    assert_eq!(
        code,
        CliDisposition::Rejected.code() as i32,
        "an argument that cannot be resolved is a real zero-effect rejection: {text}"
    );
}

/// A4 — the contract ships inside the tool.
#[test]
fn the_exit_table_is_compiled_into_the_guide() {
    let (code, text) = orch(&["guide"]);
    assert_eq!(code, 0, "guide must print: {text}");
    for needle in [
        "EffectAchieved",
        "EffectPartial",
        "Rejected",
        "EffectUnknown",
    ] {
        assert!(
            text.contains(needle),
            "orch guide must document {needle}; a table that lives only in the repo drifts"
        );
    }

    let (code, _) = orch(&["guide", "--check"]);
    assert_eq!(
        code, 0,
        "guide --check must verify the exit table the same way it verifies the command tree"
    );
}
