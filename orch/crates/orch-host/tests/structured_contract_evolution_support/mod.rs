use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use orch_core::EventRecord;
use orch_host::card::{
    FrozenAdjudicationEvidence, FrozenContractByteRange, FrozenContractDisposition,
    FrozenContractEdit, FrozenContractEditUnit, FrozenContractSupersession, FrozenEffectiveAnchor,
    FrozenPlannerAdjudicatedAuthorization, FrozenRemovedAssertion, FrozenSeedRelocationAnchor,
    FrozenStructuredEvolution, FrozenSubjectPrefix, FrozenUserAuthorization, RequiredReview,
};
use orch_host::ledger;
use orch_host::plan::{self, TaskInput};
use orch_host::verify::{ReviewBinding, RootRecordAuthorization, RootVerdictPayload};
use sha2::{Digest, Sha256};

pub const CONTRACT_ID: &str = "B290";

const ROUND: &str = "r50";
const TASK: &str = "B900";
const ATTEMPT: &str = "B900-A0001";
const TARGET: &str = "contracts/frozen_contract.rs";
const SUBJECT: &str = "src/frozen_subject.txt";
const CARD_PATH: &str = "coordination/rounds/r50/tasks/B900.md";
const IR_PATH: &str = "coordination/rounds/r50/ROUND-IR.yaml";
const ADJUDICATION_PATH: &str = "coordination/rounds/r50/planning/B900-adjudication.md";
const ORIGINAL_EVENT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const WIDE_ASSERTION: &str = "wide-prefix-assertion";
const RETAINED_COVERAGE: &str = "retained-coverage";
const ADJUDICATION: &[u8] =
    b"# B900 adjudication\n\nReplace the wide assertion with bounded retained coverage.\n";
const OLD_SUBJECT: &[u8] = b"old-subject-prefix\n";
const NEW_SUBJECT: &[u8] = b"new-subject-prefix\n";
const OLD_STRUCTURED: &str = concat!(
    "const REMOVE: &str = \"wide-prefix-assertion\";\n",
    "const KEEP: &str = \"retained-coverage\";\n",
    "const MODE: &str = \"legacy\";\n",
);
const NEW_STRUCTURED: &str = concat!(
    "const KEEP: &str = \"retained-coverage\";\n",
    "const MODE: &str = \"structured\";\n",
    "const EXTRA: &str = \"replay-byte-identical\";\n",
);
const MODE_YAML: &str = r#"
preset: relay
objective: "B290 production-path fixture"
agents:
  executor: {adapter: codex-desktop, tier: F, waitStyle: self-wait, agentId: executor-desktop}
  verifier: {adapter: root-manual, tier: none}
hitl: {planSignoff: required, mergeGate: auto}
verification: {mode: root-manual-fixed-head}
liveness: {monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}
scheduling:
  allowedAgents: [executor-desktop, executor-claw, executor-opencode]
  capacities:
    executor-desktop: {agent: 1, quota: 1, roles: [implement]}
    executor-claw: {agent: 1, quota: 1, roles: [primary-review]}
    executor-opencode: {agent: 1, quota: 1, roles: [secondary-review]}
budgets:
  round: {maxUsd: 1.0, wallMinutes: 120, maxModelWakes: 4}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
"#;

#[derive(Debug, Clone, Copy)]
pub enum Shape {
    StructuredHappy,
    StructuredMultiLine,
    StructuredUnexplainedByte,
    StructuredWholeFileSingleUnit,
    StructuredOverlappingUnits,
    StructuredMismatchedOldWindow,
    StructuredUnorderedUnits,
    LiteralHappy,
    BothShapesDeclared,
    NeitherShapeDeclared,
    LiteralWithMultiLineDiff,
    StructuredMissingRetainedCoverage,
    StructuredMissingUserAuthorization,
    StructuredWrongWholeFileDigest,
}

pub struct Fixture {
    pub root: PathBuf,
    pub declaration: FrozenContractSupersession,
    pub prior_main: String,
    pub effective_main: String,
}

pub struct EmitScene {
    pub root: PathBuf,
    pub round: String,
    pub task_id: String,
    pub existing_events: Vec<EventRecord>,
    pub authorization: RootRecordAuthorization,
    pub effective_main_sha: String,
    pub recorded: EventRecord,
    pub declaration: FrozenContractSupersession,
}

