//! B305 recovery regressions for red closure, durable miss budgeting, and an
//! interrupted `MergeExecuted -> TaskRecorded` final-tree adoption.

#![allow(dead_code)]

#[path = "final_tree_gate_runtime.rs"]
mod runtime;

use std::fs;

use orch_host::{close, ledger, verify};

const CLOSE_SOURCE: &str = include_str!("../src/close.rs");
const COLLECT_SOURCE: &str = include_str!("../src/collect.rs");
const GUIDE: &str = include_str!("../../../docs/AI-MECHANICAL-GUIDE.md");

fn function_body<'a>(source: &'a str, signature: &str) -> &'a str {
    let start = source.find(signature).unwrap();
    let rest = &source[start..];
    let end = rest[1..]
        .find("\nfn ")
        .or_else(|| rest[1..].find("\npub fn "))
        .map(|offset| offset + 1)
        .unwrap_or(rest.len());
    &rest[..end]
}

fn offset(source: &str, needle: &str) -> usize {
    source
        .find(needle)
        .unwrap_or_else(|| panic!("missing source anchor {needle}"))
}

fn phase(event: &orch_core::EventRecord) -> Option<&str> {
    event.payload.as_ref()?.get("phase")?.as_str()
}

fn root_pass(fixture: &runtime::FinalTreeFixture) -> String {
    verify::run_root_verdict(
        fixture.root(),
        runtime::FINAL_TASK,
        runtime::FINAL_ATTEMPT,
        &fixture.candidate_sha,
        &fixture.expected_main,
        verify::RootVerdict::Pass,
        None,
        false,
    )
    .unwrap();
    fixture
        .events()
        .into_iter()
        .find(|event| {
            event.kind == "VerdictIssued"
                && event.actor == "verifier:root"
                && event.task_id.as_deref() == Some(runtime::FINAL_TASK)
        })
        .unwrap()
        .event_id
}

#[test]
fn trial_red_closes_the_pending_pass_before_merge_started() {
    let fixture = runtime::FinalTreeFixture::ready("trial-red");
    let verdict_id = root_pass(&fixture);
    fs::write(
        fixture
            .root()
            .join("coordination/runtime/test-observed/final-red"),
        b"red\n",
    )
    .unwrap();
    let error = fixture.seal().unwrap_err().to_string();
    assert!(error.contains("approved-reattempt"), "{error}");
    let events = fixture.events();
    assert!(!events.iter().any(|event| {
        event.kind == "MergeStarted" && event.task_id.as_deref() == Some(runtime::FINAL_TASK)
    }));
    let terminal = events
        .iter()
        .find(|event| {
            event.kind == "AttemptBlocked" && event.task_id.as_deref() == Some(runtime::FINAL_TASK)
        })
        .unwrap();
    let payload = terminal.payload.as_ref().unwrap();
    assert_eq!(payload["stage"], "approved-reattempt");
    assert_eq!(payload["verdictEventId"], verdict_id);
    assert!(events.iter().any(|event| {
        event.kind == "GateExecuted"
            && event.task_id.as_deref() == Some(runtime::FINAL_TASK)
            && phase(event) == Some("trial")
            && event.payload.as_ref().unwrap()["exitCode"] == 23
    }));
}

