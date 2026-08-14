//! B151 seeded-red contract.
//!
//! Negative mutations that must turn the named case red:
//! M1. Silently guess a tool path (bare name / PATH lookup) when neither the
//!     machine config nor the project binding declares one.
//! M2. Let the machine config live untracked-but-committed (missing from
//!     .gitignore) so host paths leak into the repo again.
//! M3. Call CURRENT.md consistent while it does not carry the live round and
//!     main SHA markers.

use orch_host::binding::{current_md_consistent, machine_config_ignored, resolve_tool_path};

#[test]
fn tool_paths_resolve_machine_first_then_project_then_refuse() {
    assert_eq!(
        resolve_tool_path(Some("/opt/custom/cargo"), Some("/usr/bin/cargo"), "cargo").unwrap(),
        "/opt/custom/cargo"
    );
    assert_eq!(
        resolve_tool_path(None, Some("/usr/bin/cargo"), "cargo").unwrap(),
        "/usr/bin/cargo"
    );
    // M1: no declared path anywhere refuses loudly — never a silent guess.
    assert!(resolve_tool_path(None, None, "cargo").is_err());
}

#[test]
fn machine_config_must_be_gitignored() {
    assert!(machine_config_ignored(".orch/machine.yaml\ntarget/\n"));
    // M2: absence from .gitignore is a leak, not a shrug.
    assert!(!machine_config_ignored("target/\n"));
}

#[test]
fn current_md_consistency_requires_round_and_main_sha() {
    let good = "# CURRENT\nround: r51\nmain: abc123def\n";
    assert!(current_md_consistent(good, "r51", "abc123def"));
    // M3: stale round or missing sha is inconsistent.
    assert!(!current_md_consistent(good, "r52", "abc123def"));
    assert!(!current_md_consistent("# CURRENT\nround: r51\n", "r51", "abc123def"));
}
