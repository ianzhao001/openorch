use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use orch_core::EventRecord;
use orch_host::card::{
    self, Card, CardMeta, FrozenAdjudicationEvidence, FrozenBlockedAttempt,
    FrozenContractDisposition, FrozenContractSupersession, FrozenEffectiveAnchor,
    FrozenPlannerAdjudicatedAuthorization, FrozenRecordedReplacement, FrozenRemovedAssertion,
    FrozenSeedRelocationAnchor, FrozenSubjectPrefix, FrozenUserAuthorization, RequiredReview,
};
use orch_host::ledger;
use orch_host::oracle::{
    validate_frozen_contract_declaration, validate_planner_adjudicated_supersession,
    validate_seed_paths_for_candidate,
};
use orch_host::plan;
use orch_host::verify::{
    validate_frozen_contract_supersession_delta_for_replay, FrozenContractSupersededPayload,
    ReviewBinding, RootRecordAuthorization, RootVerdictPayload,
};
use sha2::{Digest, Sha256};

const ROUND: &str = "r75";
const TASK: &str = "B900";
const ATTEMPT: &str = "B900-A0001";
const TARGET: &str = "contracts/frozen_contract.rs";
const SUBJECT: &str = "src/frozen_subject.txt";
const CARD_PATH: &str = "coordination/rounds/r75/tasks/B900.md";
const IR_PATH: &str = "coordination/rounds/r75/ROUND-IR.yaml";
const ADJUDICATION_PATH: &str = "coordination/rounds/r75/planning/r75-narrow-b253.md";
const RETAINED_MARKER: &str =
    "run_fusion retained window: provision_consult_sites before thread scope";
const ORIGINAL_EVENT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const BLOCKED_EVENT: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";
const RECORDED_EVENT: &str = "01CX5ZZKBKACTAV9WEVGEMMVRZ";
const VALIDATION_DIGEST: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
const OLD_SUBJECT: &[u8] = b"old-subject-prefix\n";
const NEW_SUBJECT: &[u8] = b"new-subject-prefix\n";
const ADJUDICATION: &[u8] =
    b"# r75 adjudication\n\nThe wide digest is removed; the retained window remains.\n";

#[derive(Clone, Copy)]
enum AuthorizationVariant {
    Recovery,
    PlannerAdjudicated,
}

struct GitFixture {
    root: PathBuf,
    prior_oid: String,
    candidate_oid: String,
    merge_oid: String,
    declaration: FrozenContractSupersession,
    card_sha256: String,
    card_text: String,
}

