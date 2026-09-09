//! B146 seeded-red contract.
//!
//! Negative mutations that must turn the named case red:
//! M1. Tolerate an unknown field in a signed ROUND-IR (the exact bypass that
//!     let post-signoff YAML edits change runtime behavior in r50).
//! M2. Leave the succession/dispatch policy fields out of the validation
//!     digest so a policy change survives without a re-sign.
//! M3. Accept a ROUND-IR carrying an unmodeled schemaVersion.

use orch_host::plan::{parse_signed_round_ir, validation_digest};

const BASE: &str = r#"
schemaVersion: 2
round: r51
revision: 1
liveness:
  monitorSeconds: 15
  workingStallMinutes: 10
  confirmSamples: 2
  terminationGraceSeconds: 10
  stallEscalationMultiplier: 2
  autoTerminateStalled: true
dispatch:
  ackTimeoutSeconds: 120
tasks: []
"#;

#[test]
fn unknown_fields_are_rejected_not_ignored() {
    assert!(parse_signed_round_ir(BASE).is_ok());
    // M1: an injected unknown key must refuse loudly, never parse-and-drop.
    let smuggled = BASE.replace(
        "dispatch:",
        "sneakyPolicy: 1\ndispatch:",
    );
    assert!(parse_signed_round_ir(&smuggled).is_err());
}

#[test]
fn policy_fields_are_typed_and_digest_bound() {
    let ir = parse_signed_round_ir(BASE).expect("base parses");
    assert_eq!(ir.liveness.stall_escalation_multiplier, Some(2));
    assert_eq!(ir.liveness.auto_terminate_stalled, Some(true));
    assert_eq!(ir.dispatch.ack_timeout_seconds, Some(120));
    // M2: flipping a policy value must change the signed digest.
    let flipped = parse_signed_round_ir(&BASE.replace(
        "autoTerminateStalled: true",
        "autoTerminateStalled: false",
    ))
    .expect("flipped parses");
    assert_ne!(validation_digest(&ir), validation_digest(&flipped));
}

#[test]
fn unmodeled_schema_versions_fail_closed() {
    // M3: a future (or absent-but-required) schema version is not guessable.
    assert!(parse_signed_round_ir(&BASE.replace("schemaVersion: 2", "schemaVersion: 99")).is_err());
}
