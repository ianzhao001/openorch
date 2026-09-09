//! B130 seeded-red contract.
//!
//! Negative mutations that must turn the named case red:
//! M1. Accept legacy/typo `revision` + `digest` keys in TaskValidated.
//! M2. Treat any historical or unbound PlanSignedOff as authorization for the current IR.
//! The wave-only awaiting-root exit retired; strict IR/sign-off checks remain.

use orch_core::EventRecord;
use orch_host::plan::{
    decode_task_validated, matching_plan_signoff, task_validated_payload,
};

fn event(kind: &str, payload: serde_json::Value) -> EventRecord {
    EventRecord {
        event_id: format!("event-{kind}"),
        ts: "2026-07-26T00:00:00Z".into(),
        actor: "runtime:test".into(),
        kind: kind.into(),
        task_id: None,
        round: Some("r48".into()),
        payload: Some(payload),
        extra: Default::default(),
    }
}

#[test]
fn task_validated_uses_one_strict_canonical_schema() {
    let payload = task_validated_payload(7, "a1b2c3d4");
    assert_eq!(payload["irRevision"], 7);
    assert_eq!(payload["validationDigest"], "a1b2c3d4");

    let decoded = decode_task_validated(&event("TaskValidated", payload)).unwrap();
    assert_eq!(decoded.ir_revision, 7);
    assert_eq!(decoded.validation_digest, "a1b2c3d4");

    for bad in [
        serde_json::json!({"revision": 7, "digest": "a1b2c3d4"}),
        serde_json::json!({"irRevision": 7, "digest": "a1b2c3d4"}),
        serde_json::json!({"revision": 7, "validationDigest": "a1b2c3d4"}),
        serde_json::json!({"irRevision": "7", "validationDigest": "a1b2c3d4"}),
        serde_json::json!({"irRevision": 7, "validationDigest": "a1b2c3d4", "extra": true}),
    ] {
        assert!(decode_task_validated(&event("TaskValidated", bad)).is_err());
    }
}

#[test]
fn plan_signoff_is_bound_to_the_current_ir_revision_and_digest() {
    let unbound = event("PlanSignedOff", serde_json::json!({"note": "old"}));
    let stale = event(
        "PlanSignedOff",
        serde_json::json!({"irRevision": 6, "validationDigest": "old"}),
    );
    let exact = event(
        "PlanSignedOff",
        serde_json::json!({"irRevision": 7, "validationDigest": "a1b2c3d4"}),
    );

    assert!(!matching_plan_signoff(&[unbound], "r48", 7, "a1b2c3d4").unwrap());
    assert!(!matching_plan_signoff(&[stale], "r48", 7, "a1b2c3d4").unwrap());
    assert!(matching_plan_signoff(&[exact], "r48", 7, "a1b2c3d4").unwrap());
}