struct BuiltScene {
    root: PathBuf,
    declaration: FrozenContractSupersession,
    prior_oid: String,
    merge_oid: String,
    existing_events: Vec<EventRecord>,
    authorization: RootRecordAuthorization,
    recorded: EventRecord,
}

pub fn fixture(shape: Shape) -> Fixture {
    let scene = BuiltScene::new(shape);
    Fixture {
        root: scene.root,
        declaration: scene.declaration,
        prior_main: scene.prior_oid,
        effective_main: scene.merge_oid,
    }
}

pub fn emit_scene(shape: Shape) -> EmitScene {
    assert!(
        matches!(shape, Shape::StructuredHappy | Shape::LiteralHappy),
        "emit scene is only defined for the two valid evolution shapes"
    );
    let scene = BuiltScene::new(shape);
    EmitScene {
        root: scene.root,
        round: ROUND.to_string(),
        task_id: TASK.to_string(),
        existing_events: scene.existing_events,
        authorization: scene.authorization,
        effective_main_sha: scene.merge_oid,
        recorded: scene.recorded,
        declaration: scene.declaration,
    }
}

pub fn edit_unit_count(fixture: &Fixture) -> usize {
    fixture
        .declaration
        .structured_evolution
        .as_ref()
        .map_or(0, |evolution| evolution.units.len())
}

