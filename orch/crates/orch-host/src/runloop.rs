//! `orch run/step` foreground driver loop (design/05 §2).
//!
//! Maintenance-only entry point: new orchestration policy belongs in
//! `serve`; run/step may only compose the decision kernels frozen by
//! `tests/entry_freeze.rs`.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use orch_core::{fold, read_ledger};

use super::wake::{reconcile_review_transitions, review_fallback_tick};
use crate::{close, ledger, verify};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSnapshot {
    pub id: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Verify { task: String },
    Merge { task: String },
    CloseRound,
}

#[derive(Debug)]
pub struct LoopOutcome {
    pub round: String,
    pub tasks: Vec<TaskSnapshot>,
    pub actions: Vec<Action>,
    pub executed: usize,
    pub awaiting_root: Vec<String>,
}

pub fn next_actions(tasks: &[TaskSnapshot]) -> Vec<Action> {
    next_actions_with_inflight(tasks, &[])
}

/// 同 `next_actions`，但 in-flight 集合内的任务跳过（不重复派发 Verify/Merge）。
/// 空集合＝`next_actions` 原语义。
pub fn next_actions_with_inflight(tasks: &[TaskSnapshot], inflight: &[String]) -> Vec<Action> {
    next_actions_for_mode(tasks, inflight, false).0
}

