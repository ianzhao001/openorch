//! B131 seeded-red contract.
//!
//! Negative mutations that must turn the named case red:
//! M1. Classify the SmartClaw socket-bridge channel as ManagedPidGroup (kill -0 on the
//!     spawn wrapper PID) instead of WakeLogProxy.
//! M2. Leave the heartbeat in the waiting phase (stale wait-script pid) after an
//!     injected wake instead of rewriting it with the spawn pid and phase=working.
//! M3. Collapse an unknown durable signal into Some(false) (or drop the deprecation
//!     note), presenting a dead wait-script PID as backend death.

use orch_host::liveness::last_signal_json;
use orch_host::wake::{durable_identity_kind, working_heartbeat_json, DurableIdentityKind};

fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn durable_identity_is_typed_by_provider_topology() {
    // P8: direct CLI children are managed pid groups.
    assert_eq!(
        durable_identity_kind(&argv(&["codex", "exec", "{message}", "--json"])).unwrap(),
        DurableIdentityKind::ManagedPidGroup
    );
    assert_eq!(
        durable_identity_kind(&argv(&["opencode", "run", "{message}"])).unwrap(),
        DurableIdentityKind::ManagedPidGroup
    );
    // P7: the SmartClaw socket bridge must never be treated as a spawn child.
    assert_eq!(
        durable_identity_kind(&argv(&["sh", "coordination/scripts/wake-multica.sh", "{message}"]))
            .unwrap(),
        DurableIdentityKind::WakeLogProxy
    );
    // Fail closed: unmodelled topologies (agy or anything unknown) are errors.
    assert!(durable_identity_kind(&argv(&["agy", "-c", "-p", "{message}"])).is_err());
    assert!(durable_identity_kind(&argv(&[])).is_err());
}

#[test]
fn injected_wake_rewrites_heartbeat_into_working_phase() {
    // P3: after a wake spawn the runtime-owned heartbeat carries the spawn pid
    // and the working phase — not the exited wait-script identity.
    let hb = working_heartbeat_json("executor-desktop", 43210, "r48", "B131-A0001");
    assert_eq!(hb["agent"], "executor-desktop");
    assert_eq!(hb["phase"], "working");
    assert_eq!(hb["pid"], 43210);
    assert_eq!(hb["round"], "r48");
    assert_eq!(hb["generation"], "B131-A0001");
    assert!(hb["ts"].is_string());
}

#[test]
fn last_signal_keeps_wait_pid_and_backend_liveness_apart() {
    // P2: a dead wait-script PID with an ambiguous backend must surface as
    // waitPidAlive=false + durableAlive=null — never as backend death.
    let ambiguous = last_signal_json(Some(false), None);
    assert_eq!(ambiguous["waitPidAlive"], false);
    assert!(ambiguous["durableAlive"].is_null());
    assert_eq!(ambiguous["pidAlive"], false);
    assert!(ambiguous["pidAliveNote"]
        .as_str()
        .unwrap()
        .contains("deprecated"));

    let backend_alive = last_signal_json(Some(false), Some(true));
    assert_eq!(backend_alive["durableAlive"], true);

    let unknown_wait = last_signal_json(None, None);
    assert!(unknown_wait["waitPidAlive"].is_null());
    assert!(unknown_wait["durableAlive"].is_null());
}