impl BuiltScene {
    fn new(shape: Shape) -> Self {
        let root = orch_host::util::test_scratch_dir(&format!("b290-{shape:?}"));
        git(&root, &["init", "-q", "-b", "main"]);

        let (old_contract, new_contract) = contracts(shape);
        let declaration = declaration(shape, &old_contract, &new_contract);
        let card_text = card_text(&declaration);
        let card_sha256 = sha256(card_text.as_bytes());
        let manifest = baseline_manifest(&declaration);
        let manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        let binding_yaml = binding_yaml(&sha256(&manifest_bytes));
        let mode_yaml = MODE_YAML;

        write(&root, TARGET, old_contract.as_bytes());
        write(&root, SUBJECT, OLD_SUBJECT);
        write(&root, CARD_PATH, card_text.as_bytes());
        write(
            &root,
            "coordination/modes/relay-selfhost.yaml",
            mode_yaml.as_bytes(),
        );
        write(
            &root,
            "coordination/PROJECT-BINDING.yaml",
            binding_yaml.as_bytes(),
        );
        write(
            &root,
            "coordination/frozen-contract-baseline-v1.json",
            &manifest_bytes,
        );

        let genesis_events = genesis_events(&declaration);
        write_events(
            &root,
            "coordination/rounds/r49/events.jsonl",
            &genesis_events,
        );

        let task_input = TaskInput {
            id: TASK.to_string(),
            agent: "executor-desktop".to_string(),
            seed_protocol: "verify-only".to_string(),
            has_seeds: false,
            write_set: vec![TARGET.to_string()],
            frozen_paths: Vec::new(),
            gates_fast: vec!["testFast".to_string()],
            wall_minutes: Some(60),
            required_reviews: reviews(),
            required_evidence: vec!["structured-evolution".to_string()],
            bootstrap_pre_signoff_attempt: None,
            card_source_sha256: card_sha256.clone(),
            seed_source_sha256: BTreeMap::new(),
        };
        let ir = plan::compile_ir(ROUND, mode_yaml, &binding_yaml, &[task_input]).unwrap();
        let validation_digest = plan::validation_digest(&ir);
        let ir_yaml = serde_yaml::to_string(&ir).unwrap();
        write(&root, IR_PATH, ir_yaml.as_bytes());

        let task_validated = ledger::event(
            "TaskValidated",
            "runtime:orch",
            None,
            Some(ROUND),
            plan::task_validated_payload(1, &validation_digest),
        );
        let signoff = ledger::event(
            "PlanSignedOff",
            "user",
            None,
            Some(ROUND),
            plan::plan_signed_off_payload(
                "B290 structured-evolution production fixture",
                1,
                &validation_digest,
            )
            .unwrap(),
        );
        write_events(
            &root,
            "coordination/rounds/r50/events.jsonl",
            &[task_validated.clone(), signoff.clone()],
        );
        commit_all(&root, "signed prior main");
        let prior_oid = git(&root, &["rev-parse", "HEAD"]);

        git(&root, &["checkout", "-q", "-b", "candidate"]);
        write(&root, TARGET, new_contract.as_bytes());
        write(&root, SUBJECT, NEW_SUBJECT);
        write(&root, ADJUDICATION_PATH, ADJUDICATION);
        commit_all(&root, "candidate structured-contract evolution");
        let candidate_oid = git(&root, &["rev-parse", "HEAD"]);

        git(&root, &["checkout", "-q", "main"]);
        git(
            &root,
            &[
                "-c",
                "user.name=B290",
                "-c",
                "user.email=b290@example.invalid",
                "merge",
                "-q",
                "--no-ff",
                "-m",
                "merge candidate",
                "candidate",
            ],
        );
        let merge_oid = git(&root, &["rev-parse", "HEAD"]);

        let review_bindings = review_bindings();
        let root_payload = RootVerdictPayload {
            verdict: "PASS".to_string(),
            reason: None,
            ir_revision: 1,
            validation_digest: validation_digest.clone(),
            attempt_id: ATTEMPT.to_string(),
            attempt_no: 1,
            implementer_agent: "executor-desktop".to_string(),
            head_sha: candidate_oid.clone(),
            main_head_sha: prior_oid.clone(),
            collect_completed_event_id: "collect-completed-B900-A0001".to_string(),
            bootstrap_pre_signoff_attempt: None,
            reviews: review_bindings.clone(),
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
                "headSha": candidate_oid,
                "mainHeadSha": prior_oid,
                "collectCompletedEventId": "collect-completed-B900-A0001",
                "verdictEventId": verdict.event_id,
            }),
        );
        let merged = ledger::event(
            "MergeExecuted",
            "reviewer:orch-runtime",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({"mergeSha": merge_oid, "policy": "no-ff"}),
        );
        let recorded = ledger::event(
            "TaskRecorded",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({"postMergeGates": "all-green"}),
        );
        let existing_events = vec![
            task_validated,
            signoff.clone(),
            verdict.clone(),
            started,
            merged,
        ];
        write_events(
            &root,
            "coordination/rounds/r50/events.jsonl",
            &existing_events,
        );
        let authorization = RootRecordAuthorization {
            verdict_event_id: verdict.event_id,
            plan_signed_off_event_id: signoff.event_id,
            attempt_id: ATTEMPT.to_string(),
            attempt_no: 1,
            implementer_agent: "executor-desktop".to_string(),
            head_sha: candidate_oid.clone(),
            expected_main_sha: prior_oid.clone(),
            merge_sha: merge_oid.clone(),
            ir_revision: 1,
            validation_digest,
            task_card_sha256: card_sha256,
            reviews: review_bindings,
            already_recorded: false,
        };

        Self {
            root,
            declaration,
            prior_oid,
            merge_oid,
            existing_events,
            authorization,
            recorded,
        }
    }
}

fn contracts(shape: Shape) -> (String, String) {
    if matches!(shape, Shape::LiteralHappy) {
        let old_literal = sha256(OLD_SUBJECT);
        let new_literal = sha256(NEW_SUBJECT);
        return (
            literal_contract(&old_literal),
            literal_contract(&new_literal),
        );
    }
    let mut new = NEW_STRUCTURED.to_string();
    if matches!(shape, Shape::StructuredUnexplainedByte) {
        new.push_str("const UNEXPLAINED: &str = \"outside-units\";\n");
    }
    (OLD_STRUCTURED.to_string(), new)
}

