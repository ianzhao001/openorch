//! B304 replay regression: existing verdict, seal/record, and archive must follow `GateReused`.

#[path = "root_gate_reuse_runtime.rs"]
mod runtime_support;

use std::fs;

use orch_host::{close, tierf, verify};
use runtime_support::{Fixture, ATTEMPT, ROUND, TASK};

const VERIFY_SOURCE: &str = include_str!("../src/verify.rs");
const GUIDE: &str = include_str!("../../../docs/AI-MECHANICAL-GUIDE.md");

fn recorded_fixture(tag: &str) -> Fixture {
    let fixture = Fixture::ready(tag);
    let first = fixture
        .verdict(verify::RootVerdict::Pass, None, false)
        .unwrap();
    assert!(first.appended);
    let replay = fixture
        .verdict(verify::RootVerdict::Pass, None, false)
        .unwrap();
    assert!(!replay.appended, "existing verdict replay appended a second batch");
    close::run_seal(&fixture.root, TASK, ATTEMPT, &fixture.candidate_sha).unwrap();
    fixture
}

fn task_event_mut<'a>(
    events: &'a mut [orch_core::EventRecord],
    kind: &str,
) -> &'a mut orch_core::EventRecord {
    events
        .iter_mut()
        .find(|event| event.kind == kind && event.task_id.as_deref() == Some(TASK))
        .unwrap()
}

#[test]
fn recorded_archive_rereads_reused_source_receipt_and_raw_log_cas() {
    let fixture = recorded_fixture("archive-replay");
    let events = fixture.events();
    verify::validate_archived_record_chain(&fixture.root, ROUND, TASK, &events).unwrap();
    assert!(events.iter().any(|event| {
        event.kind == "TaskRecorded" && event.task_id.as_deref() == Some(TASK)
    }));
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event.kind == "GateReused" && event.task_id.as_deref() == Some(TASK)
            })
            .count(),
        3
    );

    let root = events
        .iter()
        .find(|event| {
            event.kind == "VerdictIssued"
                && event.actor == "verifier:root"
                && event.task_id.as_deref() == Some(TASK)
        })
        .unwrap();
    let first_gate = &root.payload.as_ref().unwrap()["gates"][0];
    let log_sha256 = first_gate["logSha256"].as_str().unwrap();
    let store = orch_host::cas::Store::new(&fixture.root.join("coordination/runtime/cas"));
    let object = store.object_path(log_sha256);
    let original = fs::read(&object).unwrap();
    fs::write(&object, b"archive-cas-corruption").unwrap();
    let error = verify::validate_archived_record_chain(&fixture.root, ROUND, TASK, &events)
        .unwrap_err()
        .to_string();
    assert!(error.contains("CAS"), "{error}");
    fs::write(&object, original).unwrap();
    verify::validate_archived_record_chain(&fixture.root, ROUND, TASK, &events).unwrap();
}

