//! B312 红种子 · historical Panel + current legacy/Quorum accounting recovery。
//!
//! 首红必须是 compile `E0432`：V2 accounting recovery 合同尚不存在。
//! 本种子冻结纯分类边界；真实 `refs/heads/main` 前缀、artifact 与 scoped accounting commit
//! 由同卡 runtime 测试覆盖。
//!
//! 决定性变异：
//! - M1 只因历史出现过 Panel 就把当前 legacy singleton 喂给 Panel batch parser；
//! - M2 legacy `ReviewDelivered` 不核对唯一先行 request / task / attempt / role / agent；
//! - M3 带 panelId 或未知键的伪 legacy singleton 被接纳；
//! - M4 Panel 三连批缺 promotion、delivery 或 seat-terminal 任一项仍落稳；
//! - M5 malformed/partial suffix 先提交一部分再报错；
//! - M6 recovery 用普通 git commit 或全仓提交替代既有 scoped accounting commit。

use anyhow::Result;
use orch_core::EventRecord;
use orch_host::ledger;
use orch_host::legacy::{
    classify_review_accounting_suffix_v1, ReviewAccountingSuffixKindV1,
    REVIEW_ACCOUNTING_RECOVERY_CONTRACT_V2,
};

const GUIDE: &str = include_str!("../../../docs/AI-MECHANICAL-GUIDE.md");
const ROUND: &str = "r82";
const TASK: &str = "B313";
const ATTEMPT: &str = "B313-A0001";
const AGENT: &str = "executor-claw";

fn request() -> EventRecord {
    ledger::event(
        "ReviewRequested",
        "runtime:orch",
        Some(TASK),
        Some(ROUND),
        serde_json::json!({
            "attemptId": ATTEMPT,
            "role": "primary",
            "agent": AGENT,
            "wakeId": "01a0b312-1111-4222-8333-444455556666",
            "continuationId": "review:r82:B313:B313-A0001:primary:executor-claw",
            "reviewedHead": "1111111111111111111111111111111111111111",
        }),
    )
}

fn delivery() -> EventRecord {
    ledger::event(
        "ReviewDelivered",
        "runtime:orch",
        Some(TASK),
        Some(ROUND),
        serde_json::json!({
            "attemptId": ATTEMPT,
            "role": "primary",
            "agent": AGENT,
            "bodyLen": 17,
        }),
    )
}

fn rejected(result: Result<Option<ReviewAccountingSuffixKindV1>>) -> bool {
    result.is_err() || result.ok().flatten().is_none()
}

#[test]
fn contract_anchor_is_version_two() {
    assert_eq!(REVIEW_ACCOUNTING_RECOVERY_CONTRACT_V2, 2);
}

#[test]
fn exact_legacy_singleton_is_classified_outside_panel_batches() {
    assert_eq!(
        classify_review_accounting_suffix_v1(&[request()], &[delivery()], ROUND).unwrap(),
        Some(ReviewAccountingSuffixKindV1::LegacySingleton)
    );
}

#[test]
fn legacy_singleton_requires_one_exact_preceding_request() {
    let req = request();
    assert!(rejected(classify_review_accounting_suffix_v1(
        &[],
        &[delivery()],
        ROUND
    )));
    assert!(rejected(classify_review_accounting_suffix_v1(
        &[req.clone(), req],
        &[delivery()],
        ROUND
    )));
    let mut cross_round = delivery();
    cross_round.round = Some("r81".to_string());
    assert!(rejected(classify_review_accounting_suffix_v1(
        &[request()],
        &[cross_round],
        ROUND
    )));
}

#[test]
fn panel_identity_cannot_hide_inside_a_legacy_singleton() {
    let mut smuggled = delivery();
    smuggled.payload.as_mut().unwrap()["panelId"] = serde_json::json!("panel-forged");
    assert!(rejected(classify_review_accounting_suffix_v1(
        &[request()],
        &[smuggled],
        ROUND
    )));
}

#[test]
fn partial_panel_shape_is_never_a_recoverable_batch() {
    let promotion = ledger::event(
        "ReviewSpoolPromoted",
        "runtime:orch",
        Some(TASK),
        Some(ROUND),
        serde_json::json!({}),
    );
    assert!(rejected(classify_review_accounting_suffix_v1(
        &[],
        &[promotion, delivery()],
        ROUND
    )));
}

#[test]
fn guide_documents_legacy_and_panel_suffixes_as_read_only_classification() {
    assert!(GUIDE.contains("orch-guide-review:accounting-suffix-recovery"));
    assert!(GUIDE.contains("legacy singleton"));
    assert!(GUIDE.contains("只读分类"));
    assert!(GUIDE.contains("不再授权 recovery writer"));
}
