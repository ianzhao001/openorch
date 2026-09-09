//! B256 frozen contract · the repo-internal scratch policy is a named public
//! constant, the default review prompt renders it, and the production wake path
//! still reaches that prompt.
//!
//! ## Why this card exists
//!
//! r69 lost a whole primary-review attempt to a rule that only existed in the
//! planner's head.  `executor-opencode` was doing formal primary review on
//! B253-A0002, decided to take a backup before a negative mutation, chose
//! `/tmp`, and its own sandbox killed the session:
//!
//! ```text
//! ! permission requested: external_directory (/tmp/*); auto-rejecting
//! ```
//!
//! The session ended with `reason: "tool-calls"` at 53717 tokens — nowhere near
//! any budget — having produced no review at all.  The discriminating evidence
//! is sharp: the same agent's two successful secondary reviews in the same round
//! had that counter at **0**, and the single failed session had it at exactly
//! **1**, with path `/tmp/*`.
//!
//! `AI-OPERATOR-RUNBOOK.md` pit 7 has carried "review sites must stay inside the
//! repository" since r56.  It was never in the machine.  Pit 1 of that same file
//! says it plainly: **rules that rely on the planner remembering them will be
//! violated.**  This card moves the rule from prose into the default prompt that
//! `orch wake --review-for` renders for every reviewer, every time.
//!
//! ## Two shapes this contract deliberately avoids
//!
//! * **`const fn` scanning of `wake.rs`.**  The first draft did its window
//!   searches in const context over `include_str!("../src/wake.rs")` (~700 KB).
//!   rustc answered with six copies of
//!   `error: constant evaluation is taking a long time` — a lint promoted to an
//!   error that carries **no `error[Edddd]` code** — and
//!   `orch round seed-verified` correctly refused it with
//!   `no coded rustc error[Edddd] diagnostics found`.  Scanning that much text
//!   belongs at test time.
//! * **A bare assertion red.**  `--expected-red` for an assertion form must name
//!   an exact failure count, which couples this seed's red proof to how many of
//!   its own cases happen to fail — and to whatever unrelated flake shares the
//!   run.  Naming the policy as a public constant restores a clean
//!   `error[E0432]` as the first stable red without weakening anything.
//!
//! ## What this contract proves
//!
//! A1  The policy is a named public constant — greppable, referenced once,
//!     impossible to "sort of" have.
//! A2  It names the three things that actually killed a session: paths outside
//!     the repository, `/tmp`, and `mktemp`-style defaults.
//! A3  It states the positive alternative (exact inverse edit) and the
//!     mechanical self-proof (`git diff --exit-code`), and it is mandatory
//!     (`MUST`), not advisory.
//! A4  The **rendered** prompt carries it — not a comment, not a doc string.
//! A5  Reachability (M4): the production chain
//!     `run_wake_with_message_inner_and_append_identity_locked`
//!     -> `prepare_review_probe_message` -> `review_probe_message_body`
//!     is intact, and the body actually interpolates the constant.
//!     r64/H116 had four reviewers return PASS on a policy production never
//!     reached; a policy nobody renders is not a policy.
//!
//! Anchor uniqueness is asserted before any offset is used.  A frozen oracle
//! that used a bare `.find` on a non-unique needle is exactly how B251 became
//! mechanically unsatisfiable (H139); that mistake is not repeated here.
//!
//! Iron rule 10: after relocation this target is byte-frozen.
//!
//! M1: inline the policy text back into the format string and delete the
//!     constant -> A1 red (compile).
//! M2: keep the constant but stop interpolating it in the prompt -> A4/A5 red.
//! M3: weaken the text — drop `/tmp`, `mktemp`, the inverse-edit alternative,
//!     `git diff --exit-code`, or the `MUST` -> A2/A3 red.
//! M4: break the wiring — stop calling `prepare_review_probe_message` from the
//!     wake path, or stop calling `review_probe_message_body` from it -> A5 red.

#![allow(dead_code)]
// 冻结种子内的窗口辅助函数按契约成组保留；未被本轮某条断言用到的
// 不得删除——删掉会让后续 M 变异无处落脚，也会让契约看起来比实际更窄。

// Compile-red until the policy exists as a named public constant.
use orch_host::wake::REVIEW_SCRATCH_POLICY;

