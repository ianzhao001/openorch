//! B181 seed — detached wake-owner lifecycle contract.
//!
//! The durable worker launched by `orch wake` must outlive cancellation of the
//! initiating CLI without becoming an unowned zombie.  This seed freezes the
//! public planning seam shared by the production spawn paths; real SIGINT and
//! SIGHUP behavior is covered by the production integration tests required by
//! the task card.
//!
//! Negative mutations that the completed task must prove:
//! M1: put the SmartClaw WakeLogProxy back in the parent orch process group.
//! M2: isolate only the managed provider, not the hidden supervisor itself.
//! M3: spawn before the reaper is armed, or drop the exact Child after spawn.
//! M4: hand the managed supervisor to the reaper before its ack is validated.

use orch_host::wake::{
    wake_owner_lifecycle_plan, WakeOwnerHandoff, WakeOwnerKind, WakeOwnerLifecyclePlan,
};

fn assert_isolated_and_owned(plan: &WakeOwnerLifecyclePlan) {
    assert!(
        plan.isolate_process_group,
        "every long-lived wake owner must leave the initiating orch process group"
    );
    assert!(
        plan.prearm_reaper,
        "the exact Child owner must exist before the irreversible spawn"
    );
}

#[test]
fn managed_hidden_supervisor_is_isolated_and_handed_off_only_after_ack() {
    let plan = wake_owner_lifecycle_plan(WakeOwnerKind::ManagedSupervisor);
    assert_isolated_and_owned(&plan);
    assert_eq!(plan.handoff, WakeOwnerHandoff::AfterSupervisorAck);
}

#[test]
fn wake_log_proxy_is_isolated_and_handed_off_immediately_after_spawn() {
    let plan = wake_owner_lifecycle_plan(WakeOwnerKind::WakeLogProxy);
    assert_isolated_and_owned(&plan);
    assert_eq!(plan.handoff, WakeOwnerHandoff::ImmediatelyAfterSpawn);
}

#[test]
fn neither_supported_owner_role_may_fall_back_to_drop_child() {
    for kind in [
        WakeOwnerKind::ManagedSupervisor,
        WakeOwnerKind::WakeLogProxy,
    ] {
        let plan = wake_owner_lifecycle_plan(kind);
        assert_isolated_and_owned(&plan);
    }
}
