//! Role/configuration and run identities share the same path-safe contract.
use orch_host::{
    channel::InvocationTuple,
    fusion_roles::{validate_config, FusionCombination, FusionConfig, FusionRole},
    fusion_run::FusionRequest,
};

fn results(id: &str) -> Vec<anyhow::Result<()>> {
    let role = FusionRole {
        id: id.into(), name: "Role".into(), harness: "configured:fixture".into(),
        instructions: String::new(), fixed: InvocationTuple::default(),
    };
    let group = FusionCombination {
        id: id.into(), name: "Group".into(), members: vec![], disabled: vec![], synthesizer: None,
    };
    vec![
        validate_config(&FusionConfig { roles: vec![role], ..Default::default() }),
        validate_config(&FusionConfig { combinations: vec![group], ..Default::default() }),
        FusionRequest { request_id: id.into(), combination_id: "group".into(), question: "Question".into() }.validate(),
        FusionRequest { request_id: "request".into(), combination_id: id.into(), question: "Question".into() }.validate(),
    ]
}

#[test]
fn every_identifier_entry_accepts_the_existing_ascii_boundaries() {
    for id in ["a", "A0_-", &"a".repeat(64)] {
        for result in results(id) {
            assert!(result.is_ok(), "{id}: {result:?}");
        }
    }
}

#[test]
fn every_identifier_entry_rejects_invalid_values_with_the_existing_error() {
    for id in ["", &"a".repeat(65), "../escape", "a/b", "a\\b", "a.b", "two words", "中文", "a\0b", "a\nb"] {
        for result in results(id) {
            assert_eq!(result.unwrap_err().to_string(), "invalid_identifier", "{id:?}");
        }
    }
}
