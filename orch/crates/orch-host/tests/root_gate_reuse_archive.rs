//! Byte-complete read-only anchors for historical collect gate reuse receipts.

#[path = "root_gate_reuse_runtime.rs"]
mod runtime_support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use orch_host::verify::{plan_root_gate_reuse_v1, RootGateReuseDecisionV1};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const VERIFY_SOURCE: &str = include_str!("../src/verify.rs");
const GUIDE: &str = include_str!("../../../docs/AI-MECHANICAL-GUIDE.md");

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AttestedGate {
    sequence: u64,
    gate_event_id: String,
    gate_run_id: String,
    command_ref: String,
    exit_code: i32,
    duration_ms: u64,
    raw_log_cas_sha256: String,
    raw_log_len: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CollectAttestation {
    attestation_version: u64,
    round: String,
    task_id: String,
    action_id: String,
    owner: String,
    lease_generation: String,
    attempt_id: String,
    attempt_no: u64,
    agent: String,
    base_sha: String,
    go_path: String,
    executing_event_id: String,
    branch_sha: String,
    evidence_path: String,
    evidence_sha256: String,
    evidence_len: u64,
    control_epoch: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    subject_tree_sha: String,
    #[serde(skip_serializing_if = "is_zero")]
    ir_revision: u32,
    #[serde(skip_serializing_if = "String::is_empty")]
    validation_digest: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    binding_sha256: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    task_card_sha256: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    resolved_command_digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_reader_descriptor_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_reader_base_sha256: Option<String>,
    toolchain_digest: String,
    environment_digest: String,
    configured_gate_count: u64,
    configured_command_refs: Vec<String>,
    gates: Vec<AttestedGate>,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap()
        .to_path_buf()
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn receipt_text(payload: &serde_json::Value, key: &str) -> String {
    payload[key].as_str().unwrap().to_string()
}

fn receipt_u64(payload: &serde_json::Value, key: &str) -> u64 {
    payload[key].as_u64().unwrap()
}

fn attestation_bytes(
    receipt: &orch_core::EventRecord,
    events: &[orch_core::EventRecord],
) -> Vec<u8> {
    let payload = receipt.payload.as_ref().unwrap();
    let action_id = payload["actionId"].as_str().unwrap();
    let executing_event_id = events
        .iter()
        .find(|event| {
            event.kind == "ReportCollectExecuting"
                && event.task_id == receipt.task_id
                && event.round == receipt.round
                && event
                    .payload
                    .as_ref()
                    .and_then(|value| value.get("actionId"))
                    .and_then(serde_json::Value::as_str)
                    == Some(action_id)
        })
        .unwrap()
        .event_id
        .clone();
    let gates = serde_json::from_value(payload["gates"].clone()).unwrap();
    let configured_command_refs =
        serde_json::from_value(payload["configuredCommandRefs"].clone()).unwrap();
    serde_json::to_vec(&CollectAttestation {
        attestation_version: 2,
        round: receipt.round.clone().unwrap(),
        task_id: receipt.task_id.clone().unwrap(),
        action_id: action_id.to_string(),
        owner: receipt_text(payload, "owner"),
        lease_generation: receipt_text(payload, "leaseGeneration"),
        attempt_id: receipt_text(payload, "attemptId"),
        attempt_no: receipt_u64(payload, "attemptNo"),
        agent: receipt_text(payload, "agent"),
        base_sha: receipt_text(payload, "baseSha"),
        go_path: receipt_text(payload, "goPath"),
        executing_event_id,
        branch_sha: receipt_text(payload, "branchSha"),
        evidence_path: receipt_text(payload, "evidencePath"),
        evidence_sha256: receipt_text(payload, "evidenceSha256"),
        evidence_len: receipt_u64(payload, "evidenceLen"),
        control_epoch: receipt_text(payload, "controlEpoch"),
        subject_tree_sha: receipt_text(payload, "subjectTreeSha"),
        ir_revision: u32::try_from(receipt_u64(payload, "irRevision")).unwrap(),
        validation_digest: receipt_text(payload, "validationDigest"),
        binding_sha256: receipt_text(payload, "bindingSha256"),
        task_card_sha256: receipt_text(payload, "taskCardSha256"),
        resolved_command_digest: receipt_text(payload, "resolvedCommandDigest"),
        source_reader_descriptor_sha256: payload
            .get("sourceReaderDescriptorSha256")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        source_reader_base_sha256: payload
            .get("sourceReaderBaseSha256")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        toolchain_digest: receipt_text(payload, "toolchainDigest"),
        environment_digest: receipt_text(payload, "environmentDigest"),
        configured_gate_count: receipt_u64(payload, "gateCount"),
        configured_command_refs,
        gates,
    })
    .unwrap()
}

#[test]
fn serialized_bundle_keeps_source_event_and_raw_log_identity() {
    let (bundle, _) = runtime_support::fixture();
    let value = serde_json::to_value(&bundle).unwrap();
    let gate = &value["gates"][0];
    assert_eq!(gate["name"], "testFast");
    assert_eq!(gate["sourceEventId"], bundle.gates[0].source_event_id);
    assert_eq!(gate["logSha256"], bundle.gates[0].log_sha256);
    assert_eq!(gate["logBytes"], bundle.gates[0].log_bytes);
}

#[test]
fn duplicate_source_events_are_not_reusable_history() {
    let (mut bundle, subject) = runtime_support::fixture();
    bundle.gates.push(bundle.gates[0].clone());
    assert!(matches!(
        plan_root_gate_reuse_v1(&bundle, &subject),
        RootGateReuseDecisionV1::Execute { .. }
    ));
}

#[test]
fn missing_raw_log_bytes_are_not_reusable_history() {
    let (mut bundle, subject) = runtime_support::fixture();
    bundle.gates[0].log_bytes = 0;
    assert!(matches!(
        plan_root_gate_reuse_v1(&bundle, &subject),
        RootGateReuseDecisionV1::Execute { .. }
    ));
}

#[test]
fn live_replay_rejects_corrupt_raw_log_cas_but_archive_uses_committed_facts() {
    const ROUND: &str = "r81";
    const TASK: &str = "B305";
    const ATTEMPT: &str = "B305-A0001";

    let source = repo_root();
    let source_head = git(&source, &["rev-parse", "HEAD"]);
    let root = source.join("orch/target/test-tmp").join(format!(
        "b323-live-vs-archive-cas-{}-{}",
        std::process::id(),
        ulid::Ulid::new()
    ));
    let _ = fs::remove_dir_all(&root);
    git(
        &source,
        &[
            "clone",
            "-q",
            source.to_str().unwrap(),
            root.to_str().unwrap(),
        ],
    );
    let ledger_path = root.join("coordination/rounds/r81/events.jsonl");
    let events = orch_core::read_ledger(&ledger_path).unwrap().events;
    let root_position = events
        .iter()
        .position(|event| {
            event.kind == "VerdictIssued"
                && event.actor == "verifier:root"
                && event.task_id.as_deref() == Some(TASK)
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("attemptId"))
                    .and_then(serde_json::Value::as_str)
                    == Some(ATTEMPT)
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("verdict"))
                    .and_then(serde_json::Value::as_str)
                    == Some("PASS")
        })
        .unwrap();
    let root_payload = events[root_position].payload.as_ref().unwrap();
    let expected_main = root_payload["mainHeadSha"].as_str().unwrap();
    let task_head = root_payload["headSha"].as_str().unwrap();
    let collect_id = root_payload["collectCompletedEventId"].as_str().unwrap();
    let completed = events
        .iter()
        .find(|event| event.event_id == collect_id)
        .unwrap();
    let receipt_id = completed.payload.as_ref().unwrap()["gateReceipt"]
        .as_str()
        .unwrap();
    let receipt = events
        .iter()
        .find(|event| event.event_id == receipt_id)
        .unwrap();

    git(&root, &["checkout", "-q", "--detach", expected_main]);
    git(&root, &["branch", "-f", "main", expected_main]);
    git(&root, &["checkout", "-q", "main"]);
    git(&root, &["branch", "-f", "task/B305", task_head]);
    fs::create_dir_all(root.join(".worktrees")).unwrap();
    git(
        &root,
        &["worktree", "add", "-q", ".worktrees/B305", "task/B305"],
    );
    fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r81\n").unwrap();
    let nongate_lease = events
        .iter()
        .find(|event| {
            event.kind == "WorkspaceLeased"
                && event.task_id.as_deref() == Some(TASK)
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("attemptId"))
                    .and_then(serde_json::Value::as_str)
                    == Some(ATTEMPT)
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("agent"))
                    .and_then(serde_json::Value::as_str)
                    == Some("executor-dsh")
        })
        .unwrap();
    let nongate_wake = events
        .iter()
        .find(|event| {
            event.kind == "WakeIssued"
                && event.task_id.as_deref() == Some(TASK)
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("attemptId"))
                    .and_then(serde_json::Value::as_str)
                    == Some(ATTEMPT)
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("agent"))
                    .and_then(serde_json::Value::as_str)
                    == Some("executor-dsh")
        })
        .unwrap();
    let nongate_wake_id = nongate_wake.payload.as_ref().unwrap()["wakeId"]
        .as_str()
        .unwrap();
    let nongate_inbox = root.join("coordination/runtime/nongate-inbox/r81");
    fs::create_dir_all(&nongate_inbox).unwrap();
    fs::write(
        nongate_inbox.join("B305-A0001-executor-dsh.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 1,
            "round": ROUND,
            "attemptId": ATTEMPT,
            "agent": "executor-dsh",
            "fixedHead": task_head,
            "wakeId": nongate_wake_id,
            "wakeIssuedEventId": nongate_wake.event_id,
            "workspaceLeasedEventId": nongate_lease.event_id,
            "invocation": {
                "provider": "one-dewu-dsh",
                "model": "deepseek-v4-pro",
                "effort": "max",
                "preset": "minimal",
                "cwd": root.join(".worktrees/review-B305-A0001-nongate-executor-dsh-g01"),
            },
            "state": "empty",
            "terminalReason": "turn/end without substantive final",
        }))
        .unwrap(),
    )
    .unwrap();

    let attestation = attestation_bytes(receipt, &events[..root_position]);
    let expected_attestation_sha = receipt.payload.as_ref().unwrap()["receiptAttestationSha256"]
        .as_str()
        .unwrap();
    assert_eq!(
        hex::encode(Sha256::digest(&attestation)),
        expected_attestation_sha
    );
    assert_eq!(
        attestation.len() as u64,
        receipt.payload.as_ref().unwrap()["receiptAttestationLen"]
            .as_u64()
            .unwrap()
    );
    let store = orch_host::cas::Store::new(&root.join("coordination/runtime/cas"));
    assert_eq!(store.put(&attestation).unwrap(), expected_attestation_sha);
    for gate in root_payload["gates"].as_array().unwrap() {
        let sha = gate["logSha256"].as_str().unwrap();
        let path = store.object_path(sha);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"corrupt raw log\n").unwrap();
    }

    let live_error = orch_host::verify::validate_root_merge_authorization(
        &root,
        ROUND,
        TASK,
        &events[..=root_position],
    )
    .unwrap_err()
    .to_string();
    assert!(live_error.contains("raw-log CAS"), "{live_error}");

    fs::remove_dir_all(root.join("coordination/runtime/cas")).unwrap();
    fs::remove_dir_all(root.join("coordination/runtime/nongate-inbox")).unwrap();
    assert!(!root.join("coordination/runtime/cas").exists());
    assert!(!root.join("coordination/runtime/nongate-inbox").exists());
    git(&root, &["checkout", "-q", "--detach", &source_head]);
    git(&root, &["branch", "-f", "main", &source_head]);
    git(&root, &["checkout", "-q", "main"]);
    orch_host::verify::validate_archived_record_chain(&root, ROUND, TASK, &events).unwrap();
    fs::remove_dir_all(root).unwrap();
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
        let nearby = VERIFY_SOURCE[..position]
            .lines()
            .rev()
            .take(12)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            nearby.contains("///"),
            "missing substantive docs: {declaration}"
        );
    }
    for marker in [
        "root-reuse-v1",
        "reusedEventId",
        "GateReuseMiss",
        "raw-log CAS",
    ] {
        assert!(
            GUIDE.contains(marker),
            "guide missing root reuse marker: {marker}"
        );
    }
    let append = VERIFY_SOURCE
        .find("fn append_root_verdict_checked_batch")
        .unwrap();
    let root = VERIFY_SOURCE.find("fn run_root_verdict_locked").unwrap();
    assert!(append < root);
    assert!(VERIFY_SOURCE[root..].contains("append_root_verdict_checked_batch"));
}
