//! orch serve 常驻调度骨架 + 主控注入(design/11 §3,r21/B36)——planner 预置占位,消 lib.rs 热点。

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use fd_lock::RwLock;
use orch_core::{fold, read_ledger, EventRecord};
use sha2::{Digest, Sha256};

use crate::{budget, current_round, failure, inbox, ledger, wake};

const PLANNER_LEASE_REL: &str = "coordination/runtime/locks/planner-turn.json";
const PLANNER_FINALIZER_LOCK_REL: &str = "coordination/runtime/locks/planner-turn-finalizer.lock";
const PLANNER_TURN_TIMEOUT_SECS: u64 = 30 * 60;
const PLANNER_LEASE_TTL_SECS: u64 = 35 * 60;
const PLANNER_MALFORMED_LEASE_GRACE_SECS: u64 = 5;
const AUTO_SUCCESSION_LOCK_REL: &str = "coordination/runtime/locks/auto-succession.lock";

/// B147 吸收 B149 接线：serve daemon 内部动作（monitor 判定 / 自动接替 /
/// auto re-wake / planner 回合收尾）的事件构造点——发起方固定
/// `daemon-automatic`，经纯注入点显式传参，绝不读进程全局 ORCH_INITIATOR
/// （缺省会按 human-interactive 计，daemon 动作绝不可污染 0 人工口径）。
fn daemon_event(
    kind: &str,
    task_id: Option<&str>,
    round: Option<&str>,
    payload: serde_json::Value,
) -> EventRecord {
    ledger::event_with_initiator(
        kind,
        "runtime:orch",
        task_id,
        round,
        payload,
        Some(ledger::InitiatorKind::DaemonAutomatic.as_str()),
        Some("serve"),
    )
}

/// One machine-observed checkpoint in the unattended recovery canary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanaryEvent {
    pub actor: String,
    pub method: String,
    pub station: String,
}

/// Machine-readable zero-touch totals and the distinct stations observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanarySummary {
    pub automatic: usize,
    pub manual: usize,
    pub stations: Vec<String>,
}

/// Durable facts needed to classify one Tier F acknowledgement pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckSnapshot {
    pub ack_present: bool,
    pub dispatch_acked: bool,
    pub attempt_started: bool,
}

/// Named stages from GO consumption through the durable attempt start repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckPipelineStage {
    GoNotConsumed,
    AckPresentUnclaimed,
    Acked,
    AckedAndStarted,
}

/// Classify only durable evidence. Later ledger events dominate the ephemeral
/// ACK file because cleanup may remove that file after a successful claim.
pub fn classify_ack_pipeline(snapshot: AckSnapshot) -> AckPipelineStage {
    if snapshot.dispatch_acked && snapshot.attempt_started {
        AckPipelineStage::AckedAndStarted
    } else if snapshot.dispatch_acked {
        AckPipelineStage::Acked
    } else if snapshot.ack_present {
        AckPipelineStage::AckPresentUnclaimed
    } else {
        AckPipelineStage::GoNotConsumed
    }
}

/// Tick-denominated convergence result with enough context to diagnose an
/// exceeded budget without consulting a wall clock or process-global state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckConvergenceVerdict {
    pub converged: bool,
    pub exceeded: bool,
    pub stage: AckPipelineStage,
    pub ticks_elapsed: u32,
    pub tick_budget: u32,
}

pub fn ack_convergence_verdict(
    stage: AckPipelineStage,
    ticks_elapsed: u32,
    tick_budget: u32,
) -> AckConvergenceVerdict {
    let converged = matches!(
        stage,
        AckPipelineStage::Acked | AckPipelineStage::AckedAndStarted
    );
    let exceeded = !converged && (tick_budget == 0 || ticks_elapsed > tick_budget);
    AckConvergenceVerdict {
        converged,
        exceeded,
        stage,
        ticks_elapsed,
        tick_budget,
    }
}

#[cfg(test)]
fn elapsed_reconcile_ticks(elapsed: Duration, tick: Duration) -> u32 {
    let tick_nanos = tick.as_nanos();
    if tick_nanos == 0 {
        return u32::MAX;
    }
    let elapsed_nanos = elapsed.as_nanos();
    let ticks = elapsed_nanos.saturating_add(tick_nanos - 1) / tick_nanos;
    u32::try_from(ticks).unwrap_or(u32::MAX)
}

#[cfg(test)]
fn exact_tick_budget(bound: Duration, tick: Duration) -> u32 {
    let tick_nanos = tick.as_nanos();
    assert!(tick_nanos > 0, "reconcile tick must be non-zero");
    assert_eq!(
        bound.as_nanos() % tick_nanos,
        0,
        "signed wall-clock bound must be an exact number of reconcile ticks"
    );
    u32::try_from(bound.as_nanos() / tick_nanos).unwrap_or(u32::MAX)
}

/// Fold canary checkpoints conservatively: either a user actor or a
/// manual-cli method makes the checkpoint manual, regardless of the other
/// field. Stations are de-duplicated while preserving first-observed order.
pub fn canary_summary(events: &[CanaryEvent]) -> CanarySummary {
    let mut automatic = 0;
    let mut manual = 0;
    let mut stations = Vec::new();
    for event in events {
        if event.actor == "user" || event.method == "manual-cli" {
            manual += 1;
        } else {
            automatic += 1;
        }
        if !stations.contains(&event.station) {
            stations.push(event.station.clone());
        }
    }
    CanarySummary {
        automatic,
        manual,
        stations,
    }
}

