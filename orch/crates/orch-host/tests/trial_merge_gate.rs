//! B161 seeded-red contract (catch same-round interaction before the merge).
//!
//! Negative mutations that must turn the named case red:
//! M1. Skip the trial merge when main has moved. That is the r53 hole: two
//!     cards, each green on its own branch, whose combination reds the gate
//!     only after the merge — by which point the barrier can no longer close.
//! M2. Run the trial merge unconditionally. A collect that pays for a second
//!     full gate on every attempt, including when main has not moved, is a
//!     tax nobody agreed to.
//! M3. Report the failure without naming the interaction. "Gate red" sends the
//!     planner hunting; "red because of these commits, this test" is the whole
//!     value of catching it early.

use orch_host::collect::{trial_merge_needed, TrialMergeVerdict};

fn v(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn the_trial_merge_fires_exactly_when_main_moved() {
    // M1 + M2: the trigger is "main advanced past this attempt's base",
    // nothing more and nothing less.
    let base = "a".repeat(40);
    let moved = "b".repeat(40);
    assert!(
        trial_merge_needed(&base, &moved),
        "main moved past the base => the interaction window is open"
    );
    assert!(
        !trial_merge_needed(&base, &base),
        "main unchanged => zero extra cost, no worktree, no second gate"
    );
}

#[test]
fn a_red_trial_merge_names_the_interaction() {
    // M3: the refusal must carry both halves — who we collided with, and what
    // broke. This is the r53 sequence: one card makes a card field mandatory,
    // the other branched before it and its fixture lacks the field.
    let verdict = TrialMergeVerdict::Red {
        gate: "testFast".to_string(),
        exit_code: 101,
        interacting_commits: v(&["956a0fb", "d7161b3"]),
        detail: "task B900: entryPoints 不得为空".to_string(),
    };
    let message = verdict.render();
    assert!(message.contains("testFast"), "the red gate must be named");
    assert!(message.contains("956a0fb"), "the interacting commits must be named");
    assert!(
        message.contains("entryPoints"),
        "the actual failure must survive into the message"
    );

    // A conflict is a distinct, equally-refusing outcome that names the files.
    let conflict = TrialMergeVerdict::Conflict {
        files: v(&["orch/crates/orch-host/src/plan.rs"]),
    };
    assert!(conflict.render().contains("plan.rs"));
    assert!(!conflict.is_ok());
    assert!(!verdict.is_ok());
    assert!(TrialMergeVerdict::Green.is_ok());
}

#[test]
fn the_trial_merge_is_a_scratch_operation() {
    // The trial must be describable without touching main or the task branch:
    // its plan carries a throwaway path and the two shas it combines, and the
    // task branch head is never among the things it may move.
    let base = "a".repeat(40);
    let head = "c".repeat(40);
    let main = "b".repeat(40);
    let plan = orch_host::collect::trial_merge_plan("/repo/root", "B900", &head, &main)
        .expect("plan is representable");
    assert!(
        plan.scratch_worktree.starts_with("/repo/root/"),
        "scratch site stays inside the repo"
    );
    assert_ne!(plan.scratch_worktree, "/repo/root");
    assert_eq!(plan.attempt_head, head);
    assert_eq!(plan.main_head, main);
    assert!(!trial_merge_needed(&base, &base));
}
