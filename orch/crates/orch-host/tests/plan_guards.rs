//! B156 seeded-red contract (H23 + H24 + H26 + H27).
//!
//! Negative mutations that must turn the named case red:
//! M1. Let a budgets-only edit change the contract digest — the whole point of
//!     H27 is that a runtime resource ceiling is not contract content, so an
//!     in-flight authorization chain must survive it.
//! M2. Let `replan_invalidation_report` clear an attempt on `ReportObserved`
//!     (or any non-terminal event). A delivered REPORT does not end the
//!     attempt; its chain is still live and a replan would still void it.
//! M3. Apply the scheduling rules to already-dispatched cards (today's
//!     `for task in tasks`), which is what makes a mid-round rule change
//!     impossible; or skip them for undispatched cards, which would let a
//!     non-compliant card through.
//! M4. Accept an `entryPoints` set that is empty, or that names a path the
//!     card froze — B155 died exactly there.

use orch_host::card::entry_points_writable;
use orch_host::plan::{contract_digest_of_mode, replan_invalidation_report, scheduling_rules_apply};

const MODE_BASE: &str = "\
apiVersion: orch/v1alpha1
kind: ModeConfig
metadata: {name: t}
preset: relay
objective: \"t\"
agents:
  planner: {adapter: root, tier: none}
  executor: {adapter: codex-desktop, tier: F, agentId: executor-desktop}
  verifier: {adapter: root-manual, tier: none}
hitl: {planSignoff: required, mergeGate: auto}
verification: {mode: root-manual-fixed-head}
liveness: {monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}
scheduling:
  allowedAgents: [executor-desktop]
  capacities:
    executor-desktop: {agent: 4, quota: 4, roles: [implement]}
budgets:
  round: {maxUsd: 12, wallMinutes: 800, maxModelWakes: 30}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
";

fn with_budget(wakes: u32) -> String {
    MODE_BASE.replace("maxModelWakes: 30", &format!("maxModelWakes: {wakes}"))
}

fn v(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// Minimal ledger line helper: the report only needs kind/taskId/payload.
fn ev(kind: &str, task: &str, attempt: &str, agent: &str) -> String {
    format!(
        r#"{{"eventId":"e-{kind}-{task}-{attempt}","ts":"2026-07-28T00:00:00Z","type":"{kind}","actor":"runtime:orch","taskId":"{task}","round":"r53","payload":{{"attemptId":"{attempt}","agent":"{agent}"}}}}"#
    )
}

#[test]
fn budget_only_edit_keeps_the_contract_digest() {
    // M1: the authorization chain binds contract content, not file bytes.
    let base = contract_digest_of_mode(MODE_BASE).expect("base mode parses");
    let raised = contract_digest_of_mode(&with_budget(45)).expect("raised mode parses");
    assert_eq!(
        base, raised,
        "raising maxModelWakes must not move the contract digest"
    );

    // Comments and blank lines are not contract content either.
    let commented = format!("# a comment\n{MODE_BASE}\n");
    assert_eq!(base, contract_digest_of_mode(&commented).expect("parses"));

    // But a real contract change must move it.
    let tightened = MODE_BASE.replace("roles: [implement]", "roles: [primary-review]");
    assert_ne!(
        base,
        contract_digest_of_mode(&tightened).expect("parses"),
        "a scheduling change is contract content and must move the digest"
    );
}

#[test]
fn replan_report_names_every_live_attempt() {
    // M2: dispatched-and-not-terminal is the whole criterion. A delivered
    // REPORT does not release the attempt.
    let delivered = [
        ev("DispatchIssued", "B900", "B900-A0001", "executor-desktop"),
        ev("ReportObserved", "B900", "B900-A0001", "executor-desktop"),
    ]
    .join("\n");
    let live = replan_invalidation_report(&delivered, "r53").expect("readable ledger");
    assert_eq!(live.len(), 1, "REPORT delivered != attempt terminated");
    assert!(live[0].contains("B900") && live[0].contains("B900-A0001"));
    assert!(live[0].contains("executor-desktop"), "must name the agent");

    // Terminal events do release it.
    for terminal in [
        "TaskRecorded",
        "AttemptBlocked",
        "AttemptCrashed",
        "AttemptTimedOut",
        "AttemptFailed",
    ] {
        let closed = format!(
            "{}\n{}",
            delivered,
            ev(terminal, "B900", "B900-A0001", "executor-desktop")
        );
        assert!(
            replan_invalidation_report(&closed, "r53")
                .expect("readable")
                .is_empty(),
            "{terminal} must end the attempt"
        );
    }

    // Fail closed: an unreadable ledger must never report "nothing in flight".
    assert!(replan_invalidation_report("{not json", "r53").is_err());
}

#[test]
fn scheduling_rules_bind_undispatched_cards_only() {
    // M3: rules bind at dispatch time. Already-dispatched cards keep the
    // deal they were dispatched under; undispatched cards face the new table.
    let dispatched = ev("DispatchIssued", "B900", "B900-A0001", "executor-desktop");
    assert!(
        !scheduling_rules_apply(&dispatched, "r53", "B900").expect("readable"),
        "an already-dispatched card must not be re-judged by a tightened table"
    );
    assert!(
        scheduling_rules_apply(&dispatched, "r53", "B901").expect("readable"),
        "an undispatched card must be judged by the current table"
    );
    assert!(scheduling_rules_apply("{not json", "r53", "B901").is_err());
}

#[test]
fn entry_points_must_be_writable_and_declared() {
    // M4: this is the B155 failure mode, mechanised.
    let write_set = v(&[
        "orch/crates/orch-host/src/serve.rs",
        "orch/crates/orch-host/src/tierf.rs",
    ]);
    let frozen = v(&["orch/crates/orch-host/src/wake.rs", "orch/crates/orch-cli/**"]);

    // Empty is refused: every card must say where its acceptance is wired.
    assert!(entry_points_writable(&[], &write_set, &frozen).is_err());

    // The B155 card verbatim: the acceptance needed wake.rs, which it froze.
    let err = entry_points_writable(
        &v(&["orch/crates/orch-host/src/wake.rs"]),
        &write_set,
        &frozen,
    )
    .unwrap_err();
    assert!(err.contains("wake.rs"), "must name the offending path");

    // Declared but not writable is refused too.
    assert!(entry_points_writable(
        &v(&["orch/crates/orch-host/src/ledger.rs"]),
        &write_set,
        &frozen
    )
    .is_err());

    // A card whose entry points are inside its writeSet is fine.
    assert!(entry_points_writable(
        &v(&["orch/crates/orch-host/src/serve.rs"]),
        &write_set,
        &frozen
    )
    .is_ok());
}
