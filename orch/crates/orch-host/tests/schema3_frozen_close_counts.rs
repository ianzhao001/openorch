//! Actorless close counts do not re-enter the legacy permanent-freeze auditor.

use std::path::Path;

use orch_host::{ledger, verify::validated_frozen_contract_supersession_counts};
use serde_json::json;

fn opened() -> orch_core::EventRecord {
    ledger::event(
        "RoundOpened",
        "runtime:orch",
        None,
        Some("r83"),
        json!({"contractSchemaVersion": 3}),
    )
}

#[test]
fn actorless_counts_need_no_legacy_repository_or_record_batch_replay() {
    // Counting is independent of record-chain admission, which round close
    // performs separately. No legacy Git/card lookup is legal in this branch.
    let root = Path::new("nonexistent-schema3-counting-root");
    assert!(validated_frozen_contract_supersession_counts(root, "r83", &[opened()])
        .unwrap()
        .is_empty());
}

#[test]
fn actorless_counts_reject_retired_frozen_events_instead_of_ignoring_them() {
    let retired = ledger::event(
        "FrozenContractSuperseded",
        "runtime:orch",
        Some("B1"),
        Some("r83"),
        json!({}),
    );
    let error = validated_frozen_contract_supersession_counts(
        Path::new("nonexistent-schema3-counting-root"),
        "r83",
        &[opened(), retired],
    )
    .unwrap_err();
    assert!(error.to_string().contains("schema 3"));
    assert!(error.to_string().contains("FrozenContractSuperseded"));
}

#[test]
fn malformed_or_duplicate_generation_markers_cannot_select_actorless_counts() {
    let root = Path::new("nonexistent-schema3-counting-root");
    assert!(validated_frozen_contract_supersession_counts(root, "r83", &[opened(), opened()]).is_err());
    let mut wrong_round = opened();
    wrong_round.round = Some("r82".into());
    assert!(validated_frozen_contract_supersession_counts(root, "r83", &[wrong_round]).is_err());
    for value in [json!(null), json!("3"), json!(2), json!(4)] {
        let mut malformed = opened();
        malformed.payload.as_mut().unwrap()["contractSchemaVersion"] = value;
        assert!(validated_frozen_contract_supersession_counts(root, "r83", &[malformed]).is_err());
    }
    let mut foreign = opened();
    foreign.actor = "not-runtime".into();
    assert!(validated_frozen_contract_supersession_counts(root, "r83", &[foreign]).is_err());
}
