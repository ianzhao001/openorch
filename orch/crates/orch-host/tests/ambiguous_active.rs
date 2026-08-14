//! B132 seeded-red contract.
//!
//! Negative mutations that must turn the named case red:
//! M1. Treat a Stall (or Ambiguous) monitor verdict as death and let the
//!     reassignment gate authorize a new attempt.
//! M2. Let the reassignment gate pass while the previous attempt's wake log is
//!     still advancing (double-dispatch on a live backend).
//! M3. Let an intent event (NudgeIssued/ResumeIssued) clear the liveness alarm
//!     on its own.

use orch_host::serve::{alarm_after_intent, monitor_verdict, MonitorVerdict};
use orch_host::tierf::reassignment_gate;

#[test]
fn monitor_verdict_keeps_stall_dead_and_ambiguous_apart() {
    // Confirmed-dead durable identity is the only Dead.
    assert_eq!(monitor_verdict(Some(false), 0, 600), MonitorVerdict::Dead);
    // Alive + quiet within threshold is healthy; beyond threshold is Stall.
    assert_eq!(monitor_verdict(Some(true), 30, 600), MonitorVerdict::Healthy);
    assert_eq!(monitor_verdict(Some(true), 601, 600), MonitorVerdict::Stall);
    // Unknown durable signal is Ambiguous no matter how long the silence — never Dead.
    assert_eq!(monitor_verdict(None, 30, 600), MonitorVerdict::Ambiguous);
    assert_eq!(monitor_verdict(None, 86_400, 600), MonitorVerdict::Ambiguous);
}

#[test]
fn reassignment_gate_fails_closed_on_anything_but_confirmed_death() {
    // The single legal shape: kind=dead + durable confirmed dead + quiet log
    // + process tree not known-alive.
    assert!(reassignment_gate(true, Some(false), false, Some(true)).is_ok());
    assert!(reassignment_gate(true, Some(false), false, None).is_ok());

    // M1: stall/ambiguous never authorize a new attempt.
    assert!(reassignment_gate(false, Some(false), false, Some(true)).is_err());
    assert!(reassignment_gate(true, None, false, Some(true)).is_err());
    assert!(reassignment_gate(true, Some(true), false, Some(true)).is_err());

    // M2: an advancing wake log blocks even a claimed death.
    assert!(reassignment_gate(true, Some(false), true, Some(true)).is_err());

    // A process tree known to be alive blocks reassignment.
    assert!(reassignment_gate(true, Some(false), false, Some(false)).is_err());
}

#[test]
fn intent_events_do_not_clear_the_alarm() {
    // M3: intent is not evidence of life.
    assert!(alarm_after_intent(true, "NudgeIssued"));
    assert!(alarm_after_intent(true, "ResumeIssued"));
    // Real activity clears it.
    assert!(!alarm_after_intent(true, "DispatchAcked"));
    assert!(!alarm_after_intent(true, "ReportObserved"));
    assert!(!alarm_after_intent(true, "AttemptStarted"));
    // An inactive alarm stays inactive.
    assert!(!alarm_after_intent(false, "NudgeIssued"));
}
