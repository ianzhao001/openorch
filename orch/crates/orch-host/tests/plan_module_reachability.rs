//! B170 seeded-red contract (H32: a new module whose crate-root declaration is
//! frozen can never be implemented inside its own card, and plan time is the
//! only place that can still say so).
//!
//! Negative mutations that must turn the named case red:
//! M1. Accept a writeSet that adds `src/<name>.rs` while `lib.rs` is frozen and
//!     carries no `pub mod <name>;`. That is exactly the r55/B162 shape: Rust
//!     never compiles an unreferenced module, so the seed's required path is
//!     unreachable and the executor can only BLOCK.
//! M2. Flag files that cargo discovers on its own — `tests/*.rs`, `build.rs`,
//!     a binary crate's `src/main.rs`. Seeds land in tests/ on every single
//!     card, so a false positive here blocks the whole protocol.
//! M3. Judge reachability from the working tree instead of the HEAD tree, or
//!     let the refusal stay vague. Leftover untracked files would mask the
//!     defect, and a refusal that does not name the module and the missing
//!     declaration line leaves the planner guessing.

use orch_host::plan::{module_reachability_error, ModuleReachabilityInput};

fn input(
    write_set: &[&str],
    head_files: &[&str],
    lib_declarations: &[(&str, &[&str])],
) -> ModuleReachabilityInput {
    ModuleReachabilityInput {
        task_id: "B900".to_string(),
        write_set: write_set.iter().map(|p| p.to_string()).collect(),
        head_files: head_files.iter().map(|p| p.to_string()).collect(),
        lib_declarations: lib_declarations
            .iter()
            .map(|(lib, mods)| {
                (
                    lib.to_string(),
                    mods.iter().map(|m| m.to_string()).collect::<Vec<String>>(),
                )
            })
            .collect(),
    }
}

const LIB: &str = "orch/crates/orch-host/src/lib.rs";

#[test]
fn an_unreachable_new_module_is_refused_and_named() {
    // M1 + M3b: replay the exact r55/B162 shape — hooks.rs in writeSet, lib.rs
    // frozen and silent about it.
    let refusal = module_reachability_error(&input(
        &[
            "orch/crates/orch-host/src/hooks.rs",
            "orch/crates/orch-host/src/round.rs",
            "orch/crates/orch-host/tests/main_guard.rs",
        ],
        &[LIB, "orch/crates/orch-host/src/round.rs"],
        &[(LIB, &["round", "wake", "plan"])],
    ))
    .expect("an unreachable new module must be refused at plan time");

    assert!(refusal.contains("B900"), "names the card: {refusal}");
    assert!(
        refusal.contains("orch/crates/orch-host/src/hooks.rs"),
        "names the module path: {refusal}"
    );
    assert!(
        refusal.contains("pub mod hooks;"),
        "names the exact missing declaration line: {refusal}"
    );
    assert!(
        refusal.contains(LIB),
        "names the crate root that must declare it: {refusal}"
    );
}

#[test]
fn a_declared_module_or_a_writable_lib_is_accepted() {
    // The two legitimate escapes, both must pass.
    // ① planner pre-placed the declaration.
    assert!(
        module_reachability_error(&input(
            &["orch/crates/orch-host/src/hooks.rs"],
            &[LIB],
            &[(LIB, &["hooks", "round"])],
        ))
        .is_none(),
        "a pre-placed `pub mod hooks;` makes the module reachable"
    );

    // ② the card may declare it itself, because lib.rs is in its own writeSet.
    assert!(
        module_reachability_error(&input(
            &["orch/crates/orch-host/src/hooks.rs", LIB],
            &[LIB],
            &[(LIB, &["round"])],
        ))
        .is_none(),
        "a card that owns lib.rs can declare the module itself"
    );

    // An already-existing module needs no declaration check at all.
    assert!(
        module_reachability_error(&input(
            &["orch/crates/orch-host/src/round.rs"],
            &[LIB, "orch/crates/orch-host/src/round.rs"],
            &[(LIB, &["round"])],
        ))
        .is_none(),
        "editing an existing module is not a reachability question"
    );
}

#[test]
fn cargo_discovered_files_are_never_flagged() {
    // M2: seeds land in tests/ on every card; build.rs arrived this very round
    // (B169); main.rs is a crate root itself. Any of these false-flagged would
    // block the protocol outright.
    let auto_discovered = input(
        &[
            "orch/crates/orch-host/tests/plan_module_reachability.rs",
            "orch/crates/orch-cli/build.rs",
            "orch/crates/orch-cli/src/main.rs",
        ],
        &[LIB],
        &[(LIB, &["round"])],
    );
    assert!(
        module_reachability_error(&auto_discovered).is_none(),
        "tests/, build.rs and src/main.rs are discovered by cargo and need no declaration"
    );

    // M3a: reachability is judged against the HEAD tree. A module that is absent
    // from HEAD is still unreachable even if an untracked leftover exists on
    // disk, so `head_files` is the only input that may decide this.
    let untracked_leftover = input(
        &["orch/crates/orch-host/src/fusion.rs"],
        &[LIB],
        &[(LIB, &["round"])],
    );
    assert!(
        module_reachability_error(&untracked_leftover).is_some(),
        "absence from the HEAD tree decides reachability, not the working tree"
    );
}
