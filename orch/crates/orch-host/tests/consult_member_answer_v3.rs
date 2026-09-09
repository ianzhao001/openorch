//! B322 companion coverage for terminal byte binding and member manifests.

use orch_host::consult::{
    classify_consult_answer_v3, ConsultAnswerInputV3, ConsultAnswerValidityV3,
    ConsultMemberManifestV3,
};
use orch_host::harness::{classify_terminal_observation, CapabilitySource, TerminalObservation};

fn answered(text: &str, managed: bool) -> orch_host::harness::TerminalRecord {
    classify_terminal_observation(TerminalObservation {
        capability: CapabilitySource::Native,
        exit_code: Some(0),
        exact_reason: "companion-terminal".into(),
        turn_ended: true,
        final_text: Some(text.to_string()),
        final_text_sha256: None,
        output_path: None,
        output_sha256: None,
        usage: None,
        usage_absent_reason: Some("fixture".into()),
        managed_scope_terminated: managed,
        activity_seen: true,
        authenticated_cancel: false,
    })
    .unwrap()
}

#[test]
fn trusted_digest_and_managed_scope_are_both_required() {
    let valid =
        ConsultAnswerInputV3::from_terminal(answered("answer", true), "answer", false, false);
    assert_eq!(
        classify_consult_answer_v3(&valid),
        ConsultAnswerValidityV3::Valid
    );

    let drift = ConsultAnswerInputV3::from_terminal(
        answered("answer", true),
        "different bytes",
        false,
        false,
    );
    assert!(matches!(
        classify_consult_answer_v3(&drift),
        ConsultAnswerValidityV3::Invalid(reason) if reason.contains("bytes")
    ));

    let live_scope =
        ConsultAnswerInputV3::from_terminal(answered("answer", false), "answer", false, false);
    assert!(matches!(
        classify_consult_answer_v3(&live_scope),
        ConsultAnswerValidityV3::Invalid(reason) if reason.contains("scope")
    ));
}

#[test]
fn member_manifest_binds_artifact_bytes_and_has_a_closed_schema() {
    let invalid = ConsultMemberManifestV3::new(
        "alpha",
        "short-head",
        "/repo",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        6,
    );
    assert!(invalid.is_err());

    let manifest = ConsultMemberManifestV3::new(
        "alpha",
        "0123456789012345678901234567890123456789",
        "/repo",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        6,
    )
    .unwrap();
    assert!(manifest.matches_artifact(
        "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        6
    ));
    assert!(!manifest.matches_artifact(
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        6
    ));

    let mut value = serde_json::to_value(&manifest).unwrap();
    value["legacyPanel"] = serde_json::json!(true);
    assert!(serde_json::from_value::<ConsultMemberManifestV3>(value).is_err());
}