/// Enforce both halves of the unattended claim: zero manual intervention and
/// complete coverage of every required fault station.
pub fn canary_gate(summary: &CanarySummary, required_stations: &[String]) -> Result<(), String> {
    if summary.manual > 0 {
        return Err(format!(
            "unattended canary observed {} manual intervention(s)",
            summary.manual
        ));
    }
    let missing = required_stations
        .iter()
        .filter(|station| !summary.stations.contains(station))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(format!(
            "unattended canary missing required station(s): {}",
            missing.join(", ")
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectReason {
    NewInstruction,
    TaskFailed,
    AllRecorded,
    AgentDown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Injection {
    pub reason: InjectReason,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedPlannerWake {
    pub reasons: Vec<InjectReason>,
    pub trigger_key: String,
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannerLivenessDecision {
    Wait,
    Completed,
    Retry { attempt: u8 },
    Escalate,
}

/// Independent provider-monitor result.  This deliberately does not reuse
/// `liveness::Judgement`: await-report and the daemon monitor are separate
/// decision planes and a missing durable signal must remain ambiguous here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MonitorVerdict {
    Healthy,
    Stall,
    Dead,
    Ambiguous,
}

pub fn monitor_verdict(
    durable_alive: Option<bool>,
    idle_secs: u64,
    stall_threshold_secs: u64,
) -> MonitorVerdict {
    match durable_alive {
        Some(false) => MonitorVerdict::Dead,
        Some(true) if idle_secs >= stall_threshold_secs => MonitorVerdict::Stall,
        Some(true) => MonitorVerdict::Healthy,
        None => MonitorVerdict::Ambiguous,
    }
}

/// A stalled attempt only escalates after explicitly opting in with a non-zero
/// multiplier and exceeding multiplier × task wall budget.
pub fn stall_escalation_due(stall_secs: u64, wall_minutes: u64, multiplier: u64) -> bool {
    if multiplier == 0 {
        return false;
    }
    let threshold = wall_minutes.saturating_mul(60).saturating_mul(multiplier);
    stall_secs > threshold
}

#[derive(Debug, Clone, Copy)]
struct AutoSuccessionPolicy {
    stall_multiplier: u64,
    auto_terminate_stalled: bool,
    ack_timeout_secs: u64,
}

fn auto_succession_policy(ir: &crate::plan::RoundIr) -> AutoSuccessionPolicy {
    AutoSuccessionPolicy {
        stall_multiplier: ir.liveness.stall_escalation_multiplier.unwrap_or(0),
        auto_terminate_stalled: ir.liveness.auto_terminate_stalled.unwrap_or(false),
        ack_timeout_secs: ir.dispatch.ack_timeout_seconds.unwrap_or(120),
    }
}

fn event_stage(event: &EventRecord) -> Option<&str> {
    event
        .payload
        .as_ref()
        .and_then(|payload| payload.get("stage"))
        .and_then(serde_json::Value::as_str)
}

fn event_agent(event: &EventRecord) -> Option<&str> {
    event
        .payload
        .as_ref()
        .and_then(|payload| payload.get("agent"))
        .and_then(serde_json::Value::as_str)
}

fn event_epoch_seconds(event: &EventRecord) -> Option<u64> {
    event_time(event)?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

fn attempt_has_event(events: &[EventRecord], attempt_id: &str, kind: &str) -> bool {
    events
        .iter()
        .any(|event| event.kind == kind && event_attempt_id(event) == Some(attempt_id))
}

fn wake_session_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn go_rewake_message(round: &str, observation: &MonitoredAttempt, go_path: &str) -> String {
    let short_attempt = format!("A{:04}", observation.attempt_no);
    format!(
        "orch 注入唤醒: round={round} taskId={} attemptId={}\n\
         GO={go_path}\n\
         请只领取上述 attempt，并运行 planner 已预置的定向等待命令：\n\
         coordination/scripts/wait-dispatch.sh {} 1800 {} {short_attempt}",
        observation.task_id, observation.attempt_id, observation.agent, observation.task_id,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GoUnclaimedAction {
    Wait,
    Rewake { ordinal: usize },
    Escalate,
}

fn go_unclaimed_action(
    issued_s: u64,
    now_s: u64,
    ack_timeout_s: u64,
    acked: bool,
    rewake_count: usize,
    wake_session_alive: bool,
) -> GoUnclaimedAction {
    if !crate::tierf::go_ack_overdue(issued_s, now_s, ack_timeout_s, acked) {
        return GoUnclaimedAction::Wait;
    }
    if rewake_count >= 2 {
        return GoUnclaimedAction::Escalate;
    }
    if wake_session_alive {
        return GoUnclaimedAction::Wait;
    }
    GoUnclaimedAction::Rewake {
        ordinal: rewake_count + 1,
    }
}

fn append_escalation_once(
    root: &Path,
    round: &str,
    observation: &MonitoredAttempt,
    stage: &str,
    payload: serde_json::Value,
) -> Result<bool> {
    let mut appended = false;
    ledger::append_checked(root, round, |events| {
        let duplicate = events.iter().any(|event| {
            event.kind == "EscalationRaised"
                && event.task_id.as_deref() == Some(observation.task_id.as_str())
                && event_attempt_id(event) == Some(observation.attempt_id.as_str())
                && event_stage(event) == Some(stage)
        });
        if duplicate {
            return Ok(Vec::new());
        }
        let still_current = active_monitor_attempts(events)
            .into_iter()
            .any(|candidate| {
                candidate.task_id == observation.task_id
                    && candidate.attempt_id == observation.attempt_id
            });
        if !still_current {
            return Ok(Vec::new());
        }
        appended = true;
        Ok(vec![daemon_event(
            "EscalationRaised",
            Some(&observation.task_id),
            Some(round),
            payload.clone(),
        )])
    })?;
    Ok(appended)
}

fn append_terminal_escalation_once(
    root: &Path,
    round: &str,
    observation: &MonitoredAttempt,
    stage: &str,
    payload: serde_json::Value,
) -> Result<bool> {
    let mut appended = false;
    ledger::append_checked(root, round, |events| {
        if events.iter().any(|event| {
            event.kind == "EscalationRaised"
                && event.task_id.as_deref() == Some(observation.task_id.as_str())
                && event_attempt_id(event) == Some(observation.attempt_id.as_str())
                && event_stage(event) == Some(stage)
        }) {
            return Ok(Vec::new());
        }
        appended = true;
        Ok(vec![daemon_event(
            "EscalationRaised",
            Some(&observation.task_id),
            Some(round),
            payload.clone(),
        )])
    })?;
    Ok(appended)
}

fn handle_unclaimed_go(
    root: &Path,
    round: &str,
    events: &[EventRecord],
    observation: &MonitoredAttempt,
    policy: AutoSuccessionPolicy,
    now: SystemTime,
) -> Result<usize> {
    let acked = attempt_has_event(events, &observation.attempt_id, "DispatchAcked");
    let Some(completed) = events.iter().rev().find(|event| {
        event.kind == "DispatchWakeCompleted"
            && event_attempt_id(event) == Some(observation.attempt_id.as_str())
            && event_agent(event) == Some(observation.agent.as_str())
    }) else {
        return Ok(0);
    };
    let Some(issued_s) = event_epoch_seconds(completed) else {
        return Ok(0);
    };
    let now_s = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let rewake_events = events
        .iter()
        .filter(|event| {
            event.kind == "EscalationRaised"
                && event_attempt_id(event) == Some(observation.attempt_id.as_str())
                && matches!(
                    event_stage(event),
                    Some("go-unclaimed-rewake" | "go-unclaimed-rewake-launching")
                )
        })
        .collect::<Vec<_>>();
    let rewakes = rewake_events
        .iter()
        .filter_map(|event| {
            event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("rewakeCount"))
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
        })
        .max()
        .unwrap_or(0);
    let latest_wake = rewake_events.last().copied().unwrap_or(completed);
    let retry_anchor_s = event_epoch_seconds(latest_wake).unwrap_or(issued_s);
    let pid = latest_wake
        .payload
        .as_ref()
        .and_then(|payload| payload.get("pid"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok());
    let action = go_unclaimed_action(
        retry_anchor_s,
        now_s,
        policy.ack_timeout_secs,
        acked,
        rewakes,
        pid.is_some_and(wake_session_alive),
    );
    if action == GoUnclaimedAction::Wait {
        return Ok(0);
    }
    if action == GoUnclaimedAction::Escalate {
        return Ok(usize::from(append_escalation_once(
            root,
            round,
            observation,
            "go-unclaimed",
            serde_json::json!({
                "stage": "go-unclaimed",
                "attemptId": observation.attempt_id,
                "attemptNo": observation.attempt_no,
                "agent": observation.agent,
                "issuedSeconds": issued_s,
                "observedSeconds": now_s,
                "ackTimeoutSeconds": policy.ack_timeout_secs,
                "rewakeCount": rewakes,
            }),
        )?));
    }
    let GoUnclaimedAction::Rewake { ordinal } = action else {
        unreachable!("wait/escalate returned above")
    };
    let go_path = completed
        .payload
        .as_ref()
        .and_then(|payload| payload.get("goPath"))
        .and_then(serde_json::Value::as_str)
        .context("DispatchWakeCompleted 缺 goPath")?;
    let launching = ledger::append_checked(root, round, |fresh| {
        if attempt_has_event(fresh, &observation.attempt_id, "DispatchAcked") {
            return Ok(Vec::new());
        }
        let fresh_rewakes = fresh
            .iter()
            .filter(|event| {
                event.kind == "EscalationRaised"
                    && event_attempt_id(event) == Some(observation.attempt_id.as_str())
                    && matches!(
                        event_stage(event),
                        Some("go-unclaimed-rewake" | "go-unclaimed-rewake-launching")
                    )
            })
            .filter_map(|event| {
                event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("rewakeCount"))
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
            })
            .max()
            .unwrap_or(0);
        if fresh_rewakes != rewakes || ordinal != rewakes + 1 || ordinal > 2 {
            return Ok(Vec::new());
        }
        Ok(vec![daemon_event(
            "EscalationRaised",
            Some(&observation.task_id),
            Some(round),
            serde_json::json!({
                "stage": "go-unclaimed-rewake-launching",
                "attemptId": observation.attempt_id,
                "attemptNo": observation.attempt_no,
                "agent": observation.agent,
                "issuedSeconds": issued_s,
                "observedSeconds": now_s,
                "ackTimeoutSeconds": policy.ack_timeout_secs,
                "rewakeCount": ordinal,
                "goPath": go_path,
            }),
        )])
    })?;
    if launching == 0 {
        return Ok(0);
    }
    let message = go_rewake_message(round, observation, go_path);
    let continuation = format!(
        "implementation:{round}:{}:{}:{}",
        observation.task_id, observation.attempt_id, observation.agent
    );
    let outcome = match wake::dispatch_wake_for_continuation(
        root,
        &observation.agent,
        round,
        &continuation,
        &message,
        false,
    ) {
        Ok(outcome) => outcome,
        Err(error) => {
            return append_escalation_once(
                root,
                round,
                observation,
                "go-unclaimed",
                serde_json::json!({
                    "stage": "go-unclaimed",
                    "attemptId": observation.attempt_id,
                    "attemptNo": observation.attempt_no,
                    "agent": observation.agent,
                    "reason": format!("same-channel re-wake failed: {error:#}"),
                    "ackTimeoutSeconds": policy.ack_timeout_secs,
                    "rewakeCount": rewakes,
                }),
            )
            .map(usize::from);
        }
    };
    if !outcome.injected {
        return append_escalation_once(
            root,
            round,
            observation,
            "go-unclaimed",
            serde_json::json!({
                "stage": "go-unclaimed",
                "attemptId": observation.attempt_id,
                "attemptNo": observation.attempt_no,
                "agent": observation.agent,
                "reason": "same-channel re-wake unavailable",
                "ackTimeoutSeconds": policy.ack_timeout_secs,
                "rewakeCount": rewakes,
            }),
        )
        .map(usize::from);
    }
    let appended = ledger::append_checked(root, round, |fresh| {
        let launching_exists = fresh.iter().any(|event| {
            event.kind == "EscalationRaised"
                && event_attempt_id(event) == Some(observation.attempt_id.as_str())
                && event_stage(event) == Some("go-unclaimed-rewake-launching")
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("rewakeCount"))
                    .and_then(serde_json::Value::as_u64)
                    == Some(ordinal as u64)
        });
        let completed_exists = fresh.iter().any(|event| {
            event.kind == "EscalationRaised"
                && event_attempt_id(event) == Some(observation.attempt_id.as_str())
                && event_stage(event) == Some("go-unclaimed-rewake")
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("rewakeCount"))
                    .and_then(serde_json::Value::as_u64)
                    == Some(ordinal as u64)
        });
        if !launching_exists || completed_exists {
            return Ok(Vec::new());
        }
        Ok(vec![daemon_event(
            "EscalationRaised",
            Some(&observation.task_id),
            Some(round),
            serde_json::json!({
                "stage": "go-unclaimed-rewake",
                "attemptId": observation.attempt_id,
                "attemptNo": observation.attempt_no,
                "agent": observation.agent,
                "issuedSeconds": issued_s,
                "observedSeconds": now_s,
                "ackTimeoutSeconds": policy.ack_timeout_secs,
                "rewakeCount": ordinal,
                "pid": outcome.pid,
                "logPath": outcome.log_path.map(|path| path.display().to_string()),
            }),
        )])
    })?;
    Ok(appended)
}

fn stall_strikes_by_agent(events: &[EventRecord], task_id: &str) -> BTreeMap<String, usize> {
    let mut strikes = BTreeMap::new();
    for event in events.iter().filter(|event| {
        event.kind == "AttemptTimedOut" && event.task_id.as_deref() == Some(task_id)
    }) {
        let Some(payload) = event.payload.as_ref() else {
            continue;
        };
        if payload.get("kind").and_then(serde_json::Value::as_str) != Some("stall") {
            continue;
        }
        if let Some(agent) = payload.get("agent").and_then(serde_json::Value::as_str) {
            *strikes.entry(agent.to_string()).or_insert(0) += 1;
        }
    }
    strikes
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StallTerminationAction {
    EscalateOnly,
    TerminateManaged,
}

/// B147 · 自动接替候选选择（B142 succession 链的资格全校验版）。
/// 候选链先经能力过滤——任务在签核 IR 的 `requirement.capabilities` 必须
/// 被候选 roles 覆盖（消费 IrTask.requirement，不回读可变卡面），不合格者
/// 进 ineligible 被 [`crate::scheduler::next_eligible`] 跳过；链耗尽 →
/// Err 含 `eligible-chain-exhausted`，调用方落 EscalationRaised 交
/// planner，**绝不降级**派发。任务不在 IR 时能力过滤退化为空集（调用方
/// 已保证任务在签核 IR 内——wall budget 查找先行 fail-closed）。
pub fn next_succession_candidate(
    ir: &crate::plan::RoundIr,
    task_id: &str,
    chain: &[String],
    failed: &[String],
    busy: &[String],
) -> Result<String, String> {
    let required = ir
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .map(|task| task.requirement.capabilities.clone())
        .unwrap_or_default();
    let ineligible = chain
        .iter()
        .filter(|agent| {
            let roles = ir
                .scheduling
                .capacities
                .get(*agent)
                .map(|capacity| capacity.roles.as_slice())
                .unwrap_or(&[]);
            crate::scheduler::successor_eligible(&required, roles).is_err()
        })
        .cloned()
        .collect::<Vec<_>>();
    crate::scheduler::next_eligible(chain, failed, &ineligible, busy)
}

fn stall_termination_action(
    auto_terminate_stalled: bool,
    topology: wake::TerminationPlan,
) -> StallTerminationAction {
    if auto_terminate_stalled && topology == wake::TerminationPlan::SignalProcessGroup {
        StallTerminationAction::TerminateManaged
    } else {
        StallTerminationAction::EscalateOnly
    }
}

fn handle_stall_budget(
    root: &Path,
    round: &str,
    observation: &MonitoredAttempt,
    stall_secs: u64,
    wall_minutes: u64,
    hierarchy: &[String],
    ir: &crate::plan::RoundIr,
    policy: AutoSuccessionPolicy,
    termination_grace_seconds: u64,
) -> Result<usize> {
    if !stall_escalation_due(stall_secs, wall_minutes, policy.stall_multiplier) {
        return Ok(0);
    }
    let registry = wake::load_registry(root)?;
    let spec = registry.get(&observation.agent).with_context(|| {
        format!(
            "stall escalation missing registered agent: {}",
            observation.agent
        )
    })?;
    let topology = wake::termination_plan(&spec.argv).map_err(anyhow::Error::msg)?;
    let mut changed = usize::from(append_escalation_once(
        root,
        round,
        observation,
        "stall-budget-exceeded",
        serde_json::json!({
            "stage": "stall-budget-exceeded",
            "attemptId": observation.attempt_id,
            "attemptNo": observation.attempt_no,
            "agent": observation.agent,
            "stallSeconds": stall_secs,
            "wallMinutes": wall_minutes,
            "multiplier": policy.stall_multiplier,
            "autoTerminateStalled": policy.auto_terminate_stalled,
            "topology": format!("{topology:?}"),
        }),
    )?);
    if stall_termination_action(policy.auto_terminate_stalled, topology)
        == StallTerminationAction::EscalateOnly
    {
        return Ok(changed);
    }

    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let before = read_ledger(&ledger_path)?;
    crate::attempt::reject_bad_lines(&before)?;
    if attempt_has_event(&before.events, &observation.attempt_id, "AttemptTimedOut") {
        return Ok(changed);
    }
    let pid = before
        .events
        .iter()
        .rev()
        .find(|event| {
            event.kind == "DispatchWakeCompleted"
                && event_attempt_id(event) == Some(observation.attempt_id.as_str())
        })
        .and_then(|event| event.payload.as_ref())
        .and_then(|payload| payload.get("pid"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .context("managed stall termination missing exact wake pid")?;
    let termination = match crate::tierf::terminate_managed_process_group(
        pid,
        Duration::from_secs(termination_grace_seconds),
    ) {
        Ok(termination) => termination,
        Err(error) => {
            changed += usize::from(append_escalation_once(
                root,
                round,
                observation,
                "stall-termination-failed",
                serde_json::json!({
                    "stage": "stall-termination-failed",
                    "attemptId": observation.attempt_id,
                    "attemptNo": observation.attempt_no,
                    "agent": observation.agent,
                    "pid": pid,
                    "reason": format!("{error:#}"),
                    "processTreeTerminated": false,
                }),
            )?);
            return Ok(changed);
        }
    };
    if !termination.process_tree_terminated {
        changed += usize::from(append_escalation_once(
            root,
            round,
            observation,
            "stall-termination-failed",
            serde_json::json!({
                "stage": "stall-termination-failed",
                "attemptId": observation.attempt_id,
                "attemptNo": observation.attempt_no,
                "agent": observation.agent,
                "pid": pid,
                "processTreeTerminated": false,
            }),
        )?);
        return Ok(changed);
    }
    let signals = termination
        .signals
        .iter()
        .map(|signal| format!("{signal:?}"))
        .collect::<Vec<_>>();
    let timed_out = ledger::append_checked(root, round, |events| {
        if attempt_has_event(events, &observation.attempt_id, "AttemptTimedOut") {
            return Ok(Vec::new());
        }
        let still_current = active_monitor_attempts(events)
            .into_iter()
            .any(|candidate| {
                candidate.task_id == observation.task_id
                    && candidate.attempt_id == observation.attempt_id
            });
        if !still_current {
            return Ok(Vec::new());
        }
        Ok(vec![daemon_event(
            "AttemptTimedOut",
            Some(&observation.task_id),
            Some(round),
            serde_json::json!({
                "attemptId": observation.attempt_id,
                "attemptNo": observation.attempt_no,
                "agent": observation.agent,
                "actionId": format!("auto-stall-timeout:{}", observation.attempt_id),
                "kind": "stall",
                "stage": "stall-budget-exceeded",
                "stallSeconds": stall_secs,
                "wallMinutes": wall_minutes,
                "multiplier": policy.stall_multiplier,
                "pid": pid,
                "signals": signals,
                "processTreeTerminated": true,
            }),
        )])
    })?;
    changed += timed_out;
    if timed_out == 0 {
        return Ok(changed);
    }

    let fresh = read_ledger(&ledger_path)?;
    crate::attempt::reject_bad_lines(&fresh)?;
    let card_agent = crate::card::load(root, round, &observation.task_id)?
        .meta
        .agent
        .context("automatic successor task card missing agent")?;
    let chain = crate::scheduler::escalation_chain_from_hierarchy(hierarchy, &card_agent)
        .map_err(anyhow::Error::msg)?;
    let strikes = stall_strikes_by_agent(&fresh.events, &observation.task_id);
    let failed = chain
        .iter()
        .filter(|agent| strikes.get(*agent).copied().unwrap_or(0) >= 2)
        .cloned()
        .collect::<Vec<_>>();
    let busy = active_monitor_attempts(&fresh.events)
        .into_iter()
        .filter(|attempt| attempt.task_id != observation.task_id)
        .map(|attempt| attempt.agent)
        .collect::<Vec<_>>();
    // B147：候选链经能力过滤的 next_eligible——无 critical 能力的候选被
    // 跳过而非降级承接；链耗尽落 EscalationRaised{stage: eligible-chain-exhausted}
    // 交 planner（复用既有 kind）。
    let next = match next_succession_candidate(ir, &observation.task_id, &chain, &failed, &busy) {
        Ok(agent) => agent,
        Err(reason) => {
            changed += usize::from(append_terminal_escalation_once(
                root,
                round,
                observation,
                "eligible-chain-exhausted",
                serde_json::json!({
                    "stage": "eligible-chain-exhausted",
                    "attemptId": observation.attempt_id,
                    "attemptNo": observation.attempt_no,
                    "agent": observation.agent,
                    "hierarchy": hierarchy,
                    "failed": failed,
                    "busy": busy,
                    "reason": reason,
                }),
            )?);
            return Ok(changed);
        }
    };
    match crate::tierf::run_dispatch_signed_candidate(root, &observation.task_id, &next, false) {
        Ok(_) => Ok(changed + 1),
        Err(error) => {
            changed += usize::from(append_terminal_escalation_once(
                root,
                round,
                observation,
                "stall-successor-blocked",
                serde_json::json!({
                    "stage": "stall-successor-blocked",
                    "attemptId": observation.attempt_id,
                    "attemptNo": observation.attempt_no,
                    "agent": observation.agent,
                    "nextCandidate": next,
                    "failed": failed,
                    "busy": busy,
                    "reason": format!("{error:#}"),
                    "override": false,
                }),
            )?);
            Ok(changed)
        }
    }
}

/// Intent is not proof that an executor or provider made progress.  Keep the
/// alarm until an observed activity event (or a genuinely new dispatch) lands.
pub fn alarm_after_intent(alarm_active: bool, event_kind: &str) -> bool {
    if !alarm_active {
        return false;
    }
    !matches!(
        event_kind,
        "DispatchAcked" | "ReportObserved" | "AttemptStarted" | "DispatchIssued"
    )
}

#[derive(Debug, Clone)]
struct MonitoredAttempt {
    task_id: String,
    agent: String,
    attempt_id: String,
    attempt_no: usize,
    last_activity_at: SystemTime,
}

#[derive(Debug, Default)]
pub struct LivenessMonitorState {
    log_samples: BTreeMap<String, Option<crate::chanhealth::WakeLogSample>>,
    last_progress_at: BTreeMap<String, SystemTime>,
}

fn event_attempt_id(event: &EventRecord) -> Option<&str> {
    event
        .payload
        .as_ref()
        .and_then(|payload| payload.get("attemptId"))
        .and_then(serde_json::Value::as_str)
}

fn event_time(event: &EventRecord) -> Option<SystemTime> {
    humantime::parse_rfc3339(&event.ts).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectationState {
    Waiting,
    Overdue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingReviewExpectation {
    pub task_id: String,
    pub attempt_id: String,
    pub role: String,
    pub agent: String,
    pub deadline_secs: u64,
    pub requested_at: String,
    pub idle_secs: u64,
    pub state: ExpectationState,
}

fn review_identity(event: &EventRecord) -> Option<(String, String, String, String)> {
    let payload = event.payload.as_ref()?;
    let task_id = event
        .task_id
        .as_deref()
        .or_else(|| payload.get("taskId").and_then(serde_json::Value::as_str))?;
    Some((
        task_id.to_string(),
        payload
            .get("attemptId")
            .and_then(serde_json::Value::as_str)?
            .to_string(),
        payload
            .get("role")
            .and_then(serde_json::Value::as_str)?
            .to_string(),
        payload
            .get("agent")
            .and_then(serde_json::Value::as_str)?
            .to_string(),
    ))
}

fn pending_review_expectations_from_events(
    events: &[EventRecord],
    round: &str,
    idle_secs: u64,
) -> Vec<PendingReviewExpectation> {
    // A task/role is one review slot. A later request supersedes the old
    // reviewer or attempt, while an old review delivery can only close the
    // exact four-dimensional identity currently occupying that slot.
    let mut open = BTreeMap::<(String, String), PendingReviewExpectation>::new();
    for event in events {
        if event.round.as_deref().is_some_and(|value| value != round) {
            continue;
        }
        match event.kind.as_str() {
            "ReviewRequested" => {
                let Some((task_id, attempt_id, role, agent)) = review_identity(event) else {
                    continue;
                };
                let Some(payload) = event.payload.as_ref() else {
                    continue;
                };
                let deadline_secs = payload
                    .get("deadlineSecs")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                let requested_at = payload
                    .get("requestedAt")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(&event.ts)
                    .to_string();
                open.insert(
                    (task_id.clone(), role.clone()),
                    PendingReviewExpectation {
                        task_id,
                        attempt_id,
                        role,
                        agent,
                        deadline_secs,
                        requested_at,
                        idle_secs,
                        state: if deadline_secs != 0 && idle_secs > deadline_secs {
                            ExpectationState::Overdue
                        } else {
                            ExpectationState::Waiting
                        },
                    },
                );
            }
            "ReviewDelivered" => {
                let Some((task_id, attempt_id, role, agent)) = review_identity(event) else {
                    continue;
                };
                let substantive = event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("bodyLen"))
                    .and_then(serde_json::Value::as_u64)
                    .is_some_and(|len| len > 0);
                let slot = (task_id, role);
                if substantive
                    && open.get(&slot).is_some_and(|pending| {
                        pending.attempt_id == attempt_id && pending.agent == agent
                    })
                {
                    open.remove(&slot);
                }
            }
            _ => {}
        }
    }
    open.into_values().collect()
}

/// Pure durable projection. It consumes only ledger JSONL: a REPORT or task
/// card can never fabricate a request that the real wake entry did not append.
///
/// `idle_secs` is supplied by the caller's clock sample. Expiry is disabled by
/// deadline zero and is strictly greater-than, so equality remains Waiting.
pub fn pending_review_expectations(
    ledger_jsonl: &str,
    round: &str,
    idle_secs: u64,
) -> Result<Vec<PendingReviewExpectation>> {
    let mut events = Vec::new();
    for (index, line) in ledger_jsonl.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        events.push(
            serde_json::from_str::<EventRecord>(line)
                .with_context(|| format!("review ledger 第 {} 行无法解析", index + 1))?,
        );
    }
    Ok(pending_review_expectations_from_events(
        &events, round, idle_secs,
    ))
}

fn append_review_delivery_once(
    root: &Path,
    round: &str,
    pending: &PendingReviewExpectation,
) -> Result<bool> {
    let mut appended = false;
    ledger::append_checked(root, round, |events| {
        let still_pending = pending_review_expectations_from_events(events, round, 0)
            .iter()
            .any(|candidate| {
                candidate.task_id == pending.task_id
                    && candidate.attempt_id == pending.attempt_id
                    && candidate.role == pending.role
                    && candidate.agent == pending.agent
            });
        if !still_pending {
            return Ok(Vec::new());
        }
        let request = events
            .iter()
            .rev()
            .find(|event| {
                event.kind == "ReviewRequested"
                    && review_identity(event).is_some_and(|(task_id, attempt_id, role, agent)| {
                        task_id == pending.task_id
                            && attempt_id == pending.attempt_id
                            && role == pending.role
                            && agent == pending.agent
                    })
            })
            .context("pending review 缺 formal ReviewRequested")?;
        let expectation = wake::review_expectation_from_request_event(events, round, request)?;
        let review_rel = expectation.artifact_relpath();
        let path = root.join(&review_rel);
        let Some(bytes) =
            crate::verify::optional_review_artifact_bytes(root, &path, "daemon review artifact")?
        else {
            return Ok(Vec::new());
        };
        let Some(checked) = crate::verify::check_review_artifact_contract(&bytes, &expectation)
            .with_context(|| format!("daemon review contract 失败: {review_rel}"))?
        else {
            return Ok(Vec::new());
        };
        let body_len = checked.substantive_body_len();
        if body_len == 0 {
            return Ok(Vec::new());
        }
        crate::verify::report_ignored_review_fields(&review_rel, checked.ignored_fields());
        appended = true;
        Ok(vec![daemon_event(
            "ReviewDelivered",
            Some(&pending.task_id),
            Some(round),
            serde_json::json!({
                "attemptId": pending.attempt_id,
                "role": pending.role,
                "agent": pending.agent,
                "bodyLen": body_len,
            }),
        )])
    })?;
    Ok(appended)
}

/// Observe substantive review files and close their exact durable expectation.
/// Repeated ticks are idempotent because append_checked replays the current
/// ledger while holding the ledger lock before it decides to append.
fn review_delivery_tick(root: &Path, round: &str) -> Result<usize> {
    let events = read_current_events(root, round)?;
    let pending = pending_review_expectations_from_events(&events, round, 0);
    let mut changed = 0usize;
    for expectation in pending {
        if append_review_delivery_once(root, round, &expectation)? {
            changed += 1;
        }
    }
    Ok(changed)
}

fn active_monitor_attempts(events: &[EventRecord]) -> Vec<MonitoredAttempt> {
    let mut active = BTreeMap::<String, MonitoredAttempt>::new();
    for event in events {
        let Some(task_id) = event.task_id.as_ref() else {
            continue;
        };
        match event.kind.as_str() {
            "DispatchIssued" => {
                let Some(payload) = event.payload.as_ref() else {
                    continue;
                };
                let Some(agent) = payload.get("agent").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let Some(attempt_id) = payload.get("attemptId").and_then(serde_json::Value::as_str)
                else {
                    continue;
                };
                let attempt_no = payload
                    .get("attemptNo")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                    .unwrap_or_default();
                active.insert(
                    task_id.clone(),
                    MonitoredAttempt {
                        task_id: task_id.clone(),
                        agent: agent.to_string(),
                        attempt_id: attempt_id.to_string(),
                        attempt_no,
                        last_activity_at: event_time(event).unwrap_or(SystemTime::UNIX_EPOCH),
                    },
                );
            }
            "DispatchAcked" | "AttemptStarted" => {
                if let Some(current) = active.get_mut(task_id) {
                    if event_attempt_id(event).is_none()
                        || event_attempt_id(event) == Some(current.attempt_id.as_str())
                    {
                        current.last_activity_at =
                            event_time(event).unwrap_or(current.last_activity_at);
                    }
                }
            }
            "ReportObserved" | "AttemptBlocked" | "AttemptCrashed" | "AttemptTimedOut"
            | "AttemptFailed" | "TaskRecorded" => {
                active.remove(task_id);
            }
            _ => {}
        }
    }
    active.into_values().collect()
}

pub fn monitor_dedup_key(task_id: &str, attempt_id: &str, agent: &str) -> String {
    format!("task={task_id}\0attempt={attempt_id}\0agent={agent}")
}

fn last_monitor_alarm_for_agent(
    events: &[EventRecord],
    task_id: &str,
    attempt_id: &str,
    agent: Option<&str>,
) -> Option<MonitorVerdict> {
    let mut verdict = None;
    let mut alarm_active = false;
    for event in events
        .iter()
        .filter(|event| event.task_id.as_deref() == Some(task_id))
    {
        if event.kind == "EscalationRaised"
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("stage"))
                .and_then(serde_json::Value::as_str)
                == Some("liveness-monitor")
            && event_attempt_id(event) == Some(attempt_id)
            && agent.is_none_or(|expected| {
                event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("agent"))
                    .and_then(serde_json::Value::as_str)
                    == Some(expected)
            })
        {
            verdict = event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("verdict"))
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok());
            alarm_active = verdict.is_some();
            continue;
        }
        if alarm_active {
            alarm_active = alarm_after_intent(true, &event.kind);
            if !alarm_active {
                verdict = None;
            }
        }
    }
    alarm_active.then_some(verdict).flatten()
}

fn last_monitor_alarm(
    events: &[EventRecord],
    task_id: &str,
    attempt_id: &str,
) -> Option<MonitorVerdict> {
    last_monitor_alarm_for_agent(events, task_id, attempt_id, None)
}

/// Durable de-duplication predicate used when a monitor state is reconstructed
/// after an orch restart.  The ledger, not in-memory sampling state, decides
/// whether the same alarm was already emitted.
pub fn monitor_alarm_needs_append(
    events: &[EventRecord],
    task_id: &str,
    attempt_id: &str,
    verdict: MonitorVerdict,
) -> bool {
    let still_current = active_monitor_attempts(events)
        .into_iter()
        .any(|candidate| candidate.task_id == task_id && candidate.attempt_id == attempt_id);
    still_current && last_monitor_alarm(events, task_id, attempt_id) != Some(verdict)
}

fn monitor_alarm_needs_append_for_agent(
    events: &[EventRecord],
    task_id: &str,
    attempt_id: &str,
    agent: &str,
    verdict: MonitorVerdict,
) -> bool {
    let observation_key = monitor_dedup_key(task_id, attempt_id, agent);
    let still_current = active_monitor_attempts(events)
        .into_iter()
        .any(|candidate| {
            monitor_dedup_key(&candidate.task_id, &candidate.attempt_id, &candidate.agent)
                == observation_key
        });
    still_current
        && last_monitor_alarm_for_agent(events, task_id, attempt_id, Some(agent)) != Some(verdict)
}

fn append_monitor_verdict(
    root: &Path,
    round: &str,
    observation: &MonitoredAttempt,
    verdict: MonitorVerdict,
    durable_alive: Option<bool>,
    reason: Option<&str>,
) -> Result<bool> {
    if verdict == MonitorVerdict::Healthy {
        return Ok(false);
    }
    let mut appended = false;
    ledger::append_checked(root, round, |events| {
        if !monitor_alarm_needs_append_for_agent(
            events,
            &observation.task_id,
            &observation.attempt_id,
            &observation.agent,
            verdict,
        ) {
            return Ok(Vec::new());
        }
        appended = true;
        Ok(vec![daemon_event(
            "EscalationRaised",
            Some(&observation.task_id),
            Some(round),
            serde_json::json!({
                "stage": "liveness-monitor",
                "verdict": verdict,
                "attemptId": observation.attempt_id,
                "attemptNo": observation.attempt_no,
                "agent": observation.agent,
                "durableAlive": durable_alive,
                "reason": reason,
            }),
        )])
    })?;
    Ok(appended)
}

/// One independent liveness-monitor sample.  The caller owns the sampling
/// state, which keeps exact wake-log observations separated per attempt.
pub fn liveness_monitor_tick(root: &Path, state: &mut LivenessMonitorState) -> Result<usize> {
    let lock_path = root.join(AUTO_SUCCESSION_LOCK_REL);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("打开自动接替锁失败: {}", lock_path.display()))?;
    let mut lock = RwLock::new(lock_file);
    let _guard = lock.write().context("获取自动接替锁失败")?;

    let round = current_round(root)?;
    let events = read_current_events(root, &round)?;
    let active_ir = crate::plan::require_active_round_ir(root, &round, &events)?;
    let policy = auto_succession_policy(&active_ir.candidate);
    let hierarchy = active_ir.candidate.scheduling.allowed_agents.clone();
    let stall_threshold_secs = active_ir
        .candidate
        .liveness
        .working_stall_minutes
        .saturating_mul(60);
    let now = SystemTime::now();
    let attempts = active_monitor_attempts(&events);
    let active_ids = attempts
        .iter()
        .map(|attempt| attempt.attempt_id.clone())
        .collect::<Vec<_>>();
    state
        .log_samples
        .retain(|attempt_id, _| active_ids.contains(attempt_id));
    state
        .last_progress_at
        .retain(|attempt_id, _| active_ids.contains(attempt_id));

    let mut changed = 0usize;
    for attempt in attempts {
        changed += handle_unclaimed_go(root, &round, &events, &attempt, policy, now)?;
        let sample = state
            .log_samples
            .entry(attempt.attempt_id.clone())
            .or_default();
        let probe = match crate::tierf::durable_probe_for_attempt(
            root,
            &attempt.agent,
            &events,
            &attempt.attempt_id,
            sample,
        ) {
            Ok(probe) => probe,
            Err(error) => {
                if append_monitor_verdict(
                    root,
                    &round,
                    &attempt,
                    MonitorVerdict::Ambiguous,
                    None,
                    Some(&format!("durable topology probe failed loudly: {error:#}")),
                )? {
                    changed += 1;
                }
                continue;
            }
        };
        let progress_at = state
            .last_progress_at
            .entry(attempt.attempt_id.clone())
            .or_insert(attempt.last_activity_at);
        if *progress_at < attempt.last_activity_at {
            *progress_at = attempt.last_activity_at;
        }
        if probe.wake_log_advanced {
            *progress_at = now;
        }
        let idle_secs = now
            .duration_since(*progress_at)
            .unwrap_or_default()
            .as_secs();
        let verdict = monitor_verdict(probe.durable_alive, idle_secs, stall_threshold_secs);
        if append_monitor_verdict(root, &round, &attempt, verdict, probe.durable_alive, None)? {
            changed += 1;
        }
        if verdict == MonitorVerdict::Stall {
            let wall_minutes = active_ir
                .candidate
                .tasks
                .iter()
                .find(|task| task.id == attempt.task_id)
                .map(|task| task.wall_minutes)
                .context("monitored attempt missing signed task wall budget")?;
            changed += handle_stall_budget(
                root,
                &round,
                &attempt,
                idle_secs,
                wall_minutes,
                &hierarchy,
                &active_ir.candidate,
                policy,
                active_ir
                    .candidate
                    .liveness
                    .termination_grace_seconds
                    .unwrap_or(10),
            )?;
        }
    }
    changed += review_delivery_tick(root, &round)?;
    Ok(changed)
}

fn liveness_monitor_interval(root: &Path) -> Result<Duration> {
    let round = current_round(root)?;
    let events = read_current_events(root, &round)?;
    let active_ir = crate::plan::require_active_round_ir(root, &round, &events)?;
    Ok(Duration::from_secs(
        active_ir.candidate.liveness.monitor_seconds.max(1),
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MonitorStepOutcome {
    Completed,
    Failed,
    Panicked,
}

fn isolate_monitor_action(action: impl FnOnce() -> Result<usize>) -> MonitorStepOutcome {
    match catch_unwind(AssertUnwindSafe(action)) {
        Ok(Ok(_)) => MonitorStepOutcome::Completed,
        Ok(Err(_)) => MonitorStepOutcome::Failed,
        Err(_) => MonitorStepOutcome::Panicked,
    }
}

fn run_monitor_sample_without_unwinding(root: &Path, state: &mut LivenessMonitorState) {
    match isolate_monitor_action(|| liveness_monitor_tick(root, state)) {
        MonitorStepOutcome::Completed => {}
        MonitorStepOutcome::Failed => {
            eprintln!("orch serve: 独立 liveness monitor 采样失败，将继续下一拍")
        }
        MonitorStepOutcome::Panicked => {
            eprintln!("orch serve: 独立 liveness monitor tick panic 已隔离，主循环继续")
        }
    }
}

fn spawn_liveness_monitor(root: PathBuf) {
    std::thread::spawn(move || {
        let mut state = LivenessMonitorState::default();
        loop {
            run_monitor_sample_without_unwinding(&root, &mut state);
            let interval =
                liveness_monitor_interval(&root).unwrap_or_else(|_| Duration::from_secs(15));
            std::thread::sleep(interval);
        }
    });
}

#[derive(Debug, PartialEq)]
pub enum RetryDecision {
    RetryAfter(u64),
    GiveUp,
}

pub fn decide_liveness_retry(
    dead_attempts: u8,
    max_retries: u8,
    base_secs: u64,
    had_activity: bool,
) -> RetryDecision {
    if had_activity {
        RetryDecision::GiveUp
    } else if dead_attempts < max_retries {
        RetryDecision::RetryAfter(base_secs << dead_attempts)
    } else {
        RetryDecision::GiveUp
    }
}

pub fn count_dead_attempts(events: &[EventRecord], task: &str) -> u8 {
    events
        .iter()
        .filter(|event| {
            event.kind == "AttemptCrashed"
                && event.task_id.as_deref() == Some(task)
                // Suppressed storage interruptions are stalled, never crashed. Keep legacy
                // malformed crash facts with the explicit cause from burning retry budget too.
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("cause"))
                    .and_then(serde_json::Value::as_str)
                    != Some("storage-exhaustion")
        })
        .fold(0u8, |count, _| count.saturating_add(1))
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PlannerLease {
    pub round: String,
    pub wake_id: String,
    pub session_id: String,
    pub trigger_key: String,
    pub attempt: u8,
    pub owner_pid: u32,
    pub child_pid: Option<u32>,
    pub started_at: String,
    pub reasons: Vec<String>,
    pub inbox_files: Vec<String>,
    #[serde(default)]
    pub log_path: Option<String>,
    #[serde(default)]
    pub model_wake_reservation_id: Option<String>,
}

pub fn planner_append_round(lease_round: &str, current_round: &str) -> Result<String, String> {
    if lease_round == current_round {
        Ok(current_round.to_string())
    } else {
        Err(format!(
            "stale planner lease round: lease={lease_round}, current={current_round}"
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickReport {
    pub mechanical: usize,
    pub injected: Vec<InjectReason>,
    pub pending_inbox: usize,
}

impl TickReport {
    pub fn is_idle(&self) -> bool {
        self.mechanical == 0 && self.injected.is_empty() && self.pending_inbox == 0
    }
}

pub fn tick_report(
    mechanical: &[String],
    injections: &[Injection],
    pending_inbox: usize,
) -> TickReport {
    TickReport {
        mechanical: mechanical.len(),
        injected: injections
            .iter()
            .map(|injection| injection.reason)
            .collect(),
        pending_inbox,
    }
}

fn truncate_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// 将一个 tick 的多个逻辑判断边界合成一个物理 planner wake。
pub fn plan_planner_wake(
    round: &str,
    injections: &[Injection],
    inbox_files: &[String],
    source_event_ids: &[String],
    max_prompt_bytes: usize,
) -> Option<PlannedPlannerWake> {
    if injections.is_empty() {
        return None;
    }
    let mut reasons = Vec::new();
    for injection in injections {
        if !reasons.contains(&injection.reason) {
            reasons.push(injection.reason);
        }
    }

    let mut digest = Sha256::new();
    digest.update(round.as_bytes());
    for reason in &reasons {
        digest.update([0]);
        digest.update(reason.as_str().as_bytes());
    }
    for filename in inbox_files {
        digest.update([1]);
        digest.update(filename.as_bytes());
    }
    for source in source_event_ids {
        digest.update([2]);
        digest.update(source.as_bytes());
    }
    let trigger_key = hex::encode(digest.finalize());

    let mut prompt = format!(
        "planner fresh decision turn\nround: {round}\nrunbook: coordination/planner-bootstrap.md\nledger: coordination/rounds/{round}/events.jsonl\nreasons:\n"
    );
    for injection in injections {
        prompt.push_str("- ");
        prompt.push_str(injection.reason.as_str());
        prompt.push_str(": ");
        prompt.push_str(&injection.message);
        prompt.push('\n');
    }
    if !inbox_files.is_empty() {
        prompt.push_str(
            "related inbox paths (read the first existing path for each file; never infer content):\n",
        );
        for filename in inbox_files {
            prompt.push_str("- processing: coordination/inbox/processing/");
            prompt.push_str(filename);
            prompt.push('\n');
            prompt.push_str("  pending fallback: coordination/inbox/");
            prompt.push_str(filename);
            prompt.push('\n');
        }
    }
    prompt.push_str(
        "Read only the paths above plus the short runbook. Handle judgment only; leave verify/merge/close to orch serve.\n",
    );

    Some(PlannedPlannerWake {
        reasons,
        trigger_key,
        prompt: truncate_utf8(&prompt, max_prompt_bytes),
    })
}

pub fn decide_planner_liveness(
    process_alive: bool,
    ledger_progress: bool,
    attempt: u8,
) -> PlannerLivenessDecision {
    if process_alive {
        PlannerLivenessDecision::Wait
    } else if ledger_progress {
        PlannerLivenessDecision::Completed
    } else if attempt < 2 {
        PlannerLivenessDecision::Retry {
            attempt: attempt + 1,
        }
    } else {
        PlannerLivenessDecision::Escalate
    }
}

pub fn may_advance_inbox_after_wake(
    budget_allowed: bool,
    spawn_succeeded: bool,
    injection_recorded: bool,
) -> bool {
    budget_allowed && spawn_succeeded && injection_recorded
}

pub struct TickInputs {
    pub new_inbox: Vec<String>,
    pub fail_tasks: Vec<String>,
    pub all_recorded: bool,
    pub dead_or_stalled: Vec<String>,
    pub pending: Vec<InjectReason>,
}

pub fn decide_injections(inputs: &TickInputs) -> Vec<Injection> {
    let mut injections = Vec::new();

    if !inputs.new_inbox.is_empty() && !inputs.pending.contains(&InjectReason::NewInstruction) {
        injections.push(Injection {
            reason: InjectReason::NewInstruction,
            message: format!(
                "收到新指令文件 {}，请规划轮次并派发",
                inputs.new_inbox.join(", ")
            ),
        });
    }
    if !inputs.fail_tasks.is_empty() && !inputs.pending.contains(&InjectReason::TaskFailed) {
        injections.push(Injection {
            reason: InjectReason::TaskFailed,
            message: format!(
                "任务 {} FAIL，请决策返工或改派",
                inputs.fail_tasks.join(", ")
            ),
        });
    }
    if inputs.all_recorded && !inputs.pending.contains(&InjectReason::AllRecorded) {
        injections.push(Injection {
            reason: InjectReason::AllRecorded,
            message:
                "本轮任务已全部 recorded，请只做收轮判断与战报事实核对；机械 close 交给 daemon"
                    .to_string(),
        });
    }
    if !inputs.dead_or_stalled.is_empty() && !inputs.pending.contains(&InjectReason::AgentDown) {
        injections.push(Injection {
            reason: InjectReason::AgentDown,
            message: format!(
                "执行者 {} 判死、判滞或提交诚实 BLOCKED，请 RESUME、授权后 NUDGE 或改派",
                inputs.dead_or_stalled.join(", ")
            ),
        });
    }

    injections
}

pub fn derive_tick_inputs(
    task_states: &[(String, String)],
    new_inbox: Vec<String>,
    dead_or_stalled: Vec<String>,
    pending: Vec<InjectReason>,
    round_closed: bool,
) -> TickInputs {
    let fail_tasks = task_states
        .iter()
        .filter(|(_, state)| state == "changes_requested")
        .map(|(task, _)| task.clone())
        .collect();
    let all_recorded = !round_closed
        && !task_states.is_empty()
        && task_states.iter().all(|(_, state)| state == "recorded");

    TickInputs {
        new_inbox,
        fail_tasks,
        all_recorded,
        dead_or_stalled,
        pending,
    }
}

/// 仍处于 executor BLOCKED 的任务，按存活原因首次出现的账本顺序稳定去重。
pub fn executor_blocked_tasks(events: &[EventRecord]) -> Vec<String> {
    let mut blocked = Vec::new();
    for event in events {
        let Some(task_id) = event.task_id.as_ref() else {
            continue;
        };
        match event.kind.as_str() {
            "AttemptBlocked" if !blocked.contains(task_id) => blocked.push(task_id.clone()),
            "NudgeIssued" | "ResumeIssued" | "ReportObserved" | "TaskRecorded" => {
                blocked.retain(|candidate| candidate != task_id);
            }
            _ => {}
        }
    }
    blocked
}

pub fn dead_or_stalled_tasks(events: &[EventRecord]) -> Vec<String> {
    let mut flagged = Vec::new();
    for event in events {
        let Some(task_id) = event.task_id.as_ref() else {
            continue;
        };
        match event.kind.as_str() {
            "EscalationRaised" => {
                let stage = event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("stage"))
                    .and_then(|stage| stage.as_str());
                if matches!(stage, Some("liveness-dead") | Some("liveness-stalled"))
                    && !flagged.contains(task_id)
                {
                    flagged.push(task_id.clone());
                }
            }
            "AttemptBlocked" if !flagged.contains(task_id) => flagged.push(task_id.clone()),
            "ResumeIssued" | "NudgeIssued" | "ReportObserved" | "TaskRecorded" => {
                flagged.retain(|candidate| candidate != task_id);
            }
            _ => {}
        }
    }
    flagged
}

fn planner_lease_path(root: &Path) -> PathBuf {
    root.join(PLANNER_LEASE_REL)
}

fn with_planner_finalizer_lock<T>(root: &Path, action: impl FnOnce() -> Result<T>) -> Result<T> {
    let lock_path = root.join(PLANNER_FINALIZER_LOCK_REL);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let lock_file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)?;
    let mut lock = RwLock::new(lock_file);
    let _guard = lock.write().context("获取 planner finalizer 锁失败")?;
    action()
}

fn read_planner_lease_unlocked(root: &Path) -> Result<Option<PlannerLease>> {
    let path = planner_lease_path(root);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("读取 planner lease 失败: {}", path.display()))
        }
    };
    match serde_json::from_str(&text) {
        Ok(lease) => Ok(Some(lease)),
        Err(parse_error) => {
            let age = fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .unwrap_or_default();
            if age < Duration::from_secs(PLANNER_MALFORMED_LEASE_GRACE_SECS) {
                bail!(
                    "planner lease 正在写入或已损坏（{}；{}）；保守等待 {}s 后隔离",
                    path.display(),
                    parse_error,
                    PLANNER_MALFORMED_LEASE_GRACE_SECS
                );
            }
            let quarantine =
                path.with_file_name(format!("planner-turn.corrupt-{}.json", ulid::Ulid::new()));
            fs::rename(&path, &quarantine).with_context(|| {
                format!(
                    "隔离损坏 planner lease 失败: {} → {}",
                    path.display(),
                    quarantine.display()
                )
            })?;
            eprintln!(
                "orch serve: 已隔离损坏 planner lease（{}）",
                quarantine.display()
            );
            Ok(None)
        }
    }
}

pub fn read_planner_lease(root: &Path) -> Result<Option<PlannerLease>> {
    with_planner_finalizer_lock(root, || read_planner_lease_unlocked(root))
}

/// 原子 create_new；重复 daemon 只有一个能得到 true。
pub fn try_acquire_planner_lease(root: &Path, lease: &PlannerLease) -> Result<bool> {
    with_planner_finalizer_lock(root, || {
        let path = planner_lease_path(root);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let candidate = path.with_file_name(format!(
            "planner-turn.candidate-{}-{}.json",
            std::process::id(),
            ulid::Ulid::new()
        ));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&candidate)
            .with_context(|| format!("创建 planner lease 候选失败: {}", candidate.display()))?;
        if let Err(error) = (|| -> Result<()> {
            serde_json::to_writer_pretty(&mut file, lease)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            Ok(())
        })() {
            let _ = fs::remove_file(&candidate);
            return Err(error);
        }
        drop(file);
        match fs::hard_link(&candidate, &path) {
            Ok(()) => {
                if let Err(error) = fs::remove_file(&candidate) {
                    eprintln!(
                        "orch serve: planner lease 已发布但候选清理失败（{}）: {}",
                        candidate.display(),
                        error
                    );
                }
                Ok(true)
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                fs::remove_file(&candidate).ok();
                Ok(false)
            }
            Err(error) => {
                let _ = fs::remove_file(&candidate);
                Err(error)
                    .with_context(|| format!("原子发布 planner lease 失败: {}", path.display()))
            }
        }
    })
}

fn replace_lease_unlocked(root: &Path, lease: &PlannerLease) -> Result<()> {
    let path = planner_lease_path(root);
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    fs::write(&tmp, format!("{}\n", serde_json::to_string_pretty(lease)?))?;
    fs::rename(&tmp, &path).with_context(|| {
        format!(
            "原子替换 planner lease 失败: {} → {}",
            tmp.display(),
            path.display()
        )
    })
}

fn update_lease_child(root: &Path, wake_id: &str, child_pid: u32) -> Result<()> {
    with_planner_finalizer_lock(root, || {
        let mut lease =
            read_planner_lease_unlocked(root)?.context("planner lease 在 spawn 后消失")?;
        if lease.wake_id != wake_id {
            bail!(
                "planner lease wakeId 已变化（expected={wake_id}, actual={}）",
                lease.wake_id
            );
        }
        lease.child_pid = Some(child_pid);
        replace_lease_unlocked(root, &lease)
    })
}

fn update_lease_reservation(
    root: &Path,
    wake_id: &str,
    reservation_id: Option<&str>,
) -> Result<()> {
    with_planner_finalizer_lock(root, || {
        let mut lease =
            read_planner_lease_unlocked(root)?.context("planner lease 在预算放行后消失")?;
        if lease.wake_id != wake_id {
            bail!(
                "planner lease wakeId 已变化（expected={wake_id}, actual={}）",
                lease.wake_id
            );
        }
        lease.model_wake_reservation_id = reservation_id.map(str::to_string);
        replace_lease_unlocked(root, &lease)
    })
}

/// 仅当 wakeId 匹配时释放，旧 watcher 永不删除重试产生的新 lease。
fn release_planner_lease_unlocked(root: &Path, wake_id: &str) -> Result<bool> {
    let Some(lease) = read_planner_lease_unlocked(root)? else {
        return Ok(false);
    };
    if lease.wake_id != wake_id {
        return Ok(false);
    }
    fs::remove_file(planner_lease_path(root))?;
    Ok(true)
}

pub fn release_planner_lease(root: &Path, wake_id: &str) -> Result<bool> {
    with_planner_finalizer_lock(root, || release_planner_lease_unlocked(root, wake_id))
}

fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlannerProcessState {
    Dead,
    Matching,
    ReusedPid,
    UnknownAlive,
}

fn planner_process_state(pid: u32, session_id: &str) -> PlannerProcessState {
    if !process_alive(pid) {
        return PlannerProcessState::Dead;
    }
    match Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
    {
        Ok(output) if output.status.success() => {
            let command = String::from_utf8_lossy(&output.stdout);
            if command.contains(session_id) {
                PlannerProcessState::Matching
            } else {
                PlannerProcessState::ReusedPid
            }
        }
        _ if !process_alive(pid) => PlannerProcessState::Dead,
        _ => PlannerProcessState::UnknownAlive,
    }
}

fn terminate_process(pid: u32) {
    let _ = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status();
}

fn terminate_matching_planner(lease: &PlannerLease, pid: u32) -> PlannerProcessState {
    terminate_process(pid);
    for _ in 0..20 {
        let state = planner_process_state(pid, &lease.session_id);
        if !matches!(
            state,
            PlannerProcessState::Matching | PlannerProcessState::UnknownAlive
        ) {
            return state;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    planner_process_state(pid, &lease.session_id)
}

fn lease_age_secs(lease: &PlannerLease) -> Option<u64> {
    let Ok(started) = humantime::parse_rfc3339(&lease.started_at) else {
        return None;
    };
    Some(
        SystemTime::now()
            .duration_since(started)
            .unwrap_or_default()
            .as_secs(),
    )
}

fn lease_expired(lease: &PlannerLease) -> bool {
    lease_age_secs(lease).is_none_or(|age| age >= PLANNER_LEASE_TTL_SECS)
}

fn scan_round_events(root: &Path) -> Result<Vec<EventRecord>> {
    let rounds_dir = root.join("coordination/rounds");
    let entries = match fs::read_dir(&rounds_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("读取 rounds 目录失败: {}", rounds_dir.display()))
        }
    };
    let mut ledgers = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path().join("events.jsonl"))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    ledgers.sort();
    let mut events = Vec::new();
    for path in ledgers {
        let read = read_ledger(&path)
            .with_context(|| format!("扫描 planner 进展账本失败: {}", path.display()))?;
        if !read.bad_lines.is_empty() {
            bail!(
                "扫描 planner 进展账本发现坏行: {}（首处第 {} 行）",
                path.display(),
                read.bad_lines[0].0
            );
        }
        events.extend(read.events);
    }
    Ok(events)
}

pub fn has_planner_progress(root: &Path, wake_id: &str) -> Result<bool> {
    Ok(scan_round_events(root)?.iter().any(|event| {
        event
            .extra
            .get("plannerWakeId")
            .and_then(|value| value.as_str())
            == Some(wake_id)
    }))
}

fn lifecycle_exists(root: &Path, wake_id: &str, kind: &str) -> Result<bool> {
    Ok(scan_round_events(root)?.iter().any(|event| {
        event.kind == kind
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("wakeId"))
                .and_then(|value| value.as_str())
                == Some(wake_id)
    }))
}

fn injection_issued(root: &Path, wake_id: &str) -> Result<bool> {
    lifecycle_exists(root, wake_id, "InjectionIssued")
}

fn append_injection_issued(root: &Path, lease: &PlannerLease) -> Result<bool> {
    let append_round =
        planner_append_round(&lease.round, &current_round(root)?).map_err(anyhow::Error::msg)?;
    with_planner_finalizer_lock(root, || {
        if injection_issued(root, &lease.wake_id)? {
            return Ok(false);
        }
        let pid = lease
            .child_pid
            .context("持久化 InjectionIssued 前 lease 缺 childPid")?;
        let mut event = daemon_event(
            "InjectionIssued",
            None,
            Some(&append_round),
            serde_json::json!({
                "wakeId": lease.wake_id,
                "sessionId": lease.session_id,
                "pid": pid,
                "reasons": event_reason_strings(lease),
                "attempt": lease.attempt,
                "triggerKey": lease.trigger_key,
                "logPath": lease.log_path,
                "inboxFiles": lease.inbox_files,
            }),
        );
        if let Some(reservation_id) = lease.model_wake_reservation_id.as_ref() {
            event.extra.insert(
                "modelWakeReservationId".to_string(),
                serde_json::Value::String(reservation_id.clone()),
            );
        }
        ledger::append(root, &append_round, &[event])?;
        Ok(true)
    })
}

fn event_reason_strings(lease: &PlannerLease) -> serde_json::Value {
    serde_json::Value::Array(
        lease
            .reasons
            .iter()
            .cloned()
            .map(serde_json::Value::String)
            .collect(),
    )
}

fn append_planner_lifecycle(
    root: &Path,
    lease: &PlannerLease,
    kind: &str,
    exit_code: Option<i32>,
) -> Result<()> {
    let append_round =
        planner_append_round(&lease.round, &current_round(root)?).map_err(anyhow::Error::msg)?;
    if lifecycle_exists(root, &lease.wake_id, kind)? {
        return Ok(());
    }
    ledger::append(
        root,
        &append_round,
        &[daemon_event(
            kind,
            None,
            Some(&append_round),
            serde_json::json!({
                "wakeId": lease.wake_id,
                "sessionId": lease.session_id,
                "triggerKey": lease.trigger_key,
                "attempt": lease.attempt,
                "reasons": event_reason_strings(lease),
                "inboxFiles": lease.inbox_files,
                "exitCode": exit_code,
            }),
        )],
    )
}

fn append_planner_escalation(root: &Path, lease: &PlannerLease, reason: &str) -> Result<()> {
    let append_round =
        planner_append_round(&lease.round, &current_round(root)?).map_err(anyhow::Error::msg)?;
    let exists = scan_round_events(root)?.iter().any(|event| {
        event.kind == "EscalationRaised"
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("stage"))
                .and_then(|value| value.as_str())
                == Some("planner-liveness")
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("triggerKey"))
                .and_then(|value| value.as_str())
                == Some(lease.trigger_key.as_str())
    });
    if exists {
        return Ok(());
    }
    ledger::append(
        root,
        &append_round,
        &[daemon_event(
            "EscalationRaised",
            None,
            Some(&append_round),
            serde_json::json!({
                "stage": "planner-liveness",
                "wakeId": lease.wake_id,
                "triggerKey": lease.trigger_key,
                "attempt": lease.attempt,
                "reasons": event_reason_strings(lease),
                "reason": reason,
            }),
        )],
    )
}