fn declaration(shape: Shape, old_contract: &str, new_contract: &str) -> FrozenContractSupersession {
    let literal_shape = matches!(
        shape,
        Shape::LiteralHappy | Shape::LiteralWithMultiLineDiff | Shape::BothShapesDeclared
    );
    let structured_shape = !matches!(
        shape,
        Shape::LiteralHappy | Shape::LiteralWithMultiLineDiff | Shape::NeitherShapeDeclared
    );
    let old_literal = sha256(OLD_SUBJECT);
    let new_literal = sha256(NEW_SUBJECT);
    let removed_assertion = if matches!(shape, Shape::LiteralHappy) {
        old_literal.clone()
    } else {
        WIDE_ASSERTION.to_string()
    };
    let retained_coverage = if matches!(shape, Shape::StructuredMissingRetainedCoverage) {
        "missing-retained-coverage".to_string()
    } else {
        RETAINED_COVERAGE.to_string()
    };
    let authorization = Some(FrozenPlannerAdjudicatedAuthorization {
            kind: "planner-adjudicated".to_string(),
            adjudication: FrozenAdjudicationEvidence {
                path: ADJUDICATION_PATH.to_string(),
                sha256: sha256(ADJUDICATION),
            },
            user_authorization: FrozenUserAuthorization {
                actor: if matches!(shape, Shape::StructuredMissingUserAuthorization) {
                    String::new()
                } else {
                    "user".to_string()
                },
                date: "2026-08-21".to_string(),
                quote: "authorize bounded structured evolution".to_string(),
            },
            removed_assertions: vec![FrozenRemovedAssertion {
                assertion: removed_assertion,
                reason: "the wide assertion is replaced by bounded coverage".to_string(),
                retained_coverage,
            }],
    });

    let mut units = if structured_shape {
        structured_units(old_contract)
    } else {
        Vec::new()
    };
    match shape {
        Shape::StructuredWholeFileSingleUnit => {
            units = vec![FrozenContractEditUnit {
                old_range: FrozenContractByteRange {
                    start: 0,
                    end: old_contract.len() as u64,
                },
                old_sha256: sha256(old_contract.as_bytes()),
                edit: FrozenContractEdit::Replace {
                    content: new_contract.to_string(),
                },
            }];
        }
        Shape::StructuredOverlappingUnits => {
            let first_end = units[0].old_range.end as usize;
            let overlap_start = first_end - WIDE_ASSERTION.len();
            let mode_end = old_contract.len();
            units[1] = FrozenContractEditUnit {
                old_range: FrozenContractByteRange {
                    start: overlap_start as u64,
                    end: mode_end as u64,
                },
                old_sha256: sha256(&old_contract.as_bytes()[overlap_start..mode_end]),
                edit: FrozenContractEdit::Replace {
                    content: NEW_STRUCTURED.to_string(),
                },
            };
        }
        Shape::StructuredMismatchedOldWindow => {
            units[1].old_sha256 = "f".repeat(64);
        }
        Shape::StructuredUnorderedUnits => units.reverse(),
        _ => {}
    }

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
        new_file_sha256: if matches!(shape, Shape::StructuredWrongWholeFileDigest) {
            "e".repeat(64)
        } else {
            sha256(new_contract.as_bytes())
        },
        old_literal_sha256: literal_shape.then_some(old_literal),
        new_literal_sha256: literal_shape.then_some(new_literal),
        subject_prefix: literal_shape.then_some(FrozenSubjectPrefix {
            path: SUBJECT.to_string(),
            bytes: OLD_SUBJECT.len() as u64,
        }),
        structured_evolution: structured_shape.then_some(FrozenStructuredEvolution { units }),
        blocked_attempt: None,
        replacement: None,
        authorization,
        dispositions: vec![FrozenContractDisposition {
            old_assertion: "wide frozen-contract assertion".to_string(),
            replacement_assertion: Some("bounded structured coverage".to_string()),
            exemption_reason: None,
        }],
        reviews: reviews(),
    }
}

fn structured_units(old: &str) -> Vec<FrozenContractEditUnit> {
    let delete_end = old.find('\n').unwrap() + 1;
    let mode_start = old.find("const MODE:").unwrap();
    let mode_end = old.len();
    vec![
        FrozenContractEditUnit {
            old_range: FrozenContractByteRange {
                start: 0,
                end: delete_end as u64,
            },
            old_sha256: sha256(&old.as_bytes()[..delete_end]),
            edit: FrozenContractEdit::Delete {
                removed_assertion: WIDE_ASSERTION.to_string(),
            },
        },
        FrozenContractEditUnit {
            old_range: FrozenContractByteRange {
                start: mode_start as u64,
                end: mode_end as u64,
            },
            old_sha256: sha256(&old.as_bytes()[mode_start..mode_end]),
            edit: FrozenContractEdit::Replace {
                content: concat!(
                    "const MODE: &str = \"structured\";\n",
                    "const EXTRA: &str = \"replay-byte-identical\";\n",
                )
                .to_string(),
            },
        },
    ]
}

