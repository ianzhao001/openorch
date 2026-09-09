//! 快照内核（r45/B102 修复 1）：把账本投影、心跳/liveness、预算三维、成本与活动流
//! 聚合成单一版本化的 `OrchSnapshot`，同时喂 TUI 与后续 WebUI。
//!
//! 纪律：快照是投影不是真值——任何裁决仍以 `events.jsonl` 为准；缺失字段一律 null/unknown，
//! 绝不估算冒充。
//!
//! IO 采样与纯聚合分层（沿用 `liveness.rs` 的 `probe`(IO) / `judge`(纯) 先例）：
//! - `probe_agents`（IO）：对每个执行者真实调用 `liveness::probe` 采样心跳/ack/worktree 活动，
//!   产出 `judgement=None` 的 `AgentInput`；`judgement=None` 意味着 `build_snapshot` 必须在
//!   纯函数侧调用 `liveness::judge` 复用既有判据（生产路径必经 judge，禁止快照侧另造阈值）。
//! - `build_snapshot`（纯）：把 `SnapshotInputs` 投影成 `OrchSnapshot`，不碰文件系统。
//! - `price_snapshot`（纯）：把 `pricing::estimate_cost` 接入 agents 的 token/model/cost 面，
//!   表达 Known / Unknown / Subscription 三态（未命中→Unknown、usd=None；包月→Subscription、usd=None）。
//!
//! 关于 `AgentInput.judgement: Option<Judgement>`：这是**测试夹具专用**的注入入口，
//! 让种子用确定性 `Judgement` 断言「映射」语义；生产路径 `probe_agents` 永远填 `None`，
//! 迫使 `build_snapshot` 走 `liveness::judge`。任何把 `Some(Judgement)` 当作生产值的实现
//! 都会被「生产路径必经 judge」的回归测试拦下（见 `judge_is_invoked_when_no_fixture_judgement`）。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use orch_core::{fold, read_ledger, EventRecord};

use crate::liveness::{self, Judgement, LivenessOpts, ProbeSnapshot};
use crate::pricing::{self, CostBasis, PricingTable, TokenUsage};

// ── 输入结构 ──

pub struct AgentInput {
    pub agent_id: String,
    pub current_task: Option<String>,
    pub probe: ProbeSnapshot,
    /// 测试夹具专用：注入确定性 `Judgement` 以断言映射语义。
    /// 生产路径（`probe_agents`）永远填 `None`，迫使 `build_snapshot` 走 `liveness::judge`。
    pub judgement: Option<Judgement>,
}

pub struct BudgetInput {
    pub max_usd: Option<f64>,
    pub max_wall_minutes: Option<u64>,
    pub max_model_wakes: Option<u64>,
    pub spent_usd: f64,
    pub spent_wall_minutes: u64,
    pub spent_model_wakes: u64,
}

pub struct SnapshotInputs {
    pub round_id: String,
    pub generated_at: String,
    pub events: Vec<EventRecord>,
    pub agents: Vec<AgentInput>,
    pub budget: BudgetInput,
    pub activity: Vec<ActivityLine>,
    pub liveness_opts: LivenessOpts,
}