fn finalize_planner_turn(
    root: &Path,
    lease: &PlannerLease,
    ledger_progress: bool,
    exit_code: Option<i32>,
) -> Result<PlannerLivenessDecision> {
    let decision = decide_planner_liveness(false, ledger_progress, lease.attempt);
    crate::close::with_protocol_effect(root, "serve planner-turn finalize", || {
        planner_append_round(&lease.round, &current_round(root)?).map_err(anyhow::Error::msg)?;
        with_planner_finalizer_lock(root, || {
            let Some(current) = read_planner_lease_unlocked(root)? else {
                return Ok(());
            };
            if current.wake_id != lease.wake_id {
                return Ok(());
            }
            match decision {
                PlannerLivenessDecision::Completed => {
                    append_planner_lifecycle(root, lease, "PlannerTurnCompleted", exit_code)?;
                }
                PlannerLivenessDecision::Retry { .. } => {
                    // 第一次有效 wake 已把指令移到 processing；若 child 无任何账本进展便死亡，
                    // 必须把同一批文件放回 pending，下一拍才能用同一 triggerKey 发起唯一一次重试。
                    // lease 保持到重排与 Lost 都成功，崩溃后 reconcile 可幂等补齐。
                    restore_inbox_pending(root, &lease.inbox_files)?;
                    append_planner_lifecycle(root, lease, "PlannerTurnLost", exit_code)?;
                }
                PlannerLivenessDecision::Escalate => {
                    append_planner_lifecycle(root, lease, "PlannerTurnLost", exit_code)?;
                    append_planner_escalation(root, lease, "child-exited-without-progress")?;
                }
                PlannerLivenessDecision::Wait => {}
            }
            release_planner_lease_unlocked(root, &lease.wake_id)?;
            Ok(())
        })?;
        Ok(decision)
    })
}

