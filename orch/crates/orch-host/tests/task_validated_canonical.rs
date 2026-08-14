use orch_core::EventRecord;
use orch_host::plan::{
    decode_runtime_task_validated, decode_task_validated, matching_user_plan_signoff,
    task_validated_payload,
};

fn event(payload: serde_json::Value) -> EventRecord {
    EventRecord {
        event_id: "canonical-probe".into(),
        ts: "2026-07-26T00:00:00Z".into(),
        actor: "runtime:orch".into(),
        kind: "TaskValidated".into(),
        task_id: None,
        round: Some("r48".into()),
        payload: Some(payload),
        extra: Default::default(),
    }
}

fn digest() -> String {
    "a".repeat(64)
}

#[test]
fn producer_output_is_accepted_by_the_only_consumer_codec() {
    let digest = digest();
    let decoded = decode_task_validated(&event(task_validated_payload(3, &digest))).unwrap();
    assert_eq!(decoded.ir_revision, 3);
    assert_eq!(decoded.validation_digest, digest);
}

#[test]
fn aliases_mixed_keys_extra_fields_and_wrong_types_are_all_rejected() {
    for payload in [
        serde_json::json!({"revision": 3, "digest": "digest-3"}),
        serde_json::json!({"irRevision": 3, "digest": "digest-3"}),
        serde_json::json!({"revision": 3, "validationDigest": "digest-3"}),
        serde_json::json!({"irRevision": "3", "validationDigest": "digest-3"}),
        serde_json::json!({"irRevision": 3, "validationDigest": 3}),
        serde_json::json!({"irRevision": 3, "validationDigest": "digest-3", "note": "no"}),
        serde_json::json!({"irRevision": 0, "validationDigest": "digest-3"}),
    ] {
        assert!(decode_task_validated(&event(payload)).is_err());
    }
}

#[test]
fn production_validation_envelope_requires_runtime_actor_round_and_no_task() {
    let payload = task_validated_payload(3, &digest());
    let mut wrong_actor = event(payload.clone());
    wrong_actor.actor = "runtime:test".into();
    assert!(decode_runtime_task_validated(&wrong_actor, "r48").is_err());

    let mut task_scoped = event(payload.clone());
    task_scoped.task_id = Some("B130".into());
    assert!(decode_runtime_task_validated(&task_scoped, "r48").is_err());

    let mut wrong_round = event(payload);
    wrong_round.round = Some("r47".into());
    assert!(decode_runtime_task_validated(&wrong_round, "r48").is_err());

    assert!(decode_runtime_task_validated(
        &event(task_validated_payload(3, "legacy-16-hex")),
        "r48"
    )
    .is_err());
}

#[test]
fn non_user_or_task_scoped_exact_signoff_cannot_authorize_or_dos_user_signoff() {
    let digest = digest();
    let payload = serde_json::json!({
        "note": "canonical sign-off",
        "irRevision": 3,
        "validationDigest": digest.clone(),
    });
    let mut non_user = event(payload.clone());
    non_user.kind = "PlanSignedOff".into();
    non_user.actor = "runtime:orch".into();
    let mut task_scoped = event(payload.clone());
    task_scoped.kind = "PlanSignedOff".into();
    task_scoped.actor = "user".into();
    task_scoped.task_id = Some("B130".into());
    let validation = event(task_validated_payload(3, &digest));
    assert!(!matching_user_plan_signoff(
        &[validation.clone(), non_user.clone(), task_scoped],
        "r48",
        3,
        &digest
    )
    .unwrap());

    let mut user = non_user;
    user.actor = "user".into();
    assert!(matching_user_plan_signoff(
        &[validation, user],
        "r48",
        3,
        &digest
    )
    .unwrap());
}

#[test]
fn bound_user_signoff_requires_nonblank_note_full_digest_and_no_extra_fields() {
    let digest = digest();
    let validation = event(task_validated_payload(3, &digest));
    for payload in [
        serde_json::json!({"irRevision": 3, "validationDigest": digest}),
        serde_json::json!({"note": " ", "irRevision": 3, "validationDigest": digest}),
        serde_json::json!({"note": "ok", "irRevision": 3, "validationDigest": digest, "extra": true}),
        serde_json::json!({"note": "ok", "irRevision": 3, "validationDigest": "short"}),
    ] {
        let mut signoff = event(payload);
        signoff.kind = "PlanSignedOff".into();
        signoff.actor = "user".into();
        assert!(matching_user_plan_signoff(
            &[validation.clone(), signoff],
            "r48",
            3,
            &digest,
        )
        .is_err());
    }

    let mut historical = event(serde_json::json!({"note": "legacy unbound"}));
    historical.kind = "PlanSignedOff".into();
    historical.actor = "user".into();
    assert!(!matching_user_plan_signoff(&[validation, historical], "r48", 3, &digest).unwrap());
}
