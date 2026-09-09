//! B149 seeded-red contract.
//!
//! Negative mutations that must turn the named case red:
//! M1. Give an unknown initiator marker a default class instead of failing
//!     closed.
//! M2. Default an unmarked invocation to an agent/daemon class — unmarked
//!     must count as human-interactive so the zero-touch gate stays strict.
//! M3. Skip injecting the classification keys into event payloads.

use orch_host::ledger::{classify_initiator, initiator_payload, InitiatorKind};

#[test]
fn initiator_classes_are_a_closed_enum() {
    assert_eq!(
        classify_initiator(Some("human-interactive")).unwrap(),
        InitiatorKind::HumanInteractive
    );
    assert_eq!(
        classify_initiator(Some("root-agent-operated")).unwrap(),
        InitiatorKind::RootAgentOperated
    );
    assert_eq!(
        classify_initiator(Some("daemon-automatic")).unwrap(),
        InitiatorKind::DaemonAutomatic
    );
    assert_eq!(
        classify_initiator(Some("test-fixture")).unwrap(),
        InitiatorKind::TestFixture
    );
    // M1: unmodeled markers refuse.
    assert!(classify_initiator(Some("robot")).is_err());
    assert!(classify_initiator(Some("")).is_err());
}

#[test]
fn unmarked_invocations_count_as_human() {
    // M2: strictness over convenience — no marker means a human at a keyboard
    // until proven otherwise. This keeps human_interventions=0 falsifiable.
    assert_eq!(
        classify_initiator(None).unwrap(),
        InitiatorKind::HumanInteractive
    );
}

#[test]
fn classification_lands_in_event_payloads() {
    // M3: the classification must be injected as payload keys, not lost.
    let v = initiator_payload(InitiatorKind::RootAgentOperated, "planner-cli");
    assert_eq!(v["initiatorKind"], "root-agent-operated");
    assert_eq!(v["invocationMode"], "planner-cli");
}