// ── 输出结构 ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum AgentState {
    Idle,
    Waiting,
    Booting,
    Working,
    WorkingNoAck,
    Stalled,
    Dead,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Severity {
    Info,
    Warn,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum SnapshotSource {
    Daemon,
    Frontend,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct BudgetDimension<T> {
    pub spent: T,
    pub max: Option<T>,
    pub remaining: Option<T>,
    pub pct: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RoundBudget {
    pub usd: BudgetDimension<f64>,
    pub wall_minutes: BudgetDimension<u64>,
    pub model_wakes: BudgetDimension<u64>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Alert {
    pub severity: Severity,
    pub kind: String,
    pub subject: String,
    pub message: String,
    pub suggested_command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ActivityLine {
    pub ts: String,
    pub agent: String,
    pub kind: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct AgentSnapshot {
    pub agent_id: String,
    pub state: AgentState,
    pub current_task: Option<String>,
    pub attempt_id: Option<String>,
    pub attempt_no: Option<u32>,
    pub model_declared: Option<String>,
    pub model_probed: Option<String>,
    /// None = 未知（无法对比声明/实测）；Some(true)=冲突；Some(false)=一致。
    pub model_conflict: Option<bool>,
    pub depth: Option<String>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub tokens_cache_read: Option<u64>,
    pub tokens_cache_write: Option<u64>,
    pub cost_usd: Option<f64>,
    pub cost_basis: Option<String>,
    pub heartbeat_age_secs: Option<u64>,
    /// 最近活动年龄：worktree 写入 ∪ 当前 attempt 的 provider 日志增长取最小。
    /// None = 两类来源都没有可读事实，绝不猜测。
    pub last_activity_age_secs: Option<u64>,
    pub acked: bool,
    /// provider 归一化状态（B111）。None = 本投影路径未持有 provider 事实
    /// （`AgentInput` 被 B102 种子字节锁定，无法承载 spawn/activity/fault 切片），
    /// **绝不**因「没事实」就猜成 Engaged。B110/B114 落地后由持有事实的上游填入。
    pub provider_state: Option<crate::chanhealth::ProviderState>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TaskSnapshot {
    pub task_id: String,
    pub state: String,
    pub merge_sha: Option<String>,
    pub event_count: usize,
    pub last_ts: String,
    pub gates_green: usize,
    pub gates_red: usize,
    pub gates_total_ms: u128,
    pub tokens: Option<u64>,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RoundSnapshot {
    pub round_id: String,
    pub closed: bool,
    pub events: usize,
    /// 坏行无法从已解析的 `events` 反推（解析时被丢弃）；诚实务必为 None。
    pub bad_lines: Option<usize>,
    pub kind_histogram: serde_json::Value,
    pub budget: RoundBudget,
    pub projected_spent_usd: Option<f64>,
    pub projected_in_flight_estimate_usd: Option<f64>,
    pub projected_basis: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct OrchSnapshot {
    pub schema_version: String,
    pub generated_at: String,
    pub source: SnapshotSource,
    pub agents: Vec<AgentSnapshot>,
    pub tasks: Vec<TaskSnapshot>,
    pub round: RoundSnapshot,
    pub alerts: Vec<Alert>,
    pub activity: Vec<ActivityLine>,
}

impl OrchSnapshot {
    /// B103 契约起点：空快照（source/alerts/activity 由调用方直接赋值，故三者必为 pub）。
    pub fn empty(round_id: &str, generated_at: &str) -> Self {
        OrchSnapshot {
            schema_version: "1.0.0".to_string(),
            generated_at: generated_at.to_string(),
            source: SnapshotSource::Daemon,
            agents: Vec::new(),
            tasks: Vec::new(),
            round: RoundSnapshot {
                round_id: round_id.to_string(),
                closed: false,
                events: 0,
                bad_lines: None,
                kind_histogram: serde_json::Value::Null,
                budget: RoundBudget {
                    usd: BudgetDimension {
                        spent: 0.0,
                        max: None,
                        remaining: None,
                        pct: None,
                    },
                    wall_minutes: BudgetDimension {
                        spent: 0,
                        max: None,
                        remaining: None,
                        pct: None,
                    },
                    model_wakes: BudgetDimension {
                        spent: 0,
                        max: None,
                        remaining: None,
                        pct: None,
                    },
                },
                projected_spent_usd: None,
                projected_in_flight_estimate_usd: None,
                projected_basis: None,
            },
            alerts: Vec::new(),
            activity: Vec::new(),
        }
    }
}

// ── 纯聚合 ──

pub fn build_snapshot(inputs: &SnapshotInputs) -> OrchSnapshot {
    let proj = fold(&inputs.events);
    let tasks = project_tasks(&inputs.events, &proj);
    let (agents, mut alerts) = project_agents(inputs);
    // B113：action-scoped 拒绝告警（幂等——同 actionId 只告警一次）。
    alerts.extend(project_action_rejections(&inputs.events));
    let kind_hist = serde_json::to_value(&kind_histogram_owned(&inputs.events))
        .unwrap_or(serde_json::Value::Null);

    let budget = RoundBudget {
        usd: budget_dim_f64(inputs.budget.spent_usd, inputs.budget.max_usd),
        wall_minutes: budget_dim_u64(
            inputs.budget.spent_wall_minutes,
            inputs.budget.max_wall_minutes,
        ),
        model_wakes: budget_dim_u64(
            inputs.budget.spent_model_wakes,
            inputs.budget.max_model_wakes,
        ),
    };

    let round = RoundSnapshot {
        round_id: inputs.round_id.clone(),
        closed: proj.round_closed,
        events: inputs.events.len(),
        bad_lines: None, // 坏行在 read_ledger 时丢弃，无法从 events 反推：诚实 None
        kind_histogram: kind_hist,
        budget,
        projected_spent_usd: None,
        projected_in_flight_estimate_usd: None,
        projected_basis: None,
    };

    OrchSnapshot {
        schema_version: "1.0.0".to_string(),
        generated_at: inputs.generated_at.clone(),
        source: SnapshotSource::Frontend,
        agents,
        tasks,
        round,
        alerts,
        activity: inputs.activity.clone(),
    }
}

/// 当前 attempt 的 provider 输出年龄。只读取该 attempt durable wake 事件绑定的
/// 精确 `logPath`；不会扫描 agent 的“最新日志”。最新匹配事件坏字段、文件缺失/
/// 不可读、mtime 不可读或系统时钟倒退都返回 `None`，从而回退原 liveness 语义。
#[doc(hidden)]
pub fn provider_output_age(
    root: &Path,
    events: &[EventRecord],
    task_id: &str,
    agent: &str,
    attempt_id: &str,
) -> Option<Duration> {
    let event = events.iter().rev().find(|event| {
        matches!(
            event.kind.as_str(),
            "DispatchWakeCompleted"
                | "DispatchWakeDelivered"
                | "ResumeWakeCompleted"
                | "ResumeWakeDelivered"
                | "NudgeIssued"
        ) && event.task_id.as_deref() == Some(task_id)
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("agent"))
                .and_then(serde_json::Value::as_str)
                == Some(agent)
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("attemptId"))
                .and_then(serde_json::Value::as_str)
                == Some(attempt_id)
    })?;
    // 先锁定“最新匹配事件”再解析；解析失败不得跳过它回退到旧日志。
    let facts = crate::wake::ActionChannelFacts::from_payload(event.payload.as_ref()?).ok()?;
    let candidate = PathBuf::from(facts.log_path);
    let log_path = if candidate.is_absolute() {
        candidate
    } else {
        root.join(candidate)
    };
    let log = fs::File::open(log_path).ok()?;
    let modified = log.metadata().ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}

/// IO 采样层（生产路径）：对每个执行者真实调用 `liveness::probe`，
/// 产出 `judgement=None` 的 `AgentInput`，迫使 `build_snapshot` 在纯函数侧调用 `liveness::judge`。
/// 这层是 IO，不是纯聚合——种子夹具不会走这条路径，因此必须单独测其调用了 probe。
///
/// `LivenessOpts` 不在本层使用（`liveness::probe` 是纯 IO 采样，不含判据）；
/// 判据 `opts` 由 `build_snapshot` 在 `SnapshotInputs.liveness_opts` 上携带并喂给 `liveness::judge`。
pub fn probe_agents(root: &Path, specs: &[AgentProbeSpec]) -> Vec<AgentInput> {
    // 快照层本来就是 IO 采样层：一次性读取当前轮账本，随后每个 agent 都只按
    // resolve_current_dispatch 得到的 task/agent/attempt 精确身份选 provider 日志。
    // 任一读取/坏行/身份解析失败都退回 output=None，保持旧语义。
    let provider_ledger: Option<(String, Vec<EventRecord>)> = (|| {
        let round = crate::current_round(root).ok()?;
        let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
        let ledger = read_ledger(&ledger_path).ok()?;
        if !ledger.bad_lines.is_empty() {
            return None;
        }
        Some((round, ledger.events))
    })();
    let provider = provider_ledger
        .as_ref()
        .map(|(round, events)| (round.as_str(), events.as_slice()));
    probe_agents_with_provider_events(root, specs, provider)
}

fn probe_agents_with_provider_events(
    root: &Path,
    specs: &[AgentProbeSpec],
    provider: Option<(&str, &[EventRecord])>,
) -> Vec<AgentInput> {
    let started = Instant::now();
    let mut out = Vec::with_capacity(specs.len());
    for spec in specs {
        let provider_age = provider.and_then(|(round, events)| {
            let task_id = spec.current_task.as_deref()?;
            let dispatch = crate::attempt::resolve_current_dispatch(events, task_id, round).ok()?;
            if dispatch.agent.as_deref() != Some(spec.agent_id.as_str()) {
                return None;
            }
            let attempt_id = dispatch.attempt_id.as_deref()?;
            provider_output_age(root, events, task_id, &spec.agent_id, attempt_id)
        });
        let mut probe = match &spec.current_task {
            Some(task_id) if !task_id.is_empty() => liveness::probe(
                root,
                &spec.agent_id,
                task_id,
                spec.go_path.as_deref(),
                &spec.write_set,
                started,
            ),
            _ => {
                // 无派发中任务：仅采样心跳（worktree 探测对空任务无意义）。
                // 复用 liveness::probe 但传空 task_id，其心跳分支仍会运行，
                // worktree 路径都不可达 → 各 age 返回 None（等价于无活动）。
                liveness::probe(
                    root,
                    &spec.agent_id,
                    "",
                    spec.go_path.as_deref(),
                    &[],
                    started,
                )
            }
        };
        probe.last_activity_age =
            liveness::working_idle_age(None, probe.last_activity_age, provider_age);
        out.push(AgentInput {
            agent_id: spec.agent_id.clone(),
            current_task: spec.current_task.clone(),
            probe,
            judgement: None, // 生产路径不注入 judgement，强制 build_snapshot 走 judge
        });
    }
    out
}

/// `probe_agents` 的输入规格：执行者身份 + 当前任务 + 该任务的 GO/写集合（供 probe 采样）。
pub struct AgentProbeSpec {
    pub agent_id: String,
    pub current_task: Option<String>,
    pub go_path: Option<PathBuf>,
    pub write_set: Vec<String>,
}

/// 纯函数侧的 agents 投影 + 告警生成。生产路径必经 `liveness::judge`：
/// `judgement=None`（`probe_agents` 的产物）时调用 `liveness::judge(probe, opts)`；
/// `Some(Judgement)` 仅服务测试夹具，不替代 judge 在生产路径上的位置。
fn project_agents(inputs: &SnapshotInputs) -> (Vec<AgentSnapshot>, Vec<Alert>) {
    let mut agents = Vec::with_capacity(inputs.agents.len());
    let mut alerts = Vec::new();

    for ai in &inputs.agents {
        let state = derive_agent_state(
            ai.current_task.is_some(),
            &ai.probe,
            &ai.judgement,
            &inputs.liveness_opts,
        );

        let model_declared = ai
            .current_task
            .as_deref()
            .and_then(|t| crate::cost::task_model(&inputs.events, t));
        let depth = ai
            .current_task
            .as_deref()
            .and_then(|t| task_env_decl(&inputs.events, t))
            .and_then(|v| v.get("depth"))
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let alert_cmd = if state == AgentState::Dead {
            Some(format!("orch handshake {}", ai.agent_id))
        } else if state == AgentState::Stalled {
            Some(format!("orch nudge {}", ai.agent_id))
        } else {
            None
        };

        if state == AgentState::Dead {
            alerts.push(Alert {
                severity: Severity::Critical,
                kind: "agent-dead".to_string(),
                subject: ai.agent_id.clone(),
                message: "执行者被判死：心跳缺失 ∧ 无 ack ∧ 无 worktree/provider 活动".to_string(),
                suggested_command: alert_cmd.clone(),
            });
        }

        if state == AgentState::Stalled {
            alerts.push(Alert {
                severity: Severity::Warn,
                kind: "agent-stalled".to_string(),
                subject: ai.agent_id.clone(),
                message: "执行者疑似停滞：ack 后长期零活动".to_string(),
                suggested_command: alert_cmd.clone(),
            });
        }

        agents.push(AgentSnapshot {
            agent_id: ai.agent_id.clone(),
            state,
            current_task: ai.current_task.clone(),
            attempt_id: None,
            attempt_no: None,
            model_declared,
            model_probed: None, // 无 prober 路径接入：诚实 None
            model_conflict: None, // 无法对比 → 诚实 None，不写 false 冒充「已知不冲突」
            depth,
            tokens_in: None,
            tokens_out: None,
            tokens_cache_read: None,
            tokens_cache_write: None,
            cost_usd: None,
            cost_basis: None,
            heartbeat_age_secs: ai.probe.hb_age.map(|d| d.as_secs()),
            last_activity_age_secs: ai.probe.last_activity_age.map(|d| d.as_secs()),
            acked: ai.probe.acked,
            // B111：AgentInput 被 B102 种子锁定，本投影路径拿不到 provider 事实
            // （spawn/activity/fault 切片）。诚实置 None，**绝不**没事实就猜 Engaged
            // （任务卡「未知 provider/未知形状显式 Pending/Unknown，不猜 Engaged」）。
            // B110/B114 落地后由持有事实的上游经独立入口填入。
            provider_state: None,
        });
    }

    (agents, alerts)
}

/// B113：action-scoped 拒绝告警投影。扫描 `ActionRejected` 事件，按 actionId 幂等
/// （重复同 action 拒绝不重复告警），新 action 可再次告警。普通校验错误不吞掉原始
/// 原因——只对显式构造的 `ActionRejected`（payload.alert==true）告警。
fn project_action_rejections(events: &[EventRecord]) -> Vec<Alert> {
    let mut alerts = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for (i, ev) in events.iter().enumerate() {
        if ev.kind != "ActionRejected" {
            continue;
        }
        let Some(payload) = ev.payload.as_ref() else { continue };
        let alert_flag = payload.get("alert").and_then(serde_json::Value::as_bool).unwrap_or(false);
        if !alert_flag {
            continue;
        }
        let action_id = payload
            .get("actionId")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        if action_id.is_empty() {
            continue;
        }
        // 幂等：同 actionId 已拒绝过则不再告警。判定用「该事件之前」的事件集，
        // 与 failure::action_already_rejected 同义。
        let before = &events[..i];
        if crate::failure::action_already_rejected(before, &action_id) {
            continue;
        }
        if seen.contains(&action_id) {
            continue;
        }
        seen.push(action_id.clone());
        let operation = payload
            .get("operation")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?")
            .to_string();
        let reason = payload
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("(no reason)")
            .to_string();
        let exit_code = payload
            .get("exitCode")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        alerts.push(Alert {
            severity: Severity::Critical,
            kind: "action-rejected".to_string(),
            subject: action_id.clone(),
            message: format!(
                "{operation} 拒绝（exit {exit_code}）：{reason}"
            ),
            suggested_command: None,
        });
    }
    alerts
}

fn derive_agent_state(
    has_task: bool,
    probe: &ProbeSnapshot,
    judgement: &Option<Judgement>,
    opts: &LivenessOpts,
) -> AgentState {
    if has_task {
        // 有派发中任务：生产路径 judgement=None → 必走 liveness::judge；
        // Some(Judgement) 仅服务测试夹具（映射注入值，不替代 judge）。
        match judgement {
            Some(Judgement::Healthy(s)) => map_healthy(*s),
            Some(Judgement::DeadCandidate(_)) => AgentState::Dead,
            Some(Judgement::StalledCandidate(_)) => AgentState::Stalled,
            None => judge_to_state(liveness::judge(probe, opts)),
        }
    } else {
        // 无派发中任务
        match probe.hb_age {
            Some(age) if age <= opts.grace => AgentState::Idle,
            _ => AgentState::Unknown, // 无心跳或心跳过期 → Unknown，绝不 Idle
        }
    }
}

fn judge_to_state(j: Judgement) -> AgentState {
    match j {
        Judgement::Healthy(s) => map_healthy(s),
        Judgement::DeadCandidate(_) => AgentState::Dead,
        Judgement::StalledCandidate(_) => AgentState::Stalled,
    }
}

fn map_healthy(s: &str) -> AgentState {
    match s {
        "waiting" => AgentState::Waiting,
        "booting" => AgentState::Booting,
        "working" => AgentState::Working,
        "working-no-ack" => AgentState::WorkingNoAck,
        _ => AgentState::Unknown,
    }
}

fn project_tasks(events: &[EventRecord], proj: &orch_core::RoundProjection) -> Vec<TaskSnapshot> {
    let mut out = Vec::with_capacity(proj.tasks.len());
    for (task_id, tp) in &proj.tasks {
        let (green, red, total_ms, tokens, cost_usd) = task_gate_stats(events, task_id);
        let state = tp
            .state
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        out.push(TaskSnapshot {
            task_id: task_id.clone(),
            state,
            merge_sha: tp.merge_sha.clone(),
            event_count: tp.event_count,
            last_ts: tp.last_ts.clone(),
            gates_green: green,
            gates_red: red,
            gates_total_ms: total_ms,
            tokens,
            cost_usd,
        });
    }
    out.sort_by(|a, b| a.task_id.cmp(&b.task_id));
    out
}

/// 从该任务的 GateExecuted 事件聚合门统计（绿/红/总毫秒）与 token/cost。
fn task_gate_stats(events: &[EventRecord], task: &str) -> (usize, usize, u128, Option<u64>, Option<f64>) {
    let mut green = 0;
    let mut red = 0;
    let mut total_ms: u128 = 0;
    let mut tokens_in: Option<u64> = None;
    let mut tokens_out: Option<u64> = None;
    let mut cost_usd: Option<f64> = None;
    for ev in events {
        if ev.task_id.as_deref() != Some(task) {
            continue;
        }
        match ev.kind.as_str() {
            "GateExecuted" => {
                let exit = ev
                    .payload
                    .as_ref()
                    .and_then(|p| p.get("exitCode"))
                    .and_then(|v| v.as_i64());
                if exit == Some(0) {
                    green += 1;
                } else {
                    red += 1;
                }
                if let Some(ms) = ev
                    .payload
                    .as_ref()
                    .and_then(|p| p.get("durationMs"))
                    .and_then(|v| v.as_u64())
                {
                    total_ms = total_ms.saturating_add(ms as u128);
                }
            }
            "CostSampled" => {
                if let Some(usage) = ev.payload.as_ref().and_then(|p| p.get("usage")) {
                    let (i, o) = extract_tokens(usage);
                    if let Some(i) = i {
                        tokens_in = Some(tokens_in.unwrap_or(0) + i);
                    }
                    if let Some(o) = o {
                        tokens_out = Some(tokens_out.unwrap_or(0) + o);
                    }
                }
            }
            "VerdictIssued" => {
                if let Some(usd) = ev
                    .payload
                    .as_ref()
                    .and_then(|p| p.get("costUsd"))
                    .and_then(|v| v.as_f64())
                {
                    cost_usd = Some(cost_usd.unwrap_or(0.0) + usd);
                }
            }
            _ => {}
        }
    }
    let tokens = match (tokens_in, tokens_out) {
        (Some(i), Some(o)) => Some(i + o),
        (Some(i), None) => Some(i),
        (None, Some(o)) => Some(o),
        (None, None) => None,
    };
    (green, red, total_ms, tokens, cost_usd)
}

/// 复刻 cost.rs::extract_tokens 的键提取逻辑（cost.rs 该函数私有，无法直接复用）。
fn extract_tokens(usage: &serde_json::Value) -> (Option<u64>, Option<u64>) {
    let pick = |keys: &[&str]| -> Option<u64> {
        for k in keys {
            if let Some(n) = usage.get(*k).and_then(|v| v.as_u64()) {
                return Some(n);
            }
            for parent in ["tokens", "usage"] {
                if let Some(n) = usage
                    .get(parent)
                    .and_then(|t| t.get(*k))
                    .and_then(|v| v.as_u64())
                {
                    return Some(n);
                }
            }
        }
        None
    };
    (
        pick(&["input_tokens", "input", "prompt_tokens"]),
        pick(&["output_tokens", "output", "completion_tokens"]),
    )
}

/// 该任务最后一条 ReportObserved 的 envDecl（返工以最新为准）；无则 None。
fn task_env_decl<'a>(events: &'a [EventRecord], task: &str) -> Option<&'a serde_json::Value> {
    events
        .iter()
        .rev()
        .find(|ev| ev.kind == "ReportObserved" && ev.task_id.as_deref() == Some(task))
        .and_then(|ev| ev.payload.as_ref())
        .and_then(|p| p.get("envDecl"))
}

fn budget_dim_f64(spent: f64, max: Option<f64>) -> BudgetDimension<f64> {
    let remaining = max.map(|m| (m - spent).max(0.0));
    let pct = max.map(|m| if m > 0.0 { (spent / m * 100.0).min(100.0) } else { 100.0 });
    BudgetDimension {
        spent,
        max,
        remaining,
        pct,
    }
}

fn budget_dim_u64(spent: u64, max: Option<u64>) -> BudgetDimension<u64> {
    let remaining = max.map(|m| m.saturating_sub(spent));
    let pct = max.map(|m| {
        if m == 0 {
            100.0
        } else {
            (spent as f64 / m as f64 * 100.0).min(100.0)
        }
    });
    BudgetDimension {
        spent,
        max,
        remaining,
        pct,
    }
}

fn kind_histogram_owned(events: &[EventRecord]) -> BTreeMap<String, usize> {
    let mut map: BTreeMap<String, usize> = BTreeMap::new();
    for ev in events {
        *map.entry(ev.kind.clone()).or_insert(0) += 1;
    }
    map
}

// ── 定价接入 ──

/// 用 `pricing::estimate_cost` 把每个执行者当前任务的 token/model 事实折算成 cost。
/// 三态语义：未命中模型 → Unknown 且 usd=None；包月 → Subscription 且 usd=None；
/// 命中 usage 模型 → Known 且 usd=按表计算。`PricingTable` 与 `events` 均为调用方传入，
/// 因为 `SnapshotInputs`/`AgentInput` 由 B102 种子字节锁定、无法新增字段承载。
pub fn price_snapshot(snap: &mut OrchSnapshot, table: &PricingTable, events: &[EventRecord]) {
    for agent in &mut snap.agents {
        let Some(task) = &agent.current_task else {
            // 无派发中任务：无可计价的事实，保持 None（诚实缺省）。
            continue;
        };
        let model = agent
            .model_declared
            .clone()
            .or_else(|| crate::cost::task_model(events, task));
        let usage = task_usage(events, task);
        let Some(model) = model.as_deref() else {
            // 模型未知：Unknown、usd=None
            agent.cost_basis = Some(cost_basis_str(CostBasis::Unknown).to_string());
            continue;
        };
        let Some(usage) = usage else {
            // 无 usage 事实：照常按表判定 basis（命中表但无 usage → Unknown、usd=None）
            let est = pricing::estimate_cost(table, model, &TokenUsage {
                input: 0,
                output: 0,
                cache_read: 0,
                cache_write: 0,
            });
            agent.cost_basis = Some(cost_basis_str(est.basis).to_string());
            // 仍把已知 token 事实填回（如果模型已知）
            agent.model_declared = Some(model.to_string());
            continue;
        };
        let est = pricing::estimate_cost(table, model, &usage);
        agent.cost_usd = est.usd;
        agent.cost_basis = Some(cost_basis_str(est.basis).to_string());
        agent.model_declared = Some(model.to_string());
        agent.tokens_in = Some(usage.input);
        agent.tokens_out = Some(usage.output);
        agent.tokens_cache_read = Some(usage.cache_read);
        agent.tokens_cache_write = Some(usage.cache_write);
    }
}

fn cost_basis_str(b: CostBasis) -> &'static str {
    match b {
        CostBasis::Known => "known",
        CostBasis::Estimated => "estimated",
        CostBasis::Subscription => "subscription",
        CostBasis::Unknown => "unknown",
    }
}

/// 从该任务的 CostSampled 事件聚合 token 事实（input/output/cache_read/cache_write）。
fn task_usage(events: &[EventRecord], task: &str) -> Option<TokenUsage> {
    let mut usage: Option<TokenUsage> = None;
    for ev in events {
        if ev.task_id.as_deref() != Some(task) {
            continue;
        }
        if ev.kind != "CostSampled" {
            continue;
        }
        let Some(u) = ev.payload.as_ref().and_then(|p| p.get("usage")) else {
            continue;
        };
        let (i, o) = extract_tokens(u);
        let cr = pick_token(u, &["cache_read_tokens", "cacheReadInputTokens", "cache_read"]);
        let cw = pick_token(u, &["cache_write_tokens", "cacheCreationInputTokens", "cache_write"]);
        let cur = usage.get_or_insert(TokenUsage {
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
        });
        cur.input = cur.input.saturating_add(i.unwrap_or(0));
        cur.output = cur.output.saturating_add(o.unwrap_or(0));
        cur.cache_read = cur.cache_read.saturating_add(cr.unwrap_or(0));
        cur.cache_write = cur.cache_write.saturating_add(cw.unwrap_or(0));
    }
    usage
}

fn pick_token(usage: &serde_json::Value, keys: &[&str]) -> Option<u64> {
    for k in keys {
        if let Some(n) = usage.get(*k).and_then(|v| v.as_u64()) {
            return Some(n);
        }
        for parent in ["tokens", "usage"] {
            if let Some(n) = usage
                .get(parent)
                .and_then(|t| t.get(*k))
                .and_then(|v| v.as_u64())
            {
                return Some(n);
            }
        }
    }
    None
}

// ── 落盘与归档 ──

static WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_tmp_path(dir: &Path, base: &str) -> PathBuf {
    let n = WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".{base}.{}.{}.tmp", std::process::id(), n))
}

pub fn write_snapshot(root: &Path, snap: &OrchSnapshot) -> Result<()> {
    let dir = root.join("coordination/runtime");
    std::fs::create_dir_all(&dir).context("创建 snapshot 目录失败")?;
    let target = dir.join("snapshot.json");
    let tmp = next_tmp_path(&dir, "snapshot");
    let json = serde_json::to_string_pretty(snap).context("序列化快照失败")?;
    std::fs::write(&tmp, json).context("写快照临时文件失败")?;
    std::fs::rename(&tmp, &target).context("快照原子改名失败")?;
    Ok(())
}

pub fn archive_round(root: &Path, round: &str, snap: &OrchSnapshot) -> Result<()> {
    let dir = root.join(format!("coordination/rounds/{round}"));
    std::fs::create_dir_all(&dir).context("创建归档目录失败")?;
    let target = dir.join("SNAPSHOT.json");
    let tmp = next_tmp_path(&dir, "snapshot-archive");
    // 归档契约：写聚合面，**不含 activity 原文**（纵深防御：即便上游漏网也不得入档）。
    let mut archived = snap.clone();
    archived.activity = Vec::new();
    let json = serde_json::to_string_pretty(&archived).context("序列化归档快照失败")?;
    std::fs::write(&tmp, json).context("写归档临时文件失败")?;
    std::fs::rename(&tmp, &target).context("归档原子改名失败")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn opts() -> LivenessOpts {
        LivenessOpts::default()
    }

    /// worktree 内临时目录（**禁止用 `std::env::temp_dir()`**——它指向 /tmp，
    /// 在 Tier F 沙箱下会被 auto-reject 当场终结 turn）。落在 `CARGO_MANIFEST_DIR` 下的
    /// `.tmp-b102-tests/`，每次调用取唯一 ULID 子目录，调用方负责清理。
    fn local_tmp(tag: &str) -> PathBuf {
        let base = std::env::var("CARGO_MANIFEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::current_dir().unwrap());
        let dir = base
            .join(".tmp-b102-tests")
            .join(format!("{tag}-{}-{}", std::process::id(), ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn probe(hb_age: Option<Duration>, acked: bool, activity: Option<Duration>) -> ProbeSnapshot {
        ProbeSnapshot {
            elapsed: Duration::from_secs(600),
            hb_age,
            hb_pid_alive: None,
            acked,
            ack_age: if acked { Some(Duration::from_secs(30)) } else { None },
            last_activity_age: activity,
        }
    }

    fn agent(id: &str, task: Option<&str>, probe: ProbeSnapshot) -> AgentInput {
        AgentInput {
            agent_id: id.to_string(),
            current_task: task.map(str::to_string),
            probe,
            judgement: None,
        }
    }

    fn inputs(agents: Vec<AgentInput>) -> SnapshotInputs {
        SnapshotInputs {
            round_id: "r45".to_string(),
            generated_at: "2026-07-26T00:00:00Z".to_string(),
            events: Vec::new(),
            agents,
            budget: BudgetInput {
                max_usd: Some(8.0),
                max_wall_minutes: Some(900),
                max_model_wakes: Some(24),
                spent_usd: 1.25,
                spent_wall_minutes: 30,
                spent_model_wakes: 3,
            },
            activity: Vec::new(),
            liveness_opts: opts(),
        }
    }

    // ── 原子写回归（B101 先例：自建测试让「非原子写」实现必红）──
    // 非原子写（直接 `std::fs::write(&target, ...)`）会跟随 symlink 写入 staging，
    // 保留 target 为 symlink；原子写（tmp+rename）会用普通文件替换 symlink。
    // 因此断言「调用后 target 是普通文件（非 symlink）」让非原子实现必红。
    #[test]
    fn write_snapshot_is_atomic_replaces_symlink_target() {
        let root = local_tmp("atomic");
        let dir = root.join("coordination/runtime");
        std::fs::create_dir_all(&dir).unwrap();
        let staging = dir.join("staging.json");
        std::fs::write(&staging, r#"{"old":true}"#).unwrap();
        let target = dir.join("snapshot.json");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&staging, &target).unwrap();

        let snap = build_snapshot(&inputs(Vec::new()));
        write_snapshot(&root, &snap).expect("write_snapshot");

        #[cfg(unix)]
        {
            let meta = std::fs::symlink_metadata(&target).unwrap();
            assert!(
                meta.file_type().is_file() && !meta.file_type().is_symlink(),
                "write_snapshot 必须以 tmp+rename 原子替换 target，不得直接 truncate 写穿 symlink"
            );
        }
        let written = std::fs::read_to_string(&target).unwrap();
        assert!(
            written.contains("\"schema_version\""),
            "落盘内容必须是快照：{written}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    // ── 生产路径必经 liveness::judge：judgement=None（probe_agents 产物）时，
    // build_snapshot 的 state 必须与 liveness::judge(probe, opts) 的映射一致。──
    #[test]
    fn judge_is_invoked_when_no_fixture_judgement() {
        // 有派发中任务 + judgement=None（生产路径）→ build_snapshot 必走 liveness::judge
        let p = probe(None, true, Some(Duration::from_secs(10)));
        let ai = agent("executor-opencode", Some("B102"), p); // judgement=None
        let snap = build_snapshot(&inputs(vec![ai]));
        let state = snap
            .agents
            .iter()
            .find(|a| a.agent_id == "executor-opencode")
            .unwrap()
            .state;
        let expected = judge_to_state(liveness::judge(
            &probe(None, true, Some(Duration::from_secs(10))),
            &opts(),
        ));
        assert_eq!(
            state, expected,
            "judgement=None 时 build_snapshot 必须调用 liveness::judge 并映射其结果"
        );
    }

    // ── probe_agents 是 IO 采样层：真实调用 liveness::probe（无心跳文件 → hb_age=None）──
    #[test]
    fn probe_agents_invokes_liveness_probe_on_real_fs() {
        let root = local_tmp("probe");
        std::fs::create_dir_all(root.join("coordination/runtime")).unwrap();
        // 不创建 heartbeat 文件 → liveness::probe 读不到 → hb_age=None
        let specs = vec![AgentProbeSpec {
            agent_id: "executor-test".to_string(),
            current_task: Some("B102".to_string()),
            go_path: None,
            write_set: vec!["orch/crates/orch-host/src/snapshot.rs".to_string()],
        }];
        let inputs_agents = probe_agents(&root, &specs);
        assert_eq!(inputs_agents.len(), 1);
        assert_eq!(inputs_agents[0].agent_id, "executor-test");
        assert_eq!(inputs_agents[0].judgement, None, "生产路径必须 judgement=None");
        assert_eq!(
            inputs_agents[0].probe.hb_age, None,
            "probe_agents 必须真实调用 liveness::probe（无 heartbeat 文件 → hb_age=None）"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    fn channel_event(attempt_id: &str, log_path: &Path) -> EventRecord {
        let attempt_no = if attempt_id.ends_with("A0001") { 1 } else { 2 };
        crate::ledger::event(
            "DispatchWakeCompleted",
            "runtime:orch",
            Some("B198"),
            Some("r61"),
            serde_json::json!({
                "actionId": format!("dispatch:{attempt_id}"),
                "attemptId": attempt_id,
                "attemptNo": attempt_no,
                "agent": "executor-desktop",
                "pid": 1,
                "logPath": log_path.display().to_string(),
                "probeOffset": 0,
                "probeEnd": 0,
            }),
        )
    }

    #[test]
    fn provider_output_age_uses_only_the_exact_current_attempt_log() {
        let root = local_tmp("provider-exact");
        let old_log = root.join("coordination/runtime/logs/wake-old.jsonl");
        std::fs::create_dir_all(old_log.parent().unwrap()).unwrap();
        std::fs::write(&old_log, b"old attempt output").unwrap();
        let missing_current = root.join("coordination/runtime/logs/wake-current.jsonl");
        let events = vec![
            channel_event("B198-A0001", &old_log),
            channel_event("B198-A0002", &missing_current),
        ];

        assert_eq!(
            provider_output_age(
                &root,
                &events,
                "B198",
                "executor-desktop",
                "B198-A0002",
            ),
            None,
            "旧 attempt 的现存日志不得救活当前 attempt"
        );

        std::fs::write(&missing_current, b"current attempt output").unwrap();
        assert!(
            provider_output_age(
                &root,
                &events,
                "B198",
                "executor-desktop",
                "B198-A0002",
            )
            .is_some(),
            "当前 attempt 绑定日志可读时应产生活动年龄"
        );

        let mut malformed = channel_event("B198-A0002", &missing_current);
        malformed.payload.as_mut().unwrap()["logPath"] = serde_json::Value::Null;
        let malformed_latest = vec![events[1].clone(), malformed];
        assert_eq!(
            provider_output_age(
                &root,
                &malformed_latest,
                "B198",
                "executor-desktop",
                "B198-A0002",
            ),
            None,
            "最新匹配事件解析失败时不得回退较旧的日志绑定"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn probe_agents_folds_provider_output_into_existing_activity_age() {
        let root = local_tmp("provider-fold");
        let log = root.join("coordination/runtime/logs/wake-current.jsonl");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, b"provider is still working").unwrap();
        let dispatch = crate::ledger::event(
            "DispatchIssued",
            "runtime:orch",
            Some("B198"),
            Some("r61"),
            serde_json::json!({
                "taskId": "B198",
                "agent": "executor-desktop",
                "attemptId": "B198-A0001",
                "attemptNo": 1,
                "baseSha": "deadbeef",
                "goPath": "coordination/rounds/r61/dispatch/executor-desktop/GO-B198-A0001.md",
            }),
        );
        let events = vec![dispatch, channel_event("B198-A0001", &log)];
        let specs = vec![AgentProbeSpec {
            agent_id: "executor-desktop".to_string(),
            current_task: Some("B198".to_string()),
            go_path: None,
            write_set: Vec::new(),
        }];

        let agents = probe_agents_with_provider_events(&root, &specs, Some(("r61", &events)));
        assert!(
            agents[0].probe.last_activity_age.is_some(),
            "provider mtime 必须折叠进既有 last_activity_age"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    // ── archive_round 必须剥离 activity 原文 ──
    #[test]
    fn archive_round_strips_activity_payload() {
        let root = local_tmp("archive");
        let mut snap = build_snapshot(&inputs(Vec::new()));
        snap.activity = vec![ActivityLine {
            ts: "2026-07-26T00:00:01Z".to_string(),
            agent: "executor-opencode".to_string(),
            kind: "message".to_string(),
            summary: "secret sk-livesecret here".to_string(),
        }];
        archive_round(&root, "r45", &snap).expect("archive_round");
        let written = std::fs::read_to_string(
            root.join("coordination/rounds/r45/SNAPSHOT.json"),
        )
        .unwrap();
        assert!(
            !written.contains("sk-livesecret"),
            "归档面不得含 activity 原文（含敏感内容）：{written}"
        );
        assert!(
            !written.contains("\"activity\"")
                || written.contains("\"activity\":[]")
                || written.contains("\"activity\": []"),
            "归档面的 activity 必须被剥离为空：{written}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    // ── price_snapshot：Known / Unknown / Subscription 三态可表达 ──
    #[test]
    fn price_snapshot_expresses_known_unknown_subscription() {
        // 注意：YAML 缩进是结构性的，必须用 raw 字符串字面量（backslash 续行会吞掉下一行的前导空格）。
        let yaml = r#"models:
  usage-model:
    billing: usage
    inputPerMTok: 1.0
    outputPerMTok: 2.0
  sub-model:
    billing: subscription
  ghost-model:
    billing: usage
    inputPerMTok: 3.0
    outputPerMTok: 4.0
"#;
        let table = PricingTable::from_yaml_str(yaml).unwrap();
        let mk = |id: &str, task: &str, probe: ProbeSnapshot| AgentInput {
            agent_id: id.to_string(),
            current_task: Some(task.to_string()),
            probe,
            judgement: None,
        };
        let usage_events = vec![
            cost_sampled_event("T1", "usage-model", 100, 200, 0, 0),
            cost_sampled_event("T2", "sub-model", 50, 50, 0, 0),
            cost_sampled_event("T3", "ghost-model", 10, 10, 0, 0),
            report_observed_event("T1", "usage-model"),
            report_observed_event("T2", "sub-model"),
            report_observed_event("T3", "ghost-model"),
        ];
        let mut snap = build_snapshot(&SnapshotInputs {
            round_id: "r45".to_string(),
            generated_at: "2026-07-26T00:00:00Z".to_string(),
            events: usage_events.clone(),
            agents: vec![
                mk("A1", "T1", probe(None, true, Some(Duration::from_secs(10)))),
                mk("A2", "T2", probe(None, true, Some(Duration::from_secs(10)))),
                mk("A3", "T3", probe(None, true, Some(Duration::from_secs(10)))),
            ],
            budget: BudgetInput {
                max_usd: Some(8.0),
                max_wall_minutes: Some(900),
                max_model_wakes: Some(24),
                spent_usd: 0.0,
                spent_wall_minutes: 0,
                spent_model_wakes: 0,
            },
            activity: Vec::new(),
            liveness_opts: opts(),
        });
        price_snapshot(&mut snap, &table, &usage_events);
        let by_id = |id: &str| {
            snap.agents
                .iter()
                .find(|a| a.agent_id == id)
                .unwrap()
                .clone()
        };
        // usage-model 命中表 + 有 usage 事实 → Known 且 usd 有值
        let a1 = by_id("A1");
        assert_eq!(a1.cost_basis.as_deref(), Some("known"));
        assert!(a1.cost_usd.is_some(), "usage 命中 → usd 必有值");
        assert_eq!(a1.tokens_in, Some(100));
        assert_eq!(a1.tokens_out, Some(200));
        // sub-model 命中表且 billing=subscription → Subscription 且 usd=None
        let a2 = by_id("A2");
        assert_eq!(a2.cost_basis.as_deref(), Some("subscription"));
        assert_eq!(a2.cost_usd, None, "包月 → Subscription 且 usd=None");
        // ghost-model 命中表（usage billing）+ 有 usage 事实 → Known
        let a3 = by_id("A3");
        assert_eq!(a3.cost_basis.as_deref(), Some("known"));
        // 未在表中登记的模型 → Unknown 且 usd=None
        let unknown_events = vec![
            cost_sampled_event("T4", "never-listed", 5, 5, 0, 0),
            report_observed_event("T4", "never-listed"),
        ];
        let mut snap2 = build_snapshot(&SnapshotInputs {
            round_id: "r45".to_string(),
            generated_at: "2026-07-26T00:00:00Z".to_string(),
            events: unknown_events.clone(),
            agents: vec![mk("A4", "T4", probe(None, true, Some(Duration::from_secs(10))))],
            budget: BudgetInput {
                max_usd: Some(8.0),
                max_wall_minutes: Some(900),
                max_model_wakes: Some(24),
                spent_usd: 0.0,
                spent_wall_minutes: 0,
                spent_model_wakes: 0,
            },
            activity: Vec::new(),
            liveness_opts: opts(),
        });
        price_snapshot(&mut snap2, &table, &unknown_events);
        let a4 = snap2
            .agents
            .iter()
            .find(|a| a.agent_id == "A4")
            .unwrap()
            .clone();
        assert_eq!(a4.cost_basis.as_deref(), Some("unknown"));
        assert_eq!(a4.cost_usd, None, "未命中模型 → Unknown 且 usd=None");
    }

    fn cost_sampled_event(
        task: &str,
        model: &str,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
    ) -> EventRecord {
        EventRecord {
            event_id: format!("cs-{task}-{model}"),
            ts: "2026-07-26T00:00:10Z".to_string(),
            actor: "runtime:orch".to_string(),
            kind: "CostSampled".to_string(),
            task_id: Some(task.to_string()),
            round: Some("r45".to_string()),
            payload: Some(serde_json::json!({
                "usage": {
                    "input_tokens": input,
                    "output_tokens": output,
                    "cache_read_tokens": cache_read,
                    "cache_write_tokens": cache_write,
                },
                "model": model,
                "durationSecs": 30
            })),
            extra: serde_json::Map::new(),
        }
    }

    fn report_observed_event(task: &str, model: &str) -> EventRecord {
        EventRecord {
            event_id: format!("ro-{task}-{model}"),
            ts: "2026-07-26T00:00:20Z".to_string(),
            actor: "runtime:orch".to_string(),
            kind: "ReportObserved".to_string(),
            task_id: Some(task.to_string()),
            round: Some("r45".to_string()),
            payload: Some(serde_json::json!({
                "reportPath": format!("reports/{task}-REPORT.md"),
                "envDecl": {"model": model, "depth": "d", "capture": "c"}
            })),
            extra: serde_json::Map::new(),
        }
    }

    // ── tasks 真实投影：fold 8 态 + 门统计 ──
    #[test]
    fn tasks_project_from_events_via_fold() {
        let events = vec![
            dispatch_event("T1", "r45"),
            gate_event("T1", 0, 1000),
            gate_event("T1", 1, 2000),
            report_observed_event("T1", "m"),
            verdict_event("T1", "PASS", 0.42),
            merge_event("T1", "abc1234"),
            record_event("T1"),
        ];
        let snap = build_snapshot(&SnapshotInputs {
            round_id: "r45".to_string(),
            generated_at: "2026-07-26T00:00:00Z".to_string(),
            events,
            agents: Vec::new(),
            budget: BudgetInput {
                max_usd: None,
                max_wall_minutes: None,
                max_model_wakes: None,
                spent_usd: 0.0,
                spent_wall_minutes: 0,
                spent_model_wakes: 0,
            },
            activity: Vec::new(),
            liveness_opts: opts(),
        });
        assert_eq!(snap.tasks.len(), 1);
        let t = &snap.tasks[0];
        assert_eq!(t.task_id, "T1");
        assert_eq!(t.state, "recorded");
        assert_eq!(t.merge_sha.as_deref(), Some("abc1234"));
        assert_eq!(t.event_count, 7);
        assert_eq!(t.gates_green, 1);
        assert_eq!(t.gates_red, 1);
        assert_eq!(t.gates_total_ms, 3000);
        assert!(t.cost_usd.is_some());
        assert!(t.cost_usd.unwrap() - 0.42 < 1e-9);
    }

    // ── round.closed / kind_histogram 从 events 真算 ──
    #[test]
    fn round_closed_and_histogram_from_events() {
        let events = vec![
            dispatch_event("T1", "r45"),
            dispatch_event("T2", "r45"),
            orch_core::EventRecord {
                event_id: "rc".to_string(),
                ts: "2026-07-26T00:00:30Z".to_string(),
                actor: "runtime:orch".to_string(),
                kind: "RoundClosed".to_string(),
                task_id: None,
                round: Some("r45".to_string()),
                payload: None,
                extra: serde_json::Map::new(),
            },
        ];
        let snap = build_snapshot(&SnapshotInputs {
            round_id: "r45".to_string(),
            generated_at: "2026-07-26T00:00:00Z".to_string(),
            events,
            agents: Vec::new(),
            budget: BudgetInput {
                max_usd: None,
                max_wall_minutes: None,
                max_model_wakes: None,
                spent_usd: 0.0,
                spent_wall_minutes: 0,
                spent_model_wakes: 0,
            },
            activity: Vec::new(),
            liveness_opts: opts(),
        });
        assert!(snap.round.closed, "RoundClosed 事件后 round.closed 必须为 true");
        assert_eq!(snap.round.events, 3);
        assert_eq!(snap.round.bad_lines, None, "坏行无法从 events 反推：诚实 None");
        let hist = &snap.round.kind_histogram;
        assert_eq!(hist["DispatchIssued"], 2);
        assert_eq!(hist["RoundClosed"], 1);
    }

    fn dispatch_event(task: &str, round: &str) -> EventRecord {
        EventRecord {
            event_id: format!("di-{task}"),
            ts: "2026-07-26T00:00:00Z".to_string(),
            actor: "runtime:orch".to_string(),
            kind: "DispatchIssued".to_string(),
            task_id: Some(task.to_string()),
            round: Some(round.to_string()),
            payload: None,
            extra: serde_json::Map::new(),
        }
    }

    fn gate_event(task: &str, exit_code: i64, duration_ms: u64) -> EventRecord {
        EventRecord {
            event_id: format!("ge-{task}-{exit_code}-{duration_ms}"),
            ts: "2026-07-26T00:00:05Z".to_string(),
            actor: "runtime:orch".to_string(),
            kind: "GateExecuted".to_string(),
            task_id: Some(task.to_string()),
            round: Some("r45".to_string()),
            payload: Some(serde_json::json!({
                "commandRef": "testFast",
                "durationMs": duration_ms,
                "exitCode": exit_code
            })),
            extra: serde_json::Map::new(),
        }
    }

    fn verdict_event(task: &str, verdict: &str, cost_usd: f64) -> EventRecord {
        EventRecord {
            event_id: format!("vi-{task}-{verdict}"),
            ts: "2026-07-26T00:00:15Z".to_string(),
            actor: "verifier".to_string(),
            kind: "VerdictIssued".to_string(),
            task_id: Some(task.to_string()),
            round: Some("r45".to_string()),
            payload: Some(serde_json::json!({
                "verdict": verdict,
                "costUsd": cost_usd
            })),
            extra: serde_json::Map::new(),
        }
    }

    fn merge_event(task: &str, sha: &str) -> EventRecord {
        EventRecord {
            event_id: format!("me-{task}-{sha}"),
            ts: "2026-07-26T00:00:20Z".to_string(),
            actor: "runtime:orch".to_string(),
            kind: "MergeExecuted".to_string(),
            task_id: Some(task.to_string()),
            round: Some("r45".to_string()),
            payload: Some(serde_json::json!({"mergeSha": sha, "policy": "no-ff"})),
            extra: serde_json::Map::new(),
        }
    }

    fn record_event(task: &str) -> EventRecord {
        EventRecord {
            event_id: format!("tr-{task}"),
            ts: "2026-07-26T00:00:25Z".to_string(),
            actor: "runtime:orch".to_string(),
            kind: "TaskRecorded".to_string(),
            task_id: Some(task.to_string()),
            round: Some("r45".to_string()),
            payload: None,
            extra: serde_json::Map::new(),
        }
    }
}
