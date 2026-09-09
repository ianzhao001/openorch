#[test]
fn mutating_cli_control_plane_is_explicitly_leased() {
    let cli = include_str!("../../orch-cli/src/main.rs");
    for needle in [
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

// Keep the comment-only lease counterexample on the retained manual dispatch path.
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



// Adversarial pin (F3/P1): a marker that survives only inside a `//` comment
// must NOT satisfy the lease check. This reproduces the reviewer's mutation
// (real wrapper replaced by direct `action()`, marker left as a comment) and
// asserts the stripped scan turns it red.
#[test]
fn comment_only_marker_is_not_a_real_lease() {
    let mutated = r#"
fn dispatch(root: &Path, task: &str) -> Result<()> {
    // with_protocol_effect(root, "schema 3 local dispatch", action) -- marker only
    action()
}
"#;
    let stripped = strip_rust_comments(mutated);
    assert!(
        !stripped.contains("with_protocol_effect(root, \"schema 3 local dispatch\""),
        "a comment-only marker must not satisfy the lease scan: {stripped}"
    );
}

#[test]
fn manual_dispatch_is_leased_through_protocol_effect() {
    let tierf = strip_rust_comments(include_str!("../src/tierf.rs"));
    assert!(tierf.contains(r#"with_protocol_effect(root, "schema 3 local dispatch""#));
}