#[test]
fn two_attempt_scoped_misses_close_the_exact_pending_pass() {
    let fixture = runtime::FinalTreeFixture::ready("two-misses");
    let verdict_id = root_pass(&fixture);
    let events = fixture.events();
    let policy_base_sha = events
        .iter()
        .find(|event| {
            event.kind == "DispatchIssued" && event.task_id.as_deref() == Some(runtime::FINAL_TASK)
        })
        .and_then(|event| event.payload.as_ref())
        .and_then(|payload| payload.get("baseSha"))
        .and_then(serde_json::Value::as_str)
        .unwrap()
        .to_string();
    let misses = [1_u32, 2]
        .into_iter()
        .map(|miss_no| {
            let actual = format!("{miss_no:064x}");
            ledger::runtime_event_v1(
                runtime::FINAL_ROUND,
                Some(runtime::FINAL_TASK),
                ledger::RuntimeEventPayloadV1::GateReuseMiss(ledger::GateReuseMissPayloadV1 {
                    schema_version: 1,
                    attempt_id: runtime::FINAL_ATTEMPT.to_string(),
                    attempt_no: 1,
                    phase: "trial".to_string(),
                    command_ref: "testFast".to_string(),
                    miss_no,
                    reason: if miss_no == 1 {
                        "input-tree".to_string()
                    } else {
                        "command".to_string()
                    },
                    input_identity_sha256: actual.clone(),
                    expected_sha256: format!("{:064x}", miss_no + 10),
                    actual_sha256: actual,
                    policy_base_sha: policy_base_sha.clone(),
                }),
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    ledger::append(fixture.root(), runtime::FINAL_ROUND, &misses).unwrap();
    let error = fixture.seal().unwrap_err().to_string();
    assert!(error.contains("miss budget"), "{error}");
    let events = fixture.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event.kind == "GateReuseMiss"
                    && event.task_id.as_deref() == Some(runtime::FINAL_TASK)
            })
            .count(),
        2
    );
    assert!(!events.iter().any(|event| {
        event.kind == "MergeStarted" && event.task_id.as_deref() == Some(runtime::FINAL_TASK)
    }));
    let terminal = events
        .iter()
        .find(|event| {
            event.kind == "AttemptBlocked" && event.task_id.as_deref() == Some(runtime::FINAL_TASK)
        })
        .unwrap();
    assert_eq!(
        terminal.payload.as_ref().unwrap()["verdictEventId"],
        verdict_id
    );
    let before_replay = events.len();
    assert!(fixture.seal().is_err());
    let replayed = fixture.events();
    assert_eq!(replayed.len(), before_replay);
    assert_eq!(
        replayed
            .iter()
            .filter(|event| {
                event.kind == "GateReuseMiss"
                    && event.task_id.as_deref() == Some(runtime::FINAL_TASK)
            })
            .count(),
        2
    );
}

fn serialize_events(events: &[orch_core::EventRecord]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for event in events {
        serde_json::to_writer(&mut bytes, event).unwrap();
        bytes.push(b'\n');
    }
    bytes
}

#[test]
fn merge_executed_recovery_reuses_trial_proof_without_a_recovery_full() {
    let fixture = runtime::FinalTreeFixture::ready("merge-executed-recovery");
    fixture.seal().unwrap();
    let complete = fixture.events();
    let merge_position = complete
        .iter()
        .position(|event| {
            event.kind == "MergeExecuted" && event.task_id.as_deref() == Some(runtime::FINAL_TASK)
        })
        .unwrap();
    let interrupted = complete[..=merge_position].to_vec();
    let bytes = serialize_events(&interrupted);
    fs::write(
        fixture.root().join("coordination/rounds/r95/events.jsonl"),
        &bytes,
    )
    .unwrap();
    fs::write(
        fixture
            .root()
            .join("coordination/runtime/ledger-wal/r95.jsonl"),
        &bytes,
    )
    .unwrap();
    close::run_record(fixture.root(), runtime::FINAL_TASK).unwrap();
    let recovered = fixture.events();
    assert!(!recovered.iter().any(|event| {
        event.kind == "GateExecuted"
            && event.task_id.as_deref() == Some(runtime::FINAL_TASK)
            && phase(event) == Some("recovery")
    }));
    assert_eq!(
        recovered
            .iter()
            .filter(|event| {
                matches!(
                    ledger::decode_runtime_event_v1(event),
                    Ok(Some(ledger::RuntimeEventPayloadV1::GateReused(ref payload)))
                        if event.task_id.as_deref() == Some(runtime::FINAL_TASK)
                            && payload.target_phase == "postmerge"
                )
            })
            .count(),
        3
    );
    assert_eq!(
        recovered
            .iter()
            .filter(|event| {
                event.kind == "TaskRecorded"
                    && event.task_id.as_deref() == Some(runtime::FINAL_TASK)
            })
            .count(),
        1
    );
}