impl GitFixture {
    fn new(tag: &str, variant: AuthorizationVariant) -> Self {
        let root = orch_host::util::test_scratch_dir(&format!("b288-{tag}"));
        git(&root, &["init", "-q", "-b", "main"]);

        let old_literal = sha256(OLD_SUBJECT);
        let new_literal = sha256(NEW_SUBJECT);
        let old_contract = contract(&old_literal);
        let new_contract = contract(&new_literal);
        let declaration = declaration(
            variant,
            &old_contract,
            &new_contract,
            &old_literal,
            &new_literal,
        );
        let card_text = card_text(&declaration);
        let card_sha256 = sha256(card_text.as_bytes());
        let manifest = serde_json::json!({
            "schemaVersion": 1,
            "baselineTreeSha": "a".repeat(40),
            "scope": {
                "declaredPairAuditThrough": "r70",
                "effectiveBaselineThrough": "r71/B269",
                "selection": "targets with a durable SeedRelocated fact; later SeedRelocated/FrozenContractSuperseded facts are deltas",
            },
            "counts": {
                "declaredPairsThroughR70": 1,
                "driftedDeclaredPairsThroughR70": 0,
                "missingDeclaredPairsThroughR70": 0,
                "uniqueEffectiveTargetsThroughB269": 1,
                "presentEffectiveTargets": 1,
                "effectiveTombstones": 0,
                "excludedUnrecordedMissingTargets": 0,
            },
            "excludedUnrecordedMissingTargets": [],
            "targets": [{
                "target": TARGET,
                "state": "present",
                "effectiveSha256": declaration.old_file_sha256,
                "effectiveAnchor": {
                    "kind": "seed-relocated",
                    "eventId": ORIGINAL_EVENT,
                    "sha256": declaration.old_file_sha256,
                },
                "grandfatheredDrift": false,
                "sources": [{
                    "round": "r70",
                    "taskId": "B001",
                    "cardPath": "coordination/rounds/r70/tasks/B001.md",
                    "seedSrc": "coordination/rounds/r70/seeds/B001/frozen_contract.rs",
                    "declaredSha256": declaration.old_file_sha256,
                    "sourceSha256": declaration.old_file_sha256,
                    "sourceMatchesDeclared": true,
                    "driftedFromSource": false,
                    "seedRelocated": [{
                        "eventId": ORIGINAL_EVENT,
                        "sha256": declaration.old_file_sha256,
                    }],
                    "taskRecordedEventIds": [],
                }],
            }],
        });
        let manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        let manifest_sha256 = sha256(&manifest_bytes);
        let binding = format!(
            "oracle:\n  landedSeedBaseline:\n    schemaVersion: 1\n    path: coordination/frozen-contract-baseline-v1.json\n    sha256: {manifest_sha256}\n"
        );
        let binding_sha256 = sha256(binding.as_bytes());
        let ir = format!(
            "schemaVersion: 2\nround: {ROUND}\nrevision: 1\nsourceBindings:\n  bindingSha256: {binding_sha256}\n  taskCards:\n    {CARD_PATH}: {card_sha256}\n"
        );
        let parsed_ir = plan::parse_signed_round_ir(&ir).unwrap();
        let validation_digest = plan::validation_digest(&parsed_ir);

        let seed_oracle = ledger::event(
            "SeedOracleVerified",
            "planner",
            Some("B001"),
            Some("r70"),
            serde_json::json!({
                "seeds": [{
                    "target": TARGET,
                    "sha256": declaration.old_file_sha256,
                }],
            }),
        );
        let mut relocated = ledger::event(
            "SeedRelocated",
            "runtime:orch",
            Some("B001"),
            Some("r70"),
            serde_json::json!({
                "target": TARGET,
                "sha256": declaration.old_file_sha256,
                "cmp": "identical",
            }),
        );
        relocated.event_id = ORIGINAL_EVENT.to_string();
        let task_validated = ledger::event(
            "TaskValidated",
            "runtime:orch",
            None,
            Some(ROUND),
            plan::task_validated_payload(1, &validation_digest),
        );
        let plan_signed_off = ledger::event(
            "PlanSignedOff",
            "user",
            None,
            Some(ROUND),
            plan::plan_signed_off_payload(
                "B288 local signed baseline fixture",
                1,
                &validation_digest,
            )
            .unwrap(),
        );

        write(&root, TARGET, old_contract.as_bytes());
        write(&root, SUBJECT, OLD_SUBJECT);
        write(&root, CARD_PATH, card_text.as_bytes());
        write(&root, IR_PATH, ir.as_bytes());
        write(
            &root,
            "coordination/frozen-contract-baseline-v1.json",
            &manifest_bytes,
        );
        write(
            &root,
            "coordination/PROJECT-BINDING.yaml",
            binding.as_bytes(),
        );
        write_events(
            &root,
            "coordination/rounds/r70/events.jsonl",
            &[seed_oracle, relocated],
        );
        write_events(
            &root,
            "coordination/rounds/r75/events.jsonl",
            &[task_validated, plan_signed_off],
        );
        write(&root, "base.txt", b"base\n");
        commit_all(&root, "signed prior main");
        let prior_oid = git(&root, &["rev-parse", "HEAD"]);

        git(&root, &["checkout", "-q", "-b", "candidate"]);
        write(&root, TARGET, new_contract.as_bytes());
        write(&root, SUBJECT, NEW_SUBJECT);
        if matches!(variant, AuthorizationVariant::PlannerAdjudicated) {
            write(&root, ADJUDICATION_PATH, ADJUDICATION);
        }
        commit_all(&root, "candidate frozen-contract supersession");
        let candidate_oid = git(&root, &["rev-parse", "HEAD"]);

        git(&root, &["checkout", "-q", "main"]);
        git(
            &root,
            &[
                "-c",
                "user.name=B288",
                "-c",
                "user.email=b288@example.invalid",
                "merge",
                "-q",
                "--no-ff",
                "-m",
                "merge candidate",
                "candidate",
            ],
        );
        let merge_oid = git(&root, &["rev-parse", "HEAD"]);

        Self {
            root,
            prior_oid,
            candidate_oid,
            merge_oid,
            declaration,
            card_sha256,
            card_text,
        }
    }

