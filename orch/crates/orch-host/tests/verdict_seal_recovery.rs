//! B204 · verdict→seal 恢复出口合同。
//!
//! 首红为 compile：四个生产判据尚不存在。它们必须接入真实 CLI/hook/review-delivery
//! 路径，不能只做满足本 seed 的旁路纯函数。
//!
//! M1. seal 成功判据漏数或只 print 不 Err；
//! M2. 未合并 root PASS 时仍允许普通 main fast-forward；
//! M3. approved 重铸无需显式开关/理由，或已有 MergeStarted 仍可重铸；
//! M4. verdict 后的同 key review 仍返回 canonical 路径或覆盖既有 late 文件。

use orch_host::attempt::approved_reattempt_next;
use orch_host::close::validate_seal_postcondition;
use orch_host::hooks::{verdict_barrier_guard_verdict, GuardVerdict};
use orch_host::ledger;
use orch_host::wake::review_delivery_relpath;

const ROUND: &str = "r62";
const TASK: &str = "B204";
const ATTEMPT: &str = "B204-A0001";

fn event(kind: &str, payload: serde_json::Value) -> orch_core::EventRecord {
    ledger::event(kind, "runtime:orch", Some(TASK), Some(ROUND), payload)
}

fn successful_chain() -> Vec<orch_core::EventRecord> {
    vec![
        event(
            "VerdictIssued",
            serde_json::json!({"attemptId": ATTEMPT, "verdict": "PASS"}),
        ),
        event(
            "MergeStarted",
            serde_json::json!({"attemptId": ATTEMPT, "verdictEventId": "verdict"}),
        ),
        event(
            "MergeExecuted",
            serde_json::json!({"mergeSha": "1111111111111111111111111111111111111111"}),
        ),
        event(
            "TaskRecorded",
            serde_json::json!({"postMergeGates": "all-green"}),
        ),
    ]
}

#[test]
fn seal_requires_one_complete_ordered_lifecycle_chain() {
    validate_seal_postcondition(&successful_chain(), ROUND, TASK, ATTEMPT).unwrap();

    for missing in ["VerdictIssued", "MergeStarted", "MergeExecuted", "TaskRecorded"] {
        let events = successful_chain()
            .into_iter()
            .filter(|event| event.kind != missing)
            .collect::<Vec<_>>();
        let error = validate_seal_postcondition(&events, ROUND, TASK, ATTEMPT).unwrap_err();
        assert!(format!("{error:#}").contains(missing));
    }

    let mut duplicate = successful_chain();
    duplicate.push(event(
        "MergeExecuted",
        serde_json::json!({"mergeSha": "2222222222222222222222222222222222222222"}),
    ));
    let error = validate_seal_postcondition(&duplicate, ROUND, TASK, ATTEMPT).unwrap_err();
    assert!(format!("{error:#}").contains("MergeExecuted"));

    let mut out_of_order = successful_chain();
    out_of_order.swap(1, 2);
    let error = validate_seal_postcondition(&out_of_order, ROUND, TASK, ATTEMPT).unwrap_err();
    assert!(format!("{error:#}").contains("order"));
}

#[test]
fn pending_root_pass_blocks_ordinary_main_advance_only() {
    assert!(matches!(
        verdict_barrier_guard_verdict("refs/heads/main", true, false),
        GuardVerdict::Block { .. }
    ));
    assert_eq!(
        verdict_barrier_guard_verdict("refs/heads/topic", true, false),
        GuardVerdict::Allow
    );
    assert_eq!(
        verdict_barrier_guard_verdict("refs/heads/main", true, true),
        GuardVerdict::Allow
    );
    assert_eq!(
        verdict_barrier_guard_verdict("refs/heads/main", false, false),
        GuardVerdict::Allow
    );
}

#[test]
fn approved_reattempt_is_explicit_reasoned_and_premerge_only() {
    assert_eq!(
        approved_reattempt_next(TASK, ATTEMPT, true, false, "signed main cannot be reconstructed")
            .unwrap(),
        "B204-A0002"
    );
    for (explicit, merge_started, reason) in [
        (false, false, "signed main cannot be reconstructed"),
        (true, true, "signed main cannot be reconstructed"),
        (true, false, "  "),
    ] {
        assert!(approved_reattempt_next(TASK, ATTEMPT, explicit, merge_started, reason).is_err());
    }
    assert!(approved_reattempt_next("B204", "B999-A0001", true, false, "reason").is_err());
}

#[test]
fn post_verdict_review_is_monotonic_late_never_canonical() {
    let canonical = review_delivery_relpath(
        ROUND,
        ATTEMPT,
        "primary",
        "executor-claw",
        false,
        &[],
    )
    .unwrap();
    assert_eq!(
        canonical,
        "coordination/rounds/r62/reviews/B204-A0001-primary-executor-claw.md"
    );

    let late = review_delivery_relpath(
        ROUND,
        ATTEMPT,
        "primary",
        "executor-claw",
        true,
        &[1, 3],
    )
    .unwrap();
    assert_eq!(
        late,
        "coordination/rounds/r62/reviews/B204-A0001-primary-executor-claw-late-4.md"
    );
    assert_ne!(late, canonical);
}
