use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use orch_host::{card, ledger, plan, round};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("repo root")
        .to_path_buf()
}

#[test]
fn every_r83_card_is_strict_actorless_v3() {
    let root = repo_root();
    for task in ["B319", "B320", "B321", "B322", "B323", "B324", "B325"] {
        let rel = format!("coordination/rounds/r83/tasks/{task}.md");
        let text = fs::read_to_string(root.join(&rel)).unwrap();
        let parsed = card::parse(&rel, task, &text).unwrap();
        assert_eq!(parsed.meta.schema_version, Some(3));
        assert_eq!(parsed.meta.agent, None);
        assert!(parsed.meta.required_reviews.is_empty());
        assert!(parsed.meta.review_fallbacks.is_empty());
        assert!(parsed.meta.nongate_seats.is_empty());
        assert!(parsed.meta.review_quorum.is_none());
        assert!(!parsed.meta.entry_points.is_empty());

        for retired in [
            "agent: null",
            "requiredReviews: []",
            "provider: null",
            "model: ''",
            "fusionMembers: []",
        ] {
            let mutated = text.replacen(
                "schemaVersion: 3",
                &format!("schemaVersion: 3\n{retired}"),
                1,
            );
            assert!(
                card::parse(&rel, task, &mutated).is_err(),
                "accepted {retired}"
            );
        }
    }
}

#[test]
fn v3_ir_has_only_the_actorless_wire_keys_and_roundtrips() {
    let yaml = format!(
        "schemaVersion: 3\nround: r83\nrevision: 1\nsourceBindings:\n  bindingSha256: {}\n  taskCards: {{}}\n  seedSources: {{}}\ntasks:\n  - id: B319\n    seedProtocol: verify-only\n    hasSeeds: false\n    writeSet: [src/lib.rs]\n    frozenPaths: [coordination/rounds/**]\n    entryPoints: [src/lib.rs]\n    gatesFast: [testFast]\n    requiredEvidence: [proof]\n    dependsOn: []\ntaskOrder: [B319]\n",
        "a".repeat(64)
    );
    let parsed = plan::parse_signed_round_ir(&yaml).unwrap();
    let value = serde_json::to_value(&parsed).unwrap();
    let root_keys = value
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    assert_eq!(
        root_keys,
        [
            "revision",
            "round",
            "schemaVersion",
            "sourceBindings",
            "taskOrder",
            "tasks"
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    );
    let source_keys = value["sourceBindings"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    assert_eq!(
        source_keys,
        ["bindingSha256", "seedSources", "taskCards"]
            .into_iter()
            .map(str::to_string)
            .collect()
    );
    let encoded = serde_yaml::to_string(&parsed).unwrap();
    let reparsed = plan::parse_signed_round_ir(&encoded).unwrap();
    assert_eq!(
        plan::validation_digest(&parsed),
        plan::validation_digest(&reparsed)
    );
    for forbidden in [
        "policy: {}",
        "modeRef: ''",
        "scheduling: {}",
        "reviewers: []",
    ] {
        let mutated = yaml.replacen("round: r83", &format!("round: r83\n{forbidden}"), 1);
        assert!(
            plan::parse_signed_round_ir(&mutated).is_err(),
            "accepted {forbidden}"
        );
    }
}

#[test]
fn legacy_round_digests_are_byte_compatible() {
    let root = repo_root();
    for (round, expected) in [
        (
            "r79",
            "5b168f649dc5ca9915363fddc85c953d6130d7df0f3cbf3986fe2849b46cf9cb",
        ),
        (
            "r80",
            "5d9f773e57111051a23bc5944dfc038fc571fb8f4559210b8a26a049d6019ecf",
        ),
        (
            "r81",
            "78f91b93ae54ee6cb5f268b7138889b36ad6395feee4043a4d71e67b5430448b",
        ),
        (
            "r82",
            "aed614d5f32e07306d5f6fdb733c9fd5a172a7503619a145efee4d8ec43ee4b0",
        ),
    ] {
        let text =
            fs::read_to_string(root.join(format!("coordination/rounds/{round}/ROUND-IR.yaml")))
                .unwrap();
        let ir = plan::parse_signed_round_ir(&text).unwrap();
        assert_eq!(plan::validation_digest(&ir), expected, "{round}");
    }
}

#[test]
fn round_open_marker_is_unique_first_and_generation_closed() {
    let first = ledger::event(
        "RoundOpened",
        "runtime:orch",
        None,
        Some("r83"),
        serde_json::json!({"purpose": "test", "contractSchemaVersion": 3}),
    );
    assert_eq!(
        round::contract_schema_from_events(&[first.clone()], "r83").unwrap(),
        Some(3)
    );
    assert!(round::contract_schema_from_events(&[], "r82")
        .unwrap()
        .is_none());
    let mut wrong = first.clone();
    wrong.actor = "planner".to_string();
    assert!(round::contract_schema_from_events(&[wrong], "r83").is_err());
    let mut wrong_round = first.clone();
    wrong_round.round = Some("r82".to_string());
    assert!(round::contract_schema_from_events(&[first.clone(), wrong_round], "r83").is_err());
    assert!(round::contract_schema_from_events(&[first.clone(), first.clone()], "r83").is_err());
    let before = ledger::event(
        "TaskValidated",
        "runtime:orch",
        None,
        Some("r83"),
        serde_json::json!({
            "irRevision": 1, "validationDigest": "a".repeat(64), "reverifyTasks": []
        }),
    );
    assert!(round::contract_schema_from_events(&[before, first], "r83").is_err());
}