    fn review_bindings(&self) -> Vec<ReviewBinding> {
        vec![
            ReviewBinding {
                path: "coordination/rounds/r75/reviews/B900-A0001-primary-executor-one.md"
                    .to_string(),
                role: "primary".to_string(),
                reviewer: "executor-one".to_string(),
                verdict: "PASS".to_string(),
                sha256: "1".repeat(64),
                bytes: 10,
                delivery_event_id: None,
                substituted_role: None,
                substituted_agent: None,
                source_terminal_event_id: None,
            },
            ReviewBinding {
                path: "coordination/rounds/r75/reviews/B900-A0001-secondary-executor-two.md"
                    .to_string(),
                role: "secondary".to_string(),
                reviewer: "executor-two".to_string(),
                verdict: "PASS".to_string(),
                sha256: "2".repeat(64),
                bytes: 10,
                delivery_event_id: None,
                substituted_role: None,
                substituted_agent: None,
                source_terminal_event_id: None,
            },
        ]
    }

    fn replay_events(&self) -> Vec<EventRecord> {
        let signoff = ledger::event(
            "PlanSignedOff",
            "user",
            None,
            Some(ROUND),
            plan::plan_signed_off_payload("B288 local replay fixture", 1, VALIDATION_DIGEST)
                .unwrap(),
        );
        let reviews = self.review_bindings();
        let root_payload = RootVerdictPayload {
            verdict: "PASS".to_string(),
            reason: None,
            ir_revision: 1,
            validation_digest: VALIDATION_DIGEST.to_string(),
            attempt_id: ATTEMPT.to_string(),
            attempt_no: 1,
            implementer_agent: "executor-desktop".to_string(),
            head_sha: self.candidate_oid.clone(),
            main_head_sha: self.prior_oid.clone(),
            collect_completed_event_id: "collect-completed-B900-A0001".to_string(),
            bootstrap_pre_signoff_attempt: None,
            reviews: reviews.clone(),
            evidence: Vec::new(),
            gates: Vec::new(),
        };
        let verdict = ledger::event(
            "VerdictIssued",
            "verifier:root",
            Some(TASK),
            Some(ROUND),
            serde_json::to_value(&root_payload).unwrap(),
        );
        let started = ledger::event(
            "MergeStarted",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "attemptId": ATTEMPT,
                "attemptNo": 1,
                "headSha": self.candidate_oid,
                "mainHeadSha": self.prior_oid,
                "collectCompletedEventId": "collect-completed-B900-A0001",
                "verdictEventId": verdict.event_id,
            }),
        );
        let merged = ledger::event(
            "MergeExecuted",
            "reviewer:orch-runtime",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "mergeSha": self.merge_oid,
                "policy": "no-ff",
            }),
        );
        let recorded = ledger::event(
            "TaskRecorded",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({"postMergeGates": "all-green"}),
        );
        let authorization = RootRecordAuthorization {
            verdict_event_id: verdict.event_id.clone(),
            plan_signed_off_event_id: signoff.event_id.clone(),
            attempt_id: ATTEMPT.to_string(),
            attempt_no: 1,
            implementer_agent: "executor-desktop".to_string(),
            head_sha: self.candidate_oid.clone(),
            expected_main_sha: self.prior_oid.clone(),
            merge_sha: self.merge_oid.clone(),
            ir_revision: 1,
            validation_digest: VALIDATION_DIGEST.to_string(),
            task_card_sha256: self.card_sha256.clone(),
            reviews,
            already_recorded: false,
        };
        let payload = FrozenContractSupersededPayload::from_signed_declaration(
            &self.declaration,
            &authorization,
            &self.merge_oid,
            &recorded.event_id,
        );
        let frozen = ledger::event(
            "FrozenContractSuperseded",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::to_value(payload).unwrap(),
        );
        vec![signoff, verdict, started, merged, recorded, frozen]
    }
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn contract(literal: &str) -> String {
    format!(
        "const WIDE_DIGEST: &str = \"{literal}\";\nconst RETAINED_MARKER: &str = \"{RETAINED_MARKER}\";\n"
    )
}

fn reviews() -> Vec<RequiredReview> {
    vec![
        RequiredReview {
            role: "primary".to_string(),
            agent: "executor-one".to_string(),
        },
        RequiredReview {
            role: "secondary".to_string(),
            agent: "executor-two".to_string(),
        },
    ]
}