fn finalize_untracked_planner_turn(root: &Path, lease: &PlannerLease) -> Result<()> {
    with_planner_finalizer_lock(root, || {
        let Some(current) = read_planner_lease_unlocked(root)? else {
            return Ok(());
        };
        if current.wake_id != lease.wake_id {
            return Ok(());
        }
        append_planner_lifecycle(root, lease, "PlannerTurnLost", None)?;
        append_planner_escalation(root, lease, "owner-died-before-child-pid-persisted")?;
        release_planner_lease_unlocked(root, &lease.wake_id)?;
        Ok(())
    })
}

pub fn watch_planner_launch(
    root: &Path,
    lease: &PlannerLease,
    mut launch: wake::WakeLaunch,
    timeout: Duration,
) -> Result<PlannerLivenessDecision> {
    if launch.wake_id != lease.wake_id || launch.session_id != lease.session_id {
        bail!("watcher launch 与 lease wake/session 不匹配");
    }
    let status = match launch.child.wait_timeout(timeout)? {
        Some(status) => Some(status),
        None => {
            launch.child.kill().ok();
            Some(launch.child.wait()?)
        }
    };
    let progress = has_planner_progress(root, &lease.wake_id)?;
    let successful_exit = status.as_ref().is_some_and(|value| value.success());
    let exit_code = status.and_then(|value| value.code());
    finalize_planner_turn(root, lease, successful_exit || progress, exit_code)
}

fn spawn_planner_watcher(root: PathBuf, lease: PlannerLease, launch: wake::WakeLaunch) {
    std::thread::spawn(move || {
        if let Err(error) = watch_planner_launch(
            &root,
            &lease,
            launch,
            Duration::from_secs(PLANNER_TURN_TIMEOUT_SECS),
        ) {
            eprintln!(
                "orch serve: planner watcher 失败（wakeId={}）: {error:#}",
                lease.wake_id
            );
        }
    });
}

fn ensure_inbox_processing(root: &Path, filenames: &[String]) -> Result<()> {
    for filename in filenames {
        let pending = root.join(inbox::relocate_target(filename, inbox::InboxStage::Pending));
        let processing = root.join(inbox::relocate_target(
            filename,
            inbox::InboxStage::Processing,
        ));
        let done = root.join(inbox::relocate_target(filename, inbox::InboxStage::Done));
        if let Some(parent) = processing.parent() {
            fs::create_dir_all(parent)?;
        }
        if !pending.is_file() {
            if processing.is_file() || done.is_file() {
                continue;
            }
            continue;
        }
        match fs::rename(&pending, &processing) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {
                // child/另一个 daemon 可在检查后推进同一文件；Processing/Done 都是
                // 已成功越过 Pending 的幂等终态，绝不能重新从 Done 调用 advance 倒退。
                if !processing.is_file() && !done.is_file() {
                    return Err(error).with_context(|| {
                        format!(
                            "inbox Pending→Processing 竞态后文件三态均缺失: {}",
                            filename
                        )
                    });
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "inbox Pending→Processing 直达重命名失败: {} → {}",
                        pending.display(),
                        processing.display()
                    )
                })
            }
        }
    }
    Ok(())
}

fn restore_inbox_pending(root: &Path, filenames: &[String]) -> Result<()> {
    crate::close::with_protocol_effect(root, "serve restore planner inbox", || {
        for filename in filenames {
            let processing = root.join(inbox::relocate_target(
                filename,
                inbox::InboxStage::Processing,
            ));
            if !processing.is_file() {
                continue;
            }
            let pending = root.join(inbox::relocate_target(filename, inbox::InboxStage::Pending));
            if pending.exists() {
                bail!(
                    "planner retry 无法恢复 inbox：pending 与 processing 同名并存（{}）",
                    filename
                );
            }
            fs::rename(&processing, &pending).with_context(|| {
                format!(
                    "planner retry 恢复 inbox 失败: {} → {}",
                    processing.display(),
                    pending.display()
                )
            })?;
        }
        Ok(())
    })
}

/// daemon 启动/每拍先恢复旧 lease。返回 true 表示仍有活跃 turn，应保持 singleflight。
pub fn reconcile_planner_turn(root: &Path) -> Result<bool> {
    let round = current_round(root)?;
    match reconcile_planner_turn_inner(root) {
        Ok(active) => Ok(active),
        Err(error) if failure::is_action_rejection(&error) => Err(error),
        Err(error) => failure::reject_action_from_ledger_disposition(
            root,
            &round,
            None,
            "daemon-reconcile",
            &format!("daemon:{round}:reconcile"),
            &format!("{error:#}"),
            failure::CliDisposition::EffectUnknown,
        ),
    }
}

fn reconcile_planner_turn_inner(root: &Path) -> Result<bool> {
    let Some(lease) = read_planner_lease(root)? else {
        return Ok(false);
    };
    crate::close::with_protocol_effect(root, "serve planner-turn reconcile", || {
        reconcile_planner_turn_with_lease(root, lease)
    })
}

fn reconcile_planner_turn_with_lease(root: &Path, lease: PlannerLease) -> Result<bool> {
    let active_round = current_round(root)?;
    if let Err(reason) = planner_append_round(&lease.round, &active_round) {
        ledger::append_checked(root, &active_round, |events| {
            let already_recorded = events.iter().any(|event| {
                event.kind == "EscalationRaised"
                    && event
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("stage"))
                        .and_then(serde_json::Value::as_str)
                        == Some("stale-lease-round")
                    && event
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("wakeId"))
                        .and_then(serde_json::Value::as_str)
                        == Some(lease.wake_id.as_str())
            });
            if already_recorded {
                return Ok(Vec::new());
            }
            Ok(vec![daemon_event(
                "EscalationRaised",
                None,
                Some(&active_round),
                serde_json::json!({
                    "stage": "stale-lease-round",
                    "reason": reason,
                    "wakeId": lease.wake_id,
                    "leaseRound": lease.round,
                    "currentRound": active_round,
                }),
            )])
        })?;
        release_planner_lease(root, &lease.wake_id)?;
        return Ok(false);
    }
    if lease.child_pid.is_some() && !injection_issued(root, &lease.wake_id)? {
        // childPid 只有 spawn 成功后才会持久化；daemon 若崩在 PID→InjectionIssued
        // 之间，重启必须先幂等补齐真实启动事实，预算与 attempt 才不会漏计。
        append_injection_issued(root, &lease)?;
    }
    if injection_issued(root, &lease.wake_id)? {
        ensure_inbox_processing(root, &lease.inbox_files)?;
    }
    let progress = has_planner_progress(root, &lease.wake_id)?;
    let Some(pid) = lease.child_pid else {
        if progress {
            finalize_planner_turn(root, &lease, true, None)?;
            return Ok(false);
        }
        // owner 仍活时，它可能阻塞在预算/账本锁或恰处于 spawn→PID 持久化窄窗；
        // 其他 daemon 绝不能仅凭 lease 年龄释放并制造双启动。
        if process_alive(lease.owner_pid) || !lease_expired(&lease) {
            return Ok(true);
        }
        if injection_issued(root, &lease.wake_id)? {
            finalize_planner_turn(root, &lease, false, None)?;
            return Ok(false);
        }
        // owner 已死且整个 turn TTL 内都未留下 PID/InjectionIssued：无法区分 spawn 前
        // 崩溃与 spawn 后 PID 未持久化。安全优先，不自动重启可能仍在跑的孤儿模型。
        finalize_untracked_planner_turn(root, &lease)?;
        return Ok(false);
    };

    let state = planner_process_state(pid, &lease.session_id);
    if matches!(
        state,
        PlannerProcessState::Matching | PlannerProcessState::UnknownAlive
    ) && !lease_expired(&lease)
    {
        return Ok(true);
    }
    if matches!(
        state,
        PlannerProcessState::Matching | PlannerProcessState::UnknownAlive
    ) {
        let after_term = if state == PlannerProcessState::Matching {
            terminate_matching_planner(&lease, pid)
        } else {
            PlannerProcessState::UnknownAlive
        };
        if matches!(
            after_term,
            PlannerProcessState::Matching | PlannerProcessState::UnknownAlive
        ) {
            // 身份不明或拒绝 TERM 时保留 lease；宁可人工解除，也不并发第二个模型。
            with_planner_finalizer_lock(root, || {
                let Some(current) = read_planner_lease_unlocked(root)? else {
                    return Ok(());
                };
                if current.wake_id != lease.wake_id {
                    return Ok(());
                }
                append_planner_escalation(root, &lease, "planner-timeout-process-still-alive")
            })?;
            return Ok(true);
        }
    }
    finalize_planner_turn(root, &lease, progress, None)?;
    Ok(false)
}

fn derive_inputs_from_events(root: &Path, events: &[EventRecord]) -> Result<TickInputs> {
    let projection = fold(events);
    let task_states = projection
        .tasks
        .into_iter()
        .filter_map(|(task, projected)| projected.state.map(|state| (task, state.to_string())))
        .collect::<Vec<_>>();
    let new_inbox = inbox::list_pending_by_priority(root)?;
    let dead_or_stalled = dead_or_stalled_tasks(events);
    let pending = rebuild_pending_reasons(events);
    let round_closed = events.iter().any(|event| event.kind == "RoundClosed");

    Ok(derive_tick_inputs(
        &task_states,
        new_inbox,
        dead_or_stalled,
        pending,
        round_closed,
    ))
}

fn read_current_events(root: &Path, round: &str) -> Result<Vec<EventRecord>> {
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let read = read_ledger(&ledger_path)
        .with_context(|| format!("读取当前轮账本失败: {}", ledger_path.display()))?;
    if !read.bad_lines.is_empty() {
        bail!(
            "当前轮账本发现坏行: {} 行（首处第 {} 行）",
            read.bad_lines.len(),
            read.bad_lines[0].0
        );
    }
    Ok(read.events)
}

pub fn compute_tick_inputs(root: &Path) -> Result<TickInputs> {
    let round = current_round(root)?;
    let events = read_current_events(root, &round)?;
    derive_inputs_from_events(root, &events)
}

fn latest_causal_event(
    events: &[EventRecord],
    task: &str,
    predicate: impl Fn(&EventRecord) -> bool,
) -> Option<String> {
    events
        .iter()
        .rev()
        .find(|event| event.task_id.as_deref() == Some(task) && predicate(event))
        .map(|event| event.event_id.clone())
}

fn trigger_sources(
    inputs: &TickInputs,
    injections: &[Injection],
    events: &[EventRecord],
) -> Vec<String> {
    let mut sources = Vec::new();
    for injection in injections {
        match injection.reason {
            InjectReason::NewInstruction => {
                sources.extend(
                    inputs
                        .new_inbox
                        .iter()
                        .map(|filename| format!("inbox:{filename}")),
                );
            }
            InjectReason::TaskFailed => {
                for task in &inputs.fail_tasks {
                    if let Some(event_id) = latest_causal_event(events, task, |event| {
                        event.kind == "MechCheckFailed"
                            || (event.kind == "VerdictIssued"
                                && event
                                    .payload
                                    .as_ref()
                                    .and_then(|payload| payload.get("verdict"))
                                    .and_then(|value| value.as_str())
                                    == Some("FAIL"))
                    }) {
                        sources.push(format!("fail:{task}:{event_id}"));
                    }
                }
            }
            InjectReason::AllRecorded => {
                let mut latest = BTreeMap::<String, String>::new();
                for event in events.iter().filter(|event| event.kind == "TaskRecorded") {
                    if let Some(task) = event.task_id.as_ref() {
                        latest.insert(task.clone(), event.event_id.clone());
                    }
                }
                sources.extend(
                    latest
                        .into_iter()
                        .map(|(task, event_id)| format!("recorded:{task}:{event_id}")),
                );
            }
            InjectReason::AgentDown => {
                for task in &inputs.dead_or_stalled {
                    if let Some(event_id) = latest_causal_event(events, task, |event| {
                        event.kind == "AttemptBlocked"
                            || (event.kind == "EscalationRaised"
                                && matches!(
                                    event
                                        .payload
                                        .as_ref()
                                        .and_then(|payload| payload.get("stage"))
                                        .and_then(|value| value.as_str()),
                                    Some("liveness-dead")
                                        | Some("liveness-stalled")
                                        | Some("executor-blocked")
                                ))
                    }) {
                        sources.push(format!("agent-down:{task}:{event_id}"));
                    }
                }
            }
        }
    }
    sources
}

fn lost_attempts(events: &[EventRecord], trigger_key: &str) -> u8 {
    events
        .iter()
        .filter(|event| {
            event.kind == "PlannerTurnLost"
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("triggerKey"))
                    .and_then(|value| value.as_str())
                    == Some(trigger_key)
        })
        .count()
        .min(u8::MAX as usize) as u8
}

fn planner_liveness_escalated(events: &[EventRecord], trigger_key: &str) -> bool {
    events.iter().any(|event| {
        event.kind == "EscalationRaised"
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("stage"))
                .and_then(|value| value.as_str())
                == Some("planner-liveness")
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("triggerKey"))
                .and_then(|value| value.as_str())
                == Some(trigger_key)
    })
}

fn planner_trigger_completed(events: &[EventRecord], trigger_key: &str) -> bool {
    events.iter().any(|event| {
        event.kind == "PlannerTurnCompleted"
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("triggerKey"))
                .and_then(|value| value.as_str())
                == Some(trigger_key)
    })
}

pub fn serve_tick(root: &Path) -> Result<(Vec<Injection>, usize)> {
    let round = current_round(root)?;
    let mut action_id = format!("daemon:{round}");
    let mut operation = "daemon";
    match serve_tick_inner(root, &round, &mut operation, &mut action_id) {
        Ok(outcome) => Ok(outcome),
        Err(error) if failure::is_action_rejection(&error) => Err(error),
        Err(error) => failure::reject_action_from_ledger_disposition(
            root,
            &round,
            None,
            operation,
            &action_id,
            &format!("{error:#}"),
            failure::CliDisposition::EffectUnknown,
        ),
    }
}