/// Root-manual action projection: collected tasks wait for the external root
/// fixed-HEAD verdict without fabricating VerifyStarted/VerifyFailed.  A legal
/// root PASS projects Approved and is mechanically mergeable on the next tick.
pub fn next_actions_for_mode(
    tasks: &[TaskSnapshot],
    inflight: &[String],
    root_manual: bool,
) -> (Vec<Action>, Vec<String>) {
    if !tasks.is_empty() && tasks.iter().all(|task| task.state == "recorded") {
        return (vec![Action::CloseRound], Vec::new());
    }

    let mut awaiting_root = Vec::new();
    let actions = tasks
        .iter()
        .filter(|task| !inflight.contains(&task.id))
        .filter_map(|task| match task.state.as_str() {
            "ready_for_verification" if root_manual => {
                awaiting_root.push(task.id.clone());
                None
            }
            "ready_for_verification" => Some(Action::Verify {
                task: task.id.clone(),
            }),
            "approved" => Some(Action::Merge {
                task: task.id.clone(),
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    (actions, awaiting_root)
}

/// 从账本 (taskId, kind) 序列推断 in-flight 集合：
/// VerifyStarted/MergeStarted 记入（按首现顺序去重）；
/// 同 task 后继 VerdictIssued/MergeExecuted 清除。其余 kind 忽略。
pub fn infer_inflight(events: &[(String, String)]) -> Vec<String> {
    let mut inflight: Vec<String> = Vec::new();
    for (task, kind) in events {
        match kind.as_str() {
            "VerifyStarted" | "MergeStarted" => {
                if !inflight.contains(task) {
                    inflight.push(task.clone());
                }
            }
            "VerdictIssued" | "MergeExecuted" => inflight.retain(|id| id != task),
            _ => {}
        }
    }
    inflight
}

/// in-flight TTL 默认值（秒）：Started 落账后超此时长仍无完成事件，判为崩溃遗留，
/// 踢出 in-flight 使其重新可调度（B20 REPORT §6 自省①还债）。pub 常量供后续配置化。
pub const INFLIGHT_TTL_SECS: i64 = 900;

/// 带时间戳的 in-flight 推断：返回 `(inflight, expired)`。
/// VerifyStarted/MergeStarted 记 in-flight（同 task 取**最新** Started 计时，输出按首现顺序）；
/// VerdictIssued/MergeExecuted 清除该 task 的在飞记录（正常收口两不在）；
/// 最新 Started 距 `now_epoch` 超 `ttl_secs` 且无后继完成 → 移入 expired（崩溃遗留，重新可调度）。
/// 无时间戳版 `infer_inflight` 保持原语义，供不需要 TTL 的调用方。
pub fn infer_inflight_with_ttl(
    events: &[(String, String, i64)],
    now_epoch: i64,
    ttl_secs: i64,
) -> (Vec<String>, Vec<String>) {
    // 每 task 只记最新 Started 时间戳；首现顺序保序（与 infer_inflight 一致）
    let mut latest: Vec<(String, i64)> = Vec::new();
    for (task, kind, ts) in events {
        match kind.as_str() {
            "VerifyStarted" | "MergeStarted" => {
                if let Some(entry) = latest.iter_mut().find(|(id, _)| id == task) {
                    entry.1 = *ts; // 重复 Started 刷新计时窗口（重试不被旧时间戳拖死）
                } else {
                    latest.push((task.clone(), *ts));
                }
            }
            "VerdictIssued" | "MergeExecuted" => latest.retain(|(id, _)| id != task),
            _ => {}
        }
    }
    let mut inflight = Vec::new();
    let mut expired = Vec::new();
    for (task, ts) in latest {
        if now_epoch - ts > ttl_secs {
            expired.push(task);
        } else {
            inflight.push(task);
        }
    }
    (inflight, expired)
}

/// TTL 过期升级的节流器：账本即节流记忆，无新状态。
/// 报出条件（逐 task 判定，输出保 `expired` 入参相对顺序）：
/// - 该 task 无 inflight-ttl-expired 升级前科 → 报；
/// - 否则仅当最新 Started ts **严格大于**最近一次升级 ts（新一轮尝试）才再报；
///   ts 相等视为「无新尝试」→ 不报；前科存在但无 Started 记录 → 无法证明新尝试 → 不报。
/// 同一 task 多条记录时两侧均取最大 ts（最近一次）。
pub fn throttle_expired(
    expired: &[String],
    prior_escalations: &[(String, i64)],
    latest_started: &[(String, i64)],
) -> Vec<String> {
    let latest_ts = |entries: &[(String, i64)], task: &str| {
        entries
            .iter()
            .filter(|(id, _)| id == task)
            .map(|(_, ts)| *ts)
            .max()
    };
    expired
        .iter()
        .filter(|task| match latest_ts(prior_escalations, task) {
            None => true,
            Some(prior_ts) => latest_ts(latest_started, task).is_some_and(|ts| ts > prior_ts),
        })
        .cloned()
        .collect()
}

pub fn run_loop(root: &Path, once: bool) -> Result<LoopOutcome> {
    loop {
        let outcome = run_tick(root)?;
        print_outcome(&outcome, once);
        if once {
            return Ok(outcome);
        }
        std::thread::sleep(Duration::from_secs(30));
    }
}

fn run_tick(root: &Path) -> Result<LoopOutcome> {
    let round = crate::current_round(root)?;
    let _nongate_deliveries = reconcile_review_transitions(root)?;
    // Review fallback is a production transition, not a legacy serve-only
    // policy hook. Both `orch step` and `orch serve` reach this exact tick;
    // the wake layer serializes repeated/concurrent ticks and reuses the same
    // `ReviewFallbackSelected + WakeIssued + ReviewRequested` effect.
    let review_fallbacks = review_fallback_tick(root)?;
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let ledger = read_ledger(&ledger_path)
        .with_context(|| format!("读取运行循环账本失败: {}", ledger_path.display()))?;
    if !ledger.bad_lines.is_empty() {
        bail!(
            "运行循环拒绝在坏账本上执行：{} 坏行（首个 #{}）",
            ledger.bad_lines.len(),
            ledger.bad_lines[0].0
        );
    }
    let active = crate::plan::require_active_round_ir(root, &round, &ledger.events)?;
    let root_manual = active.candidate.verification.mode == "root-manual-fixed-head";

    let projection = fold(&ledger.events);
    let mut tasks = projection
        .tasks
        .into_iter()
        .map(|(id, task)| TaskSnapshot {
            id,
            state: task
                .state
                .map(|state| state.to_string())
                .unwrap_or_else(|| "unknown".into()),
        })
        .collect::<Vec<_>>();
    if root_manual {
        // orch-core intentionally remains historical and actor-agnostic.  At
        // the production edge, an Approved projection is mergeable only when
        // the full current root authorization recheck succeeds; forged,
        // wrong-round, stale, or byte-drifted PASS stays at the root barrier.
        for task in &mut tasks {
            if task.state == "approved"
                && crate::verify::validate_root_merge_authorization(
                    root,
                    &round,
                    &task.id,
                    &ledger.events,
                )
                .is_err()
            {
                task.state = "ready_for_verification".into();
            }
        }
    }
    let now_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let timed_events = ledger
        .events
        .iter()
        .filter_map(|ev| {
            ev.task_id.clone().map(|task| {
                // 时间戳不可解析时按"刚刚开始"处理：保守不过期，绝不误踢在飞任务
                let ts = humantime::parse_rfc3339(&ev.ts)
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(now_epoch);
                (task, ev.kind.clone(), ts)
            })
        })
        .collect::<Vec<_>>();
    let (inflight, expired_raw) =
        infer_inflight_with_ttl(&timed_events, now_epoch, INFLIGHT_TTL_SECS);
    // 节流记忆来自账本本身（无新状态）：各 task 最近一次 inflight-ttl-expired 升级的 ts，
    // 与各 task 最新 Started 的 ts。ts 解析沿用 infer_inflight_with_ttl 的保守语义——
    // 解析失败按 now（recent 化前科 ⇒ 宁可少报不刷屏，绝不误踢）。
    let ts_of = |ev: &orch_core::EventRecord| {
        humantime::parse_rfc3339(&ev.ts)
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(now_epoch)
    };
    let prior_escalations = ledger
        .events
        .iter()
        .filter(|ev| {
            ev.kind == "EscalationRaised"
                && ev
                    .payload
                    .as_ref()
                    .and_then(|p| p.get("reason"))
                    .and_then(|r| r.as_str())
                    == Some("inflight-ttl-expired")
        })
        .filter_map(|ev| ev.task_id.clone().map(|task| (task, ts_of(ev))))
        .collect::<Vec<_>>();
    let latest_started = timed_events
        .iter()
        .filter(|(_, kind, _)| kind == "VerifyStarted" || kind == "MergeStarted")
        .map(|(task, _, ts)| (task.clone(), *ts))
        .collect::<Vec<_>>();
    let expired = throttle_expired(&expired_raw, &prior_escalations, &latest_started);
    if expired_raw.len() > expired.len() {
        println!(
            "  in-flight TTL 节流：{} 个任务已升级过且尚无新 Started，本 tick 不再重复落账",
            expired_raw.len() - expired.len()
        );
    }
    if !expired.is_empty() {
        println!(
            "  in-flight TTL 到期（{}s）：{} 个任务判为崩溃遗留，落账 EscalationRaised 并本 tick 重新可调度",
            INFLIGHT_TTL_SECS,
            expired.len()
        );
        let escalation_events = expired
            .iter()
            .map(|task| {
                ledger::event(
                    "EscalationRaised",
                    "runtime:orch",
                    Some(task),
                    Some(&round),
                    serde_json::json!({
                        "reason": "inflight-ttl-expired",
                        "ttlSecs": INFLIGHT_TTL_SECS,
                    }),
                )
            })
            .collect::<Vec<_>>();
        ledger::append(root, &round, &escalation_events)?;
    }
    let (actions, awaiting_root) = next_actions_for_mode(&tasks, &inflight, root_manual);
    let mut executed = review_fallbacks;

    for action in &actions {
        match action {
            Action::Verify { task } => {
                let model = active
                    .candidate
                    .verification
                    .model
                    .as_deref()
                    .context("verifier model 必须来自 signed ROUND-IR")?;
                println!("  execute: verify {task} (model={model}, timeout=900s)");
                ledger::append(
                    root,
                    &round,
                    &[ledger::event(
                        "VerifyStarted",
                        "runtime:orch",
                        Some(task),
                        Some(&round),
                        serde_json::json!({"model": model, "timeoutSecs": 900}),
                    )],
                )?;
                verify::run_verify(root, task, model, 900)?;
                executed += 1;
            }
            Action::Merge { task } => {
                println!("  execute: merge {task}");
                close::run_merge(root, task)?;
                executed += 1;
            }
            Action::CloseRound => {
                println!(
                    "  建议：全部任务已 recorded；请人工运行 `orch round close`（不会自动收轮）"
                );
            }
        }
    }

    Ok(LoopOutcome {
        round,
        tasks,
        actions,
        executed,
        awaiting_root,
    })
}

fn print_outcome(outcome: &LoopOutcome, once: bool) {
    let command = if once { "step" } else { "run" };
    println!(
        "orch {command} · round={} · tasks={} · actions={} · executed={}",
        outcome.round,
        outcome.tasks.len(),
        outcome.actions.len(),
        outcome.executed
    );
    if outcome.actions.is_empty() {
        println!(
            "  无动作：等待状态变化{}",
            if once {
                ""
            } else {
                "（30s 后重读账本）"
            }
        );
    }
    if !outcome.awaiting_root.is_empty() {
        println!("  awaiting_root: {}", outcome.awaiting_root.join(", "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, state: &str) -> TaskSnapshot {
        TaskSnapshot {
            id: id.into(),
            state: state.into(),
        }
    }

    #[test]
    fn mixed_actions_preserve_input_order_and_ignore_waiting_states() {
        let world = [
            task("B1", "dispatched"),
            task("B2", "approved"),
            task("B3", "ready_for_verification"),
            task("B4", "blocked"),
        ];
        assert_eq!(
            next_actions(&world),
            vec![
                Action::Merge { task: "B2".into() },
                Action::Verify { task: "B3".into() },
            ]
        );
    }

    #[test]
    fn recorded_and_non_recorded_mix_does_not_close_round() {
        let world = [task("B1", "recorded"), task("B2", "dispatched")];
        assert!(next_actions(&world).is_empty());
    }

    #[test]
    fn unknown_state_waits_without_action() {
        assert!(next_actions(&[task("B1", "future-state")]).is_empty());
    }

    #[test]
    fn close_round_is_not_blocked_by_inflight() {
        // recorded 是终态：全部 recorded 时即使有残留 in-flight 记录也应收轮
        let world = [task("B1", "recorded"), task("B2", "recorded")];
        let inflight = vec!["B9".to_string()];
        assert_eq!(
            next_actions_with_inflight(&world, &inflight),
            vec![Action::CloseRound]
        );
    }

    #[test]
    fn infer_inflight_ignores_unrelated_kinds_and_dedups_repeated_started() {
        let events = vec![
            ("B1".to_string(), "TaskDispatched".to_string()),
            ("B1".to_string(), "VerifyStarted".to_string()),
            ("B1".to_string(), "VerifyStarted".to_string()), // 重复 Started 不重复入账
            ("B2".to_string(), "MergeExecuted".to_string()), // 无 Started 的完成事件＝无操作
        ];
        assert_eq!(infer_inflight(&events), vec!["B1".to_string()]);
    }

    #[test]
    fn ttl_mixed_tasks_partition_inflight_and_expired() {
        // 多任务混合：新鲜 VerifyStarted 在飞；陈旧 VerifyStarted/MergeStarted 均过期；
        // MergeStarted+MergeExecuted 完成事件对正常收口（两不在）。分区输出各按首现顺序。
        let events = vec![
            ("B1".to_string(), "VerifyStarted".to_string(), 190),
            ("B2".to_string(), "VerifyStarted".to_string(), 100),
            ("B3".to_string(), "MergeStarted".to_string(), 100),
            ("B3".to_string(), "MergeExecuted".to_string(), 120),
            ("B4".to_string(), "MergeStarted".to_string(), 100),
        ];
        let (inflight, expired) = infer_inflight_with_ttl(&events, 200, 60);
        assert_eq!(inflight, vec!["B1".to_string()]);
        assert_eq!(expired, vec!["B2".to_string(), "B4".to_string()]);
    }

    #[test]
    fn ttl_completion_then_stale_restart_expires() {
        // 正常收口后的重试：旧完成事件不庇护新 Started；新 Started 超 TTL 照样过期
        let events = vec![
            ("B1".to_string(), "VerifyStarted".to_string(), 100),
            ("B1".to_string(), "VerdictIssued".to_string(), 120),
            ("B1".to_string(), "VerifyStarted".to_string(), 250),
        ];
        let (inflight, expired) = infer_inflight_with_ttl(&events, 400, 60);
        assert!(inflight.is_empty());
        assert_eq!(expired, vec!["B1".to_string()]);
    }

    #[test]
    fn ttl_boundary_exact_age_stays_inflight() {
        // 边界：age == ttl 不算"超"，仍在飞；age = ttl+1 才踢出
        let events = vec![("B1".to_string(), "VerifyStarted".to_string(), 100)];
        let (inflight, expired) = infer_inflight_with_ttl(&events, 160, 60);
        assert_eq!(inflight, vec!["B1".to_string()]);
        assert!(expired.is_empty());
        let (inflight, expired) = infer_inflight_with_ttl(&events, 161, 60);
        assert!(inflight.is_empty());
        assert_eq!(expired, vec!["B1".to_string()]);
    }

    #[test]
    fn throttle_prior_without_any_started_stays_silent() {
        // 边界：有升级前科但账本里该 task 无任何 Started 记录
        // → 无法证明「新一轮尝试」→ 不报
        let out = throttle_expired(&["T1".to_string()], &[("T1".to_string(), 150)], &[]);
        assert!(out.is_empty());
    }

    #[test]
    fn throttle_equal_ts_means_no_new_attempt() {
        // ts 相等语义钉死：最新 Started ts == 前科 ts → 视为无新尝试 → 不报
        let out = throttle_expired(
            &["T1".to_string()],
            &[("T1".to_string(), 150)],
            &[("T1".to_string(), 150)],
        );
        assert!(out.is_empty());
    }

    #[test]
    fn throttle_takes_latest_of_both_sides() {
        // 前科/Started 各多条时两侧均取最新：prior max=300 > started max=250 → 不报；
        // 若误取旧前科（150 < 250）会错报一次
        let out = throttle_expired(
            &["T1".to_string()],
            &[("T1".to_string(), 150), ("T1".to_string(), 300)],
            &[("T1".to_string(), 200), ("T1".to_string(), 250)],
        );
        assert!(out.is_empty());
    }

    #[test]
    fn throttle_preserves_expired_relative_order() {
        // 输出保 expired 入参相对顺序：B 被节流，A/C 首报
        let out = throttle_expired(
            &["A".to_string(), "B".to_string(), "C".to_string()],
            &[("B".to_string(), 100)],
            &[("A".to_string(), 10)],
        );
        assert_eq!(out, vec!["A".to_string(), "C".to_string()]);
    }
}
