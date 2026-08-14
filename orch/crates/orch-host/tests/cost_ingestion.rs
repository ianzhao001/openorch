//! B101 契约种子 · 成本/token 入账、计价表与 envDecl 回归修复（r45）
//!
//! 落位：`orch/crates/orch-host/tests/cost_ingestion.rs`（逐字节搬运，禁止修改）
//! 预期红形态：**compile**（`orch_host::pricing::*` 与 `cost::*` 新项尚不存在 → E0432/E0425）
//!
//! ## 契约
//!
//! ### 1. 计价（`pricing.rs`）
//! `estimate_cost(table, model, usage) -> CostEstimate{usd: Option<f64>, basis}`
//!   - 表内命中且 `billing: usage` → `basis = Known`，usd = 按单价算出
//!   - 表内命中且 `billing: subscription` → `basis = Subscription`，**usd = None**（不计增量）
//!   - 表内未命中 → `basis = Unknown`，**usd = None**（**绝不用默认单价估算**）
//!   - 空表（`pricing.yaml` 不存在）→ 一律 Unknown，不得报错
//!
//! ### 2. verifier 流消费（`verify.rs` → `cost.rs`）
//! `parse_verify_usage(jsonl) -> VerifyUsage{total_cost_usd, per_model[], rate_limit}`
//!   - 取**最后一条** `{"type":"result"}` 的 `total_cost_usd` 与 `modelUsage`（per-model
//!     inputTokens/outputTokens/cacheReadInputTokens/costUSD）
//!   - 取**最后一条** `{"type":"rate_limit_event"}` 的 `utilization`/`status`
//!   - 无 result 行 → 全 None（诚实缺省，不得填 0）
//!
//! ### 3. envDecl 回归（`tierf.rs` 定点）
//! `report_observed_payload_has_env_decl(payload) -> bool`（cost.rs 侧的判据函数）
//!   - B97 的 `da4b656` 删掉了 `ReportObserved.payload.envDecl`，读取侧 `cost::task_model`
//!     因此对新事件恒 None。修回后 `task_model` 必须能拿到自报模型。
//!
//! ## 负向变异清单（REPORT §5 逐条自证）
//! 1. 未命中时用默认单价估算 → `unknown_model_never_estimates` 红
//! 2. 把 subscription 计入增量金额 → `subscription_is_not_incremental` 红
//! 3. verify 只取 total_cost_usd、丢 modelUsage → `parses_per_model_usage` 红
//! 4. 无 result 行时把 token 填 0 → `missing_result_is_none_not_zero` 红
//! 5. 去掉 envDecl 回填 → `env_decl_round_trips_to_task_model` 红

use orch_core::EventRecord;
use orch_host::cost::{parse_verify_usage, task_model};
use orch_host::pricing::{estimate_cost, CostBasis, PricingTable, TokenUsage};

fn table() -> PricingTable {
    // 形如 coordination/pricing.yaml：model → 单价与计费方式
    PricingTable::from_yaml_str(
        r#"
models:
  claude-sonnet-5:
    billing: usage
    inputPerMTok: 3.0
    outputPerMTok: 15.0
  gpt-5.6-sol:
    billing: subscription
"#,
    )
    .expect("解析计价表失败")
}

fn usage(input: u64, output: u64) -> TokenUsage {
    TokenUsage {
        input,
        output,
        cache_read: 0,
        cache_write: 0,
    }
}

#[test]
fn known_model_computes_usd() {
    let est = estimate_cost(&table(), "claude-sonnet-5", &usage(1_000_000, 1_000_000));
    assert_eq!(est.basis, CostBasis::Known);
    let usd = est.usd.expect("命中表内 usage 计费必须给出金额");
    assert!((usd - 18.0).abs() < 1e-6, "3.0 + 15.0 = 18.0，实际 {usd}");
}

#[test]
fn unknown_model_never_estimates() {
    let est = estimate_cost(&table(), "some-unlisted-model", &usage(1_000_000, 1_000_000));
    assert_eq!(est.basis, CostBasis::Unknown, "未命中必须标 Unknown");
    assert!(
        est.usd.is_none(),
        "未命中一律不给金额——绝不用默认单价估算冒充"
    );
}

#[test]
fn subscription_is_not_incremental() {
    let est = estimate_cost(&table(), "gpt-5.6-sol", &usage(9_000_000, 9_000_000));
    assert_eq!(est.basis, CostBasis::Subscription);
    assert!(
        est.usd.is_none(),
        "包月制不产生增量金额，不得按 token 折算成美元"
    );
}

#[test]
fn empty_table_is_unknown_not_error() {
    let empty = PricingTable::from_yaml_str("").expect("空表必须可解析（计价表缺失时面板照常工作）");
    let est = estimate_cost(&empty, "claude-sonnet-5", &usage(1000, 1000));
    assert_eq!(est.basis, CostBasis::Unknown);
    assert!(est.usd.is_none());
}

#[test]
fn parses_per_model_usage() {
    let jsonl = concat!(
        r#"{"type":"system","subtype":"init","model":"claude-sonnet-5"}"#,
        "\n",
        r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","utilization":0.9}}"#,
        "\n",
        r#"{"type":"result","total_cost_usd":2.06,"modelUsage":{"claude-sonnet-5":{"inputTokens":2985,"outputTokens":25893,"cacheReadInputTokens":4413498,"costUSD":2.371}}}"#,
        "\n"
    );
    let parsed = parse_verify_usage(jsonl);
    assert_eq!(parsed.total_cost_usd, Some(2.06));
    assert_eq!(parsed.per_model.len(), 1, "modelUsage 必须被折出，不能只取 total_cost_usd");
    let entry = &parsed.per_model[0];
    assert_eq!(entry.model, "claude-sonnet-5");
    assert_eq!(entry.usage.input, 2985);
    assert_eq!(entry.usage.output, 25893);
    assert_eq!(entry.usage.cache_read, 4_413_498);
    let rl = parsed.rate_limit.expect("rate_limit_event 必须被采集，用于配额告警");
    assert!((rl.utilization - 0.9).abs() < 1e-9);
}

#[test]
fn missing_result_is_none_not_zero() {
    let parsed = parse_verify_usage(r#"{"type":"system","subtype":"init"}"#);
    assert!(parsed.total_cost_usd.is_none(), "缺 result 行必须 None，不得填 0");
    assert!(parsed.per_model.is_empty());
    assert!(parsed.rate_limit.is_none());
}

#[test]
fn env_decl_round_trips_to_task_model() {
    // 模拟修回后的 ReportObserved：payload 必须重新带上 envDecl（B97 da4b656 曾删除）
    let observed = EventRecord {
        event_id: "EV-1".to_string(),
        ts: "2026-07-26T00:00:00Z".to_string(),
        actor: "runtime:orch".to_string(),
        kind: "ReportObserved".to_string(),
        task_id: Some("B101".to_string()),
        round: Some("r45".to_string()),
        payload: Some(serde_json::json!({
            "actionId": "report-observed",
            "attemptId": "B101-A0001",
            "reportPath": "coordination/rounds/r45/reports/B101-REPORT.md",
            "envDecl": {"model": "gpt-5.6-sol", "depth": "high", "capture": "self-report"}
        })),
        extra: serde_json::Map::new(),
    };
    assert_eq!(
        task_model(&[observed], "B101").as_deref(),
        Some("gpt-5.6-sol"),
        "envDecl 必须重新入账，否则面板拿不到「当前模型/思考深度」"
    );
}