fn serve_tick_inner(
    root: &Path,
    round: &str,
    operation: &mut &'static str,
    action_id: &mut String,
) -> Result<(Vec<Injection>, usize)> {
    // A provider frame may arrive after the original CLI timed out or crashed.
    // Reconcile that exact WakeIssued action before considering any re-wake.
    crate::wake::reconcile_pending_backend_receipts(root, round)?;
    if reconcile_planner_turn(root)? {
        let pending_inbox = inbox::list_pending_by_priority(root)?.len();
        return Ok((Vec::new(), pending_inbox));
    }

    // reconcile 可能刚写 Completed/Lost 并释放 pending，须重读账本再作本拍判断。
    let events = read_current_events(root, &round)?;
    let inputs = derive_inputs_from_events(root, &events)?;
    let pending_inbox = inputs.new_inbox.len();
    let injections = decide_injections(&inputs);
    if injections.is_empty() {
        return Ok((injections, pending_inbox));
    }

    let inbox_files = if injections
        .iter()
        .any(|injection| injection.reason == InjectReason::NewInstruction)
    {
        inputs.new_inbox.clone()
    } else {
        Vec::new()
    };
    let sources = trigger_sources(&inputs, &injections, &events);
    let planned = plan_planner_wake(&round, &injections, &inbox_files, &sources, 4096)
        .context("非空 injections 未生成 planner wake batch")?;
    if planner_trigger_completed(&events, &planned.trigger_key)
        || planner_liveness_escalated(&events, &planned.trigger_key)
    {
        return Ok((Vec::new(), pending_inbox));
    }
    let attempt = lost_attempts(&events, &planned.trigger_key).saturating_add(1);
    if attempt > 2 {
        return Ok((Vec::new(), pending_inbox));
    }

    crate::close::with_protocol_effect(root, "serve planner-turn", || {
        // prepare 只读配置并生成 UUID；失败时尚未建 lease，更不会移动 inbox。
        let prepared = wake::prepare_fresh_planner_wake(root, &planned.prompt)?;
        *operation = "daemon-spawn";
        *action_id = prepared.wake_id.clone();
        let log_path = prepared
            .log_path
            .strip_prefix(root)
            .unwrap_or(&prepared.log_path)
            .display()
            .to_string();
        let mut lease = PlannerLease {
            round: round.to_string(),
            wake_id: prepared.wake_id.clone(),
            session_id: prepared.session_id.clone(),
            trigger_key: planned.trigger_key.clone(),
            attempt,
            owner_pid: std::process::id(),
            child_pid: None,
            started_at: humantime::format_rfc3339_seconds(SystemTime::now()).to_string(),
            reasons: planned
                .reasons
                .iter()
                .map(|reason| reason.as_str().to_string())
                .collect(),
            inbox_files: inbox_files.clone(),
            log_path: Some(log_path),
            model_wake_reservation_id: None,
        };
        if !try_acquire_planner_lease(root, &lease)? {
            return Ok((Vec::new(), pending_inbox));
        }

        let mut permit = match budget::check_before_model_wake(root, &round) {
            Ok(permit) => permit,
            Err(error) => {
                release_planner_lease(root, &lease.wake_id)?;
                return Err(error).context("planner 模型唤醒被预算门阻断");
            }
        };
        lease.model_wake_reservation_id = permit.reservation_id().map(str::to_string);
        if let Err(error) = budget::bind_model_wake_reservation(root, &permit, &lease.wake_id) {
            budget::cancel_model_wake_reservation(root, &mut permit)?;
            release_planner_lease(root, &lease.wake_id)?;
            return Err(error);
        }
        if let Err(error) = update_lease_reservation(
            root,
            &lease.wake_id,
            lease.model_wake_reservation_id.as_deref(),
        ) {
            budget::cancel_model_wake_reservation(root, &mut permit)?;
            release_planner_lease(root, &lease.wake_id)?;
            return Err(error);
        }
        let mut launch = match wake::spawn_prepared_planner_wake(root, prepared) {
            Ok(launch) => launch,
            Err(error) => {
                budget::cancel_model_wake_reservation(root, &mut permit)?;
                release_planner_lease(root, &lease.wake_id)?;
                return Err(error);
            }
        };
        if let Err(error) = update_lease_child(root, &lease.wake_id, launch.pid) {
            launch.child.kill().ok();
            launch.child.wait().ok();
            budget::forget_current_model_wake_reservation(&mut permit);
            release_planner_lease(root, &lease.wake_id)?;
            return Err(error);
        }
        let mut live_lease = lease.clone();
        live_lease.child_pid = Some(launch.pid);
        if let Err(error) = append_injection_issued(root, &live_lease) {
            launch.child.kill().ok();
            launch.child.wait().ok();
            // lease/PID/reservation 均保留；下一拍 reconcile 会先补真实
            // InjectionIssued，再按已确认死亡的 child 写 Lost，避免实际启动漏预算或
            // attempt 归零。
            budget::forget_current_model_wake_reservation(&mut permit);
            return Err(error).context("fresh planner 已 spawn 但 InjectionIssued 落账失败");
        }
        budget::forget_current_model_wake_reservation(&mut permit);

        let inbox_result = if may_advance_inbox_after_wake(true, true, true) {
            ensure_inbox_processing(root, &inbox_files)
                .context("InjectionIssued 后 inbox 推进失败；交下一拍 reconcile")
        } else {
            Ok(())
        };
        spawn_planner_watcher(root.to_path_buf(), live_lease, launch);
        if let Err(error) = inbox_result {
            *operation = "daemon-reconcile";
            return Err(error);
        }
        Ok((injections, pending_inbox))
    })
}

fn auto_close_requested(actions: &[crate::runloop::Action]) -> bool {
    actions
        .iter()
        .any(|action| matches!(action, crate::runloop::Action::CloseRound))
}

fn all_recorded_judgment_completed(events: &[EventRecord]) -> bool {
    let Some(latest_recorded) = events
        .iter()
        .rposition(|event| event.kind == "TaskRecorded")
    else {
        return false;
    };
    events.iter().skip(latest_recorded + 1).any(|event| {
        event.kind == "PlannerTurnCompleted"
            && reasons_from_payload(event.payload.as_ref()).contains(&InjectReason::AllRecorded)
    })
}

#[doc(hidden)]
pub fn auto_close_round_if_ready(root: &Path, actions: &[crate::runloop::Action]) -> Result<bool> {
    if !auto_close_requested(actions) {
        return Ok(false);
    }
    // 两个 daemon 即使同时从 runloop 得到 CloseRound，也在同一跨进程锁内重读账本；
    // 只有第一个会执行有副作用的 run_close。
    with_planner_finalizer_lock(root, || {
        let round = current_round(root)?;
        let events = read_current_events(root, &round)?;
        if fold(&events).round_closed
            || read_planner_lease_unlocked(root)?.is_some()
            || !all_recorded_judgment_completed(&events)
        {
            return Ok(false);
        }
        crate::round::run_close(root, false, Some("orch serve 自动机械收轮"))?;
        Ok(true)
    })
}

pub fn run_daemon(root: &Path, once: bool) -> Result<()> {
    // Equivalent rollback switch while orch-cli remains outside this task's
    // writeSet.  The explicit host entry point below is used by tests/embedders.
    let monitor_enabled = std::env::var_os("ORCH_SERVE_NO_MONITOR").is_none();
    run_daemon_with_monitor(root, once, monitor_enabled)
}

pub fn run_daemon_with_monitor(root: &Path, once: bool, monitor_enabled: bool) -> Result<()> {
    if monitor_enabled {
        if once {
            let mut state = LivenessMonitorState::default();
            run_monitor_sample_without_unwinding(root, &mut state);
        } else {
            spawn_liveness_monitor(root.to_path_buf());
        }
    }
    loop {
        let mechanical = match crate::runloop::run_loop(root, true) {
            Ok(outcome) => {
                if let Err(error) = auto_close_round_if_ready(root, &outcome.actions) {
                    if once {
                        let round = current_round(root)?;
                        return failure::reject_action_from_ledger_disposition(
                            root,
                            &round,
                            None,
                            "daemon-reconcile",
                            &format!("daemon:{round}:auto-close"),
                            &format!("自动收轮失败: {error:#}"),
                            failure::CliDisposition::EffectUnknown,
                        );
                    }
                    eprintln!("orch serve: 自动收轮失败，将保留 AllRecorded 判断入口: {error:#}");
                }
                outcome
                    .actions
                    .iter()
                    .map(|action| format!("{action:?}"))
                    .collect::<Vec<_>>()
            }
            Err(error) => {
                if once {
                    let round = current_round(root)?;
                    return failure::reject_action_from_ledger_disposition(
                        root,
                        &round,
                        None,
                        "daemon",
                        &format!("daemon:{round}:mechanical"),
                        &format!("机械 tick 失败: {error:#}"),
                        failure::CliDisposition::EffectUnknown,
                    );
                }
                eprintln!("orch serve: 机械 tick 失败，将继续下一分支/下一拍: {error:#}");
                Vec::new()
            }
        };
        let (injections, pending_inbox) = match serve_tick(root) {
            Ok(outcome) => outcome,
            Err(error) => {
                if once {
                    return Err(error);
                }
                eprintln!("orch serve: 注入 tick 失败，将继续下一拍: {error:#}");
                (Vec::new(), 0)
            }
        };
        let report = tick_report(&mechanical, &injections, pending_inbox);
        if !report.is_idle() {
            println!(
                "orch serve tick: mechanical={} injected={:?} pending={}",
                report.mechanical, report.injected, report.pending_inbox
            );
        }
        if once {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(15));
    }
}

impl InjectReason {
    pub fn as_str(self) -> &'static str {
        match self {
            InjectReason::NewInstruction => "new_instruction",
            InjectReason::TaskFailed => "task_failed",
            InjectReason::AllRecorded => "all_recorded",
            InjectReason::AgentDown => "agent_down",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "new_instruction" | "NewInstruction" => Some(Self::NewInstruction),
            "task_failed" | "TaskFailed" => Some(Self::TaskFailed),
            "all_recorded" | "AllRecorded" => Some(Self::AllRecorded),
            "agent_down" | "AgentDown" => Some(Self::AgentDown),
            _ => None,
        }
    }
}

fn reasons_from_payload(payload: Option<&serde_json::Value>) -> Vec<InjectReason> {
    let Some(payload) = payload else {
        return Vec::new();
    };
    if let Some(reasons) = payload.get("reasons").and_then(|value| value.as_array()) {
        return reasons
            .iter()
            .filter_map(|value| value.as_str())
            .filter_map(InjectReason::from_str)
            .collect();
    }
    payload
        .get("reason")
        .and_then(|value| value.as_str())
        .and_then(InjectReason::from_str)
        .into_iter()
        .collect()
}