fn declaration(
    variant: AuthorizationVariant,
    old_contract: &str,
    new_contract: &str,
    old_literal: &str,
    new_literal: &str,
) -> FrozenContractSupersession {
    let (blocked_attempt, replacement, authorization) = match variant {
        AuthorizationVariant::Recovery => (
            Some(FrozenBlockedAttempt {
                round: "r73".to_string(),
                task_id: "B279".to_string(),
                attempt_id: "B279-A0002".to_string(),
                event_id: BLOCKED_EVENT.to_string(),
            }),
            Some(FrozenRecordedReplacement {
                round: "r74".to_string(),
                task_id: "B283".to_string(),
                task_recorded_event_id: RECORDED_EVENT.to_string(),
            }),
            None,
        ),
        AuthorizationVariant::PlannerAdjudicated => (
            None,
            None,
            Some(FrozenPlannerAdjudicatedAuthorization {
                kind: "planner-adjudicated".to_string(),
                adjudication: FrozenAdjudicationEvidence {
                    path: ADJUDICATION_PATH.to_string(),
                    sha256: sha256(ADJUDICATION),
                },
                user_authorization: FrozenUserAuthorization {
                    actor: "user".to_string(),
                    date: "2026-08-19".to_string(),
                    quote: "加 B288 先修演进通道".to_string(),
                },
                removed_assertions: vec![FrozenRemovedAssertion {
                    assertion: old_literal.to_string(),
                    reason: "the whole-prefix digest overstates the retained contract".to_string(),
                    retained_coverage: RETAINED_MARKER.to_string(),
                }],
            }),
        ),
    };
    FrozenContractSupersession {
        target: TARGET.to_string(),
        initiator: TASK.to_string(),
        original_seed_relocated: FrozenSeedRelocationAnchor {
            event_id: ORIGINAL_EVENT.to_string(),
            sha256: sha256(old_contract.as_bytes()),
        },
        effective_anchor: FrozenEffectiveAnchor {
            kind: "seed-relocated".to_string(),
            event_id: Some(ORIGINAL_EVENT.to_string()),
            baseline_tree_sha: None,
            sha256: Some(sha256(old_contract.as_bytes())),
        },
        old_file_sha256: sha256(old_contract.as_bytes()),
        new_file_sha256: sha256(new_contract.as_bytes()),
        old_literal_sha256: Some(old_literal.to_string()),
        new_literal_sha256: Some(new_literal.to_string()),
        subject_prefix: Some(FrozenSubjectPrefix {
            path: SUBJECT.to_string(),
            bytes: OLD_SUBJECT.len() as u64,
        }),
        structured_evolution: None,
        blocked_attempt,
        replacement,
        authorization,
        dispositions: vec![FrozenContractDisposition {
            old_assertion: "whole frozen-contract prefix is byte-identical".to_string(),
            replacement_assertion: Some(
                "the retained run_fusion window remains mechanically covered".to_string(),
            ),
            exemption_reason: None,
        }],
        reviews: reviews(),
    }
}

fn card_text(declaration: &FrozenContractSupersession) -> String {
    let frontmatter = serde_json::json!({
        "taskId": TASK,
        "round": ROUND,
        "requiredReviews": reviews(),
        "frozenContractSupersessions": [declaration],
    });
    format!(
        "---\n{}---\nlocal B288 replay fixture\n",
        serde_yaml::to_string(&frontmatter).unwrap()
    )
}

fn write(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("fixture path must have a parent")).unwrap();
    fs::write(path, bytes).unwrap();
}

fn write_events(root: &Path, relative: &str, events: &[EventRecord]) {
    let mut bytes = events
        .iter()
        .map(|event| serde_json::to_string(event).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes();
    bytes.push(b'\n');
    write(root, relative, &bytes);
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("git must be available for B288 local fixtures");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output must be UTF-8")
        .trim()
        .to_string()
}

fn commit_all(root: &Path, subject: &str) {
    git(root, &["add", "-A"]);
    git(
        root,
        &[
            "-c",
            "user.name=B288",
            "-c",
            "user.email=b288@example.invalid",
            "commit",
            "-q",
            "-m",
            subject,
        ],
    );
}

fn project_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("orch-host must live under the project worktree")
        .to_path_buf()
}