#[test]
fn archive_rejects_wrong_actor_source_identity_and_execution_reference_shape() {
    let fixture = recorded_fixture("archive-negative");
    let events = fixture.events();

    let mut wrong_actor = events.clone();
    task_event_mut(&mut wrong_actor, "GateReused").actor = "runtime:other".to_string();
    assert!(
        verify::validate_archived_record_chain(&fixture.root, ROUND, TASK, &wrong_actor).is_err()
    );

    let mut wrong_source = events.clone();
    task_event_mut(&mut wrong_source, "GateReused")
        .payload
        .as_mut()
        .unwrap()["sourceGateEventId"] = serde_json::json!(ulid::Ulid::new().to_string());
    assert!(
        verify::validate_archived_record_chain(&fixture.root, ROUND, TASK, &wrong_source).is_err()
    );

    let mut wrong_input = events.clone();
    task_event_mut(&mut wrong_input, "GateReused")
        .payload
        .as_mut()
        .unwrap()["inputIdentitySha256"] = serde_json::json!("f".repeat(64));
    assert!(
        verify::validate_archived_record_chain(&fixture.root, ROUND, TASK, &wrong_input).is_err()
    );

    let mut both_refs = events.clone();
    let verdict = task_event_mut(&mut both_refs, "VerdictIssued");
    verdict.payload.as_mut().unwrap()["gates"][0]["gateRunId"] =
        serde_json::json!(ulid::Ulid::new().to_string());
    assert!(
        verify::validate_archived_record_chain(&fixture.root, ROUND, TASK, &both_refs).is_err()
    );

    let mut missing_reuse = events.clone();
    let remove = missing_reuse
        .iter()
        .position(|event| {
            event.kind == "GateReused" && event.task_id.as_deref() == Some(TASK)
        })
        .unwrap();
    missing_reuse.remove(remove);
    assert!(
        verify::validate_archived_record_chain(&fixture.root, ROUND, TASK, &missing_reuse).is_err()
    );

    // A modern root-reuse verdict must not be downgradeable to the historical
    // both-reference-absent shape.  Rewrite the names as well so this proof is
    // carried by the execution-reference contract, not by the incidental
    // candidate-vs-merge lane name difference in this fixture.
    let mut downgraded = events.clone();
    downgraded.retain(|event| {
        !(event.kind == "GateReused" && event.task_id.as_deref() == Some(TASK))
    });
    let verdict = task_event_mut(&mut downgraded, "VerdictIssued");
    for (gate, name) in verdict.payload.as_mut().unwrap()["gates"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .zip(["testFast", "testExclusive", "check"])
    {
        let gate = gate.as_object_mut().unwrap();
        gate.insert("name".to_string(), serde_json::json!(name));
        gate.remove("gateRunId");
        gate.remove("reusedEventId");
    }
    assert!(
        verify::validate_archived_record_chain(&fixture.root, ROUND, TASK, &downgraded).is_err(),
        "an active root-reuse verdict accepted a forged legacy execution-reference shape"
    );
}

#[test]
fn modern_spawned_root_archive_requires_gate_run_ids() {
    let fixture = Fixture::ready("archive-spawned-reference");
    let bundle = tierf::load_validated_collect_gate_bundle_v1(
        &fixture.root,
        ROUND,
        TASK,
        ATTEMPT,
    )
    .unwrap()
    .unwrap();
    let store = orch_host::cas::Store::new(&fixture.root.join("coordination/runtime/cas"));
    fs::write(store.object_path(&bundle.gates[0].log_sha256), b"corrupt").unwrap();
    fixture
        .verdict(verify::RootVerdict::Pass, None, false)
        .unwrap();
    close::run_seal(&fixture.root, TASK, ATTEMPT, &fixture.candidate_sha).unwrap();
    let events = fixture.events();
    assert!(events.iter().any(|event| {
        event.kind == "GateExecuted"
            && event.task_id.as_deref() == Some(TASK)
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("phase"))
                .and_then(serde_json::Value::as_str)
                == Some("root")
    }));

    let mut downgraded = events.clone();
    let verdict = task_event_mut(&mut downgraded, "VerdictIssued");
    for gate in verdict.payload.as_mut().unwrap()["gates"]
        .as_array_mut()
        .unwrap()
    {
        gate.as_object_mut().unwrap().remove("gateRunId");
    }
    assert!(
        verify::validate_archived_record_chain(&fixture.root, ROUND, TASK, &downgraded).is_err(),
        "a modern spawned-root verdict accepted missing gateRunId references"
    );
}

#[test]
fn public_docs_and_guide_keep_the_root_reuse_contract_visible() {
    for declaration in [
        "pub const ROOT_GATE_REUSE_CONTRACT_V1",
        "pub struct ReusedGateV1",
        "pub struct CollectGateBundleV1",
        "pub struct RootGateSubjectV1",
        "pub enum RootGateReuseDecisionV1",
        "pub fn plan_root_gate_reuse_v1",
    ] {
        let position = VERIFY_SOURCE.find(declaration).unwrap();
        let prefix = &VERIFY_SOURCE[..position];
        let nearby = prefix
            .lines()
            .rev()
            .take(12)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(nearby.contains("///"), "missing substantive docs: {declaration}");
    }
    for marker in [
        "root-reuse-v1",
        "reusedEventId",
        "GateReuseMiss",
        "raw-log CAS",
    ] {
        assert!(GUIDE.contains(marker), "guide missing root reuse marker: {marker}");
    }
    let append = VERIFY_SOURCE
        .find("fn append_root_verdict_checked_batch")
        .unwrap();
    let root = VERIFY_SOURCE.find("fn run_root_verdict_locked").unwrap();
    assert!(append < root);
    assert!(VERIFY_SOURCE[root..].contains("append_root_verdict_checked_batch"));
}
