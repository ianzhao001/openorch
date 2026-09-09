//! B137 seeded-red contract (assertion red: these tests fail on the current
//! tree and only pass once the migration lands).
//!
//! Negative mutations that must turn the named case red:
//! M1. Reintroduce std::env::temp_dir() into any migrated integration test.
//! M2. Drop the repo-local test-tmp scratch helper from new_cli_adapters.
//! M3. Reintroduce a bare shared-path remove_dir_all().unwrap() teardown in
//!     new_cli_adapters.

use std::path::Path;

use orch_host::mech::scan_repo_temp_path_hygiene;

fn test_source(name: &str) -> String {
    let path = format!("{}/tests/{}", env!("CARGO_MANIFEST_DIR"), name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

#[test]
fn integration_tests_do_not_use_global_temp_dir() {
    // M1: coverage is derived from the real tree, so newly added Rust files are
    // included without extending a second hard-coded list.
    let crates_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(1)
        .expect("manifest must be below crates/");
    let report = scan_repo_temp_path_hygiene(crates_root).expect("scan real crates tree");
    assert!(report.scanned_files >= 100, "unexpectedly narrow traversal");
    println!("temp-path-hygiene scanned_files={}", report.scanned_files);
    for finding in &report.findings {
        assert!(
            report.scanned_paths.contains(&finding.path),
            "finding must refer to a traversed file: {finding:?}"
        );
        assert!(finding.line > 0 && !finding.evidence.trim().is_empty());
        println!(
            "temp-path-hygiene {}:{}: {}",
            finding.path.display(),
            finding.line,
            finding.evidence
        );
    }
}

#[test]
fn adapter_evidence_lives_under_repo_local_test_tmp() {
    // M2: evidence scratch dirs must live under orch/target/test-tmp with
    // pid+seq+nanos uniqueness, not in the machine-wide temp dir.
    let src = test_source("new_cli_adapters.rs");
    assert!(
        src.contains("test-tmp"),
        "expected repo-local test-tmp scratch root"
    );
    assert!(
        src.contains("process::id()"),
        "expected pid-scoped uniqueness in scratch naming"
    );
}

#[test]
fn adapter_evidence_teardown_tolerates_concurrent_cleanup() {
    // M3: the historical flake was remove_dir_all(root).unwrap() racing a
    // sibling test's cleanup of a colliding shared path.
    let src = test_source("new_cli_adapters.rs");
    assert!(
        !src.contains("remove_dir_all(root).unwrap()"),
        "teardown must not unwrap a shared-path remove_dir_all"
    );
}
