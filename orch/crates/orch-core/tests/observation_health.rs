//! B245 · 观测合同 v0：ModuleObservationV0 + 纯 HealthReducer（研究包卡 1）。
//!
//! 首红形态：**compile** —— `orch_core::observation` 模块尚不存在（`error[E0432]`）。
//! 种子字节冻结；改种子 = 整棒 FAIL。
//!
//! 合同边界（研究包冻结判断，本种子逐条钉住可机检的那部分）：
//!   - metadata-only：观测不携带 prompt/response/header/路径正文（结构上无处可放）。
//!   - 纯函数：reducer 无 IO（orch-core 是 no-IO 域，审查者源级核对——种子只测语义）。
//!   - 健康不是布尔：operational state / health level / freshness 三正交。
//!   - 证据缺失/过期/冲突 ⇒ Unknown；observer 停用 ⇒ Disabled（不冒充 Unknown）。
//!   - 请求成功 ≠ 任务成功：本合同不产生任何 verdict/调度输入。
//!
//! 用例 ↔ 验收映射：
//!   M1 fresh_inflight_heartbeat_folds_running_ok —— r65 zcode-xhigh 误判钉死：
//!      心跳在跳且 inflight=true、正文零输出 ⇒ running/ok，绝不允许折成告警
//!   M2 unbound_evidence_is_quarantined           —— unbound 证据不得改变 agent 健康
//!   M3 stale_evidence_folds_unknown              —— freshness 过期 ⇒ Unknown + stale
//!   M4 duplicate_observation_id_is_idempotent    —— 同源重放幂等
//!   M5 conflicting_bound_terminals_fold_unknown  —— 证据冲突 ⇒ Unknown（不选边）
//!   M6 disabled_is_not_unknown                   —— 停用席显式 Disabled；无证据才是 Unknown
//!
//! 时间口径：观测与 now 都用 epoch 秒（i64）。M1 的 10s 必须判 fresh、M3 的 7200s
//! 必须判 stale ⇒ 库缺省新鲜视界必须落在 (10, 7200) 秒开区间内，具体值不在此钉死。

use orch_core::observation::{
    reduce_health, HealthLevel, HealthPolicy, ModuleObservationV0, OperationalState,
    TerminalOutcome,
};

const NOW: i64 = 1_000_000;
const AGENT: &str = "executor-zcode";

#[test]
fn fresh_inflight_heartbeat_folds_running_ok() {
    let obs = vec![
        ModuleObservationV0::heartbeat("hb-1", AGENT, NOW - 10)
            .bound_exact("B9-A0001")
            .inflight(true),
        // 三分钟前的最后一帧正文活动：xhigh 纯推理期正文零增长，不得因此降级。
        ModuleObservationV0::activity("act-1", AGENT, NOW - 180).bound_exact("B9-A0001"),
    ];
    let health = reduce_health(AGENT, &obs, NOW, &HealthPolicy::library_default());
    assert_eq!(health.operational_state(), OperationalState::Running);
    assert_eq!(health.health_level(), HealthLevel::Ok);
    assert!(!health.is_stale(), "10s 前的心跳必须判 fresh");
}

#[test]
fn unbound_evidence_is_quarantined() {
    let obs = vec![
        ModuleObservationV0::heartbeat("hb-1", AGENT, NOW - 10)
            .bound_exact("B9-A0001")
            .inflight(true),
        // 一条绑定不上任何 attempt 的失败终态：只能进隔离计数，不得折进健康。
        ModuleObservationV0::terminal("t-1", AGENT, NOW - 5, TerminalOutcome::Failed),
    ];
    let health = reduce_health(AGENT, &obs, NOW, &HealthPolicy::library_default());
    assert_eq!(
        health.health_level(),
        HealthLevel::Ok,
        "unbound 失败证据不得改变 agent 健康（精确相关是冻结判断）"
    );
    assert_eq!(health.unbound_evidence_count(), 1, "unbound 证据必须可见（隔离计数）");
}

#[test]
fn stale_evidence_folds_unknown() {
    let obs = vec![ModuleObservationV0::activity("act-1", AGENT, NOW - 7200).bound_exact("B9-A0001")];
    let health = reduce_health(AGENT, &obs, NOW, &HealthPolicy::library_default());
    assert_eq!(health.health_level(), HealthLevel::Unknown, "过期证据只能给 Unknown");
    assert!(health.is_stale());
}

#[test]
fn duplicate_observation_id_is_idempotent() {
    let single = vec![ModuleObservationV0::heartbeat("hb-1", AGENT, NOW - 10)
        .bound_exact("B9-A0001")
        .inflight(true)];
    let doubled = vec![
        ModuleObservationV0::heartbeat("hb-1", AGENT, NOW - 10)
            .bound_exact("B9-A0001")
            .inflight(true),
        ModuleObservationV0::heartbeat("hb-1", AGENT, NOW - 10)
            .bound_exact("B9-A0001")
            .inflight(true),
    ];
    let a = reduce_health(AGENT, &single, NOW, &HealthPolicy::library_default());
    let b = reduce_health(AGENT, &doubled, NOW, &HealthPolicy::library_default());
    assert_eq!(a.operational_state(), b.operational_state(), "同 id 重放必须幂等");
    assert_eq!(a.health_level(), b.health_level());
    assert_eq!(a.unbound_evidence_count(), b.unbound_evidence_count());
}

#[test]
fn conflicting_bound_terminals_fold_unknown() {
    let obs = vec![
        ModuleObservationV0::terminal("t-ok", AGENT, NOW - 20, TerminalOutcome::Ok)
            .bound_exact("B9-A0001"),
        ModuleObservationV0::terminal("t-fail", AGENT, NOW - 10, TerminalOutcome::Failed)
            .bound_exact("B9-A0001"),
    ];
    let health = reduce_health(AGENT, &obs, NOW, &HealthPolicy::library_default());
    assert_eq!(
        health.health_level(),
        HealthLevel::Unknown,
        "同 attempt 的冲突终态不得选边——冲突即 Unknown，留给人裁"
    );
}

#[test]
fn disabled_is_not_unknown() {
    let disabled = HealthPolicy::library_default().mark_disabled("executor-claw2");
    let health = reduce_health("executor-claw2", &[], NOW, &disabled);
    assert_eq!(
        health.operational_state(),
        OperationalState::Disabled,
        "停用席必须显式 Disabled，不得冒充 Unknown（观测覆盖率口径）"
    );
    // 对照：未标记停用且零证据 ⇒ 才是 Unknown。
    let bare = reduce_health("executor-claw2", &[], NOW, &HealthPolicy::library_default());
    assert_eq!(bare.operational_state(), OperationalState::Unknown);
    assert_eq!(bare.health_level(), HealthLevel::Unknown);
}
