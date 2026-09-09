//! B330 evolves the recorded B285 target: writers are retired; historical
//! digest continuity, signed-plan reads and captured wake pins remain checked.
mod support_legacy_plan;
mod audited_agent_pin_amendment_support;
use std::{fs, path::{Path, PathBuf}};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().parent().unwrap().to_path_buf()
}

#[test]
fn the_readonly_validation_folds_amendments() {
    let source=fs::read_to_string(repo().join("orch/crates/orch-host/src/plan.rs")).unwrap();
    assert!(source.contains("expected_registry_digest_with_amendments"));
    let root=audited_agent_pin_amendment_support::fixture_root("b330-historical-reader");
    let ledger=root.join("coordination/rounds/r9999/events.jsonl");
    let before=fs::read(&ledger).unwrap();let registry=fs::read(root.join("coordination/agents.yaml")).unwrap();
    let events=orch_core::read_ledger(&ledger).unwrap();
    orch_host::plan::require_active_round_ir(&root,"r9999",&events.events).unwrap();
    assert_eq!(fs::read(ledger).unwrap(),before);
    assert_eq!(fs::read(root.join("coordination/agents.yaml")).unwrap(),registry);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn an_amendment_never_signs_the_plan_itself() {
    let source=fs::read_to_string(repo().join("orch/crates/orch-host/src/registry.rs")).unwrap();
    assert!(!source.contains("PlanSignedOff"));
}

#[test]
fn backend_reconciliation_still_reads_the_recorded_wake_pin() {
    let source=fs::read_to_string(repo().join("orch/crates/orch-host/src/wake.rs")).unwrap();
    assert!(source.contains("facts.requested_provider") && source.contains("facts.requested_model"));
    assert!(!source.contains("load_agent_definitions") || source.contains("facts.requested_model"));
}
