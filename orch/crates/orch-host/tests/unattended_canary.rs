//! B143 seeded-red contract.
//!
//! Negative mutations that must turn the named case red:
//! M1. Count a manual (user-actor or manual-cli) intervention as automatic,
//!     inflating the zero-touch score.
//! M2. Let the zero-manual gate pass while manual interventions are present.
//! M3. Let the canary pass without visiting every required fault station.

use orch_host::serve::{canary_gate, canary_summary, CanaryEvent};

fn ev(actor: &str, method: &str, station: &str) -> CanaryEvent {
    CanaryEvent {
        actor: actor.to_string(),
        method: method.to_string(),
        station: station.to_string(),
    }
}

const STATIONS: [&str; 6] = [
    "go-claim",
    "go-rewake",
    "stall-escalation",
    "auto-terminate",
    "succession",
    "chain-exhausted",
];

#[test]
fn summary_separates_manual_from_automatic() {
    let events = vec![
        ev("runtime:orch", "auto", "go-claim"),
        ev("user", "manual-cli", "go-rewake"),
        ev("runtime:orch", "manual-cli", "stall-escalation"),
    ];
    let s = canary_summary(&events);
    // M1: user actor or manual-cli method is manual, no matter who ran it.
    assert_eq!(s.automatic, 1);
    assert_eq!(s.manual, 2);
}

#[test]
fn zero_manual_gate_is_strict() {
    let clean = vec![ev("runtime:orch", "auto", "go-claim")];
    let dirty = vec![
        ev("runtime:orch", "auto", "go-claim"),
        ev("user", "manual-cli", "succession"),
    ];
    assert!(canary_gate(&canary_summary(&clean), &["go-claim".to_string()]).is_ok());
    // M2: any manual intervention fails the unattended gate.
    assert!(canary_gate(&canary_summary(&dirty), &["go-claim".to_string()]).is_err());
}

#[test]
fn all_fault_stations_must_be_visited() {
    let all: Vec<CanaryEvent> = STATIONS
        .iter()
        .map(|s| ev("runtime:orch", "auto", s))
        .collect();
    let required: Vec<String> = STATIONS.iter().map(|s| s.to_string()).collect();
    assert!(canary_gate(&canary_summary(&all), &required).is_ok());
    // M3: skipping a station (here: chain-exhausted) fails the gate loudly.
    let partial: Vec<CanaryEvent> = STATIONS[..5]
        .iter()
        .map(|s| ev("runtime:orch", "auto", s))
        .collect();
    assert!(canary_gate(&canary_summary(&partial), &required).is_err());
}
