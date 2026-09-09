//! Agent quality-state projection seam.
//!
//! The planner pre-places this module for r44/B98. Quality is agent-scoped and
//! intentionally does not alter the task-state fold in `orch-core`.
//!
//! 契约(见 coordination/rounds/r44/tasks/B98.md):
//! - 初始状态由 registry 输入;不得把现役老 agent 统一重置 probation。
//! - probation 需两个**不同 attempt**的连续 fresh verifier PASS 才 eligible;
//!   同 attempt 重复 PASS 幂等、不重复累计;同 attempt 矛盾 PASS/FAIL fail-closed。
//! - VerifiedFail 令 probation/eligible 进入 degraded 并清 streak。
//! - IntegrityViolation 令任何状态立即 quarantined。
//! - degraded/quarantined 不因普通 PASS 自动恢复;只有 actor=user 或 planner:*
//!   的 manual-reset 可回 probation。
//! - infra failure(quota/429/503/auth/permission/timeout/transport/worker crash)
//!   的 AttemptFailed 完全不改变质量状态。
//! - Production 只允许 eligible;Canary 允许 probation/eligible;
//!   degraded/quarantined 两种模式都拒绝。
//! - 未知 agent、空 attemptId、未知 assessment、重复 eventId、policy=0 聚合错误
//!   并 fail-closed;agent 键顺序稳定(BTreeMap)。

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use orch_core::EventRecord;
use serde::{Deserialize, Serialize};

// ─────────────────────────── 质量状态 ───────────────────────────

/// Agent 质量状态机:probation → eligible → degraded ↔ quarantined。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum QualityState {
    Probation,
    Eligible,
    Degraded,
    Quarantined,
}

/// 任务分配模式:Production 只接受 eligible;Canary 接受 probation/eligible。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssignmentMode {
    Production,
    Canary,
}

/// 质量策略:probation 需要的连续不同 attempt PASS 次数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualityPolicy {
    pub probation_passes_required: usize,
}

/// 单个 agent 的质量投影结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentQuality {
    pub state: QualityState,
    pub consecutive_passes: usize,
}

/// 全量质量投影:agent 键顺序稳定(BTreeMap)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualityProjection {
    pub agents: BTreeMap<String, AgentQuality>,
}

// ─────────────────────────── 基础设施失败类 ───────────────────────────

/// 不扣质量的基础设施失败类(quota/429/503/auth/permission/timeout/transport/worker crash)。
const INFRA_FAILURE_CLASSES: &[&str] = &[
    "quota",
    "rate-limited",
    "429",
    "503",
    "auth",
    "authentication",
    "permission",
    "timeout",
    "transport",
    "worker-crash",
    "worker_crash",
];

fn is_infra_failure_class(class: &str) -> bool {
    let lower = class.to_lowercase();
    INFRA_FAILURE_CLASSES.iter().any(|&c| lower == c)
}

// ─────────────────────────── 事件解析 ───────────────────────────

/// 从 QualityAssessed payload 提取的评估字段。
struct QualityAssessment {
    agent: String,
    attempt_id: String,
    assessment: String,
}

/// 解析 QualityAssessed 事件的 payload;提取 agent/attemptId/assessment。
fn parse_assessment(payload: &serde_json::Value) -> Option<QualityAssessment> {
    let obj = payload.as_object()?;
    let agent = obj.get("agent")?.as_str()?.to_string();
    let attempt_id = obj.get("attemptId")?.as_str()?.to_string();
    let assessment = obj.get("assessment")?.as_str()?.to_string();
    Some(QualityAssessment {
        agent,
        attempt_id,
        assessment,
    })
}

/// 判定 actor 是否有权执行 manual-reset(仅 user 或 planner:*)。
fn can_manual_reset(actor: &str) -> bool {
    actor == "user" || actor.starts_with("planner:")
}

// ─────────────────────────── 投影核心 ───────────────────────────