fn landed_probe_target(root: &Path) -> String {
    let bytes = fs::read(root.join("coordination/frozen-contract-baseline-v1.json")).unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    manifest["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|entry| {
            let target = entry.get("target")?.as_str()?;
            let present = entry.get("state")?.as_str()? == "present";
            let relocated = entry.pointer("/effectiveAnchor/kind")?.as_str()? == "seed-relocated";
            (present && relocated && target.starts_with("orch/crates/orch-host/tests/"))
                .then(|| target.to_string())
        })
        .expect("signed baseline must contain a present orch-host test target")
}

/// Compile-time marker called by the immutable B288 seed target. The tests in
/// this module carry the substantive candidate-tree and payload/replay proof.
pub fn contract_loaded() {}

#[test]
fn planner_adjudication_is_bound_to_the_candidate_tree_not_the_worktree() {
    let fixture = GitFixture::new(
        "candidate-binding",
        AuthorizationVariant::PlannerAdjudicated,
    );
    validate_planner_adjudicated_supersession(
        &fixture.root,
        &fixture.declaration,
        &fixture.prior_oid,
        &fixture.candidate_oid,
    )
    .unwrap();
    validate_frozen_contract_declaration(
        &fixture.root,
        &fixture.declaration,
        &fixture.prior_oid,
        &fixture.candidate_oid,
    )
    .expect("the production snapshot validator must call the candidate-bound planner arm");

    write(
        &fixture.root,
        ADJUDICATION_PATH,
        b"uncommitted worktree forgery\n",
    );
    validate_planner_adjudicated_supersession(
        &fixture.root,
        &fixture.declaration,
        &fixture.prior_oid,
        &fixture.candidate_oid,
    )
    .expect("an uncommitted worktree edit must not replace the candidate blob");

    let mut wrong_digest = fixture.declaration.clone();
    wrong_digest
        .authorization
        .as_mut()
        .unwrap()
        .adjudication
        .sha256 = "e".repeat(64);
    assert!(validate_planner_adjudicated_supersession(
        &fixture.root,
        &wrong_digest,
        &fixture.prior_oid,
        &fixture.candidate_oid,
    )
    .is_err());

    let mut worktree_only = fixture.declaration.clone();
    let worktree_only_path = "coordination/rounds/r75/planning/worktree-only.md";
    worktree_only.authorization.as_mut().unwrap().adjudication = FrozenAdjudicationEvidence {
        path: worktree_only_path.to_string(),
        sha256: sha256(b"worktree only\n"),
    };
    write(&fixture.root, worktree_only_path, b"worktree only\n");
    assert!(validate_planner_adjudicated_supersession(
        &fixture.root,
        &worktree_only,
        &fixture.prior_oid,
        &fixture.candidate_oid,
    )
    .is_err());

    let mut wrong_path = fixture.declaration.clone();
    wrong_path.authorization.as_mut().unwrap().adjudication.path =
        "coordination/runtime/forged-adjudication.md".to_string();
    assert!(validate_planner_adjudicated_supersession(
        &fixture.root,
        &wrong_path,
        &fixture.prior_oid,
        &fixture.candidate_oid,
    )
    .is_err());
}

#[test]
fn every_removed_assertion_maps_old_to_new_retained_coverage() {
    let fixture = GitFixture::new(
        "assertion-mapping",
        AuthorizationVariant::PlannerAdjudicated,
    );
    let validate = |declaration: &FrozenContractSupersession| {
        validate_planner_adjudicated_supersession(
            &fixture.root,
            declaration,
            &fixture.prior_oid,
            &fixture.candidate_oid,
        )
    };
    validate(&fixture.declaration).unwrap();

    let mut empty = fixture.declaration.clone();
    empty
        .authorization
        .as_mut()
        .unwrap()
        .removed_assertions
        .clear();
    assert!(validate(&empty).is_err());

    let mut absent_old = fixture.declaration.clone();
    absent_old
        .authorization
        .as_mut()
        .unwrap()
        .removed_assertions[0]
        .assertion = "not present in old".to_string();
    assert!(validate(&absent_old).is_err());

    let mut still_present = fixture.declaration.clone();
    let removed = &mut still_present
        .authorization
        .as_mut()
        .unwrap()
        .removed_assertions[0];
    removed.assertion = RETAINED_MARKER.to_string();
    removed.retained_coverage = "RETAINED_MARKER".to_string();
    assert!(validate(&still_present).is_err());

    let mut missing_coverage = fixture.declaration.clone();
    missing_coverage
        .authorization
        .as_mut()
        .unwrap()
        .removed_assertions[0]
        .retained_coverage = "missing retained marker".to_string();
    assert!(validate(&missing_coverage).is_err());

    let mut duplicated = fixture.declaration.clone();
    let duplicate = duplicated
        .authorization
        .as_ref()
        .unwrap()
        .removed_assertions[0]
        .clone();
    duplicated
        .authorization
        .as_mut()
        .unwrap()
        .removed_assertions
        .push(duplicate);
    assert!(validate(&duplicated).is_err());
}

