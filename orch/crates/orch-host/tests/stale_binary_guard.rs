//! B169 seeded-red contract (H35: a stale binary must not silently run
//! state-changing commands, and the criterion must not false-red on the
//! worktree builds and ledger commits that happen every single round).
//!
//! Negative mutations that must turn the named case red:
//! M1. Reduce the criterion to "main moved" or to a timestamp comparison.
//!     Almost every commit on main is ledger/BOARD/CURRENT bookkeeping — r56
//!     measured the binary at 15:59, the last card merged 15:58, main tip 16:56,
//!     everything between them being ledger commits. Either shortcut produces a
//!     stable false red, and a guard that cries wolf gets switched off.
//! M2. Forget that executors build inside their own worktrees, where the build
//!     commit is not an ancestor of main. Treating that as staleness blocks
//!     every executor's every orch invocation.
//! M3. Hard-fail on a missing stamp, or classify read-only commands as blocked.
//!     Diagnostics must survive a broken main — that is exactly when they are
//!     needed — and a binary built outside git must still run.

use orch_host::staleness::{staleness_verdict, BuildStamp, StaleVerdict};

const BUILD: &str = "1111111111111111111111111111111111111111";
const MAIN: &str = "2222222222222222222222222222222222222222";

fn stamp(sha: &str) -> BuildStamp {
    BuildStamp {
        commit: Some(sha.to_string()),
    }
}

fn changed(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|path| path.to_string()).collect()
}

#[test]
fn only_real_compilation_inputs_mark_a_binary_stale() {
    // M1: the anti-false-red case, and the reason the naive criterion is wrong.
    let bookkeeping = changed(&[
        "coordination/rounds/r57/events.jsonl",
        "coordination/BOARD.md",
        "coordination/CURRENT.md",
        "coordination/SESSION-HANDOFF-r56.md",
        "参考资料/fusion-confidence-open-source-2026-07-29/00-README.md",
    ]);
    assert_eq!(
        staleness_verdict(&stamp(BUILD), MAIN, true, &bookkeeping),
        StaleVerdict::Fresh,
        "ledger and doc commits must never mark the binary stale"
    );

    assert_eq!(
        staleness_verdict(
            &stamp(BUILD),
            MAIN,
            true,
            &changed(&["orch/crates/orch-host/src/round.rs"])
        ),
        StaleVerdict::Stale,
        "a change under orch/** is a real compilation input"
    );

    // The hook script is compiled in via include_bytes! (hooks.rs), so it is a
    // compilation input even though it lives outside orch/**.
    assert_eq!(
        staleness_verdict(
            &stamp(BUILD),
            MAIN,
            true,
            &changed(&[".githooks/reference-transaction"])
        ),
        StaleVerdict::Stale,
        "include_bytes! inputs count as compilation inputs"
    );

    // Mixed batches follow the source change, not the majority.
    assert_eq!(
        staleness_verdict(
            &stamp(BUILD),
            MAIN,
            true,
            &changed(&[
                "coordination/BOARD.md",
                "coordination/rounds/r57/events.jsonl",
                "orch/crates/orch-cli/src/main.rs",
            ])
        ),
        StaleVerdict::Stale,
        "one real input among bookkeeping is still stale"
    );
}

#[test]
fn a_worktree_build_is_not_a_stale_build() {
    // M2: executors build at their own detached HEAD every single attempt.
    assert_eq!(
        staleness_verdict(
            &stamp(BUILD),
            MAIN,
            false,
            &changed(&["orch/crates/orch-host/src/round.rs"])
        ),
        StaleVerdict::Fresh,
        "a build commit that is not an ancestor of main is a worktree build"
    );
    // Building exactly at main is trivially fresh regardless of the diff set.
    assert_eq!(
        staleness_verdict(&stamp(MAIN), MAIN, true, &changed(&["orch/crates/orch-cli/src/main.rs"])),
        StaleVerdict::Fresh,
        "a binary built at main tip is never stale"
    );
}

#[test]
fn a_missing_stamp_degrades_instead_of_failing() {
    // M3: built outside git, or git unavailable at build time. The guard must
    // say "unknown" so the caller can let the command through with a note —
    // never panic, never hard-fail, never silently claim freshness.
    let verdict = staleness_verdict(
        &BuildStamp { commit: None },
        MAIN,
        true,
        &changed(&["orch/crates/orch-host/src/round.rs"]),
    );
    assert_eq!(
        verdict,
        StaleVerdict::Unknown,
        "a missing stamp is unknown, not fresh and not stale"
    );
    assert_ne!(
        verdict,
        StaleVerdict::Fresh,
        "unknown must stay distinguishable from a verified-fresh binary"
    );

    // An empty diff set with an ancestor build commit is genuinely fresh.
    assert_eq!(
        staleness_verdict(&stamp(BUILD), MAIN, true, &[]),
        StaleVerdict::Fresh,
        "no changed files means nothing could have invalidated the binary"
    );
}