#[test]
fn malformed_equal_tree_ids_are_never_reused() {
    let tested = close::TestedMergeTreeV1 {
        tree_sha: "abc".to_string(),
        main_sha: "1".repeat(40),
        candidate_sha: "2".repeat(40),
        input_identity_sha256: "3".repeat(64),
    };
    assert!(matches!(
        close::compare_actual_merge_tree_v1(&tested, "abc"),
        close::FinalTreeDecisionV1::RunPostMerge { .. }
    ));
}

#[test]
fn equal_prefix_full_tree_ids_are_never_reused() {
    let mut actual = "4e2490524f0b678eb5f4a24d31fce306c797596b".to_string();
    actual.replace_range(39..40, "c");
    let tested = close::TestedMergeTreeV1 {
        tree_sha: "4e2490524f0b678eb5f4a24d31fce306c797596b".to_string(),
        main_sha: "1".repeat(40),
        candidate_sha: "2".repeat(40),
        input_identity_sha256: "3".repeat(64),
    };
    assert!(matches!(
        close::compare_actual_merge_tree_v1(&tested, &actual),
        close::FinalTreeDecisionV1::RunPostMerge { .. }
    ));
}

#[test]
fn real_input_drift_cannot_be_masked_by_a_simultaneous_ref_retry() {
    assert_eq!(
        close::decide_gate_reuse_miss_v1(0, true, true),
        close::GateReuseMissDecisionV1::RetryInput { miss_count: 1 }
    );
}

#[test]
fn production_wiring_keeps_full_and_comparison_on_the_safe_sides_of_merge() {
    let merge = function_body(CLOSE_SOURCE, "fn run_merge_locked(");
    assert!(
        offset(merge, "prepare_final_tree_proof_v1(")
            < offset(merge, "let started_payload = serde_json::json!(")
    );
    assert!(
        offset(merge, "format!(\"{merge_sha}^{{tree}}\")")
            < offset(merge, "compare_actual_merge_tree_v1(")
    );
    assert!(merge.contains(
        "compare_actual_merge_tree_v1(&attestation.tested, &actual_tree_sha)"
    ));
    let reuse_builder = function_body(CLOSE_SOURCE, "fn final_tree_reused_events_v1(");
    assert!(reuse_builder.contains("RuntimeEventPayloadV1::GateReused("));
    let batch = function_body(CLOSE_SOURCE, "fn append_final_tree_reused_record_v1(");
    assert_eq!(batch.matches("append_checked_merge_lifecycle(").count(), 1);
    assert!(batch.contains("batch.extend(final_tree_reused_events_v1("));
    let prepare = function_body(CLOSE_SOURCE, "fn prepare_final_tree_proof_v1(");
    assert!(prepare.contains("let durable_miss_count = events"));
    assert!(prepare.contains("if durable_miss_count >= 2"));
    let attestation = function_body(CLOSE_SOURCE, "fn validate_final_tree_attestation_v1(");
    assert!(attestation.contains("bail!(\"final-tree source raw-log CAS 对象损坏\")"));
    let collect = function_body(COLLECT_SOURCE, "pub fn check_and_gate(");
    assert!(collect.contains("let trial_refs = if final_tree_active"));
    assert!(collect.contains("&[][..]"));
}

#[test]
fn public_contract_and_guide_keep_the_b305_explanations() {
    for marker in [
        "/// Version of the seal-time final-tree proof contract.",
        "/// Immutable proof subject captured after the seal lifecycle",
        "/// Decision made after the real no-ff merge exposes its actual tree.",
        "/// Attempt-scoped response to a seal input recheck.",
        "/// Compare complete Git object identities",
        "/// Apply the V1 miss budget",
    ] {
        assert!(CLOSE_SOURCE.contains(marker), "missing rustdoc marker {marker}");
    }
    for marker in [
        "final-tree-v1",
        "GateReused(trial→postmerge)",
        "approved-reattempt",
        "carryForward",
        "70% SLO",
    ] {
        assert!(GUIDE.contains(marker), "missing guide marker {marker}");
    }
}