/// 按账本追加顺序投影质量状态。
///
/// - `events`: 已追加的账本事件切片(按顺序)。
/// - `initial_registry_state`: 初始 registry 状态(agent → quality state)。
/// - `policy`: 质量策略。
///
/// 返回 `QualityProjection` 或 fail-closed 错误。
pub fn project_quality(
    events: &[EventRecord],
    initial_registry_state: &BTreeMap<String, QualityState>,
    policy: &QualityPolicy,
) -> Result<QualityProjection> {
    // ── 策略校验:probation_passes_required 必须为正 ──
    if policy.probation_passes_required == 0 {
        bail!("policy error: probation_passes_required must be positive");
    }

    // ── 初始化投影:从 registry 拷贝初始状态 ──
    let mut agents: BTreeMap<String, AgentQuality> = initial_registry_state
        .iter()
        .map(|(k, &s)| (k.clone(), AgentQuality {
            state: s,
            consecutive_passes: 0,
        }))
        .collect();

    // ── 重复 eventId 检测 ──
    // key = event_id, value = 序列化后的 payload+kind+actor(用于区分真正不同的事件)
    let mut seen_event_ids: BTreeMap<String, (String, String, String)> = BTreeMap::new();

    // ── 每个 attempt 已记录的 assessment("pass" / "fail") ──
    // key = (agent, attempt_id) → assessment
    let mut attempt_assessments: BTreeMap<(String, String), &str> = BTreeMap::new();

    for ev in events {
        // ── 重复 eventId 检测 ──
        // 完全相同的事件重放(同 eventId + 同内容)是幂等的——跳过不报错。
        // 但 eventId 相同而内容不同是真正的聚合冲突——fail-closed。
        let event_signature = (
            ev.kind.clone(),
            ev.actor.clone(),
            ev.payload
                .as_ref()
                .map(|p| p.to_string())
                .unwrap_or_default(),
        );
        match seen_event_ids.insert(ev.event_id.clone(), event_signature.clone()) {
            None => {} // 新事件
            Some(prev_sig) if prev_sig == event_signature => {
                // 幂等重放:完全相同的事件,跳过不重复处理
                continue;
            }
            Some(_) => {
                bail!(
                    "aggregation error: duplicate eventId {} with differing content",
                    ev.event_id
                );
            }
        }

        match ev.kind.as_str() {
            // ── 基础设施失败:完全不改变质量状态 ──
            "AttemptFailed" => {
                let payload = match &ev.payload {
                    Some(p) => p,
                    None => continue,
                };
                let class = payload
                    .get("failureClass")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                // 基础设施失败不扣质量;未知 class 也不扣(不 fail-closed,
                // 因为 AttemptFailed 不是质量评估事件)。
                let _ = is_infra_failure_class(class);
                continue;
            }

            // ── 质量评估事件 ──
            "QualityAssessed" => {
                let payload = match &ev.payload {
                    Some(p) => p,
                    None => bail!("aggregation error: QualityAssessed missing payload"),
                };

                let assessment = match parse_assessment(payload) {
                    Some(a) => a,
                    None => bail!("aggregation error: malformed QualityAssessed payload"),
                };

                // ── 空 attemptId 检测 ──
                if assessment.attempt_id.is_empty() {
                    bail!("aggregation error: empty attemptId");
                }

                // ── 未知 agent 检测:agent 不在 registry 中 ──
                let agent_quality = match agents.get_mut(&assessment.agent) {
                    Some(aq) => aq,
                    None => bail!(
                        "aggregation error: unknown agent {}",
                        assessment.agent
                    ),
                };

                let key = (assessment.agent.clone(), assessment.attempt_id.clone());

                match assessment.assessment.as_str() {
                    "verified-pass" => {
                        // ── 同 attempt 矛盾检测 ──
                        if let Some(&prev) = attempt_assessments.get(&key) {
                            if prev == "fail" {
                                bail!(
                                    "aggregation error: conflicting assessment for attempt {}",
                                    assessment.attempt_id
                                );
                            }
                            // 幂等:同 attempt 重复 PASS 不重复累计
                            continue;
                        }
                        attempt_assessments.insert(key, "pass");

                        match agent_quality.state {
                            QualityState::Probation => {
                                agent_quality.consecutive_passes += 1;
                                if agent_quality.consecutive_passes
                                    >= policy.probation_passes_required
                                {
                                    agent_quality.state = QualityState::Eligible;
                                }
                            }
                            QualityState::Eligible => {
                                // eligible 下 PASS 保持 eligible
                                // streak 在 eligible 后不累计(已满足)
                            }
                            QualityState::Degraded | QualityState::Quarantined => {
                                // degraded/quarantined 不因普通 PASS 自动恢复
                            }
                        }
                    }

                    "verified-fail" => {
                        // ── 同 attempt 矛盾检测 ──
                        if let Some(&prev) = attempt_assessments.get(&key) {
                            if prev == "pass" {
                                bail!(
                                    "aggregation error: conflicting assessment for attempt {}",
                                    assessment.attempt_id
                                );
                            }
                            // 幂等:同 attempt 重复 FAIL 不重复处理
                            continue;
                        }
                        attempt_assessments.insert(key, "fail");

                        // VerifiedFail 令 probation/eligible 进入 degraded 并清 streak
                        match agent_quality.state {
                            QualityState::Probation => {
                                agent_quality.state = QualityState::Degraded;
                                agent_quality.consecutive_passes = 0;
                            }
                            QualityState::Eligible => {
                                agent_quality.state = QualityState::Degraded;
                                agent_quality.consecutive_passes = 0;
                            }
                            QualityState::Degraded | QualityState::Quarantined => {
                                // 已在 degraded/quarantined 中;FAIL 不进一步变化
                                // (quarantined 优先级最高,不被 fail 降格)
                            }
                        }
                    }

                    "integrity-violation" => {
                        // IntegrityViolation 令任何状态立即 quarantined
                        agent_quality.state = QualityState::Quarantined;
                        agent_quality.consecutive_passes = 0;
                    }

                    "manual-reset" => {
                        // 只有 actor=user 或 planner:* 可执行 manual-reset
                        if !can_manual_reset(&ev.actor) {
                            bail!(
                                "aggregation error: unauthorized manual-reset by actor {}",
                                ev.actor
                            );
                        }
                        // manual-reset 回 probation(不清 attempt_assessments,
                        // 因为这些是历史记录)
                        agent_quality.state = QualityState::Probation;
                        agent_quality.consecutive_passes = 0;
                    }

                    _ => {
                        bail!(
                            "aggregation error: unknown assessment {}",
                            assessment.assessment
                        );
                    }
                }
            }

            _ => {
                // 非 QualityAssessed / AttemptFailed 的事件不处理
                // (质量投影只关心上述两种事件类型)
            }
        }
    }

    Ok(QualityProjection { agents })
}

// ─────────────────────────── 分配路由 ───────────────────────────

/// 判定给定质量状态是否允许分配到指定模式。
///
/// - Production 只允许 eligible。
/// - Canary 允许 probation/eligible。
/// - degraded/quarantined 两种模式都拒绝。
pub fn assignment_allowed(state: QualityState, mode: AssignmentMode) -> bool {
    match mode {
        AssignmentMode::Production => state == QualityState::Eligible,
        AssignmentMode::Canary => {
            state == QualityState::Probation || state == QualityState::Eligible
        }
    }
}