fn literal_contract(literal: &str) -> String {
    format!(
        "const WIDE_DIGEST: &str = \"{literal}\";\nconst KEEP: &str = \"{RETAINED_COVERAGE}\";\n"
    )
}

fn baseline_manifest(declaration: &FrozenContractSupersession) -> serde_json::Value {
    serde_json::json!({
        "schemaVersion": 1,
        "baselineTreeSha": "a".repeat(40),
        "scope": {
            "declaredPairAuditThrough": "r70",
            "effectiveBaselineThrough": "r49/B001",
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
                "round": "r49",
                "taskId": "B001",
                "cardPath": "coordination/rounds/r49/tasks/B001.md",
                "seedSrc": "coordination/rounds/r49/seeds/B001/frozen_contract.rs",
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
    })
}

fn genesis_events(declaration: &FrozenContractSupersession) -> Vec<EventRecord> {
    let oracle = ledger::event(
        "SeedOracleVerified",
        "planner",
        Some("B001"),
        Some("r49"),
        serde_json::json!({
            "seeds": [{"target": TARGET, "sha256": declaration.old_file_sha256}],
        }),
    );
    let mut relocated = ledger::event(
        "SeedRelocated",
        "runtime:orch",
        Some("B001"),
        Some("r49"),
        serde_json::json!({
            "target": TARGET,
            "sha256": declaration.old_file_sha256,
            "cmp": "identical",
        }),
    );
    relocated.event_id = ORIGINAL_EVENT.to_string();
    vec![oracle, relocated]
}

fn reviews() -> Vec<RequiredReview> {
    vec![
        RequiredReview {
            role: "primary".to_string(),
            agent: "executor-claw".to_string(),
        },
        RequiredReview {
            role: "secondary".to_string(),
            agent: "executor-opencode".to_string(),
        },
    ]
}

fn review_bindings() -> Vec<ReviewBinding> {
    vec![
        ReviewBinding {
            path: "coordination/rounds/r50/reviews/B900-A0001-primary-executor-claw.md".to_string(),
            role: "primary".to_string(),
            reviewer: "executor-claw".to_string(),
            verdict: "PASS".to_string(),
            sha256: "1".repeat(64),
            bytes: 10,
            delivery_event_id: None,
            substituted_role: None,
            substituted_agent: None,
            source_terminal_event_id: None,
        },
        ReviewBinding {
            path: "coordination/rounds/r50/reviews/B900-A0001-secondary-executor-opencode.md"
                .to_string(),
            role: "secondary".to_string(),
            reviewer: "executor-opencode".to_string(),
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

fn card_text(declaration: &FrozenContractSupersession) -> String {
    let frontmatter = serde_json::json!({
        "taskId": TASK,
        "round": ROUND,
        "agent": "executor-desktop",
        "seedProtocol": "verify-only",
        "writeSet": [TARGET],
        "frozenPaths": [],
        "gates": {"fast": ["testFast"]},
        "budgets": {"wallMinutes": 60},
        "requiredReviews": reviews(),
        "requiredEvidence": ["structured-evolution"],
        "frozenContractSupersessions": [declaration],
    });
    format!(
        "---\n{}---\nB290 structured evolution fixture\n",
        serde_yaml::to_string(&frontmatter).unwrap()
    )
}

fn binding_yaml(manifest_sha256: &str) -> String {
    format!(
        r#"
project: {{ecosystems: [rust]}}
commands:
  testFast: {{argv: [cargo, test]}}
  check: {{argv: [cargo, check]}}
gates: {{fast: [testFast, check]}}
scope:
  protectedPaths: ["coordination/**", "design/**"]
git: {{pushPolicy: forbidden}}
oracle:
  dialect: cargo
  landedSeedBaseline:
    schemaVersion: 1
    path: coordination/frozen-contract-baseline-v1.json
    sha256: "{manifest_sha256}"
"#
    )
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
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
        .expect("git must be available for B290 fixtures");
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
            "user.name=B290",
            "-c",
            "user.email=b290@example.invalid",
            "commit",
            "-q",
            "-m",
            subject,
        ],
    );
}
