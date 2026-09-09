//! B323: generic review deadlines remain workload-derived after formal attach retirement.

use orch_host::legacy::review_deadline_secs;
use orch_host::wake::MANAGED_WAKE_MAX_RUNTIME_SECS;

const CLI: &str = include_str!("../../orch-cli/src/main.rs");
const WAKE: &str = include_str!("../src/wake.rs");

#[test]
fn generic_review_deadline_scales_with_evidence_and_caps_safely() {
    assert_eq!(review_deadline_secs("review", 0), 1_800);
    assert_eq!(review_deadline_secs("review", 3), 2_700);
    assert_eq!(
        review_deadline_secs("review", usize::MAX),
        MANAGED_WAKE_MAX_RUNTIME_SECS
    );
}

#[test]
fn historical_primary_formula_remains_read_only_compatibility() {
    assert_eq!(review_deadline_secs("primary", 3), 5_400);
    assert!(CLI.contains("orch_host::wake::review_deadline_secs("));
    assert!(CLI.contains("\"review\","));
    assert!(CLI.contains("task.required_evidence.len()"));
    assert!(!CLI.contains("value_parser = [\"primary\", \"secondary\", \"nongate\"]"));
}

#[test]
fn attach_surface_is_explicitly_retired_before_legacy_continuation() {
    assert!(WAKE.contains("legacy formal-review attach 已退役"));
    assert!(!WAKE.contains("\nfn run_managed_wake_attach_inner"));
    assert!(!WAKE.contains("fn build_opencode_attach_argv"));
}