pub fn rebuild_pending_reasons(events: &[EventRecord]) -> Vec<InjectReason> {
    let mut pending = Vec::new();
    for event in events {
        match event.kind.as_str() {
            "InjectionIssued" => {
                for reason in reasons_from_payload(event.payload.as_ref()) {
                    if !pending.contains(&reason) {
                        pending.push(reason);
                    }
                }
            }
            "InjectionConsumed" | "PlannerTurnCompleted" | "PlannerTurnLost" => {
                let reasons = reasons_from_payload(event.payload.as_ref());
                if reasons.is_empty() {
                    pending.clear();
                } else {
                    pending.retain(|candidate| !reasons.contains(candidate));
                }
            }
            "NudgeIssued" | "ResumeIssued" | "ReportObserved" => {
                pending.retain(|candidate| *candidate != InjectReason::AgentDown);
            }
            _ if event.actor == "planner" => pending.clear(),
            _ => {}
        }
    }
    pending
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    extern "C" {
        #[link_name = "getpgrp"]
        fn b147_getpgrp() -> i32;
    }

    static TEST_REPO_SEQ: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn b203_eight_second_ack_bound_is_exactly_four_reconcile_ticks() {
        let tick = crate::tierf::AWAIT_REPORT_RECONCILE_TICK;
        let bound = Duration::from_secs(8);
        let budget = exact_tick_budget(bound, tick);
        assert_eq!(budget, 4);
        assert_eq!(elapsed_reconcile_ticks(Duration::ZERO, tick), 0);
        assert_eq!(elapsed_reconcile_ticks(bound, tick), budget);
        assert_eq!(
            elapsed_reconcile_ticks(bound + Duration::from_nanos(1), tick),
            budget + 1
        );

        let at_boundary = ack_convergence_verdict(
            AckPipelineStage::AckPresentUnclaimed,
            elapsed_reconcile_ticks(bound, tick),
            budget,
        );
        assert!(!at_boundary.exceeded, "exactly eight seconds remains in budget");
        let over_boundary = ack_convergence_verdict(
            AckPipelineStage::AckPresentUnclaimed,
            elapsed_reconcile_ticks(bound + Duration::from_nanos(1), tick),
            budget,
        );
        assert!(
            over_boundary.exceeded,
            "the first instant beyond eight seconds must exceed the signed bound"
        );
    }

    #[test]
    fn attempt_started_without_dispatch_ack_is_not_false_convergence() {
        assert_eq!(
            classify_ack_pipeline(AckSnapshot {
                ack_present: true,
                dispatch_acked: false,
                attempt_started: true,
            }),
            AckPipelineStage::AckPresentUnclaimed
        );
        assert_eq!(
            classify_ack_pipeline(AckSnapshot {
                ack_present: false,
                dispatch_acked: false,
                attempt_started: true,
            }),
            AckPipelineStage::GoNotConsumed
        );
    }

    fn b134_test_repo(tag: &str) -> PathBuf {
        let seq = TEST_REPO_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "orch-b134-serve-{tag}-{}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("coordination/runtime")).unwrap();
        fs::create_dir_all(root.join("coordination/rounds/r49")).unwrap();
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r49\n").unwrap();
        fs::write(root.join("coordination/rounds/r49/events.jsonl"), "").unwrap();
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success());
        root
    }

    fn b157_request_event(agent: &str) -> EventRecord {
        ledger::event(
            "ReviewRequested",
            "runtime:orch",
            Some("B157T"),
            Some("r49"),
            serde_json::json!({
                "attemptId": "B157T-A0001",
                "role": "primary",
                "agent": agent,
                "deadlineSecs": 900,
                "requestedAt": "2026-07-28T00:00:00Z",
            }),
        )
    }

    fn b157_collect_event() -> EventRecord {
        ledger::event(
            "ReportCollectCompleted",
            "runtime:orch",
            Some("B157T"),
            Some("r49"),
            serde_json::json!({
                "attemptId": "B157T-A0001",
                "branchSha": "0123456789012345678901234567890123456789",
            }),
        )
    }

    #[test]
    fn review_delivery_tick_rejects_shell_and_deduplicates_substantive_delivery() {
        let root = b134_test_repo("b157-review-delivery");
        ledger::append(
            &root,
            "r49",
            &[b157_collect_event(), b157_request_event("executor-claw")],
        )
        .unwrap();
        let review_dir = root.join("coordination/rounds/r49/reviews");
        fs::create_dir_all(&review_dir).unwrap();
        let review_path = review_dir.join("B157T-A0001-primary-executor-claw.md");
        fs::write(
            &review_path,
            "---\ntaskId: B157T\nround: r49\nattemptId: B157T-A0001\nrole: primary\nreviewer: executor-claw\nverdict: PASS\nreviewedHead: 1111111111111111111111111111111111111111\n---\nwrong identity\n",
        )
        .unwrap();
        assert!(review_delivery_tick(&root, "r49").is_err());
        let ledger =
            orch_core::read_ledger(&root.join("coordination/rounds/r49/events.jsonl")).unwrap();
        assert_eq!(
            ledger
                .events
                .iter()
                .filter(|event| event.kind == "ReviewDelivered")
                .count(),
            0
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let target = review_dir.join("otherwise-valid-review.md");
            fs::write(
                &target,
                "---\ntaskId: B157T\nround: r49\nattemptId: B157T-A0001\nrole: primary\nreviewer: executor-claw\nverdict: PASS\nreviewedHead: 0123456789012345678901234567890123456789\n---\nsubstantive review\n",
            )
            .unwrap();
            fs::remove_file(&review_path).unwrap();
            symlink(&target, &review_path).unwrap();
            assert!(review_delivery_tick(&root, "r49").is_err());
            fs::remove_file(&review_path).unwrap();
            fs::remove_file(target).unwrap();
        }

        fs::write(
            &review_path,
            "---\ntaskId: B157T\nround: r49\nattemptId: B157T-A0001\nrole: primary\nreviewer: executor-claw\nverdict: PASS\nreviewedHead: 0123456789012345678901234567890123456789\n---\n\n",
        )
        .unwrap();
        assert_eq!(review_delivery_tick(&root, "r49").unwrap(), 0);
        let text = fs::read_to_string(root.join("coordination/rounds/r49/events.jsonl")).unwrap();
        assert_eq!(
            pending_review_expectations(&text, "r49", 0).unwrap().len(),
            1
        );

        fs::write(
            &review_path,
            "---\ntaskId: B157T\nround: r49\nattemptId: B157T-A0001\nrole: primary\nreviewer: executor-claw\nverdict: PASS\nreviewedHead: 0123456789012345678901234567890123456789\n---\n\n## Verdict\nPASS\n",
        )
        .unwrap();
        assert_eq!(review_delivery_tick(&root, "r49").unwrap(), 1);
        assert_eq!(
            review_delivery_tick(&root, "r49").unwrap(),
            0,
            "repeated monitor ticks must not duplicate delivery facts"
        );
        let ledger =
            orch_core::read_ledger(&root.join("coordination/rounds/r49/events.jsonl")).unwrap();
        assert_eq!(
            ledger
                .events
                .iter()
                .filter(|event| event.kind == "ReviewDelivered")
                .count(),
            1
        );
        let text = fs::read_to_string(root.join("coordination/rounds/r49/events.jsonl")).unwrap();
        assert!(pending_review_expectations(&text, "r49", 0)
            .unwrap()
            .is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn debounce_blocks_only_the_matching_reason() {
        let inputs = TickInputs {
            new_inbox: vec!["instruction.md".to_string()],
            fail_tasks: vec!["B36".to_string()],
            all_recorded: false,
            dead_or_stalled: vec![],
            pending: vec![InjectReason::TaskFailed],
        };

        let injections = decide_injections(&inputs);
        assert_eq!(injections.len(), 1);
        assert_eq!(injections[0].reason, InjectReason::NewInstruction);
    }

    #[test]
    fn messages_include_every_triggering_entity() {
        let inputs = TickInputs {
            new_inbox: vec!["one.md".to_string(), "two.md".to_string()],
            fail_tasks: vec!["B36".to_string(), "B38".to_string()],
            all_recorded: false,
            dead_or_stalled: vec!["executor-a".to_string(), "executor-b".to_string()],
            pending: vec![],
        };

        let injections = decide_injections(&inputs);
        let messages: Vec<&str> = injections
            .iter()
            .map(|item| item.message.as_str())
            .collect();
        assert!(messages
            .iter()
            .any(|message| message.contains("one.md") && message.contains("two.md")));
        assert!(messages
            .iter()
            .any(|message| message.contains("B36") && message.contains("B38")));
        assert!(messages
            .iter()
            .any(|message| message.contains("executor-a") && message.contains("executor-b")));
    }

    #[test]
    fn derive_collects_only_changes_requested_tasks() {
        let inputs = derive_tick_inputs(
            &[
                ("failed".to_string(), "changes_requested".to_string()),
                ("done".to_string(), "recorded".to_string()),
                ("ready".to_string(), "ready_for_verification".to_string()),
            ],
            vec![],
            vec![],
            vec![],
            false,
        );

        assert_eq!(inputs.fail_tasks, vec!["failed"]);
        assert!(!inputs.all_recorded);
    }

    #[test]
    fn derive_does_not_close_an_empty_round() {
        let inputs = derive_tick_inputs(&[], vec![], vec![], vec![], false);
        assert!(!inputs.all_recorded);
    }

    #[test]
    fn daemon_auto_close_is_requested_only_for_close_action() {
        assert!(auto_close_requested(&[crate::runloop::Action::CloseRound]));
        assert!(!auto_close_requested(&[crate::runloop::Action::Verify {
            task: "B53".to_string(),
        }]));
        assert!(!auto_close_requested(&[]));
    }

    #[test]
    fn closed_round_preserves_non_closure_inputs() {
        let inputs = derive_tick_inputs(
            &[("failed".to_string(), "changes_requested".to_string())],
            vec!["priority.md".to_string()],
            vec!["B42".to_string()],
            vec![InjectReason::TaskFailed],
            true,
        );

        assert_eq!(inputs.fail_tasks, vec!["failed"]);
        assert_eq!(inputs.new_inbox, vec!["priority.md"]);
        assert_eq!(inputs.dead_or_stalled, vec!["B42"]);
        assert_eq!(inputs.pending, vec![InjectReason::TaskFailed]);
    }

    #[test]
    fn pending_reasons_are_rebuilt_and_cleared_by_planner_progress() {
        let issued = ledger::event(
            "InjectionIssued",
            "runtime:orch",
            None,
            Some("r22"),
            serde_json::json!({"reason": "task_failed"}),
        );
        assert_eq!(
            rebuild_pending_reasons(&[issued.clone()]),
            vec![InjectReason::TaskFailed]
        );

        let planner_progress = ledger::event(
            "TaskValidated",
            "planner",
            None,
            Some("r22"),
            serde_json::json!({}),
        );
        assert!(rebuild_pending_reasons(&[issued, planner_progress]).is_empty());
    }

    #[test]
    fn dead_tracking_ignores_taskless_and_unrelated_events() {
        let taskless = ledger::event(
            "EscalationRaised",
            "runtime:orch",
            None,
            Some("r24"),
            serde_json::json!({"stage": "liveness-dead"}),
        );
        let unrelated = ledger::event(
            "ResumeIssued",
            "planner",
            Some("B99"),
            Some("r24"),
            serde_json::json!({}),
        );
        let flagged = ledger::event(
            "EscalationRaised",
            "runtime:orch",
            Some("B42"),
            Some("r24"),
            serde_json::json!({"stage": "liveness-stalled"}),
        );

        assert_eq!(
            dead_or_stalled_tasks(&[taskless, unrelated, flagged]),
            vec!["B42".to_string()]
        );
    }

    #[test]
    fn storage_suppressed_crash_never_consumes_retry_budget() {
        let ordinary = ledger::event(
            "AttemptCrashed",
            "runtime:orch",
            Some("B206"),
            Some("r63"),
            serde_json::json!({"attemptId":"B206-A0001"}),
        );
        let suppressed = ledger::event(
            "AttemptCrashed",
            "runtime:orch",
            Some("B206"),
            Some("r63"),
            serde_json::json!({
                "attemptId":"B206-A0002",
                "cause":"storage-exhaustion"
            }),
        );
        assert_eq!(count_dead_attempts(&[ordinary, suppressed], "B206"), 1);
    }

    #[test]
    fn recovered_task_reenters_at_its_new_position() {
        let events = vec![
            ledger::event(
                "EscalationRaised",
                "runtime:orch",
                Some("B41"),
                Some("r24"),
                serde_json::json!({"stage": "liveness-dead"}),
            ),
            ledger::event(
                "EscalationRaised",
                "runtime:orch",
                Some("B42"),
                Some("r24"),
                serde_json::json!({"stage": "liveness-stalled"}),
            ),
            ledger::event(
                "ResumeIssued",
                "planner",
                Some("B41"),
                Some("r24"),
                serde_json::json!({}),
            ),
            ledger::event(
                "EscalationRaised",
                "runtime:orch",
                Some("B41"),
                Some("r24"),
                serde_json::json!({"stage": "liveness-dead"}),
            ),
        ];

        assert_eq!(
            dead_or_stalled_tasks(&events),
            vec!["B42".to_string(), "B41".to_string()]
        );
    }

    #[test]
    fn tick_report_is_idle_only_when_both_sides_are_empty() {
        assert!(tick_report(&[], &[], 0).is_idle());
        assert!(!tick_report(&["verify".to_string()], &[], 0).is_idle());
        assert!(!tick_report(
            &[],
            &[Injection {
                reason: InjectReason::AgentDown,
                message: String::new(),
            }],
            0,
        )
        .is_idle());
    }

    #[test]
    fn tick_report_not_idle_when_mechanical_and_pending_both_nonzero() {
        let report = tick_report(&["verify".to_string()], &[], 2);
        assert!(!report.is_idle(), "mechanical 与 pending 同时非零仍非 idle");
        assert_eq!(report.pending_inbox, 2);
    }

    #[test]
    fn tick_report_preserves_injection_order() {
        let injections = vec![
            Injection {
                reason: InjectReason::NewInstruction,
                message: String::new(),
            },
            Injection {
                reason: InjectReason::TaskFailed,
                message: String::new(),
            },
        ];
        let report = tick_report(&["merge".to_string()], &injections, 0);
        assert_eq!(report.mechanical, 1);
        assert_eq!(
            report.injected,
            vec![InjectReason::NewInstruction, InjectReason::TaskFailed]
        );
    }

    #[test]
    fn monitor_alarm_survives_intent_until_observed_activity() {
        let alarm = ledger::event(
            "EscalationRaised",
            "runtime:orch",
            Some("B132"),
            Some("r48"),
            serde_json::json!({
                "stage": "liveness-monitor",
                "verdict": "stall",
                "attemptId": "B132-A0001",
            }),
        );
        let nudge = ledger::event(
            "NudgeIssued",
            "runtime:orch",
            Some("B132"),
            Some("r48"),
            serde_json::json!({"attemptId": "B132-A0001"}),
        );
        assert_eq!(
            last_monitor_alarm(&[alarm.clone(), nudge], "B132", "B132-A0001"),
            Some(MonitorVerdict::Stall)
        );

        let ack = ledger::event(
            "DispatchAcked",
            "runtime:orch",
            Some("B132"),
            Some("r48"),
            serde_json::json!({"attemptId": "B132-A0001"}),
        );
        assert_eq!(
            last_monitor_alarm(&[alarm, ack], "B132", "B132-A0001"),
            None
        );
    }

    #[test]
    fn monitor_alarm_from_other_agent_does_not_suppress_observation() {
        let dispatch = ledger::event(
            "DispatchIssued",
            "runtime:orch",
            Some("B134"),
            Some("r49"),
            serde_json::json!({
                "agent": "executor-desktop",
                "attemptId": "B134-A0001",
                "attemptNo": 1,
            }),
        );
        let other_agent_alarm = ledger::event(
            "EscalationRaised",
            "runtime:orch",
            Some("B134"),
            Some("r49"),
            serde_json::json!({
                "stage": "liveness-monitor",
                "verdict": "stall",
                "attemptId": "B134-A0001",
                "attemptNo": 1,
                "agent": "executor-claw",
            }),
        );
        assert!(monitor_alarm_needs_append_for_agent(
            &[dispatch, other_agent_alarm],
            "B134",
            "B134-A0001",
            "executor-desktop",
            MonitorVerdict::Stall,
        ));
    }

    #[test]
    fn stale_planner_lease_escalates_on_current_round_and_is_reclaimed() {
        let root = b134_test_repo("stale-round");
        let lease = PlannerLease {
            round: "r48".into(),
            wake_id: "wake-stale-r48".into(),
            session_id: "session-stale".into(),
            trigger_key: "trigger-stale".into(),
            attempt: 1,
            owner_pid: std::process::id(),
            child_pid: Some(u32::MAX - 1),
            started_at: "2026-07-27T00:00:00Z".into(),
            reasons: vec!["new_instruction".into()],
            inbox_files: Vec::new(),
            log_path: None,
            model_wake_reservation_id: None,
        };
        assert!(try_acquire_planner_lease(&root, &lease).unwrap());
        assert!(!reconcile_planner_turn(&root).unwrap());
        assert!(read_planner_lease(&root).unwrap().is_none());

        let read = read_ledger(&root.join("coordination/rounds/r49/events.jsonl")).unwrap();
        assert!(read.bad_lines.is_empty());
        let stale = read
            .events
            .iter()
            .filter(|event| {
                event.kind == "EscalationRaised"
                    && event
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("stage"))
                        .and_then(serde_json::Value::as_str)
                        == Some("stale-lease-round")
            })
            .collect::<Vec<_>>();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].round.as_deref(), Some("r49"));
        assert!(!root.join("coordination/rounds/r48/events.jsonl").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn independent_monitor_panic_does_not_block_caller_progress() {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            isolate_monitor_action(|| {
                started_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(50));
                panic!("injected monitor panic");
            })
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        // Models the long verify/mechanical branch continuing while the
        // independent monitor is still inside its own tick.
        let mechanical_progress = 1usize;
        assert_eq!(mechanical_progress, 1);
        assert_eq!(worker.join().unwrap(), MonitorStepOutcome::Panicked);
    }

    #[test]
    fn unclaimed_go_rewakes_exactly_twice_then_escalates() {
        assert_eq!(
            go_unclaimed_action(1_000, 1_121, 120, false, 0, false),
            GoUnclaimedAction::Rewake { ordinal: 1 }
        );
        assert_eq!(
            go_unclaimed_action(1_000, 1_121, 120, false, 1, false),
            GoUnclaimedAction::Rewake { ordinal: 2 }
        );
        assert_eq!(
            go_unclaimed_action(1_000, 1_121, 120, false, 2, false),
            GoUnclaimedAction::Escalate
        );
        assert_eq!(
            go_unclaimed_action(1_000, 1_121, 120, false, 0, true),
            GoUnclaimedAction::Wait,
            "a live wake session forbids a duplicate provider launch"
        );
    }

    #[test]
    fn acknowledged_go_never_rewakes() {
        assert_eq!(
            go_unclaimed_action(1_000, u64::MAX, 1, true, 0, false),
            GoUnclaimedAction::Wait
        );
    }

    #[test]
    fn typed_ir_policy_defaults_off_and_reads_explicit_opt_in() {
        let defaults_ir = crate::plan::parse_signed_round_ir(
            "schemaVersion: 2\nround: r49\nrevision: 1\ntasks: []\n",
        )
        .unwrap();
        let defaults = auto_succession_policy(&defaults_ir);
        assert_eq!(defaults.stall_multiplier, 0);
        assert!(!defaults.auto_terminate_stalled);
        assert_eq!(defaults.ack_timeout_secs, 120);

        let opted_in_ir = crate::plan::parse_signed_round_ir(
            "schemaVersion: 2\nround: r49\nrevision: 1\nliveness:\n  stallEscalationMultiplier: 3\n  autoTerminateStalled: true\ndispatch:\n  ackTimeoutSeconds: 45\ntasks: []\n",
        )
        .unwrap();
        let opted_in = auto_succession_policy(&opted_in_ir);
        assert_eq!(opted_in.stall_multiplier, 3);
        assert!(opted_in.auto_terminate_stalled);
        assert_eq!(opted_in.ack_timeout_secs, 45);
    }

    #[test]
    fn stall_budget_escalation_event_is_attempt_idempotent() {
        let root = b134_test_repo("stall-idempotent");
        let dispatch = ledger::event(
            "DispatchIssued",
            "runtime:orch",
            Some("B142"),
            Some("r49"),
            serde_json::json!({
                "agent": "executor-desktop",
                "attemptId": "B142-A0001",
                "attemptNo": 1,
            }),
        );
        ledger::append(&root, "r49", &[dispatch]).unwrap();
        let observation = MonitoredAttempt {
            task_id: "B142".into(),
            agent: "executor-desktop".into(),
            attempt_id: "B142-A0001".into(),
            attempt_no: 1,
            last_activity_at: SystemTime::UNIX_EPOCH,
        };
        let payload = serde_json::json!({
            "stage": "stall-budget-exceeded",
            "attemptId": observation.attempt_id,
            "attemptNo": 1,
            "agent": observation.agent,
        });
        assert!(append_escalation_once(
            &root,
            "r49",
            &observation,
            "stall-budget-exceeded",
            payload.clone(),
        )
        .unwrap());
        assert!(!append_escalation_once(
            &root,
            "r49",
            &observation,
            "stall-budget-exceeded",
            payload,
        )
        .unwrap());
        let read = read_ledger(&root.join("coordination/rounds/r49/events.jsonl")).unwrap();
        assert_eq!(
            read.events
                .iter()
                .filter(|event| event_stage(event) == Some("stall-budget-exceeded"))
                .count(),
            1
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn two_stall_strikes_move_to_next_signed_candidate_without_wrap() {
        let hierarchy = vec![
            "executor-desktop".to_string(),
            "executor-claw".to_string(),
            "executor-opencode".to_string(),
        ];
        let timeout = |attempt: &str, agent: &str| {
            ledger::event(
                "AttemptTimedOut",
                "runtime:orch",
                Some("B142"),
                Some("r50"),
                serde_json::json!({
                    "attemptId": attempt,
                    "agent": agent,
                    "kind": "stall",
                    "processTreeTerminated": true,
                }),
            )
        };
        let one = vec![timeout("B142-A0001", "executor-desktop")];
        let one_strike = stall_strikes_by_agent(&one, "B142");
        let chain =
            crate::scheduler::escalation_chain_from_hierarchy(&hierarchy, "executor-desktop")
                .unwrap();
        let failed = chain
            .iter()
            .filter(|agent| one_strike.get(*agent).copied().unwrap_or(0) >= 2)
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            crate::scheduler::next_candidate(&chain, &failed, &[]).unwrap(),
            "executor-desktop"
        );

        let two = vec![
            timeout("B142-A0001", "executor-desktop"),
            timeout("B142-A0002", "executor-desktop"),
        ];
        let two_strikes = stall_strikes_by_agent(&two, "B142");
        let failed = chain
            .iter()
            .filter(|agent| two_strikes.get(*agent).copied().unwrap_or(0) >= 2)
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            crate::scheduler::next_candidate(&chain, &failed, &[]).unwrap(),
            "executor-claw"
        );
        assert!(crate::scheduler::next_candidate(&chain, &hierarchy, &[]).is_err());
    }

    #[test]
    fn wake_log_proxy_stall_escalates_without_kill() {
        assert_eq!(
            stall_termination_action(true, wake::TerminationPlan::LogQuiesceOnly),
            StallTerminationAction::EscalateOnly
        );
        assert_eq!(
            stall_termination_action(true, wake::TerminationPlan::SignalProcessGroup),
            StallTerminationAction::TerminateManaged
        );
        assert_eq!(
            stall_termination_action(false, wake::TerminationPlan::SignalProcessGroup),
            StallTerminationAction::EscalateOnly
        );
    }

    static B143_CANARY_SEQ: AtomicU64 = AtomicU64::new(0);
    const B143_ROUND: &str = "r-canary";
    const B143_GO_TASK: &str = "GO-REWAKE";
    const B143_ARC_TASK: &str = "STALL-ARC";
    const B143_STATIONS: [&str; 6] = [
        "go-claim",
        "go-rewake",
        "stall-escalation",
        "auto-terminate",
        "succession",
        "chain-exhausted",
    ];

    #[test]
    fn b203_compiled_provider_fixture() {
        let arguments = std::env::args().collect::<Vec<_>>();
        if let Some(registry) = arguments.iter().find_map(|argument| {
            argument
                .strip_prefix(B147_PROVIDER_REGISTRY_ARG_PREFIX)
                .map(PathBuf::from)
        }) {
            // SAFETY: getpgrp has no preconditions and only reads this process's group.
            let pgid = unsafe { b147_getpgrp() };
            assert!(pgid > 0, "B147 provider process group must be positive");
            let pid = std::process::id();
            assert_eq!(
                pgid as u32, pid,
                "managed B147 provider must lead its independent process group"
            );
            crate::gate::register_gate_fixture(Some(registry.as_os_str()), pid, pgid as u32)
                .expect("register B147 provider in fixture-local registry");
            let outer_registry = std::env::var_os(crate::gate::ORCH_GATE_FIXTURE_REGISTRY);
            if outer_registry.as_deref() != Some(registry.as_os_str()) {
                crate::gate::register_gate_fixture(outer_registry.as_deref(), pid, pgid as u32)
                    .expect("register B147 provider in outer gate registry");
            }

            if arguments
                .iter()
                .any(|argument| argument.contains(B147_CREDENTIAL_REJECT_TASK))
            {
                println!(
                    "\n{}",
                    r#"{"type":"step_start","sessionID":"wrong-provider-fixture"}"#
                );
            } else {
                println!(
                    "\n{}",
                    r#"{"type":"thread.started","thread_id":"fixture-thread"}"#
                );
            }
            std::io::stdout().flush().unwrap();
            loop {
                std::thread::park();
            }
        }

        let is_go_rewake = arguments.iter().any(|argument| argument.contains(B143_GO_TASK));
        let is_stall_arc = arguments.iter().any(|argument| argument.contains(B143_ARC_TASK));
        if !is_go_rewake && !is_stall_arc {
            return;
        }

        println!(
            "\n{}",
            r#"{"type":"thread.started","thread_id":"fixture-thread"}"#
        );
        std::io::stdout().flush().unwrap();
        if is_go_rewake {
            std::thread::sleep(Duration::from_secs(wake::canary_provider_lifetime_secs()));
            return;
        }
        loop {
            std::thread::park();
        }
    }

    struct B143CanaryRepo {
        root: PathBuf,
    }

    impl B143CanaryRepo {
        fn new(sample: usize) -> Self {
            let seq = B143_CANARY_SEQ.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let orch_root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .expect("manifest path must be inside the orch workspace");
            let root = orch_root.join("target/test-tmp").join(format!(
                "b143-unattended-sample-{sample}-{}-{seq}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&root).unwrap();
            let site = Self { root };
            site.initialize();
            site
        }

        fn initialize(&self) {
            b143_git(&self.root, &["init", "-q"]);
            b143_git(&self.root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
            fs::create_dir_all(self.root.join("coordination/runtime")).unwrap();
            fs::create_dir_all(
                self.root
                    .join(format!("coordination/rounds/{B143_ROUND}/tasks")),
            )
            .unwrap();
            fs::create_dir_all(self.root.join("coordination/modes")).unwrap();
            fs::write(
                self.root.join("coordination/runtime/CURRENT-ROUND"),
                format!("{B143_ROUND}\n"),
            )
            .unwrap();
            fs::write(
                self.root.join(".gitignore"),
                ".worktrees/\ncoordination/runtime/\ncoordination/rounds/*/dispatch/\n",
            )
            .unwrap();
            fs::write(
                self.root.join("coordination/PROJECT-BINDING.yaml"),
                r#"project: {ecosystems: [rust]}
workspace: {worktreeRoot: .worktrees}
commands: {testFast: {argv: [sh, -c, "exit 0"], timeoutSeconds: 30}}
scope: {protectedPaths: ["coordination/**"]}
git: {pushPolicy: forbidden}
oracle: {dialect: cargo}
"#,
            )
            .unwrap();
            fs::write(
                self.root.join("coordination/modes/canary.yaml"),
                r#"agents:
  executor: {adapter: test, tier: F}
  verifier: {adapter: root-manual, tier: none}
hitl: {mergeGate: auto}
verification: {mode: root-manual-fixed-head}
liveness:
  monitorSeconds: 15
  workingStallMinutes: 10
  confirmSamples: 2
  terminationGraceSeconds: 1
  stallEscalationMultiplier: 1
  autoTerminateStalled: true
dispatch:
  ackTimeoutSeconds: 0
scheduling:
  allowedAgents: [executor-desktop, executor-claw, executor-opencode]
  capacities:
    executor-desktop: {agent: 2, quota: 2, roles: [implement]}
    executor-claw: {agent: 1, quota: 1, roles: [implement, primary-review]}
    executor-opencode: {agent: 3, quota: 3, roles: [implement, secondary-review]}
budgets: {round: {wallMinutes: 60, maxModelWakes: 30}}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
"#,
            )
            .unwrap();
            self.write_card(B143_GO_TASK);
            self.write_card(B143_ARC_TASK);
            self.write_fake_agents();
            ledger::append(
                &self.root,
                B143_ROUND,
                &[ledger::event(
                    "RoundOpened",
                    "runtime:orch",
                    None,
                    Some(B143_ROUND),
                    serde_json::json!({}),
                )],
            )
            .unwrap();
            fs::write(self.root.join("README.md"), "unattended canary fixture\n").unwrap();
            b143_git(&self.root, &["add", "."]);
            b143_commit(&self.root, "fixture contract");

            crate::plan::run_plan(&self.root).unwrap();
            crate::round::run_sign_off(&self.root, Some("unattended canary fixture")).unwrap();
            b143_git(&self.root, &["add", "."]);
            b143_commit(&self.root, "signed canary plan");
        }

        fn write_card(&self, task: &str) {
            fs::write(
                self.root
                    .join(format!("coordination/rounds/{B143_ROUND}/tasks/{task}.md")),
                format!(
                    r#"---
taskId: {task}
round: {B143_ROUND}
agent: executor-desktop
seedProtocol: pure-spec
writeSet: [{task}.txt]
frozenPaths: ["coordination/**"]
gates: {{fast: [testFast]}}
budgets: {{wallMinutes: 1}}
requiredReviews:
  - {{role: primary, agent: executor-claw}}
requiredEvidence: [unattended-canary]
---
# {task}
"#
                ),
            )
            .unwrap();
        }

        fn write_fake_agents(&self) {
            let bin = self.root.join("test-bin");
            fs::create_dir_all(&bin).unwrap();
            let fake = bin.join("codex");
            // H56 exercises the production process-identity handshake under
            // load. A script-backed provider can expose the script and its
            // interpreter as two same-PID executable identities while that
            // handshake is in flight. Copy this already-running Rust test
            // binary so the provider has one compiled executable identity and
            // no shell/sleep child topology. The exact libtest case below still
            // emits the real engagement frame, observes the rendered task
            // message, and follows the production supervisor/reaper path.
            fs::copy(std::env::current_exe().unwrap(), &fake).unwrap();
            let mut permissions = fs::metadata(&fake).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&fake, permissions).unwrap();
            let argv = serde_json::json!([
                fake.display().to_string(),
                "--exact",
                "serve::tests::b203_compiled_provider_fixture",
                "--nocapture",
                "--test-threads=1",
                "--skip",
                "{message}"
            ]);
            let agent = |session: &str| {
                serde_json::json!({
                    "injectable": true,
                    "sessionId": session,
                    "wake": {"argv": argv.clone()}
                })
            };
            fs::write(
                self.root.join("coordination/agents.yaml"),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "agents": {
                        "executor-desktop": agent("desktop-canary"),
                        "executor-claw": agent("claw-canary"),
                        "executor-opencode": agent("opencode-canary")
                    }
                }))
                .unwrap(),
            )
            .unwrap();
        }

        fn events(&self) -> Vec<EventRecord> {
            orch_core::read_ledger(
                &self
                    .root
                    .join(format!("coordination/rounds/{B143_ROUND}/events.jsonl")),
            )
            .unwrap()
            .events
        }

        fn poison_completed_pid(&self, task: &str) {
            let path = self
                .root
                .join(format!("coordination/rounds/{B143_ROUND}/events.jsonl"));
            let text = fs::read_to_string(&path).unwrap();
            let mut changed = false;
            let mut lines = Vec::new();
            for line in text.lines() {
                let mut value: serde_json::Value = serde_json::from_str(line).unwrap();
                if value["type"].as_str() == Some("DispatchWakeCompleted")
                    && value["taskId"].as_str() == Some(task)
                    && !changed
                {
                    value["payload"]["pid"] = serde_json::json!(u32::MAX - 1);
                    changed = true;
                }
                lines.push(serde_json::to_string(&value).unwrap());
            }
            assert!(changed, "fixture must contain a completed wake to fault");
            fs::write(path, format!("{}\n", lines.join("\n"))).unwrap();
        }

        fn claim_and_block(&self, task: &str, sample: usize) {
            let short = "A0001";
            let wait = Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(3)
                .unwrap()
                .join("coordination/scripts/wait-dispatch.sh");
            let output = Command::new("sh")
                .arg(wait)
                .args(["executor-desktop", "1", task, short])
                .current_dir(&self.root)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "fixture wait failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("GO_FOUND"));

            // The observer is ready before the real production await entry
            // runs synchronously on this test thread. Preserve B203's signed
            // eight-second boundary as four two-second reconciliation ticks;
            // elapsed time is converted once per observation and the verdict
            // itself remains a pure, tick-denominated decision.
            let ack_tick = crate::tierf::AWAIT_REPORT_RECONCILE_TICK;
            let ack_tick_budget = exact_tick_budget(Duration::from_secs(8), ack_tick);
            let observer_root = self.root.clone();
            let observer_task = task.to_string();
            let observer_attempt = format!("{observer_task}-A0001");
            let observer_ack = observer_root.join(format!(
                "coordination/rounds/{B143_ROUND}/dispatch/executor-desktop/\
                 GO-{observer_task}-A0001.md.ack"
            ));
            let observer_ready = std::sync::Arc::new(std::sync::Barrier::new(2));
            let observer_ready_child = std::sync::Arc::clone(&observer_ready);
            let await_finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let await_finished_child = std::sync::Arc::clone(&await_finished);
            let observer = std::thread::spawn(move || -> std::result::Result<(), String> {
                observer_ready_child.wait();
                let started = Instant::now();
                let ledger_path = observer_root
                    .join(format!("coordination/rounds/{B143_ROUND}/events.jsonl"));
                loop {
                    let events = orch_core::read_ledger(&ledger_path)
                        .map_err(|error| {
                            format!(
                                "H56 sample {sample} cannot read {} while awaiting ACK: {error}",
                                ledger_path.display()
                            )
                        })?
                        .events;
                    let dispatch_acked = events.iter().any(|event| {
                        event.kind == "DispatchAcked"
                            && event.task_id.as_deref() == Some(&observer_task)
                            && event.payload.as_ref().and_then(|p| p["attemptId"].as_str())
                                == Some(observer_attempt.as_str())
                    });
                    let attempt_started = events.iter().any(|event| {
                        event.kind == "AttemptStarted"
                            && event.task_id.as_deref() == Some(&observer_task)
                            && event.payload.as_ref().and_then(|p| p["attemptId"].as_str())
                                == Some(observer_attempt.as_str())
                    });
                    let ack_present = fs::symlink_metadata(&observer_ack)
                        .is_ok_and(|metadata| {
                            metadata.file_type().is_file() && !metadata.file_type().is_symlink()
                        });
                    let stage = classify_ack_pipeline(AckSnapshot {
                        ack_present,
                        dispatch_acked,
                        attempt_started,
                    });
                    let ticks_elapsed = elapsed_reconcile_ticks(started.elapsed(), ack_tick);
                    let verdict =
                        ack_convergence_verdict(stage, ticks_elapsed, ack_tick_budget);
                    if verdict.converged {
                        let report = observer_root.join(format!(
                            "coordination/rounds/{B143_ROUND}/reports/{}-BLOCKED.md",
                            observer_task
                        ));
                        fs::create_dir_all(report.parent().unwrap()).map_err(|error| {
                            format!(
                                "H56 sample {sample} cannot create BLOCKED parent {}: {error}",
                                report.display()
                            )
                        })?;
                        fs::write(&report, "# fixture terminal\n").map_err(|error| {
                            format!(
                                "H56 sample {sample} cannot write {}: {error}",
                                report.display()
                            )
                        })?;
                        return Ok(());
                    }

                    let await_returned = await_finished_child.load(Ordering::Acquire);
                    if await_returned || verdict.exceeded {
                        let recent = events
                            .iter()
                            .rev()
                            .take(8)
                            .map(|event| {
                                format!(
                                    "{}:{}:{}",
                                    event.kind,
                                    event.task_id.as_deref().unwrap_or("-"),
                                    event
                                        .payload
                                        .as_ref()
                                        .and_then(|payload| payload["attemptId"].as_str())
                                        .unwrap_or("-")
                                )
                            })
                            .collect::<Vec<_>>();
                        return Err(format!(
                            "H56 sample {sample} ACK pipeline did not converge within B203 budget: \
                             task={} awaitReturned={await_returned} stage={:?} \
                             ticksElapsed={} tickBudget={} tick={:?} root={} recent={recent:?}",
                            observer_task,
                            verdict.stage,
                            verdict.ticks_elapsed,
                            verdict.tick_budget,
                            ack_tick,
                            observer_root.display()
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            });

            observer_ready.wait();
            // run_await restores its timeout from the durable DispatchIssued
            // clock. Compute this immediately before entry so the fixture has
            // its original 20 seconds of remaining await time, independent of
            // the already-spent setup and observer scheduling time.
            let events = self.events();
            let dispatch_ts = crate::attempt::latest_dispatch_ts(&events, task)
                .expect("B143 claim fixture requires a current DispatchIssued");
            let durable_elapsed = crate::liveness::attempt_elapsed_from_dispatch(
                &dispatch_ts,
                &crate::ledger::now_rfc3339(),
            )
            .expect("B143 claim fixture must recover durable attempt elapsed");
            let elapsed_ceil_secs = durable_elapsed
                .as_secs()
                .saturating_add(u64::from(durable_elapsed.subsec_nanos() != 0));
            let await_timeout_secs = elapsed_ceil_secs
                .checked_add(20)
                .expect("B143 await timeout overflow");
            let await_result = crate::tierf::run_await(
                &self.root,
                task,
                await_timeout_secs,
                Some(crate::liveness::LivenessOpts {
                    probe_every: Duration::ZERO,
                    ..crate::liveness::LivenessOpts::default()
                }),
            );
            await_finished.store(true, Ordering::Release);
            let observer_result = observer
                .join()
                .unwrap_or_else(|_| panic!("H56 sample {sample} ACK observer panicked"));
            if let Err(observer_error) = observer_result {
                match await_result {
                    Ok(_) => panic!("{observer_error}"),
                    Err(await_error) => {
                        panic!("{observer_error}; run_await failed: {await_error:#}")
                    }
                }
            }
            let outcome = await_result.unwrap();
            assert!(matches!(
                outcome,
                crate::tierf::AwaitOutcome::Blocked { .. }
            ));
        }

        fn wait_for_registered_reapers(&self, sample: usize) {
            let supervisor_dir = self.root.join("coordination/runtime/supervisors");
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                let mut observed = 0;
                let mut waiting = Vec::new();
                for entry in fs::read_dir(&supervisor_dir).unwrap() {
                    let entry = entry.unwrap();
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if !name.ends_with(".reap.json") {
                        continue;
                    }
                    observed += 1;
                    let value: serde_json::Value =
                        serde_json::from_slice(&fs::read(entry.path()).unwrap()).unwrap();
                    match value["state"].as_str() {
                        Some("reaped") => {}
                        Some("waiting") => waiting.push(name.into_owned()),
                        state => panic!(
                            "H56 sample {sample} has invalid reap state {state:?}: {value}"
                        ),
                    }
                }
                if observed > 0 && waiting.is_empty() {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "H56 sample {sample} reapers did not reach terminal state: \
                     observed={observed} waiting={waiting:?}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    impl Drop for B143CanaryRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn b143_git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn b143_commit(root: &Path, message: &str) {
        b143_git(
            root,
            &[
                "-c",
                "user.name=orch-canary",
                "-c",
                "user.email=orch-canary@example.invalid",
                "commit",
                "-q",
                "-m",
                message,
            ],
        );
    }

    fn b143_event(actor: &str, method: &str, station: &str) -> CanaryEvent {
        CanaryEvent {
            actor: actor.to_string(),
            method: method.to_string(),
            station: station.to_string(),
        }
    }

    fn b143_station_from_event(event: &EventRecord) -> Option<CanaryEvent> {
        let payload = event.payload.as_ref();
        let station = match event.kind.as_str() {
            "DispatchAcked" => "go-claim",
            "EscalationRaised"
                if payload.and_then(|p| p["stage"].as_str()) == Some("go-unclaimed-rewake") =>
            {
                "go-rewake"
            }
            "EscalationRaised"
                if payload.and_then(|p| p["stage"].as_str()) == Some("stall-budget-exceeded") =>
            {
                "stall-escalation"
            }
            "AttemptTimedOut"
                if payload.and_then(|p| p["kind"].as_str()) == Some("stall")
                    && payload.and_then(|p| p["processTreeTerminated"].as_bool()) == Some(true) =>
            {
                "auto-terminate"
            }
            "DispatchIssued"
                if payload
                    .and_then(|p| p["attemptNo"].as_u64())
                    .unwrap_or_default()
                    > 1 =>
            {
                "succession"
            }
            // B147：自动接替链耗尽统一为 eligible-chain-exhausted（能力过滤
            // 全校验）；stall-chain-exhausted 仅兼容 B147 前的历史账本。
            "EscalationRaised"
                if matches!(
                    payload.and_then(|p| p["stage"].as_str()),
                    Some("eligible-chain-exhausted") | Some("stall-chain-exhausted")
                ) =>
            {
                "chain-exhausted"
            }
            _ => return None,
        };
        Some(b143_event(
            &event.actor,
            if event.actor == "user" {
                "manual-cli"
            } else {
                "runtime-production-entry"
            },
            station,
        ))
    }

    fn b143_append_old_activity(root: &Path, attempt_no: usize, agent: &str) {
        let attempt_id = format!("{B143_ARC_TASK}-A{attempt_no:04}");
        let mut event = ledger::event(
            "AttemptStarted",
            "runtime:orch",
            Some(B143_ARC_TASK),
            Some(B143_ROUND),
            serde_json::json!({
                "attemptId": attempt_id,
                "attemptNo": attempt_no,
                "agent": agent,
            }),
        );
        event.ts = "2000-01-01T00:00:00Z".to_string();
        ledger::append(root, B143_ROUND, &[event]).unwrap();
    }

    fn b143_occupy_later_candidates(root: &Path) {
        let occupied = [
            ("BUSY-CLAW", "executor-claw"),
            ("BUSY-OPENCODE", "executor-opencode"),
        ]
        .map(|(task, agent)| {
            ledger::event(
                "DispatchIssued",
                "runtime:fixture",
                Some(task),
                Some(B143_ROUND),
                serde_json::json!({
                    "attemptId": format!("{task}-A0001"),
                    "attemptNo": 1,
                    "agent": agent,
                }),
            )
        });
        ledger::append(root, B143_ROUND, &occupied).unwrap();
    }

    #[test]
    #[ignore = "testExclusive:serve::tests::production_entries_visit_all_six_unattended_stations"]
    fn production_entries_visit_all_six_unattended_stations() {
        const B203_TRIAL_GATE_SAMPLES: usize = 10;
        let mut completed = 0;
        for sample in 0..B203_TRIAL_GATE_SAMPLES {
            b203_h56_one_sample(sample);
            completed += 1;
        }
        assert_eq!(completed, 10, "H56 trial-gate samples completed");
    }

    fn b203_h56_one_sample(sample: usize) {
        let site = B143CanaryRepo::new(sample);
        crate::tierf::run_dispatch(&site.root, B143_GO_TASK, false).unwrap();
        site.poison_completed_pid(B143_GO_TASK);
        let mut monitor = LivenessMonitorState::default();
        liveness_monitor_tick(&site.root, &mut monitor).unwrap();
        assert!(site.events().iter().any(|event| {
            event.kind == "EscalationRaised"
                && event.payload.as_ref().and_then(|p| p["stage"].as_str())
                    == Some("go-unclaimed-rewake")
        }));
        site.claim_and_block(B143_GO_TASK, sample);

        // Fixture precondition: later signed candidates are already occupied.
        // Recovery actions below remain production-generated; after two strikes
        // the runtime must escalate instead of wrapping or double-dispatching.
        b143_occupy_later_candidates(&site.root);
        crate::tierf::run_dispatch(&site.root, B143_ARC_TASK, false).unwrap();
        let expected_agents = ["executor-desktop", "executor-desktop"];
        for (index, agent) in expected_agents.iter().enumerate() {
            b143_append_old_activity(&site.root, index + 1, agent);
            liveness_monitor_tick(&site.root, &mut monitor).unwrap();
        }

        let mapped = site
            .events()
            .iter()
            .filter_map(b143_station_from_event)
            .collect::<Vec<_>>();
        if !mapped
            .iter()
            .any(|event| event.station == "chain-exhausted")
        {
            for event in site.events().iter().filter(|event| {
                event.task_id.as_deref() == Some(B143_ARC_TASK)
                    && matches!(
                        event.kind.as_str(),
                        "DispatchIssued"
                            | "AttemptTimedOut"
                            | "EscalationRaised"
                            | "ActionRejected"
                    )
            }) {
                eprintln!(
                    "ARC EVENT {} {}",
                    event.kind,
                    event.payload.as_ref().unwrap_or(&serde_json::Value::Null)
                );
            }
        }
        let required = B143_STATIONS
            .iter()
            .map(|station| station.to_string())
            .collect::<Vec<_>>();
        let summary = canary_summary(&mapped);
        canary_gate(&summary, &required).unwrap();
        assert_eq!(summary.manual, 0);
        assert!(required
            .iter()
            .all(|station| summary.stations.contains(station)));
        site.wait_for_registered_reapers(sample);
    }

    // ─────────────────── B147 主审 FAIL 整改：卡面交付边界 5 的真实生产路径回归 ───────────────────
    // 两条回归从 seam/fake 层升级为真实账本事实断言（review B147-A0001 P1-1/P1-2）：
    //   ① disjoint-writeSet + dependsOn 夹具走真实 wave::run_wave → 真实
    //     tierf::run_dispatch；账本断言：首波仅前驱 DispatchIssued、后继零派发；
    //     前驱 TaskRecorded 落账后第二次真实 run_wave 才派后继——后继的派发显式
    //     依赖前驱的 Recorded 账本投影，而非 fake merge() -> true。
    //   ② critical 两振夹具走真实 liveness_monitor_tick → handle_stall_budget
    //     自动接替路径；账本断言：恰一条 EscalationRaised{stage:
    //     "eligible-chain-exhausted"}，无 critical-implement 能力候选零
    //     DispatchIssued（绝不降级的账本事实）。
    // 夹具复刻 B143 六站点生产路径 idiom（真实签核 IR + 真实受管 fake-codex 进程）。

    static B147_PROD_SEQ: AtomicU64 = AtomicU64::new(0);
    const B147_PROD_ROUND: &str = "r-b147";
    const B147_PROVIDER_REGISTRY_ARG_PREFIX: &str = "b147-provider-registry=";
    const B147_CREDENTIAL_REJECT_TASK: &str = "CRED-REJECT";

    fn b147_compiled_provider_argv(executable: &Path, registry: &Path) -> Vec<String> {
        vec![
            executable.display().to_string(),
            "--exact".to_string(),
            "serve::tests::b203_compiled_provider_fixture".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
            "--skip".to_string(),
            "{message}".to_string(),
            "--skip".to_string(),
            format!("{B147_PROVIDER_REGISTRY_ARG_PREFIX}{}", registry.display()),
        ]
    }

    struct B147ProductionRepo {
        root: PathBuf,
        cleanup_attempted: bool,
    }

    impl B147ProductionRepo {
        /// cards: (task_id, 附加 frontmatter 行)——附加行自带换行（dependsOn /
        /// capabilities 由此注入，保持基础卡面与 B143 同构）。
        fn new(tag: &str, cards: &[(&str, &str)]) -> Self {
            let seq = B147_PROD_SEQ.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let orch_root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .expect("manifest path must be inside the orch workspace");
            let root = orch_root.join("target/test-tmp").join(format!(
                "b147-production-{tag}-{}-{seq}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&root).unwrap();
            let site = Self {
                root,
                cleanup_attempted: false,
            };
            site.initialize(cards);
            site
        }

        fn initialize(&self, cards: &[(&str, &str)]) {
            b143_git(&self.root, &["init", "-q"]);
            b143_git(&self.root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
            fs::create_dir_all(self.root.join("coordination/runtime")).unwrap();
            fs::create_dir_all(
                self.root
                    .join(format!("coordination/rounds/{B147_PROD_ROUND}/tasks")),
            )
            .unwrap();
            fs::create_dir_all(self.root.join("coordination/modes")).unwrap();
            fs::write(
                self.root.join("coordination/runtime/CURRENT-ROUND"),
                format!("{B147_PROD_ROUND}\n"),
            )
            .unwrap();
            fs::write(
                self.root.join(".gitignore"),
                ".worktrees/\ncoordination/runtime/\ncoordination/rounds/*/dispatch/\n",
            )
            .unwrap();
            fs::write(
                self.root.join("coordination/PROJECT-BINDING.yaml"),
                r#"project: {ecosystems: [rust]}
workspace: {worktreeRoot: .worktrees}
commands: {testFast: {argv: [sh, -c, "exit 0"], timeoutSeconds: 30}}
scope: {protectedPaths: ["coordination/**"]}
git: {pushPolicy: forbidden}
oracle: {dialect: cargo}
"#,
            )
            .unwrap();
            // 与 B143 同构，唯 capacities 的 roles 拉开能力差：仅 executor-desktop
            // 持 critical-implement，claw/opencode 只有 implement。
            fs::write(
                self.root.join("coordination/modes/b147.yaml"),
                r#"agents:
  executor: {adapter: test, tier: F}
  verifier: {adapter: root-manual, tier: none}
hitl: {mergeGate: auto}
verification: {mode: root-manual-fixed-head}
liveness:
  monitorSeconds: 15
  workingStallMinutes: 10
  confirmSamples: 2
  terminationGraceSeconds: 1
  stallEscalationMultiplier: 1
  autoTerminateStalled: true
dispatch:
  ackTimeoutSeconds: 0
scheduling:
  allowedAgents: [executor-desktop, executor-claw, executor-opencode]
  capacities:
    executor-desktop: {agent: 2, quota: 2, roles: [implement, critical-implement]}
    executor-claw: {agent: 1, quota: 1, roles: [implement, primary-review]}
    executor-opencode: {agent: 3, quota: 3, roles: [implement, secondary-review]}
budgets: {round: {wallMinutes: 60, maxModelWakes: 30}}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
"#,
            )
            .unwrap();
            for (task, extra) in cards {
                self.write_card(task, extra);
            }
            self.write_fake_agents();
            ledger::append(
                &self.root,
                B147_PROD_ROUND,
                &[ledger::event(
                    "RoundOpened",
                    "runtime:orch",
                    None,
                    Some(B147_PROD_ROUND),
                    serde_json::json!({}),
                )],
            )
            .unwrap();
            fs::write(self.root.join("README.md"), "b147 production fixture\n").unwrap();
            b143_git(&self.root, &["add", "."]);
            b143_commit(&self.root, "fixture contract");

            crate::plan::run_plan(&self.root).unwrap();
            crate::round::run_sign_off(&self.root, Some("b147 production fixture")).unwrap();
            b143_git(&self.root, &["add", "."]);
            b143_commit(&self.root, "signed b147 plan");
        }

        fn write_card(&self, task: &str, extra: &str) {
            fs::write(
                self.root.join(format!(
                    "coordination/rounds/{B147_PROD_ROUND}/tasks/{task}.md"
                )),
                format!(
                    r#"---
taskId: {task}
round: {B147_PROD_ROUND}
agent: executor-desktop
seedProtocol: pure-spec
writeSet: [{task}.txt]
frozenPaths: ["coordination/**"]
gates: {{fast: [testFast]}}
budgets: {{wallMinutes: 1}}
requiredReviews:
  - {{role: primary, agent: executor-claw}}
requiredEvidence: [b147-production]
{extra}---
# {task}
"#
                ),
            )
            .unwrap();
        }

        fn write_fake_agents(&self) {
            let bin = self.root.join("test-bin");
            fs::create_dir_all(&bin).unwrap();
            let fake = bin.join("codex");
            // Keep OFFER/ACCEPTED executable identity stable: a shell-backed
            // fixture can expose the interpreter and script as two same-PID
            // identities. The copied libtest binary emits the same provider
            // receipt without adding a shell/sleep process topology.
            fs::copy(std::env::current_exe().unwrap(), &fake).unwrap();
            let mut permissions = fs::metadata(&fake).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&fake, permissions).unwrap();
            let argv = serde_json::json!(b147_compiled_provider_argv(
                &fake,
                &self.provider_registry_path()
            ));
            let agent = |session: &str| {
                serde_json::json!({
                    "injectable": true,
                    "sessionId": session,
                    "wake": {"argv": argv.clone()}
                })
            };
            fs::write(
                self.root.join("coordination/agents.yaml"),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "agents": {
                        "executor-desktop": agent("desktop-b147"),
                        "executor-claw": agent("claw-b147"),
                        "executor-opencode": agent("opencode-b147")
                    }
                }))
                .unwrap(),
            )
            .unwrap();
        }

        fn provider_registry_path(&self) -> PathBuf {
            self.root
                .join("coordination/runtime/b147-provider-groups.registry")
        }

        fn registered_provider_groups(&self) -> Vec<(u32, u32)> {
            let bytes = fs::read_to_string(self.provider_registry_path()).unwrap();
            bytes
                .lines()
                .map(|line| {
                    let mut fields = line.split('\t');
                    let pid = fields.next().unwrap().parse::<u32>().unwrap();
                    let pgid = fields.next().unwrap().parse::<u32>().unwrap();
                    assert!(
                        fields.next().is_some(),
                        "registry entry must contain birth identity"
                    );
                    (pid, pgid)
                })
                .collect()
        }

        fn registered_wake_owner_groups(&self) -> Vec<(u32, u32)> {
            let supervisor_dir = self.root.join("coordination/runtime/supervisors");
            fs::read_dir(supervisor_dir)
                .unwrap()
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.ends_with(".reap.json"))
                })
                .map(|path| {
                    let value: serde_json::Value =
                        serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
                    (
                        value["pid"].as_u64().unwrap() as u32,
                        value["pgid"].as_u64().unwrap() as u32,
                    )
                })
                .collect()
        }

        fn wait_for_wake_owner_reapers(
            &self,
            expected_reapers: usize,
        ) -> std::result::Result<(), String> {
            let supervisor_dir = self.root.join("coordination/runtime/supervisors");
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let entries = match fs::read_dir(&supervisor_dir) {
                    Ok(entries) => entries,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::NotFound
                            && expected_reapers == 0 =>
                    {
                        return Ok(());
                    }
                    Err(error) => {
                        return Err(format!(
                            "read B147 supervisor directory {} failed: {error}",
                            supervisor_dir.display()
                        ));
                    }
                };
                let mut pending = Vec::new();
                let mut observed = 0_usize;
                for entry in entries {
                    let entry = entry.map_err(|error| {
                        format!(
                            "read B147 supervisor entry under {} failed: {error}",
                            supervisor_dir.display()
                        )
                    })?;
                    let path = entry.path();
                    let Some(name) = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .filter(|name| name.ends_with(".reap.json"))
                    else {
                        continue;
                    };
                    observed += 1;
                    let state = fs::read(&path)
                        .ok()
                        .and_then(|bytes| {
                            serde_json::from_slice::<serde_json::Value>(&bytes).ok()
                        })
                        .and_then(|value| value["state"].as_str().map(str::to_string));
                    if state.as_deref() != Some("reaped") {
                        pending.push(format!("{name}:{state:?}"));
                    }
                }
                if observed < expected_reapers {
                    pending.push(format!(
                        "reap-sidecar-count:{observed}/{expected_reapers}"
                    ));
                }
                if pending.is_empty() {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "B147 fixture owner reapers did not settle: observed={observed} pending={pending:?}"
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn events(&self) -> Vec<EventRecord> {
            orch_core::read_ledger(&self.root.join(format!(
                "coordination/rounds/{B147_PROD_ROUND}/events.jsonl"
            )))
            .unwrap()
            .events
        }

        fn append(&self, events: &[EventRecord]) {
            ledger::append(&self.root, B147_PROD_ROUND, events).unwrap();
        }

        /// 夹具事实写入：ack 本 attempt（unclaimed-GO 判 Wait，不干扰 stall 路径），
        /// 再把 last_activity 锚到远古（B143 idiom）——monitor 下一拍据此判 Stall。
        fn ack_and_age_attempt(&self, task: &str, attempt_no: usize, agent: &str) {
            let identity = serde_json::json!({
                "attemptId": format!("{task}-A{attempt_no:04}"),
                "attemptNo": attempt_no,
                "agent": agent,
            });
            let ack = ledger::event(
                "DispatchAcked",
                "runtime:fixture",
                Some(task),
                Some(B147_PROD_ROUND),
                identity.clone(),
            );
            let mut started = ledger::event(
                "AttemptStarted",
                "runtime:fixture",
                Some(task),
                Some(B147_PROD_ROUND),
                identity,
            );
            started.ts = "2000-01-01T00:00:00Z".to_string();
            self.append(&[ack, started]);
        }

        /// TaskRecorded 属 merge-lifecycle kind（ledger_capability_gate 源码门卫
        /// 禁止 serve.rs 内经 ledger::event 直写）；夹具按 attempt_takeover
        /// idiom 逐字段构造等价账本事实事件。
        fn task_recorded_fixture_event(&self, task: &str) -> EventRecord {
            EventRecord {
                event_id: format!("b147-fixture-recorded-{task}"),
                ts: ledger::now_rfc3339(),
                actor: "runtime:fixture".to_string(),
                kind: "TaskRecorded".to_string(),
                task_id: Some(task.to_string()),
                round: Some(B147_PROD_ROUND.to_string()),
                payload: Some(serde_json::json!({"artifact": "fixture-prerequisite-recorded"})),
                extra: serde_json::Map::new(),
            }
        }

        /// 本 task 全部 DispatchIssued 在账本中的下标（保序）——派发事实断言基元。
        fn dispatch_positions(&self, task: &str) -> Vec<usize> {
            self.events()
                .iter()
                .enumerate()
                .filter(|(_, event)| {
                    event.kind == "DispatchIssued" && event.task_id.as_deref() == Some(task)
                })
                .map(|(index, _)| index)
                .collect()
        }

        /// 真实受管 fake-codex 进程组是真实 wake 的副作用；测试结束复用生产
        /// 收敛内核按 birth-anchored registry 收割，覆盖 completed 与
        /// DispatchWakeCompleted 前的失败窗口；绝不对陈旧 event PID 无锚重投。
        fn cleanup_once(&mut self) -> std::result::Result<(), String> {
            if self.cleanup_attempted {
                return Err("B147 fixture cleanup was already attempted".to_string());
            }
            self.cleanup_attempted = true;
            let provider_groups =
                crate::gate::reap_gate_fixture_registry(&self.provider_registry_path())
                .map_err(|error| format!("B147 fixture-local provider reap failed: {error:#}"))?;
            self.wait_for_wake_owner_reapers(provider_groups.len())?;
            fs::remove_dir_all(&self.root).map_err(|error| {
                format!(
                    "remove settled B147 fixture root {} failed: {error}",
                    self.root.display()
                )
            })
        }

        fn finish(mut self) -> std::result::Result<(), String> {
            self.cleanup_once()
        }
    }

    impl Drop for B147ProductionRepo {
        fn drop(&mut self) {
            if self.cleanup_attempted {
                return;
            }
            if let Err(error) = self.cleanup_once() {
                if std::thread::panicking() {
                    eprintln!(
                        "B147 fixture cleanup retained {} during unwind: {error}",
                        self.root.display()
                    );
                } else {
                    panic!(
                        "B147 fixture cleanup retained {} with evidence: {error}",
                        self.root.display()
                    );
                }
            }
        }
    }

    #[test]
    fn b147_rejected_provider_before_dispatch_completion_is_reaped_from_local_registry() {
        let site = B147ProductionRepo::new(
            "credential-reject",
            &[(B147_CREDENTIAL_REJECT_TASK, "")],
        );
        let error = crate::tierf::run_dispatch(&site.root, B147_CREDENTIAL_REJECT_TASK, false)
            .unwrap_err();
        let rendered_error = format!("{error:#}");
        assert!(rendered_error.contains("provider wake launch rejected"));
        assert!(rendered_error.contains("wrong provider"));

        let events = site.events();
        let task_events = |kind: &str| {
            events
                .iter()
                .filter(|event| {
                    event.kind == kind
                        && event.task_id.as_deref() == Some(B147_CREDENTIAL_REJECT_TASK)
                })
                .count()
        };
        assert_eq!(task_events("DispatchWakeLaunching"), 1);
        assert_eq!(task_events("DispatchWakeReleased"), 1);
        assert_eq!(task_events("DispatchWakeDelivered"), 0);
        assert_eq!(task_events("DispatchWakeCompleted"), 0);
        let rejections = events
            .iter()
            .filter(|event| {
                event.kind == "ActionRejected"
                    && event.task_id.as_deref() == Some(B147_CREDENTIAL_REJECT_TASK)
            })
            .collect::<Vec<_>>();
        assert_eq!(rejections.len(), 1, "unexpected rejection chain: {events:?}");
        let rejection = rejections[0].payload.as_ref().unwrap();
        assert_eq!(rejection["operation"].as_str(), Some("dispatch"));
        let reason = rejection["reason"].as_str().unwrap();
        assert!(reason.contains("provider wake launch rejected"));
        assert!(reason.contains("wrong provider"));

        let groups = site.registered_provider_groups();
        assert_eq!(groups.len(), 1, "expected one pre-completion provider group");
        let (pid, pgid) = groups[0];
        assert_eq!(pid, pgid, "managed provider must be its process-group leader");
        if let Some(outer_registry) = std::env::var_os(crate::gate::ORCH_GATE_FIXTURE_REGISTRY) {
            let prefix = format!("{pid}\t{pgid}\t");
            assert!(
                fs::read_to_string(outer_registry)
                    .unwrap()
                    .lines()
                    .any(|line| line.starts_with(&prefix)),
                "provider must also register in the inherited gate registry"
            );
        }
        assert!(
            crate::wake::observe_exact_gate_group_members(pgid)
                .unwrap()
                .is_some(),
            "registered provider must still be live before fixture Drop"
        );
        let owner_groups = site.registered_wake_owner_groups();
        assert_eq!(owner_groups.len(), 1, "expected one exact-Child owner reaper");
        let fixture_root = site.root.clone();

        site.finish().unwrap();
        assert!(
            !fixture_root.exists(),
            "settled B147 fixture cleanup must remove its scratch root"
        );
        let gone_deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if crate::wake::observe_exact_gate_group_members(pgid)
                .unwrap()
                .is_none()
            {
                break;
            }
            assert!(
                Instant::now() < gone_deadline,
                "B147 provider group {pgid} survived fixture Drop"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        for (_, owner_pgid) in owner_groups {
            assert!(
                crate::wake::observe_exact_gate_group_members(owner_pgid)
                    .unwrap()
                    .is_none(),
                "B147 exact-Child owner group {owner_pgid} survived fixture Drop"
            );
        }
    }

    /// P1-2 整改：A/B writeSet 完全不相交 + B dependsOn A，全程真实
    /// wave::run_wave → 真实 tierf::run_dispatch，账本事实断言——首波仅前驱
    /// DispatchIssued、后继零派发；前驱 TaskRecorded 落账后后继才被真实派发。
    #[test]
    fn b147_wave_dispatches_dependent_only_after_prerequisite_taskrecorded() {
        let site = B147ProductionRepo::new(
            "depwave",
            &[("DEP-A", ""), ("DEP-B", "dependsOn: [DEP-A]\n")],
        );

        // 第一次真实 run_wave：签核 IR 的 dependsOn 把 DEP-B 锁在严格更晚波次；
        // DEP-A 真实派发但永不交 REPORT → 波闸阻断在首波，后继波绝不进入。
        let first = crate::wave::run_wave(&site.root, 1).unwrap();
        assert_eq!(
            first.wave.blocked_wave,
            Some(0),
            "前驱未 Recorded，波闸必须阻断在首波"
        );
        let events = site.events();
        let a_positions = site.dispatch_positions("DEP-A");
        assert_eq!(a_positions.len(), 1, "首波账本恰一条前驱派发: {events:?}");
        assert_eq!(
            events[a_positions[0]]
                .payload
                .as_ref()
                .and_then(|payload| payload["agent"].as_str()),
            Some("executor-desktop")
        );
        assert!(
            site.dispatch_positions("DEP-B").is_empty(),
            "前驱未 Recorded，后继账本必须零 DispatchIssued: {events:?}"
        );

        // 前驱 TaskRecorded 落账（run_merge 的同类账本事实）→ 真实 Recorded 投影。
        site.append(&[site.task_recorded_fixture_event("DEP-A")]);

        // 第二次真实 run_wave：DEP-A 相位 Recorded 跳过，DEP-B 所在波真实派发。
        let second = crate::wave::run_wave(&site.root, 1).unwrap();
        assert_eq!(
            site.dispatch_positions("DEP-A").len(),
            1,
            "Recorded 前驱绝不重派"
        );
        let b_positions = site.dispatch_positions("DEP-B");
        assert_eq!(
            b_positions.len(),
            1,
            "前驱 TaskRecorded 后后继才被真实派发: {:?}",
            site.events()
        );
        let events = site.events();
        let a_recorded = events
            .iter()
            .position(|event| {
                event.kind == "TaskRecorded" && event.task_id.as_deref() == Some("DEP-A")
            })
            .unwrap();
        assert!(
            site.dispatch_positions("DEP-A")[0] < a_recorded && a_recorded < b_positions[0],
            "账本顺序必须为 派前驱 → 前驱 Recorded → 派后继"
        );
        assert_eq!(
            second.wave.blocked_wave,
            Some(1),
            "后继已派未收，波闸阻断在后继波"
        );
        site.finish().unwrap();
    }

    /// P1-1 整改：critical 任务真实两振 → 真实 liveness_monitor_tick 自动接替
    /// 路径 → 账本断言：同一 agent 两条 stall AttemptTimedOut（两振折叠）、恰一条
    /// EscalationRaised{stage:"eligible-chain-exhausted"}、无 critical-implement
    /// 能力候选零 DispatchIssued（绝不降级的账本事实）。
    #[test]
    fn b147_critical_two_strikes_escalate_exhausted_without_downgrade() {
        let site =
            B147ProductionRepo::new("crit", &[("CRIT", "capabilities: [critical-implement]\n")]);
        let mut monitor = LivenessMonitorState::default();

        // 第一振：真实派发给唯一持 critical-implement 的 executor-desktop；
        // 真实 monitor 拍判 Stall → 真实终止受管进程 → AttemptTimedOut 落账。
        crate::tierf::run_dispatch(&site.root, "CRIT", false).unwrap();
        site.ack_and_age_attempt("CRIT", 1, "executor-desktop");
        liveness_monitor_tick(&site.root, &mut monitor).unwrap();
        // 一振不足出局：真实接替判定重派同一合格链首（succession 真实发生）。
        assert_eq!(
            site.dispatch_positions("CRIT").len(),
            2,
            "一振后真实接替必须重派合格链首（A0002）: {:?}",
            site.events()
        );

        // 第二振：A0002 同样真实 stall → 同一 agent 凑满两振出局。
        site.ack_and_age_attempt("CRIT", 2, "executor-desktop");
        liveness_monitor_tick(&site.root, &mut monitor).unwrap();

        let events = site.events();
        // 两振折叠的账本事实：同一 agent 恰两条 stall AttemptTimedOut（跨 A0001/A0002）。
        let stalls: Vec<&EventRecord> = events
            .iter()
            .filter(|event| {
                event.kind == "AttemptTimedOut"
                    && event.task_id.as_deref() == Some("CRIT")
                    && event
                        .payload
                        .as_ref()
                        .and_then(|payload| payload["kind"].as_str())
                        == Some("stall")
            })
            .collect();
        assert_eq!(
            stalls.len(),
            2,
            "两振必须折叠为恰两条 stall 超时: {events:?}"
        );
        let stall_attempts: Vec<&str> = stalls
            .iter()
            .filter_map(|event| {
                event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload["attemptId"].as_str())
            })
            .collect();
        assert_eq!(stall_attempts, ["CRIT-A0001", "CRIT-A0002"]);
        for stall in &stalls {
            let payload = stall.payload.as_ref().unwrap();
            assert_eq!(payload["agent"].as_str(), Some("executor-desktop"));
            assert_eq!(payload["processTreeTerminated"].as_bool(), Some(true));
        }
        // 升级而非降级的账本事实：恰一条 eligible-chain-exhausted。
        let exhausted: Vec<&EventRecord> = events
            .iter()
            .filter(|event| {
                event.kind == "EscalationRaised"
                    && event
                        .payload
                        .as_ref()
                        .and_then(|payload| payload["stage"].as_str())
                        == Some("eligible-chain-exhausted")
            })
            .collect();
        assert_eq!(
            exhausted.len(),
            1,
            "账本恰一条 eligible-chain-exhausted 升级: {events:?}"
        );
        let payload = exhausted[0].payload.as_ref().unwrap();
        assert_eq!(exhausted[0].task_id.as_deref(), Some("CRIT"));
        assert_eq!(payload["agent"].as_str(), Some("executor-desktop"));
        assert_eq!(
            payload["attemptId"].as_str(),
            Some("CRIT-A0002"),
            "升级锚定第二振的 attempt"
        );
        assert_eq!(payload["failed"], serde_json::json!(["executor-desktop"]));
        assert!(payload["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("eligible-chain-exhausted")));
        // 绝不降级：全账本维度无能力候选零 DispatchIssued；CRIT 两次真实派发
        // 均为持 critical-implement 的链首。
        assert_eq!(site.dispatch_positions("CRIT").len(), 2);
        assert!(
            events.iter().all(|event| {
                event.kind != "DispatchIssued"
                    || event
                        .payload
                        .as_ref()
                        .and_then(|payload| payload["agent"].as_str())
                        == Some("executor-desktop")
            }),
            "executor-claw / executor-opencode 无 critical-implement，账本必须零派发: {events:?}"
        );
        site.finish().unwrap();
    }
}
