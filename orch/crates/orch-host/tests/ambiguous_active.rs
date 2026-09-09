//! B132: manual reassignment still requires confirmed death and no backend writes.
//! The daemon monitor/alarm interface retired with serve.

use orch_host::tierf::reassignment_gate;



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
