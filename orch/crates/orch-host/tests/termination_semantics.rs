//! B133 seeded-red contract.
//!
//! Negative mutations that must turn the named case red:
//! M1. Treat a WakeLogProxy provider (SmartClaw) as killable by process group
//!     and return SignalProcessGroup for it.
//! M2. Let the WIP archive gate pass while the process tree is unconfirmed or
//!     the wake log is still advancing (archiving a live attempt's worktree).
//! M3. Give an unknown provider topology a default termination plan instead of
//!     failing closed.

use orch_host::tierf::wip_archive_gate;
use orch_host::wake::{termination_plan, TerminationPlan};

fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn termination_plan_is_typed_by_topology() {
    // Managed CLI providers own their process group: TERM -> grace -> KILL.
    assert_eq!(
        termination_plan(&argv(&["codex", "exec"])).unwrap(),
        TerminationPlan::SignalProcessGroup
    );
    assert_eq!(
        termination_plan(&argv(&["opencode", "run"])).unwrap(),
        TerminationPlan::SignalProcessGroup
    );
    // M1: SmartClaw's daemon is out of reach — the only honest plan is to wait
    // for wake-log quiescence; socket cancel is explicitly out of scope (H16).
    assert_eq!(
        termination_plan(&argv(&["sh", "wake-multica.sh"])).unwrap(),
        TerminationPlan::LogQuiesceOnly
    );
}

#[test]
fn wip_archive_gate_fails_closed() {
    // Legal shapes only: pid-group side needs a confirmed-dead tree and a
    // quiet log; log-quiesce side needs a quiet log and no known-alive tree.
    assert!(wip_archive_gate(false, Some(true), false).is_ok());
    assert!(wip_archive_gate(true, None, false).is_ok());

    // M2: unconfirmed tree, advancing log, or known-alive tree all block.
    assert!(wip_archive_gate(false, None, false).is_err());
    assert!(wip_archive_gate(false, Some(true), true).is_err());
    assert!(wip_archive_gate(true, None, true).is_err());
    assert!(wip_archive_gate(true, Some(false), false).is_err());
    assert!(wip_archive_gate(false, Some(false), false).is_err());
}

#[test]
fn unknown_topology_termination_is_err() {
    // M3: no default plan for unmodeled providers (agy, ad-hoc adapters).
    assert!(termination_plan(&argv(&["agy", "run"])).is_err());
    assert!(termination_plan(&argv(&["echo", "hi"])).is_err());
    assert!(termination_plan(&argv(&[])).is_err());
}
