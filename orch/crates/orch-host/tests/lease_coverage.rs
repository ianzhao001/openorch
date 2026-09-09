//! B134: preserve the fresh in-lock termination recheck.
//! Cross-round append refusal is exercised through real storage-audit append in ledger::manual_round_scope_tests.

use orch_host::tierf::termination_commit_gate;



#[test]
fn termination_commit_gate_requires_fresh_recheck() {
    // The only legal commit shape: evidence says terminated AND the
    // in-closure recheck confirms the group is not alive.
    assert!(termination_commit_gate(true, Some(false)).is_ok());

    // M3: no recheck, a revived group, or unterminated evidence all refuse.
    assert!(termination_commit_gate(true, None).is_err());
    assert!(termination_commit_gate(true, Some(true)).is_err());
    assert!(termination_commit_gate(false, Some(false)).is_err());
}