const WAKE: &str = include_str!("../src/wake.rs");

/// Column-0 closing brace: with rustfmt this marks the end of a top-level item
/// and nothing else, so it is a sound window bound for a free function.
const ITEM_END: &str = "\n}\n";

const BODY_FN: &str = "pub fn review_probe_message_body(";
const PREP_FN: &str = "fn prepare_review_probe_message(";
const WAKE_FN: &str = "fn run_wake_with_message_inner_and_append_identity_locked<F>(";

/// Resolve a unique anchor to its item window `[start, end)`.
fn window(anchor: &str) -> (usize, usize) {
    assert_eq!(
        WAKE.matches(anchor).count(),
        1,
        "B256: anchor {anchor:?} must occur exactly once in wake.rs"
    );
    let start = WAKE.find(anchor).expect("anchor present");
    let end = WAKE[start..]
        .find(ITEM_END)
        .map(|offset| start + offset)
        .unwrap_or_else(|| panic!("B256: item at {anchor:?} never closes at column 0"));
    (start, end)
}

fn count_in(range: (usize, usize), needle: &str) -> usize {
    WAKE[range.0..range.1].matches(needle).count()
}

/// A2/A3 — the policy says the things that would have saved B253-A0002.
#[test]
fn the_scratch_policy_names_what_actually_kills_a_review_session() {
    for needle in ["outside the repository", "/tmp", "mktemp"] {
        assert!(
            REVIEW_SCRATCH_POLICY.contains(needle),
            "the policy must name {needle:?}: /tmp is the path that killed \
             B253-A0002, and mktemp is the default that silently lands there.\n\
             policy = {REVIEW_SCRATCH_POLICY}"
        );
    }
    for needle in ["exact inverse edit", "git diff --exit-code"] {
        assert!(
            REVIEW_SCRATCH_POLICY.contains(needle),
            "the policy must prescribe {needle:?}; a prohibition without an \
             alternative just relocates the problem.\npolicy = {REVIEW_SCRATCH_POLICY}"
        );
    }
    assert!(
        REVIEW_SCRATCH_POLICY.contains("MUST"),
        "the policy must be mandatory, not advisory — r56 through r69 proves that \
         a reviewer offered a choice eventually chooses /tmp.\n\
         policy = {REVIEW_SCRATCH_POLICY}"
    );
    assert!(
        REVIEW_SCRATCH_POLICY.contains("SCRATCH POLICY"),
        "the policy must carry its own searchable heading; a reviewer skimming a \
         long handoff needs one fixed place to look"
    );
}

/// A4/A5 — it is rendered, and the production path still reaches the renderer.
#[test]
fn the_rendered_prompt_carries_the_policy_and_production_still_reaches_it() {
    let body = orch_host::wake::review_probe_message_body(
        "/repo/.worktrees/review-B900-primary-executor-claw-g01",
        "/repo/orch/target/review-B900-primary-executor-claw-g01",
        "0123456789abcdef0123456789abcdef01234567",
        "/repo/orch/target/review-B900-primary-executor-claw-g01/.orch-review-probe-abc",
        "repo-root-only (safe default)",
        "please review B900",
    );

    // A4 — assert on the value the reviewer actually receives. This is the one
    // check a comment or a doc string cannot satisfy.
    assert!(
        body.contains(REVIEW_SCRATCH_POLICY),
        "the rendered review prompt must carry the policy verbatim; got:\n{body}"
    );
    assert_eq!(
        body.matches("SCRATCH POLICY").count(),
        1,
        "exactly one SCRATCH POLICY clause in the rendered prompt:\n{body}"
    );

    // A5 — the body interpolates the constant rather than duplicating its text,
    // and the two production hops above it are intact.
    let body_window = window(BODY_FN);
    assert!(
        count_in(body_window, "REVIEW_SCRATCH_POLICY") >= 1,
        "review_probe_message_body must interpolate the named policy constant"
    );
    assert_eq!(
        count_in(window(PREP_FN), "review_probe_message_body("),
        1,
        "prepare_review_probe_message must render the body exactly once"
    );
    assert_eq!(
        count_in(window(WAKE_FN), "prepare_review_probe_message("),
        1,
        "the locked wake entry must call prepare_review_probe_message exactly once"
    );
}
