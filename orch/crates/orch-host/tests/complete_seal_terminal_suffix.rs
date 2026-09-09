//! B313 红种子 · complete seal replay 的 post-TaskRecorded managed terminal。
//!
//! 首红必须是 compile `E0432`：窄 suffix validator 与合同锚尚不存在。
//! 该 helper 必须由 `validate_expected_main_contract` 的
//! `CanonicalPostMergeSuffix` 分支消费；完整 seal replay 放同卡 runtime 测试。
//!
//! 决定性变异：
//! - M1 任意 `ManagedWakeTerminated` 都放行；
//! - M2 terminal 没有唯一先行 exact `WakeIssued` 仍放行；
//! - M3 terminal 位于 target 的唯一 `TaskRecorded` 之前、只有 other-task Recorded 锚或跨 round
//!   仍放行；
//! - M4 actor、wakeId、agent 或 task envelope 漂移仍放行；
//! - M5 同 wakeId 已有 terminal 后再次 replay 仍放行；
//! - M6 有 lease 时缺/错 release 仍放行，或把许可扩大到其它 mode。

use orch_core::EventRecord;
use orch_host::ledger;
use orch_host::verify::{
    canonical_complete_seal_managed_terminal_suffix_v1, COMPLETE_SEAL_TERMINAL_SUFFIX_CONTRACT_V1,
};

const GUIDE: &str = include_str!("../../../docs/AI-MECHANICAL-GUIDE.md");
const ROUND: &str = "r82";
const TASK: &str = "B313";
const WAKE: &str = "01a0b313-1111-4222-8333-444455556666";
const AGENT: &str = "executor-desktop";

fn wake() -> EventRecord {
    ledger::event(
        "WakeIssued",
        "runtime:orch",
        Some(TASK),
        Some(ROUND),
        serde_json::json!({
            "wakeId": WAKE,
            "agent": AGENT,
            "controlWakeId": WAKE,
        }),
    )
}

fn recorded(task: &str) -> EventRecord {
    ledger::event(
        "TaskRecorded",
        "runtime:orch",
        Some(task),
        Some(ROUND),
        serde_json::json!({"postMergeGates": "all-green"}),
    )
}

fn leased() -> EventRecord {
    ledger::event(
        "WorkspaceLeased",
        "runtime:orch",
        Some(TASK),
        Some(ROUND),
        serde_json::json!({
            "siteId": "B313-implement-executor-desktop-g01",
            "generation": 1,
            "attemptId": "B313-A0001",
            "role": "implement",
            "agent": AGENT,
            "reviewedHead": "1111111111111111111111111111111111111111",
            "wakeId": WAKE,
            "paths": {
                "worktree": ".worktrees/B313",
                "target": ".worktrees/B313/orch/target"
            }
        }),
    )
}

fn terminal() -> EventRecord {
    ledger::event(
        "ManagedWakeTerminated",
        "runtime:orch",
        Some(TASK),
        Some(ROUND),
        serde_json::json!({
            "wakeId": WAKE,
            "agent": AGENT,
            "completionReason": "natural-exit",
            "terminalSeen": false,
            "exitedNaturally": true,
            "hardDeadlineReached": false,
            "cancelRequestId": null,
            "signals": [],
            "managedScopeTerminated": true,
            "logBytesRead": 41,
            "outcomeClass": "TruncatedNoTerminal",
            "processTreeTerminated": false,
        }),
    )
}

#[test]
fn contract_anchor_is_version_one() {
    assert_eq!(COMPLETE_SEAL_TERMINAL_SUFFIX_CONTRACT_V1, 1);
}

#[test]
fn exact_terminal_after_task_recorded_is_authorized() {
    assert!(canonical_complete_seal_managed_terminal_suffix_v1(
        &[wake(), recorded(TASK)],
        &[terminal()],
        ROUND,
        TASK,
    )
    .unwrap());
}

#[test]
fn leased_terminal_requires_its_exact_adjacent_release() {
    let prior = [wake(), leased(), recorded(TASK)];
    let terminal = terminal();
    let release = orch_host::sites::workspace_release_for_termination(&prior, ROUND, &terminal)
        .unwrap()
        .expect("leased terminal produces one release");
    assert!(canonical_complete_seal_managed_terminal_suffix_v1(
        &prior,
        &[terminal.clone(), release],
        ROUND,
        TASK,
    )
    .unwrap());
    assert!(
        !canonical_complete_seal_managed_terminal_suffix_v1(&prior, &[terminal], ROUND, TASK,)
            .unwrap()
    );
}

#[test]
fn terminal_before_record_or_without_its_wake_is_refused() {
    assert!(!canonical_complete_seal_managed_terminal_suffix_v1(
        &[wake()],
        &[terminal()],
        ROUND,
        TASK,
    )
    .unwrap());
    assert!(!canonical_complete_seal_managed_terminal_suffix_v1(
        &[recorded(TASK)],
        &[terminal()],
        ROUND,
        TASK,
    )
    .unwrap());
}

#[test]
fn other_task_recorded_or_duplicate_target_anchor_is_refused() {
    assert!(!canonical_complete_seal_managed_terminal_suffix_v1(
        &[wake(), recorded("B-other")],
        &[terminal()],
        ROUND,
        TASK,
    )
    .unwrap());

    assert!(!canonical_complete_seal_managed_terminal_suffix_v1(
        &[wake(), recorded(TASK), recorded(TASK)],
        &[terminal()],
        ROUND,
        TASK,
    )
    .unwrap());
}

#[test]
fn identity_or_round_drift_is_refused() {
    let visible = [wake(), recorded(TASK)];
    let mut forged_actor = terminal();
    forged_actor.actor = "planner".to_string();
    assert!(!canonical_complete_seal_managed_terminal_suffix_v1(
        &visible,
        &[forged_actor],
        ROUND,
        TASK,
    )
    .unwrap());

    let mut cross_round = terminal();
    cross_round.round = Some("r81".to_string());
    assert!(!canonical_complete_seal_managed_terminal_suffix_v1(
        &visible,
        &[cross_round],
        ROUND,
        TASK,
    )
    .unwrap());

    let mut wrong_wake = terminal();
    wrong_wake.payload.as_mut().unwrap()["wakeId"] =
        serde_json::json!("01a0b313-7777-4888-8999-aaaabbbbcccc");
    assert!(!canonical_complete_seal_managed_terminal_suffix_v1(
        &visible,
        &[wrong_wake],
        ROUND,
        TASK,
    )
    .unwrap());
}

#[test]
fn duplicate_terminal_for_one_wake_is_refused() {
    let first = terminal();
    assert!(!canonical_complete_seal_managed_terminal_suffix_v1(
        &[wake(), recorded(TASK), first],
        &[terminal()],
        ROUND,
        TASK,
    )
    .unwrap());
}

#[test]
fn guide_documents_the_complete_seal_replay_boundary() {
    assert!(GUIDE.contains("orch-guide-seal:post-recorded-managed-terminal"));
    assert!(GUIDE.contains("complete seal"));
    assert!(GUIDE.contains("ManagedWakeTerminated"));
}
