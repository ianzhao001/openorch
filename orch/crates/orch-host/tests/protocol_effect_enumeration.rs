#[test]
fn mutating_cli_control_plane_is_explicitly_leased() {
    let cli = include_str!("../../orch-cli/src/main.rs");
    for needle in [
        "orch approve",
        "orch inbox add",
        "orch inbox done",
        "orch round close CLI snapshot",
    ] {
        assert!(cli.contains(needle), "missing effect policy for {needle}");
    }
    assert!(cli.contains("active_round_contract(root)?"));
}

#[test]
fn root_manual_legacy_verifier_paths_stay_pre_spawn_rejected() {
    let verify = include_str!("../src/verify.rs");
    let mode_gate = verify.find("root-manual-fixed-head").unwrap();
    let log_create = verify.find("File::create").unwrap();
    let spawn = verify.find(".spawn()").unwrap();
    assert!(mode_gate < log_create && mode_gate < spawn);
}

#[test]
fn merge_disables_hooks_after_final_authorization_recheck() {
    let close = include_str!("../src/close.rs");
    assert!(close.contains("\"--no-verify\""));
    assert!(!close.contains("gitx::merge_no_ff("));
}

// B138 (O2 widening): B134 wired run-task and serve into the protocol lease.
// These meta-tests scan runtask.rs and serve.rs so the leased entrances are
// pinned in source — silent removal of either `with_protocol_effect(...)` call
// is caught here. The test file's own source listing both surfaces is what the
// resume_rejection M2 case asserts (it reads this file and checks the surface
// names appear), so the filenames below double as the contract anchor.
//
// B138-A0002 repair (F3/P1): the prior `src.contains(...)` check could be
// satisfied by a `//` comment that merely quoted the call marker. The scan now
// strips `//` comments (full-line and trailing) before matching, so a mutation
// that replaces the real `with_protocol_effect(...)` wrapper with a direct
// `action()` call and leaves the marker only in a comment turns this red. The
// dedicated `comment_only_marker_is_not_a_real_lease` adversarial test pins
// that property.
fn strip_rust_comments(src: &str) -> String {
    src.lines()
        .map(|line| {
            // Strip a trailing `// ...` comment. A `//` inside a string literal
            // would be mis-split, but the leased callsites here have no such
            // literals on the same line; the matched marker is itself a string
            // literal that never contains `//`.
            match line.find("//") {
                Some(idx) => &line[..idx],
                None => line,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn runtask_dispatch_is_leased_through_protocol_effect() {
    let runtask = strip_rust_comments(include_str!("../src/runtask.rs"));
    assert!(
        runtask.contains("with_protocol_effect(root, \"run-task dispatch\""),
        "run-task dispatch must lease through a real with_protocol_effect call (not a comment)"
    );
}

#[test]
fn serve_planner_turn_is_leased_through_protocol_effect() {
    let serve = strip_rust_comments(include_str!("../src/serve.rs"));
    assert!(
        serve.contains("with_protocol_effect(root, \"serve planner-turn\""),
        "serve planner-turn must lease through a real with_protocol_effect call (not a comment)"
    );
}

// Adversarial pin (F3/P1): a marker that survives only inside a `//` comment
// must NOT satisfy the lease check. This reproduces the reviewer's mutation
// (real wrapper replaced by direct `action()`, marker left as a comment) and
// asserts the stripped scan turns it red.
#[test]
fn comment_only_marker_is_not_a_real_lease() {
    let mutated = r#"
fn run_task(root: &Path, task: &str) -> Result<()> {
    // with_protocol_effect(root, "run-task dispatch", action) -- marker only
    action()
}
"#;
    let stripped = strip_rust_comments(mutated);
    assert!(
        !stripped.contains("with_protocol_effect(root, \"run-task dispatch\""),
        "a comment-only marker must not satisfy the lease scan: {stripped}"
    );
}