#[test]
fn frozen_events_roundtrip_both_authorization_variants_without_cross_shape() {
    for (tag, variant) in [
        ("recovery-replay", AuthorizationVariant::Recovery),
        (
            "planner-adjudicated-replay",
            AuthorizationVariant::PlannerAdjudicated,
        ),
    ] {
        let fixture = GitFixture::new(tag, variant);
        let events = fixture.replay_events();
        let replayed = validate_frozen_contract_supersession_delta_for_replay(
            &fixture.root,
            &fixture.merge_oid,
            &events,
            events.len() - 1,
        )
        .unwrap();
        assert_eq!(replayed.signed_declaration(), fixture.declaration);
        let payload_value = serde_json::to_value(&replayed).unwrap();
        assert!(payload_value.get("oldLiteralSha256").is_some());
        assert!(payload_value.get("newLiteralSha256").is_some());
        assert!(payload_value.get("subjectPrefix").is_some());
        assert!(payload_value.get("structuredEvolution").is_none());
        match variant {
            AuthorizationVariant::Recovery => {
                assert!(payload_value.get("blockedAttempt").is_some());
                assert!(payload_value.get("replacement").is_some());
                assert!(payload_value.get("authorization").is_none());
                let old_shape = serde_json::to_vec(&replayed).unwrap();
                let reparsed: FrozenContractSupersededPayload =
                    serde_json::from_slice(&old_shape).unwrap();
                assert_eq!(serde_json::to_vec(&reparsed).unwrap(), old_shape);
                assert!(!fixture.card_text.contains("authorization:"));
                card::parse(CARD_PATH, TASK, &fixture.card_text).unwrap();
            }
            AuthorizationVariant::PlannerAdjudicated => {
                assert!(payload_value.get("blockedAttempt").is_none());
                assert!(payload_value.get("replacement").is_none());
                assert!(payload_value.get("authorization").is_some());
            }
        }
    }
}

#[test]
fn replay_rejects_a_payload_that_drops_the_signed_planner_authorization() {
    let fixture = GitFixture::new(
        "planner-replay-mismatch",
        AuthorizationVariant::PlannerAdjudicated,
    );
    let mut events = fixture.replay_events();
    events
        .last_mut()
        .unwrap()
        .payload
        .as_mut()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("authorization");
    assert!(validate_frozen_contract_supersession_delta_for_replay(
        &fixture.root,
        &fixture.merge_oid,
        &events,
        events.len() - 1,
    )
    .is_err());
}

#[test]
fn an_undeclared_candidate_edit_of_a_landed_contract_is_still_refused() {
    let root = orch_host::util::test_scratch_dir("b288-undeclared-landed-edit");
    let source = project_root();
    git(
        &root,
        &[
            "clone",
            "-q",
            "--shared",
            "--single-branch",
            "--branch",
            "main",
            source.to_str().unwrap(),
            ".",
        ],
    );
    let main_oid = git(&root, &["rev-parse", "refs/heads/main^{commit}"]);
    let target = landed_probe_target(&root);
    git(&root, &["checkout", "-q", "-b", "candidate", &main_oid]);
    let mut tampered = fs::read(root.join(&target)).unwrap();
    tampered.extend_from_slice(b"\n// undeclared B288 mutation\n");
    write(&root, &target, &tampered);
    commit_all(&root, "undeclared landed mutation");
    let candidate_oid = git(&root, &["rev-parse", "HEAD"]);
    let meta: CardMeta = serde_json::from_value(serde_json::json!({
        "taskId": "T1",
        "writeSet": [target],
        "frozenPaths": [],
    }))
    .unwrap();
    let card = Card {
        meta,
        body: String::new(),
        rel_path: "coordination/rounds/r-test/tasks/T1.md".to_string(),
    };
    let error = validate_seed_paths_for_candidate(&root, &card, &main_oid, &candidate_oid)
        .expect_err("the existing undeclared-frozen-edit guard must remain active");
    assert!(
        error.to_string().contains("无签名 supersession"),
        "{error:#}"
    );
}
