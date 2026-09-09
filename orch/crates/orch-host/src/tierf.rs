//! 手动任务派发与收取：`dispatch` 建立固定工作树，`await-report` 验证并收取结果。
//! 本地 stat 是等待保证，notify 只加速唤醒；本地 root 实现不需要 provider 心跳。
//! 历史 bootstrap 文本与事件读者不授权恢复已退役的任务 nudge/resume 或自动调度。

// Existing selfhost callers retain their paths; the kernel has one implementation.
pub use crate::channel::process::{TerminationSignal, TerminationEvidence, termination_commit_gate, ManagedTerminationPolicy, ManagedProcessGroupControl, terminate_managed_process_group_with_control, terminate_managed_process_group};
use crate::channel::process::process_group_alive;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use orch_core::{read_ledger, EventRecord};
use sha2::Digest;

use crate::{card, collect, current_round, gitx, ledger, liveness, wake};


const IMPLEMENT_DISPATCH_SITE_ROLE: crate::sites::SiteRole = crate::sites::SiteRole::Implement;

/// await-report 的四种归宿：收到并过机检门 / 执行者报告阻塞 / liveness 判死 / liveness 判滞
pub enum AwaitOutcome {
    Collected(collect::CollectOutcome),
    Blocked { report_rel: String },
    LivenessDead { reason: String },
    LivenessStalled { reason: String },
}

#[derive(Debug, PartialEq)]
pub enum AwaitPoll {
    ReportFound,
    Blocked,
    Waiting,
}

pub fn await_poll(report_found: bool, blocked_found: bool) -> AwaitPoll {
    if report_found {
        AwaitPoll::ReportFound
    } else if blocked_found {
        AwaitPoll::Blocked
    } else {
        AwaitPoll::Waiting
    }
}

/// Preserve REPORT priority except for the narrow, ledger-proven escape hatch.
pub fn await_poll_with_escape(
    report_found: bool,
    blocked_found: bool,
    report_collect_hard_rejected: bool,
    blocked_is_current: bool,
) -> AwaitPoll {
    if report_found && blocked_found && report_collect_hard_rejected && blocked_is_current {
        AwaitPoll::Blocked
    } else {
        await_poll(report_found, blocked_found)
    }
}

/// Whether the exact REPORT currently on disk has already reached a terminal
/// machine-check rejection for this exact attempt.
///
/// `MechCheckFailed` is the machine verdict but legacy producers do not carry
/// attempt identity.  The surrounding immutable `ReportObserved` evidence
/// binding and terminal `ActionRejected(operation=await-report)` supply that
/// missing scope.  Neither a generic await failure nor an older REPORT can
/// open the escape hatch.
fn report_collect_hard_rejection_index<'a>(
    events: &'a [EventRecord],
    round: &str,
    task_id: &str,
    attempt: &crate::attempt::AttemptRef,
    report: &crate::attempt::EvidenceObservation,
    control_epoch: Option<&'a str>,
) -> Option<usize> {
    let control_epoch = control_epoch.or_else(|| {
        events
            .iter()
            .rev()
            .find(|event| {
                event.task_id.as_deref() == Some(task_id)
                    && matches!(
                        event.kind.as_str(),
                        "DispatchIssued" | "NudgeIssued" | "ResumeIssued"
                    )
            })
            .map(|event| event.event_id.as_str())
            .filter(|event_id| !event_id.is_empty())
    })?;
    let canonical_path = report.canonical_path.to_string_lossy();
    let evidence_matches = |payload: &serde_json::Value| {
        payload.get("attemptId").and_then(serde_json::Value::as_str)
            == Some(attempt.attempt_id.as_str())
            && payload.get("attemptNo").and_then(serde_json::Value::as_u64)
                == Some(attempt.ordinal as u64)
            && payload
                .get("evidencePath")
                .and_then(serde_json::Value::as_str)
                == Some(canonical_path.as_ref())
            && payload
                .get("evidenceSha256")
                .and_then(serde_json::Value::as_str)
                == Some(report.sha256.as_str())
            && payload
                .get("evidenceLen")
                .and_then(serde_json::Value::as_u64)
                == Some(report.len)
            && payload
                .get("controlEpoch")
                .and_then(serde_json::Value::as_str)
                == Some(control_epoch)
    };
    let latest_report = events.iter().rposition(|event| {
        let payload = event.payload.as_ref();
        event.kind == "ReportObserved"
            && event.actor == "runtime:orch"
            && event.round.as_deref() == Some(round)
            && event.task_id.as_deref() == Some(task_id)
            && payload
                .and_then(|value| value.get("actionId"))
                .and_then(|value| value.as_str())
                == Some("report-observed")
            && payload
                .and_then(|value| value.get("attemptId"))
                .and_then(|value| value.as_str())
                == Some(attempt.attempt_id.as_str())
            && payload
                .and_then(|value| value.get("attemptNo"))
                .and_then(|value| value.as_u64())
                == Some(attempt.ordinal as u64)
    });
    let Some(latest_report) = latest_report else {
        return None;
    };
    let observed_payload = match events[latest_report].payload.as_ref() {
        Some(payload) => payload,
        None => return None,
    };
    if !evidence_matches(observed_payload) {
        return None;
    }

    let action_id = format!("await-report:{round}:{task_id}");
    let rejection_matches = |event: &EventRecord| {
        let payload = event.payload.as_ref();
        event.kind == "ActionRejected"
            && event.actor == "runtime:orch"
            && event.round.as_deref() == Some(round)
            && event.task_id.as_deref() == Some(task_id)
            && payload
                .and_then(|value| value.get("operation"))
                .and_then(serde_json::Value::as_str)
                == Some("await-report")
            && payload
                .and_then(|value| value.get("actionId"))
                .and_then(serde_json::Value::as_str)
                == Some(action_id.as_str())
            && payload
                .and_then(|value| value.get("attemptId"))
                .and_then(serde_json::Value::as_str)
                == Some(attempt.attempt_id.as_str())
            && payload
                .and_then(|value| value.get("attemptNo"))
                .and_then(serde_json::Value::as_u64)
                == Some(attempt.ordinal as u64)
            && payload
                .and_then(|value| value.get("exitCode"))
                .and_then(serde_json::Value::as_i64)
                == Some(4)
            && payload
                .and_then(|value| value.get("alert"))
                .and_then(serde_json::Value::as_bool)
                == Some(true)
            && payload
                .and_then(|value| value.get("reason"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|reason| !reason.is_empty())
    };
    let Some(rejection) = (latest_report + 1..events.len())
        .rev()
        .find(|index| rejection_matches(&events[*index]))
    else {
        return None;
    };

    let collect_event_matches = |event: &EventRecord| {
        event.actor == "runtime:orch"
            && event.round.as_deref() == Some(round)
            && event.task_id.as_deref() == Some(task_id)
            && event.payload.as_ref().is_some_and(&evidence_matches)
    };
    // A retry or successful completion after the rejection makes that old
    // rejection non-terminal.  This is common in real ledgers where the same
    // immutable REPORT is collected again after a transient repair.
    if events[rejection + 1..].iter().any(|event| {
        matches!(
            event.kind.as_str(),
            "ReportCollectClaimed"
                | "ReportCollectExecuting"
                | "ReportCollectExecuted"
                | "ReportCollectCompleted"
                | "ReportCollectReleased"
        ) && collect_event_matches(event)
    }) {
        return None;
    }

    // A machine verdict belongs only to the latest durable generation before
    // the terminal rejection.  Completed/Executed or a fresh claim therefore
    // clears an older failure rather than leaving a permanent latch.
    let generation_boundary = (latest_report + 1..rejection)
        .rev()
        .find(|index| {
            matches!(
                events[*index].kind.as_str(),
                "ReportCollectClaimed"
                    | "ReportCollectExecuting"
                    | "ReportCollectExecuted"
                    | "ReportCollectCompleted"
            ) && collect_event_matches(&events[*index])
        })
        .unwrap_or(latest_report);

    for machine_index in generation_boundary + 1..rejection {
        let machine = &events[machine_index];
        let Some(payload) = machine.payload.as_ref() else {
            continue;
        };
        if machine.kind != "MechCheckFailed"
            || machine.actor != "runtime:orch"
            || machine.round.as_deref() != Some(round)
            || machine.task_id.as_deref() != Some(task_id)
            || payload
                .get("stage")
                .and_then(serde_json::Value::as_str)
                .is_none()
            || payload
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .is_none()
        {
            continue;
        }

        // Model-identity failures carry their own exact attempt scope and can
        // occur atomically with ReportObserved, before any durable claim.
        if payload.get("attemptId").is_some() {
            if payload.get("attemptId").and_then(serde_json::Value::as_str)
                == Some(attempt.attempt_id.as_str())
                && payload.get("attemptNo").and_then(serde_json::Value::as_u64)
                    == Some(attempt.ordinal as u64)
            {
                return Some(machine_index);
            }
            continue;
        }

        // Legacy collect::record_failure events have no attempt fields.  Bind
        // them through an exact Executing -> MechCheckFailed -> Released
        // lineage for the current evidence instead of guessing their owner.
        let Some(executing) = (generation_boundary..machine_index).rev().find(|index| {
            events[*index].kind == "ReportCollectExecuting"
                && collect_event_matches(&events[*index])
        }) else {
            continue;
        };
        let Some(executing_payload) = events[executing].payload.as_ref() else {
            continue;
        };
        let same_lineage = |candidate: &EventRecord| {
            let Some(candidate_payload) = candidate.payload.as_ref() else {
                return false;
            };
            collect_event_matches(candidate)
                && ["actionId", "owner", "leaseGeneration"]
                    .iter()
                    .all(|field| {
                        candidate_payload.get(*field) == executing_payload.get(*field)
                            && executing_payload.get(*field).is_some()
                    })
        };
        if events[machine_index + 1..rejection]
            .iter()
            .any(|event| event.kind == "ReportCollectReleased" && same_lineage(event))
        {
            return Some(machine_index);
        }
    }
    None
}

fn report_collect_hard_rejected(
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    attempt: &crate::attempt::AttemptRef,
    report: &crate::attempt::EvidenceObservation,
) -> bool {
    report_collect_hard_rejection_index(events, round, task_id, attempt, report, None).is_some()
}

#[cfg(test)]
mod b242_escape_tests {
    use super::*;

    fn event(kind: &str, payload: serde_json::Value) -> EventRecord {
        EventRecord {
            event_id: format!("event-{kind}"),
            ts: "2026-08-08T00:00:00Z".into(),
            actor: "runtime:orch".into(),
            kind: kind.into(),
            task_id: Some("B242".into()),
            round: Some("r67".into()),
            payload: Some(payload),
            extra: serde_json::Map::new(),
        }
    }

    fn report() -> crate::attempt::EvidenceObservation {
        crate::attempt::EvidenceObservation {
            path: PathBuf::from("/tmp/B242-REPORT.md"),
            canonical_path: PathBuf::from("/tmp/B242-REPORT.md"),
            mtime: SystemTime::UNIX_EPOCH,
            len: 42,
            sha256: "report-sha".into(),
            bytes: Vec::new(),
        }
    }

    fn attempt() -> crate::attempt::AttemptRef {
        crate::attempt::AttemptRef {
            task_id: "B242".into(),
            ordinal: 1,
            attempt_id: "B242-A0001".into(),
        }
    }

    fn observed(sha: &str) -> EventRecord {
        event(
            "ReportObserved",
            serde_json::json!({
                "actionId": "report-observed",
                "attemptId": "B242-A0001",
                "attemptNo": 1,
                "evidencePath": "/tmp/B242-REPORT.md",
                "evidenceSha256": sha,
                "evidenceLen": 42,
                "controlEpoch": "event-DispatchIssued",
            }),
        )
    }

    fn collect_event(kind: &str) -> EventRecord {
        event(
            kind,
            serde_json::json!({
                "actionId": "collect-action",
                "attemptId": "B242-A0001",
                "attemptNo": 1,
                "evidencePath": "/tmp/B242-REPORT.md",
                "evidenceSha256": "report-sha",
                "evidenceLen": 42,
                "controlEpoch": "event-DispatchIssued",
                "owner": "collect-owner",
                "leaseGeneration": "collect-generation",
            }),
        )
    }

    fn rejected(attempt_id: &str) -> EventRecord {
        event(
            "ActionRejected",
            serde_json::json!({
                "actionId": "await-report:r67:B242",
                "operation": "await-report",
                "attemptId": attempt_id,
                "attemptNo": 1,
                "exitCode": 4,
                "alert": true,
                "reason": "machine check rejected the report",
            }),
        )
    }

    fn control() -> EventRecord {
        event(
            "DispatchIssued",
            serde_json::json!({"attemptId": "B242-A0001", "attemptNo": 1}),
        )
    }

    #[test]
    fn exact_report_needs_machine_failure_and_terminal_attempt_rejection() {
        let machine_failure = event(
            "MechCheckFailed",
            serde_json::json!({"stage": "red-replay", "reason": "identity mismatch"}),
        );
        let complete = vec![
            control(),
            observed("report-sha"),
            collect_event("ReportCollectClaimed"),
            collect_event("ReportCollectExecuting"),
            machine_failure.clone(),
            collect_event("ReportCollectReleased"),
            rejected("B242-A0001"),
        ];
        assert!(report_collect_hard_rejected(
            &complete,
            "r67",
            "B242",
            &attempt(),
            &report(),
        ));

        assert!(!report_collect_hard_rejected(
            &[
                control(),
                observed("report-sha"),
                collect_event("ReportCollectExecuting"),
                collect_event("ReportCollectReleased"),
                rejected("B242-A0001"),
            ],
            "r67",
            "B242",
            &attempt(),
            &report(),
        ));
        assert!(!report_collect_hard_rejected(
            &[
                control(),
                observed("report-sha"),
                collect_event("ReportCollectExecuting"),
                machine_failure,
                collect_event("ReportCollectReleased"),
            ],
            "r67",
            "B242",
            &attempt(),
            &report(),
        ));
    }

    #[test]
    fn wrong_attempt_or_newer_report_cannot_open_escape_hatch() {
        let machine_failure = event(
            "MechCheckFailed",
            serde_json::json!({
                "stage": "model-identity",
                "reason": "bad report",
                "attemptId": "B242-A0001",
                "attemptNo": 1,
            }),
        );
        assert!(!report_collect_hard_rejected(
            &[
                control(),
                observed("report-sha"),
                machine_failure.clone(),
                rejected("B242-A0002"),
            ],
            "r67",
            "B242",
            &attempt(),
            &report(),
        ));
        assert!(!report_collect_hard_rejected(
            &[
                control(),
                observed("report-sha"),
                machine_failure.clone(),
                rejected("B242-A0001"),
                observed("new-report-sha"),
                machine_failure,
                rejected("B242-A0001"),
            ],
            "r67",
            "B242",
            &attempt(),
            &report(),
        ));
    }

    #[test]
    fn retry_or_success_after_rejection_clears_the_escape_hatch() {
        let machine_failure = event(
            "MechCheckFailed",
            serde_json::json!({"stage": "red-replay", "reason": "identity mismatch"}),
        );
        let mut events = vec![
            control(),
            observed("report-sha"),
            collect_event("ReportCollectExecuting"),
            machine_failure,
            collect_event("ReportCollectReleased"),
            rejected("B242-A0001"),
        ];
        events.extend([
            collect_event("ReportCollectClaimed"),
            collect_event("ReportCollectExecuting"),
            collect_event("ReportCollectExecuted"),
            collect_event("ReportCollectCompleted"),
        ]);
        assert!(!report_collect_hard_rejected(
            &events,
            "r67",
            "B242",
            &attempt(),
            &report(),
        ));
    }
}

/// Correctness fallback for `await-report`: notify only accelerates this scan.
#[doc(hidden)]
pub const AWAIT_REPORT_RECONCILE_TICK: Duration = Duration::from_secs(2);

/// `await-report` 未显式给出超时时，由任务卡 wall budget 推导的缺省值。
/// 正常预算严格多留 30 分钟余量；未声明/legacy 的 0 分钟沿用旧缺省 1800 秒。
#[doc(hidden)]
pub fn default_await_timeout_secs(wall_minutes: u64) -> u64 {
    wall_minutes.saturating_mul(60).saturating_add(1_800)
}

/// An exact directory registered with the await-report watcher.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwaitReportWatchRoot {
    pub path: PathBuf,
    pub recursive: bool,
}

/// All terminal evidence files and parent directories relevant to one task.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwaitReportWatchPlan {
    pub evidence_paths: Vec<PathBuf>,
    pub watch_roots: Vec<AwaitReportWatchRoot>,
}

/// Normalized notify input. Events are merely wake-up hints, never evidence.
#[doc(hidden)]
pub enum AwaitReportWatchNotice<'a> {
    Paths(&'a [PathBuf]),
    Rescan,
    WatcherError,
}

/// Build the only paths that may wake an await-report scan for this task.
#[doc(hidden)]
pub fn await_report_watch_plan(root: &Path, round: &str, task_id: &str) -> AwaitReportWatchPlan {
    let reports_rel = format!("coordination/rounds/{round}/reports");
    let main_reports = root.join(&reports_rel);
    let worktree_reports = root.join(".worktrees").join(task_id).join(&reports_rel);
    let report_name = format!("{task_id}-REPORT.md");
    let blocked_name = format!("{task_id}-BLOCKED.md");

    AwaitReportWatchPlan {
        evidence_paths: vec![
            main_reports.join(&report_name),
            worktree_reports.join(&report_name),
            main_reports.join(&blocked_name),
            worktree_reports.join(&blocked_name),
        ],
        watch_roots: vec![
            AwaitReportWatchRoot {
                path: main_reports,
                recursive: false,
            },
            AwaitReportWatchRoot {
                path: worktree_reports,
                recursive: false,
            },
        ],
    }
}

/// Filter a backend notice before it reaches the capacity-one wake queue.
#[doc(hidden)]
pub fn await_report_event_is_relevant(
    plan: &AwaitReportWatchPlan,
    notice: AwaitReportWatchNotice<'_>,
) -> bool {
    match notice {
        AwaitReportWatchNotice::Rescan | AwaitReportWatchNotice::WatcherError => true,
        AwaitReportWatchNotice::Paths(paths) => paths.iter().any(|path| {
            plan.evidence_paths
                .iter()
                .any(|candidate| candidate == path)
                || plan.watch_roots.iter().any(|root| root.path == *path)
        }),
    }
}

/// Return newly available parents without creating any future worktree path.
#[doc(hidden)]
pub fn await_report_pending_watch_roots<F>(
    plan: &AwaitReportWatchPlan,
    watched: &BTreeSet<PathBuf>,
    mut is_dir: F,
) -> Vec<AwaitReportWatchRoot>
where
    F: FnMut(&Path) -> bool,
{
    plan.watch_roots
        .iter()
        .filter(|root| !watched.contains(&root.path) && is_dir(&root.path))
        .cloned()
        .collect()
}

/// Cloneable nonblocking producer for a single pending reconciliation wake.
#[doc(hidden)]
#[derive(Clone)]
pub struct AwaitReportWakeSender {
    sender: SyncSender<()>,
}

impl AwaitReportWakeSender {
    /// Returns true only when this call installs the one pending token.
    pub fn signal(&self) -> bool {
        match self.sender.try_send(()) {
            Ok(()) => true,
            Err(TrySendError::Full(())) | Err(TrySendError::Disconnected(())) => false,
        }
    }
}

/// Capacity-one channel used by the real notify callback to coalesce bursts.
#[doc(hidden)]
pub fn await_report_wake_channel() -> (AwaitReportWakeSender, Receiver<()>) {
    let (sender, receiver) = std::sync::mpsc::sync_channel::<()>(1);
    (AwaitReportWakeSender { sender }, receiver)
}

type AwaitReportNotifyCallback = Box<dyn FnMut(notify::Result<notify::Event>) + Send + 'static>;

/// Crate-private seam around the blocking/environmental edges of the real
/// await-report loop. Production delegates one-for-one to notify, fs, mpsc,
/// and thread::sleep; tests drive the same loop without wall-clock waits or a
/// platform-specific filesystem watcher.
trait AwaitReportRuntime {
    fn install_watcher(&mut self, callback: AwaitReportNotifyCallback);
    fn disable_watcher(&mut self);
    fn watcher_available(&self) -> bool;
    fn create_dir_all(&mut self, path: &Path) -> std::io::Result<()>;
    fn is_dir(&mut self, path: &Path) -> bool;
    fn watch(&mut self, path: &Path, mode: notify::RecursiveMode) -> bool;
    fn recv_timeout<F>(
        &mut self,
        timeout: Duration,
        native_wait: F,
    ) -> std::result::Result<(), std::sync::mpsc::RecvTimeoutError>
    where
        F: FnOnce() -> std::result::Result<(), std::sync::mpsc::RecvTimeoutError>;
    fn sleep(&mut self, duration: Duration);
}

#[derive(Default)]
struct ProductionAwaitReportRuntime {
    watcher: Option<notify::RecommendedWatcher>,
}

impl AwaitReportRuntime for ProductionAwaitReportRuntime {
    fn install_watcher(&mut self, callback: AwaitReportNotifyCallback) {
        self.watcher = notify::recommended_watcher(callback).ok();
    }

    fn disable_watcher(&mut self) {
        self.watcher = None;
    }

    fn watcher_available(&self) -> bool {
        self.watcher.is_some()
    }

    fn create_dir_all(&mut self, path: &Path) -> std::io::Result<()> {
        fs::create_dir_all(path)
    }

    fn is_dir(&mut self, path: &Path) -> bool {
        path.is_dir()
    }

    fn watch(&mut self, path: &Path, mode: notify::RecursiveMode) -> bool {
        use notify::Watcher;
        self.watcher
            .as_mut()
            .is_some_and(|watcher| watcher.watch(path, mode).is_ok())
    }

    fn recv_timeout<F>(
        &mut self,
        _timeout: Duration,
        native_wait: F,
    ) -> std::result::Result<(), std::sync::mpsc::RecvTimeoutError>
    where
        F: FnOnce() -> std::result::Result<(), std::sync::mpsc::RecvTimeoutError>,
    {
        native_wait()
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// `orch await-report <task>`：双根 stat 轮询（PITFALLS #2）→ ReportObserved → 机检+门。
/// O7：轮询期间按 design/03 §4.3 探测阶梯做 liveness 联动（`live=None` 关闭＝旧行为）。
pub fn run_await(
    root: &Path,
    task_id: &str,
    timeout_secs: u64,
    live: Option<liveness::LivenessOpts>,
) -> Result<AwaitOutcome> {
    run_await_with_hook(root, task_id, timeout_secs, live, &mut |_| Ok(()))
}

/// Deterministic production-path hook for evidence/attempt switch tests.
#[doc(hidden)]
pub fn run_await_with_hook(
    root: &Path,
    task_id: &str,
    timeout_secs: u64,
    live: Option<liveness::LivenessOpts>,
    hook: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<AwaitOutcome> {
    let round = current_round(root)?;
    let action_id = format!("await-report:{round}:{task_id}");
    match run_await_with_hook_inner(root, task_id, timeout_secs, live, hook, &round) {
        Ok(outcome) => Ok(outcome),
        Err(error)
            if crate::failure::is_action_rejection(&error) || is_critical_point_drift(&error) =>
        {
            Err(error)
        }
        Err(error) => crate::failure::reject_action_from_ledger_command_outcome(
            root,
            &round,
            Some(task_id),
            "await-report",
            &action_id,
            &format!("{error:#}"),
            crate::failure::CommandOutcome::PendingConsumption,
        ),
    }
}

fn run_await_with_hook_inner(
    root: &Path,
    task_id: &str,
    timeout_secs: u64,
    live: Option<liveness::LivenessOpts>,
    hook: &mut dyn FnMut(&str) -> Result<()>,
    round: &str,
) -> Result<AwaitOutcome> {
    run_await_with_hook_inner_impl(root, task_id, timeout_secs, live, hook, round)
}

fn require_await_entry_card(root: &Path, round: &str, task_id: &str) -> Result<card::Card> {
    critical_point(require_active_tierf_task(root, round, task_id))
}

#[allow(clippy::too_many_arguments)]
fn reconcile_await_bootstrap<R: AwaitReportRuntime>(
    root: &Path,
    round: &str,
    task_id: &str,
    strict_attempt: &crate::attempt::AttemptRef,
    cur_agent: Option<&str>,
    ack_evidence_observation: Option<&crate::attempt::EvidenceObservation>,
    ack_observed: bool,
    mut ack_logged: bool,
    hook: &mut dyn FnMut(&str) -> Result<()>,
    runtime: &mut R,
    watcher_callback: &mut Option<AwaitReportNotifyCallback>,
    watcher_failed: &AtomicBool,
    watcher_disabled: &mut bool,
    watch_plan: &AwaitReportWatchPlan,
    watched: &mut BTreeSet<PathBuf>,
) -> Result<bool> {
    // This is the real, once-per-frame orchestration path. Keep the canonical
    // durable claim and every optional watcher edge in this one body so the
    // frozen source-order contract observes execution order, not the order of
    // disconnected helper definitions.
    if ack_observed && !ack_logged {
        let agent = cur_agent.context("await ACK claim 缺 current agent")?;
        let ack_evidence =
            ack_evidence_observation.context("await ACK claim 缺 evidence observation")?;
        ack_logged = claim_observed_dispatch_ack(
            root,
            round,
            task_id,
            &strict_attempt.attempt_id,
            strict_attempt.ordinal,
            agent,
            ack_evidence,
            hook,
        )?;
    }

    if let Some(callback) = watcher_callback.take() {
        runtime.install_watcher(callback);
        let reports_dir = root.join(format!("coordination/rounds/{round}/reports"));
        let _ = runtime.create_dir_all(&reports_dir);
    }
    if !*watcher_disabled && watcher_failed.load(Ordering::Acquire) {
        *watcher_disabled = true;
        runtime.disable_watcher();
    }
    if runtime.watcher_available() {
        for pending in
            await_report_pending_watch_roots(watch_plan, watched, |path| runtime.is_dir(path))
        {
            let mode = if pending.recursive {
                notify::RecursiveMode::Recursive
            } else {
                notify::RecursiveMode::NonRecursive
            };
            if runtime.watch(&pending.path, mode) {
                watched.insert(pending.path);
            }
        }
    }

    Ok(ack_logged)
}

fn before_await_liveness_probe(hook: &mut dyn FnMut(&str) -> Result<()>) -> Result<()> {
    hook("before-liveness-probe")
}

fn wait_for_await_reconcile<R, F>(runtime: &mut R, tick: Duration, native_wait: F)
where
    R: AwaitReportRuntime,
    F: FnOnce() -> std::result::Result<(), std::sync::mpsc::RecvTimeoutError>,
{
    match runtime.recv_timeout(tick, native_wait) {
        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => runtime.sleep(tick),
    }
}

/// REPORT 写盘到 commit 有界等待期间，branch HEAD 对 REPORT 路径的观察态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportCommitState {
    /// HEAD 还不含 REPORT 路径（写盘后尚未 commit）。
    MissingFromHead,
    /// HEAD 含相同路径且字节与 observed 完全一致。
    InHeadSameBytes,
    /// HEAD 含该路径但字节不同（已被改写）。
    InHeadDifferentBytes,
}

/// `decide_report_commit_wait` 的等待决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportCommitDecision {
    /// 仍在 grace 内，继续重读 HEAD。
    Wait,
    /// 到期仍未 commit，拒绝并报“未 commit”。
    RejectUncommitted,
    /// HEAD 含该路径且字节一致，可进入 durable collect。
    Ready,
    /// HEAD 字节不同，立即拒绝。
    RejectDifferentBytes,
}

/// 纯决策：REPORT 被观察到写盘后，在任何 durable ReportCollect claim 之前，最多等
/// `deadline`、每 2 秒重读 branch HEAD。HEAD 含相同路径且字节完全一致才 Ready；字节不同
/// 立即拒绝；到期仍未 commit 报“未 commit”，不得误报 writeSet。等待期间不得钉死旧 SHA，
/// 不得预写 claim/receipt。`elapsed` 由可注入 clock 提供，测试禁止真实等待。
pub fn decide_report_commit_wait(
    state: ReportCommitState,
    elapsed: Duration,
    deadline: Duration,
) -> ReportCommitDecision {
    match state {
        ReportCommitState::InHeadSameBytes => ReportCommitDecision::Ready,
        ReportCommitState::InHeadDifferentBytes => ReportCommitDecision::RejectDifferentBytes,
        ReportCommitState::MissingFromHead => {
            if elapsed >= deadline {
                ReportCommitDecision::RejectUncommitted
            } else {
                ReportCommitDecision::Wait
            }
        }
    }
}

/// REPORT commit grace 常量（r16 语义恢复）：observed 写盘后最多等 120s、每 2s 重读 HEAD。
const REPORT_COMMIT_GRACE: Duration = Duration::from_secs(120);
const REPORT_COMMIT_POLL: Duration = Duration::from_secs(2);

/// HEAD 对 REPORT 路径的精确观察结果（B107 self-repair）：区分「真 Missing」
/// 与「仓库/对象/读取故障」——后者由 `read_report_at_head` 以 Err 立即传播，
/// 绝不进入等待决策。
enum ReportAtHead {
    /// ls-tree 退出码为零且精确输出为空：HEAD 树确实不含该路径。
    Missing,
    /// ls-tree 命中条目，字节经 show_bytes 读出。
    Present(Vec<u8>),
}

/// HEAD 对 REPORT 路径的精确读取器（B107 self-repair）：先跑
/// `git -C <root> ls-tree --full-tree -z <head> -- <report_rel>`——
/// 退出码非零是仓库/对象/读取故障（Fault），立即 bail 传播、不进等待；
/// 退出码为零且输出精确为空才是真 Missing；命中条目才 `show_bytes` 读字节，
/// 读取失败同样立即传播。不解析 stderr 文案，只凭退出码 + 输出是否为空分类。
fn read_report_at_head(root: &Path, head: &str, report_rel: &str) -> Result<ReportAtHead> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-tree", "--full-tree", "-z", head, "--", report_rel])
        .output()
        .with_context(|| format!("git ls-tree 启动失败（HEAD={}）", gitx::short(head)))?;
    if !out.status.success() {
        bail!(
            "git ls-tree 观察 HEAD={} 的 REPORT 路径失败({}): {}",
            gitx::short(head),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    if out.stdout.is_empty() {
        return Ok(ReportAtHead::Missing);
    }
    let bytes = gitx::show_bytes(root, head, report_rel)
        .with_context(|| format!("HEAD={} 树含 REPORT 路径但读取字节失败", gitx::short(head)))?;
    Ok(ReportAtHead::Present(bytes))
}

/// `decide_report_commit_wait` 的生产消费点：observed REPORT 写盘后、任何 durable
/// ReportCollect claim 之前的有界等待。每轮**重读当前 branch HEAD**（不钉死旧 SHA），
/// HEAD 缺 REPORT 路径继续等、字节完全一致才把**此刻的 HEAD** 作为 pin 返回、字节不同
/// 立即拒绝、到期报「未 commit」（不得误报 writeSet）。等待期间不写任何
/// claim/receipt。`reader`/`elapsed`/`sleeper` 可注入，测试用脚本化 clock 取得确定性，
/// 禁止真实等待 120s。`reader` 返回的 Err 是仓库/对象/读取故障（Fault），
/// **立即传播、不进等待**（self-repair：不再把 Fault 误归为 Missing 白等 120s
/// 再误报「未 commit」）；只有 `ReportAtHead::Missing`（ls-tree 成功且精确空输出）
/// 才进入等待/到期语义。
#[allow(clippy::too_many_arguments)]
fn wait_for_report_commit(
    root: &Path,
    branch: &str,
    report_rel: &str,
    claimed: &crate::attempt::ClaimedEvidence,
    deadline: Duration,
    poll: Duration,
    reader: &mut dyn FnMut(&Path, &str, &str) -> Result<ReportAtHead>,
    elapsed: &mut dyn FnMut() -> Duration,
    sleeper: &mut dyn FnMut(Duration),
) -> Result<String> {
    loop {
        // 每轮重读 HEAD：等待期间 executor 可能随时 commit，不得钉死旧 SHA。
        let head = gitx::rev_parse(root, branch)?;
        // Fault 立即传播：`?` 不经过等待决策，不会误报「未 commit」。
        let state = match reader(root, &head, report_rel)? {
            ReportAtHead::Present(bytes) if bytes == claimed.bytes => {
                ReportCommitState::InHeadSameBytes
            }
            ReportAtHead::Present(_) => ReportCommitState::InHeadDifferentBytes,
            ReportAtHead::Missing => ReportCommitState::MissingFromHead,
        };
        match decide_report_commit_wait(state, elapsed(), deadline) {
            // 只有观察到字节一致的此刻才钉 HEAD，随后才允许进入 durable claim。
            ReportCommitDecision::Ready => return Ok(head),
            ReportCommitDecision::RejectDifferentBytes => {
                bail!("task branch HEAD 的 committed REPORT 字节与 observed evidence 不一致，拒绝 collect")
            }
            ReportCommitDecision::RejectUncommitted => {
                bail!(
                    "REPORT 已观察到写盘，但 {}s 内未 commit 到 task branch",
                    deadline.as_secs()
                )
            }
            ReportCommitDecision::Wait => sleeper(poll),
        }
    }
}

fn evidence_event_matches(
    event: &EventRecord,
    kind: &str,
    action_id: &str,
    attempt_id: &str,
    evidence: &crate::attempt::ClaimedEvidence,
) -> bool {
    let payload = match event.payload.as_ref() {
        Some(payload) => payload,
        None => return false,
    };
    event.kind == kind
        && payload.get("attemptId").and_then(serde_json::Value::as_str) == Some(attempt_id)
        && payload.get("actionId").and_then(serde_json::Value::as_str) == Some(action_id)
        && payload
            .get("evidencePath")
            .and_then(serde_json::Value::as_str)
            == Some(evidence.canonical_path.as_str())
        && payload
            .get("evidenceSha256")
            .and_then(serde_json::Value::as_str)
            == Some(evidence.sha256.as_str())
        && payload
            .get("evidenceLen")
            .and_then(serde_json::Value::as_u64)
            == Some(evidence.len)
        && payload
            .get("controlEpoch")
            .and_then(serde_json::Value::as_str)
            == Some(evidence.control_epoch.as_str())
}

fn action_event_matches(
    event: &EventRecord,
    kind: &str,
    action_id: &str,
    attempt_id: &str,
    control_epoch: &str,
) -> bool {
    let payload = match event.payload.as_ref() {
        Some(payload) => payload,
        None => return false,
    };
    event.kind == kind
        && payload.get("attemptId").and_then(serde_json::Value::as_str) == Some(attempt_id)
        && payload.get("actionId").and_then(serde_json::Value::as_str) == Some(action_id)
        && payload
            .get("controlEpoch")
            .and_then(serde_json::Value::as_str)
            == Some(control_epoch)
}

struct DetachedGateWorktree<'a> {
    root: &'a Path,
    path: PathBuf,
}

impl Drop for DetachedGateWorktree<'_> {
    fn drop(&mut self) {
        let _ = gitx::worktree_remove(self.root, &self.path);
    }
}

// ───────────────────── durable action 共享机械（fold-first） ─────────────────────
//
// revision 8：三条 durable 生产路径（DispatchWake / ResumeWake / ReportCollect）
// 在任何 replay / terminal / release 决策前必须先跑共同 fold（attempt.rs）。
// 每次 claim 生成独立 owner + leaseGeneration（ULID），同 action 的所有 state
// event 都携带同一 owner + generation；release 在账本锁内做 fold CAS（kind
// 前驱表 + owner/generation 等值）。

/// scope 内最新一条 durable 事件（与 fold 相同的选择口径）。
fn latest_durable_event<'a>(
    events: &'a [EventRecord],
    kind: crate::attempt::DurableActionKind,
    expectation: &crate::attempt::DurableActionExpectation,
) -> Option<&'a EventRecord> {
    events.iter().rev().find(|event| {
        event.task_id.as_deref() == Some(expectation.task_id.as_str())
            && event.round.as_deref() == Some(expectation.round.as_str())
            && event.kind.starts_with(kind.prefix())
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("actionId"))
                .and_then(serde_json::Value::as_str)
                == Some(expectation.action_id.as_str())
    })
}

/// 从一条 fold 已验证的事件提取锚点 owner / leaseGeneration（缺失为内部矛盾，fail-closed）。
fn durable_anchor_owner_gen(event: &EventRecord, label: &str) -> Result<(String, String)> {
    let payload = event
        .payload
        .as_ref()
        .with_context(|| format!("{label} 事件缺 payload"))?;
    let owner = payload
        .get("owner")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .with_context(|| format!("{label} 事件缺 owner"))?;
    let generation = payload
        .get("leaseGeneration")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .with_context(|| format!("{label} 事件缺 leaseGeneration"))?;
    Ok((owner.to_string(), generation.to_string()))
}

/// `ReportCollectExecuting` 租约过期后的作废重来计划。
///
/// release 身份必须来自 durable 锚点；reclaim 身份则必须是全新一代。调用者负责在
/// 同一个 ledger fold 事务中按 `Released -> Claimed` 的顺序落账。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpiredExecutingRecovery {
    pub release_owner: String,
    pub release_generation: String,
    pub outcome_unknown: bool,
    pub reclaim_owner: String,
    pub reclaim_generation: String,
    pub reclaim_lease_until: String,
}

/// 为已过期的 `ReportCollectExecuting` 锚点生成 fail-closed 恢复计划。
pub fn plan_expired_executing_recovery(
    anchor: &EventRecord,
    now: SystemTime,
) -> Result<ExpiredExecutingRecovery> {
    if anchor.kind != "ReportCollectExecuting" {
        bail!(
            "expired executing recovery requires ReportCollectExecuting anchor, got {}",
            anchor.kind
        );
    }
    let (release_owner, release_generation) =
        durable_anchor_owner_gen(anchor, "ReportCollectExecuting")?;
    let lease_until = anchor
        .payload
        .as_ref()
        .and_then(|payload| payload.get("leaseUntil"))
        .and_then(serde_json::Value::as_str)
        .context("ReportCollectExecuting leaseUntil missing")?;
    let lease_until = humantime::parse_rfc3339(lease_until)
        .context("ReportCollectExecuting leaseUntil invalid")?;
    if lease_until > now {
        bail!("ReportCollectExecuting lease is still live");
    }
    let reclaim_deadline = now
        .checked_add(Duration::from_secs(3600))
        .context("ReportCollect reclaim lease overflow")?;
    Ok(ExpiredExecutingRecovery {
        release_owner,
        release_generation,
        outcome_unknown: true,
        reclaim_owner: ulid::Ulid::new().to_string(),
        reclaim_generation: ulid::Ulid::new().to_string(),
        reclaim_lease_until: humantime::format_rfc3339_seconds(reclaim_deadline).to_string(),
    })
}

/// 事件携带的 leaseUntil 是否仍存活。
fn durable_lease_live(event: &EventRecord, label: &str) -> Result<bool> {
    let lease = event
        .payload
        .as_ref()
        .and_then(|payload| payload.get("leaseUntil"))
        .and_then(serde_json::Value::as_str)
        .with_context(|| format!("{label} leaseUntil missing"))?;
    Ok(
        humantime::parse_rfc3339(lease).with_context(|| format!("{label} leaseUntil invalid"))?
            > SystemTime::now(),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CollectGateReceiptRef {
    event_id: String,
    attestation_sha256: String,
    attestation_len: u64,
}

#[derive(Debug, Clone)]
struct ActualGateSummary {
    command_ref: String,
    exit_code: i32,
    duration_ms: u64,
    raw_log_cas_sha256: String,
    raw_log_len: u64,
}

/// One fully validated collect gate that a later root-reuse consumer may adopt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedCollectGateV1 {
    /// One-based invocation position; repeated command refs remain distinct by this value.
    pub sequence: u64,
    /// Durable `GateExecuted` event identity.
    pub source_event_id: String,
    /// Phase-scoped run identity, unique within the receipt.
    pub gate_run_id: String,
    /// Semantic command reference recorded by the exact ten-key event.
    pub command_ref: String,
    /// Observed child exit code; reusable bundles require zero.
    pub exit_code: i32,
    /// SHA-256 of the independently reread raw log CAS object.
    pub log_sha256: String,
    /// Exact raw log byte length.
    pub log_bytes: u64,
}

/// Validated V1 bridge from Tier-F collect receipts to B304 root-gate reuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedCollectGateBundleV1 {
    /// Candidate commit whose tree was gated.
    pub candidate_sha: String,
    /// Candidate tree identity; current main is intentionally not part of it.
    pub subject_tree_sha: String,
    /// Exact attempt that produced this receipt.
    pub attempt_id: String,
    /// Immutable `DispatchIssued.baseSha` used for policy resolution.
    pub policy_base_sha: String,
    /// Signed active ROUND-IR revision at collect time.
    pub ir_revision: u32,
    /// Signed active ROUND-IR validation digest.
    pub validation_digest: String,
    /// SHA-256 of the active IR's bound project binding.
    pub binding_sha256: String,
    /// SHA-256 of this task's active signed card.
    pub task_card_sha256: String,
    /// Digest of the exact ordered reconstructed argv sequence.
    pub resolved_command_digest: String,
    /// Shared toolchain fingerprint across all gates.
    pub toolchain_digest: String,
    /// Shared environment fingerprint across all gates.
    pub environment_digest: String,
    /// Ordered, independently replayed green gate sources.
    pub gates: Vec<ValidatedCollectGateV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CollectAttestedGate {
    sequence: u64,
    gate_event_id: String,
    gate_run_id: String,
    command_ref: String,
    exit_code: i32,
    duration_ms: u64,
    raw_log_cas_sha256: String,
    raw_log_len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CollectReceiptAttestation {
    attestation_version: u64,
    round: String,
    task_id: String,
    action_id: String,
    owner: String,
    lease_generation: String,
    attempt_id: String,
    attempt_no: u64,
    agent: String,
    base_sha: String,
    go_path: String,
    executing_event_id: String,
    branch_sha: String,
    evidence_path: String,
    evidence_sha256: String,
    evidence_len: u64,
    control_epoch: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    subject_tree_sha: String,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    ir_revision: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    validation_digest: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    binding_sha256: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    task_card_sha256: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    resolved_command_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_reader_descriptor_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_reader_base_sha256: Option<String>,
    toolchain_digest: String,
    environment_digest: String,
    configured_gate_count: u64,
    configured_command_refs: Vec<String>,
    gates: Vec<CollectAttestedGate>,
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

fn collect_cas(root: &Path) -> crate::cas::Store {
    crate::cas::Store::new(&root.join("coordination/runtime/cas"))
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn read_verified_cas_object(
    root: &Path,
    sha256: &str,
    expected_len: u64,
    label: &str,
) -> Result<Vec<u8>> {
    if !valid_sha256(sha256) {
        bail!("{label} CAS SHA-256 不是 lowercase hex64");
    }
    let bytes = collect_cas(root)
        .get(sha256)?
        .with_context(|| format!("{label} CAS object 缺失: {sha256}"))?;
    let actual_len = u64::try_from(bytes.len()).context("CAS object 长度溢出 u64")?;
    let actual_sha256 = hex::encode(sha2::Sha256::digest(&bytes));
    if actual_len != expected_len || actual_sha256 != sha256 {
        bail!(
            "{label} CAS object 校验失败: expected={sha256}/{expected_len} actual={actual_sha256}/{actual_len}"
        );
    }
    Ok(bytes)
}

fn verify_attested_raw_logs(root: &Path, gates: &[CollectAttestedGate]) -> Result<()> {
    for gate in gates {
        read_verified_cas_object(
            root,
            &gate.raw_log_cas_sha256,
            gate.raw_log_len,
            &format!("gate #{} raw-log", gate.sequence),
        )?;
    }
    Ok(())
}

fn read_collect_attestation(
    root: &Path,
    receipt_ref: &CollectGateReceiptRef,
) -> Result<CollectReceiptAttestation> {
    let bytes = read_verified_cas_object(
        root,
        &receipt_ref.attestation_sha256,
        receipt_ref.attestation_len,
        "collect attestation",
    )?;
    let attestation: CollectReceiptAttestation =
        serde_json::from_slice(&bytes).context("collect attestation CAS JSON 解析失败")?;
    let canonical =
        serde_json::to_vec(&attestation).context("collect attestation canonical JSON 编码失败")?;
    if canonical != bytes {
        bail!("collect attestation CAS 文档不是 canonical JSON");
    }
    Ok(attestation)
}

fn write_collect_attestation(
    root: &Path,
    attestation: &CollectReceiptAttestation,
) -> Result<CollectGateReceiptRef> {
    let bytes =
        serde_json::to_vec(attestation).context("collect attestation canonical JSON 编码失败")?;
    let len = u64::try_from(bytes.len()).context("collect attestation 长度溢出 u64")?;
    let sha256 = collect_cas(root).put(&bytes)?;
    let reference = CollectGateReceiptRef {
        event_id: String::new(),
        attestation_sha256: sha256,
        attestation_len: len,
    };
    let reread = read_collect_attestation(root, &reference)?;
    if reread != *attestation {
        bail!("collect attestation CAS 创建后逐字段回读不一致");
    }
    verify_attested_raw_logs(root, &reread.gates)?;
    Ok(reference)
}

fn collect_gate_summaries(
    root: &Path,
    outcome: &collect::CollectOutcome,
) -> Result<Vec<ActualGateSummary>> {
    outcome
        .gates
        .iter()
        .map(|gate| {
            let bytes = fs::read(&gate.log_path)
                .with_context(|| format!("读取 gate 结果日志失败: {}", gate.log_path))?;
            let raw_log_len = u64::try_from(bytes.len()).context("gate 日志长度溢出 u64")?;
            let raw_log_cas_sha256 = collect_cas(root).put(&bytes)?;
            read_verified_cas_object(
                root,
                &raw_log_cas_sha256,
                raw_log_len,
                &format!("gate {} raw-log", gate.name),
            )?;
            Ok(ActualGateSummary {
                command_ref: gate.name.clone(),
                exit_code: gate.exit_code,
                duration_ms: u64::try_from(gate.duration_ms).context("gate durationMs 溢出 u64")?,
                raw_log_cas_sha256,
                raw_log_len,
            })
        })
        .collect()
}

fn receipt_ref_from_payload(
    payload: &serde_json::Value,
    label: &str,
) -> Result<CollectGateReceiptRef> {
    let attestation_sha256 = payload
        .get("receiptAttestationSha256")
        .and_then(serde_json::Value::as_str)
        .filter(|value| valid_sha256(value))
        .with_context(|| format!("{label} 缺合法 receiptAttestationSha256"))?;
    if payload
        .get("gateReceiptDigest")
        .and_then(serde_json::Value::as_str)
        != Some(attestation_sha256)
    {
        bail!("{label} gateReceiptDigest 未绑定 attestation CAS");
    }
    Ok(CollectGateReceiptRef {
        event_id: payload
            .get("gateReceipt")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .with_context(|| format!("{label} 缺 gateReceipt"))?
            .to_string(),
        attestation_sha256: attestation_sha256.to_string(),
        attestation_len: payload
            .get("receiptAttestationLen")
            .and_then(serde_json::Value::as_u64)
            .with_context(|| format!("{label} 缺 receiptAttestationLen"))?,
    })
}

fn bind_attestation_ref(payload: &mut serde_json::Value, receipt_ref: &CollectGateReceiptRef) {
    payload["gateReceiptDigest"] = serde_json::json!(receipt_ref.attestation_sha256);
    payload["receiptAttestationSha256"] = serde_json::json!(receipt_ref.attestation_sha256);
    payload["receiptAttestationLen"] = serde_json::json!(receipt_ref.attestation_len);
}

fn receipt_attestation_ref_from_payload(
    payload: &serde_json::Value,
    label: &str,
) -> Result<(String, u64)> {
    Ok((
        payload
            .get("receiptAttestationSha256")
            .and_then(serde_json::Value::as_str)
            .filter(|value| valid_sha256(value))
            .with_context(|| format!("{label} 缺合法 receiptAttestationSha256"))?
            .to_string(),
        payload
            .get("receiptAttestationLen")
            .and_then(serde_json::Value::as_u64)
            .with_context(|| format!("{label} 缺 receiptAttestationLen"))?,
    ))
}

struct CollectContractIdentity {
    ir_revision: u32,
    validation_digest: String,
    binding_sha256: String,
    task_card_sha256: String,
}

fn collect_contract_identity(
    root: &Path,
    events: &[EventRecord],
    round: &str,
    task_id: &str,
) -> Result<CollectContractIdentity> {
    let validation = crate::plan::require_active_round_ir(root, round, events)?;
    let card_rel = format!("coordination/rounds/{round}/tasks/{task_id}.md");
    let task_card_sha256 = validation
        .candidate
        .source_bindings
        .task_cards
        .get(&card_rel)
        .with_context(|| format!("active ROUND-IR 未绑定 collect card: {card_rel}"))?
        .clone();
    Ok(CollectContractIdentity {
        ir_revision: validation.persisted_revision,
        validation_digest: validation.persisted_digest,
        binding_sha256: validation.candidate.source_bindings.binding_sha256,
        task_card_sha256,
    })
}

fn expected_collect_lane_plan(
    root: &Path,
    card: &card::Card,
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
    policy_base_sha: &str,
    candidate_sha: &str,
) -> Result<collect::CollectLanePlan> {
    if card.meta.task_id != task_id {
        bail!("collect lane resolver card/task identity 漂移");
    }
    collect::resolve_collect_lane_plan_at_candidate(
        root,
        round,
        card,
        events,
        attempt_id,
        policy_base_sha,
        candidate_sha,
    )
}

fn validate_lane_escalation_precedes_first_collect_gate(
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
    expects_escalation: bool,
    first_gate_position: usize,
) -> Result<()> {
    if !expects_escalation {
        return Ok(());
    }
    let positions = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "GateLaneEscalated"
                && event.task_id.as_deref() == Some(task_id)
                && event.round.as_deref() == Some(round)
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("attemptId"))
                    .and_then(serde_json::Value::as_str)
                    == Some(attempt_id)
        })
        .map(|(position, _)| position)
        .collect::<Vec<_>>();
    if positions.len() != 1 || positions[0] >= first_gate_position {
        bail!("GateLaneEscalated 必须唯一且先于首条 collect GateExecuted");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_collect_gate_receipt(
    root: &Path,
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    card: &card::Card,
    ctx: &crate::attempt::DispatchContext,
    claimed: &crate::attempt::ClaimedEvidence,
    action_id: &str,
    branch_sha: &str,
    owner: &str,
    generation: &str,
    executing_event_id: &str,
    summaries: &[ActualGateSummary],
) -> Result<(EventRecord, CollectGateReceiptRef)> {
    let attempt_id = ctx
        .attempt_id
        .as_deref()
        .context("collect attestation 缺 attemptId")?;
    let policy_base_sha = ctx
        .base_sha
        .as_deref()
        .context("collect attestation 缺 baseSha")?;
    let lane_plan = expected_collect_lane_plan(
        root,
        card,
        events,
        round,
        task_id,
        attempt_id,
        policy_base_sha,
        branch_sha,
    )?;
    let expected_refs = lane_plan.ordered_command_refs();
    let resolved_command_digest = lane_plan.resolved_command_digest().to_string();
    let source_reader_digests = lane_plan
        .source_reader_digests()
        .map(|(descriptor, base)| (descriptor.to_string(), base.to_string()));
    let contract = collect_contract_identity(root, events, round, task_id)?;
    let executing = events
        .iter()
        .rposition(|event| event.event_id == executing_event_id)
        .context("collect receipt 缺 Executing 锚点")?;
    if events
        .iter()
        .filter(|event| event.event_id == executing_event_id)
        .count()
        != 1
    {
        bail!("collect receipt Executing eventId 不是全局唯一");
    }
    let gate_events: Vec<&EventRecord> = events[executing + 1..]
        .iter()
        .filter(|event| is_collect_gate_event(event, task_id, round))
        .collect();
    let first_gate_position = events[executing + 1..]
        .iter()
        .position(|event| is_collect_gate_event(event, task_id, round))
        .map(|offset| executing + 1 + offset)
        .context("collect receipt 缺首条 collect GateExecuted")?;
    validate_lane_escalation_precedes_first_collect_gate(
        events,
        round,
        task_id,
        attempt_id,
        lane_plan.requires_escalation_event(),
        first_gate_position,
    )?;
    if gate_events.len() != expected_refs.len() || summaries.len() != expected_refs.len() {
        bail!(
            "collect receipt gate 数量不匹配：ledger={} actual={} configured={}",
            gate_events.len(),
            summaries.len(),
            expected_refs.len()
        );
    }
    let mut gates = Vec::with_capacity(expected_refs.len());
    let mut gate_event_ids = std::collections::HashSet::new();
    let mut gate_run_ids = std::collections::HashSet::new();
    let expected_subject_tree = gitx::rev_parse(root, &format!("{branch_sha}^{{tree}}"))?;
    let mut toolchain_digest: Option<String> = None;
    let mut environment_digest: Option<String> = None;
    for (index, ((event, summary), command_ref)) in gate_events
        .iter()
        .zip(summaries)
        .zip(&expected_refs)
        .enumerate()
    {
        let observed = collect_gate_observation(event)?;
        if !gate_event_ids.insert(event.event_id.as_str())
            || !gate_run_ids.insert(observed.gate_run_id)
            || events
                .iter()
                .filter(|candidate| candidate.event_id == event.event_id)
                .count()
                != 1
            || !observation_matches_summary(&observed, summary, command_ref, &expected_subject_tree)
        {
            bail!("collect receipt gate #{index} 与实际/configured gate 不一致");
        }
        bind_shared_digest(
            &mut toolchain_digest,
            observed.toolchain_digest,
            "toolchain",
        )?;
        bind_shared_digest(
            &mut environment_digest,
            observed.environment_digest,
            "environment",
        )?;
        gates.push(CollectAttestedGate {
            sequence: (index + 1) as u64,
            gate_event_id: event.event_id.clone(),
            gate_run_id: observed.gate_run_id.to_string(),
            command_ref: command_ref.clone(),
            exit_code: summary.exit_code,
            duration_ms: summary.duration_ms,
            raw_log_cas_sha256: summary.raw_log_cas_sha256.clone(),
            raw_log_len: summary.raw_log_len,
        });
    }
    let attestation = CollectReceiptAttestation {
        attestation_version: 2,
        round: round.to_string(),
        task_id: task_id.to_string(),
        action_id: action_id.to_string(),
        owner: owner.to_string(),
        lease_generation: generation.to_string(),
        attempt_id: attempt_id.to_string(),
        attempt_no: ctx.attempt_no.context("collect attestation 缺 attemptNo")? as u64,
        agent: ctx.agent.clone().context("collect attestation 缺 agent")?,
        base_sha: policy_base_sha.to_string(),
        go_path: ctx
            .go_path
            .clone()
            .context("collect attestation 缺 goPath")?,
        executing_event_id: executing_event_id.to_string(),
        branch_sha: branch_sha.to_string(),
        evidence_path: claimed.canonical_path.clone(),
        evidence_sha256: claimed.sha256.clone(),
        evidence_len: claimed.len,
        control_epoch: claimed.control_epoch.clone(),
        subject_tree_sha: expected_subject_tree,
        ir_revision: contract.ir_revision,
        validation_digest: contract.validation_digest,
        binding_sha256: contract.binding_sha256,
        task_card_sha256: contract.task_card_sha256,
        resolved_command_digest,
        source_reader_descriptor_sha256: source_reader_digests
            .as_ref()
            .map(|(descriptor, _)| descriptor.clone()),
        source_reader_base_sha256: source_reader_digests.map(|(_, base)| base),
        toolchain_digest: toolchain_digest.context("collect attestation 缺 toolchainDigest")?,
        environment_digest: environment_digest
            .context("collect attestation 缺 environmentDigest")?,
        configured_gate_count: expected_refs.len() as u64,
        configured_command_refs: expected_refs,
        gates,
    };
    let mut receipt_ref = write_collect_attestation(root, &attestation)?;
    let mut payload = report_collect_payload(
        ctx,
        claimed,
        action_id,
        Some(branch_sha),
        owner,
        generation,
        None,
    )?;
    payload["receiptVersion"] = serde_json::json!(3);
    payload["gateCount"] = serde_json::json!(attestation.configured_gate_count);
    payload["configuredCommandRefs"] = serde_json::to_value(&attestation.configured_command_refs)?;
    payload["gates"] = serde_json::to_value(&attestation.gates)?;
    payload["toolchainDigest"] = serde_json::json!(attestation.toolchain_digest);
    payload["environmentDigest"] = serde_json::json!(attestation.environment_digest);
    payload["subjectTreeSha"] = serde_json::json!(attestation.subject_tree_sha);
    payload["irRevision"] = serde_json::json!(attestation.ir_revision);
    payload["validationDigest"] = serde_json::json!(attestation.validation_digest);
    payload["bindingSha256"] = serde_json::json!(attestation.binding_sha256);
    payload["taskCardSha256"] = serde_json::json!(attestation.task_card_sha256);
    payload["resolvedCommandDigest"] = serde_json::json!(attestation.resolved_command_digest);
    if let Some(descriptor) = &attestation.source_reader_descriptor_sha256 {
        payload["sourceReaderDescriptorSha256"] = serde_json::json!(descriptor);
    }
    if let Some(base) = &attestation.source_reader_base_sha256 {
        payload["sourceReaderBaseSha256"] = serde_json::json!(base);
    }
    payload["receiptDigest"] = serde_json::json!(receipt_ref.attestation_sha256);
    bind_attestation_ref(&mut payload, &receipt_ref);
    let receipt_event = ledger::event(
        "CollectGateSuccessReceipt",
        "runtime:orch",
        Some(task_id),
        Some(round),
        payload,
    );
    receipt_ref.event_id = receipt_event.event_id.clone();
    Ok((receipt_event, receipt_ref))
}

#[allow(clippy::too_many_arguments)]
fn validate_collect_gate_receipt(
    root: &Path,
    card: &card::Card,
    events: &[EventRecord],
    expectation: &crate::attempt::DurableActionExpectation,
    claimed: &crate::attempt::ClaimedEvidence,
    terminal: &EventRecord,
    receipt_ref: &CollectGateReceiptRef,
) -> Result<()> {
    let task_id = expectation.task_id.as_str();
    let round = expectation.round.as_str();
    let matching = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.event_id == receipt_ref.event_id)
        .collect::<Vec<_>>();
    if matching.len() != 1 {
        bail!(
            "collect gate receipt eventId 必须唯一，实际 {}",
            matching.len()
        );
    }
    let (receipt_position, receipt) = matching[0];
    if receipt.kind != "CollectGateSuccessReceipt"
        || receipt.actor != "runtime:orch"
        || receipt.task_id.as_deref() != Some(task_id)
        || receipt.round.as_deref() != Some(round)
    {
        bail!("collect gate receipt top-level identity 不匹配");
    }
    let payload = receipt
        .payload
        .as_ref()
        .context("CollectGateSuccessReceipt 缺 payload")?;
    if !evidence_event_matches(
        receipt,
        "CollectGateSuccessReceipt",
        &expectation.action_id,
        &expectation.attempt_id,
        claimed,
    ) {
        bail!("collect gate receipt action/evidence identity 不匹配");
    }
    let terminal_payload = terminal
        .payload
        .as_ref()
        .context("ReportCollect terminal 缺 payload")?;
    let (terminal_owner, terminal_generation) =
        durable_anchor_owner_gen(terminal, "ReportCollect terminal")?;
    if terminal.actor != "runtime:orch"
        || terminal.task_id.as_deref() != Some(task_id)
        || terminal.round.as_deref() != Some(round)
        || receipt_ref_from_payload(terminal_payload, "ReportCollect terminal")? != *receipt_ref
    {
        bail!("ReportCollect terminal 未绑定同一 attestation receipt");
    }
    for (key, expected) in [
        ("owner", terminal_owner.as_str()),
        ("leaseGeneration", terminal_generation.as_str()),
        ("agent", expectation.agent.as_str()),
        ("baseSha", expectation.base_sha.as_str()),
        ("goPath", expectation.go_path.as_str()),
        (
            "branchSha",
            expectation
                .branch_sha
                .as_deref()
                .context("collect receipt expectation 缺 branchSha")?,
        ),
        ("evidencePath", claimed.canonical_path.as_str()),
    ] {
        if payload.get(key).and_then(serde_json::Value::as_str) != Some(expected) {
            bail!("collect gate receipt {key} 不匹配");
        }
    }
    if payload.get("attemptNo").and_then(serde_json::Value::as_u64)
        != Some(expectation.attempt_no as u64)
    {
        bail!("collect gate receipt attemptNo 不匹配");
    }
    let (receipt_attestation_sha256, receipt_attestation_len) =
        receipt_attestation_ref_from_payload(payload, "CollectGateSuccessReceipt")?;
    if receipt_attestation_sha256 != receipt_ref.attestation_sha256
        || receipt_attestation_len != receipt_ref.attestation_len
        || payload
            .get("receiptDigest")
            .and_then(serde_json::Value::as_str)
            != Some(receipt_ref.attestation_sha256.as_str())
        || payload
            .get("gateReceiptDigest")
            .and_then(serde_json::Value::as_str)
            != Some(receipt_ref.attestation_sha256.as_str())
    {
        bail!("collect gate receipt 未精确绑定 attestation CAS");
    }
    let attestation = read_collect_attestation(root, receipt_ref)?;
    let receipt_version = payload
        .get("receiptVersion")
        .and_then(serde_json::Value::as_u64)
        .context("collect gate receipt 缺 receiptVersion")?;
    if !matches!(
        (attestation.attestation_version, receipt_version),
        (1, 2) | (2, 3)
    ) {
        bail!("collect gate receipt/attestation version 组合未建模");
    }
    let belongs = |event: &EventRecord, kind: &str| {
        event.kind == kind
            && event.task_id.as_deref() == Some(task_id)
            && event.round.as_deref() == Some(round)
            && event
                .payload
                .as_ref()
                .and_then(|value| value.get("actionId"))
                .and_then(serde_json::Value::as_str)
                == Some(expectation.action_id.as_str())
    };
    let executing_matches = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.event_id == attestation.executing_event_id)
        .collect::<Vec<_>>();
    if executing_matches.len() != 1 {
        bail!("attestation Executing eventId 必须全局唯一");
    }
    let (executing, executing_event) = executing_matches[0];
    if !belongs(executing_event, "ReportCollectExecuting")
        || executing_event.actor != "runtime:orch"
    {
        bail!("attestation Executing 锚点 identity/actor 不匹配");
    }
    let (executing_owner, executing_generation) =
        durable_anchor_owner_gen(executing_event, "ReportCollectExecuting")?;
    if executing_owner != terminal_owner || executing_generation != terminal_generation {
        bail!("attestation Executing 与 terminal lineage 不匹配");
    }
    let executed_matches = events
        .iter()
        .enumerate()
        .filter(|(_, event)| belongs(event, "ReportCollectExecuted"))
        .collect::<Vec<_>>();
    if executed_matches.len() != 1 {
        bail!("collect gate receipt 必须精确绑定一条 ReportCollectExecuted");
    }
    let (executed, executed_event) = executed_matches[0];
    let executed_payload = executed_event
        .payload
        .as_ref()
        .context("ReportCollectExecuted 缺 payload")?;
    if executed_event.actor != "runtime:orch"
        || receipt_ref_from_payload(executed_payload, "ReportCollectExecuted")? != *receipt_ref
    {
        bail!("ReportCollectExecuted 未绑定同一 attestation receipt");
    }
    if !(executing < receipt_position && receipt_position < executed) {
        bail!("collect gate receipt 不在 Executing→Executed 窗口");
    }
    let gate_events = events[executing + 1..receipt_position]
        .iter()
        .filter(|event| is_collect_gate_event(event, task_id, round))
        .collect::<Vec<_>>();
    let first_gate_position = events[executing + 1..receipt_position]
        .iter()
        .position(|event| is_collect_gate_event(event, task_id, round))
        .map(|offset| executing + 1 + offset)
        .context("collect attestation 缺首条 collect GateExecuted")?;
    if events[receipt_position + 1..executed]
        .iter()
        .any(|event| is_collect_gate_event(event, task_id, round))
    {
        bail!("collect gate receipt 后、Executed 前出现额外 GateExecuted");
    }
    let candidate_sha = expectation
        .branch_sha
        .as_deref()
        .context("collect attestation expectation 缺 branchSha")?;
    let expected_subject_tree = gitx::rev_parse(root, &format!("{candidate_sha}^{{tree}}"))?;
    let (configured, v2_identity_matches) = if attestation.attestation_version == 1 {
        let binding = crate::binding::load(root)?;
        let available = binding.commands.keys().cloned().collect::<Vec<_>>();
        (
            crate::binding::resolve_gates(&card.meta.gates.fast, &available)
                .map_err(anyhow::Error::msg)?,
            true,
        )
    } else {
        let lane_plan = expected_collect_lane_plan(
            root,
            card,
            events,
            round,
            task_id,
            &expectation.attempt_id,
            &expectation.base_sha,
            candidate_sha,
        )?;
        validate_lane_escalation_precedes_first_collect_gate(
            events,
            round,
            task_id,
            &expectation.attempt_id,
            lane_plan.requires_escalation_event(),
            first_gate_position,
        )?;
        let configured = lane_plan.ordered_command_refs();
        let expected_resolved_command_digest = lane_plan.resolved_command_digest();
        let expected_source_reader_digests = lane_plan.source_reader_digests();
        let contract = collect_contract_identity(root, events, round, task_id)?;
        let matches = attestation.attestation_version == 2
            && attestation.subject_tree_sha == expected_subject_tree
            && attestation.ir_revision == contract.ir_revision
            && attestation.validation_digest == contract.validation_digest
            && attestation.binding_sha256 == contract.binding_sha256
            && attestation.task_card_sha256 == contract.task_card_sha256
            && attestation.resolved_command_digest == expected_resolved_command_digest
            && match expected_source_reader_digests {
                Some((descriptor, base)) => {
                    attestation.source_reader_descriptor_sha256.as_deref() == Some(descriptor)
                        && attestation.source_reader_base_sha256.as_deref() == Some(base)
                }
                None => {
                    attestation.source_reader_descriptor_sha256.is_none()
                        && attestation.source_reader_base_sha256.is_none()
                }
            };
        (configured, matches)
    };
    if !v2_identity_matches
        || attestation.round != expectation.round
        || attestation.task_id != expectation.task_id
        || attestation.action_id != expectation.action_id
        || attestation.owner != terminal_owner
        || attestation.lease_generation != terminal_generation
        || attestation.attempt_id != expectation.attempt_id
        || attestation.attempt_no != expectation.attempt_no as u64
        || attestation.agent != expectation.agent
        || attestation.base_sha != expectation.base_sha
        || attestation.go_path != expectation.go_path
        || attestation.branch_sha
            != expectation
                .branch_sha
                .as_deref()
                .context("collect attestation expectation 缺 branchSha")?
        || attestation.evidence_path != claimed.canonical_path
        || attestation.evidence_sha256 != claimed.sha256
        || attestation.evidence_len != claimed.len
        || attestation.control_epoch != claimed.control_epoch
        || !valid_sha256(&attestation.toolchain_digest)
        || !valid_sha256(&attestation.environment_digest)
        || attestation.configured_gate_count != configured.len() as u64
        || attestation.configured_command_refs != configured
        || attestation.gates.len() != configured.len()
        || gate_events.len() != configured.len()
    {
        bail!("collect attestation action/evidence/configured gate 字段不匹配");
    }
    if !receipt_payload_matches_attestation(payload, &attestation)? {
        bail!("CollectGateSuccessReceipt ledger binding 与 attestation CAS 不匹配");
    }
    let mut referenced_gate_ids = std::collections::HashSet::new();
    let mut referenced_gate_run_ids = std::collections::HashSet::new();
    for (index, ((attested_gate, gate_event), command_ref)) in attestation
        .gates
        .iter()
        .zip(gate_events)
        .zip(configured)
        .enumerate()
    {
        let observed = collect_gate_observation(gate_event)?;
        if !referenced_gate_ids.insert(attested_gate.gate_event_id.as_str())
            || !referenced_gate_run_ids.insert(attested_gate.gate_run_id.as_str())
            || events
                .iter()
                .filter(|event| event.event_id == attested_gate.gate_event_id)
                .count()
                != 1
            || attested_gate.sequence != (index + 1) as u64
            || attested_gate.gate_event_id != gate_event.event_id
            || !observation_matches_attestation(
                &observed,
                attested_gate,
                &command_ref,
                &expected_subject_tree,
                &attestation.toolchain_digest,
                &attestation.environment_digest,
            )
        {
            bail!("collect gate receipt gate #{index} 顺序/结果不匹配");
        }
    }
    // `runtime:orch` 是当前 runtime 内的结构约束，不是签名或认证。独立 CAS 回读
    // 抵抗只能改 ledger 的伪造；能同时改 ledger 与 CAS/worktree 的进程已经跨过
    // 本 runtime 的信任边界，没有外部密钥/签名服务时无法与合法 runtime 区分。
    verify_attested_raw_logs(root, &attestation.gates)?;
    Ok(())
}

/// Load and independently replay the latest completed collect receipt for one exact attempt.
///
/// `None` means no completed receipt exists. A present receipt is returned only after its active
/// signed card/IR identity, immutable dispatch-base lane resolution, exact ten-key gate events,
/// positional run IDs, attestation CAS, and every raw-log CAS object have all been reread. Legacy
/// v1 attestations remain valid for historical lifecycle replay but are intentionally not exposed
/// as reusable V1 bundles because they lack the signed contract and resolved-argv identity.
pub fn load_validated_collect_gate_bundle_v1(
    root: &Path,
    round: &str,
    task_id: &str,
    attempt_id: &str,
) -> Result<Option<ValidatedCollectGateBundleV1>> {
    card::validate_task_id(task_id)?;
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let ledger_read = read_ledger(&ledger_path)
        .with_context(|| format!("读取 collect bundle ledger 失败: {}", ledger_path.display()))?;
    crate::attempt::reject_bad_lines(&ledger_read)?;
    let validation = crate::plan::require_active_round_ir(root, round, &ledger_read.events)?;
    let card = crate::plan::load_bound_task_card(root, round, task_id, &validation)?;
    let terminal = ledger_read.events.iter().rev().find(|event| {
        event.kind == "ReportCollectCompleted"
            && event.round.as_deref() == Some(round)
            && event.task_id.as_deref() == Some(task_id)
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("attemptId"))
                .and_then(serde_json::Value::as_str)
                == Some(attempt_id)
    });
    let Some(terminal) = terminal else {
        return Ok(None);
    };
    let terminal_payload = terminal
        .payload
        .as_ref()
        .context("ReportCollectCompleted 缺 payload")?;
    let receipt_ref = receipt_ref_from_payload(terminal_payload, "ReportCollectCompleted")?;
    let attestation = read_collect_attestation(root, &receipt_ref)?;
    if attestation.attestation_version != 2 {
        bail!("legacy collect attestation 缺 root-reuse contract identity");
    }
    let dispatch = crate::attempt::resolve_current_dispatch(&ledger_read.events, task_id, round)?;
    if dispatch.attempt_id.as_deref() != Some(attempt_id) {
        bail!("collect bundle attempt 不是 durable current DispatchIssued");
    }
    let terminal_str = |key: &str| {
        terminal_payload
            .get(key)
            .and_then(serde_json::Value::as_str)
            .with_context(|| format!("ReportCollectCompleted 缺 {key}"))
    };
    let evidence_path = terminal_str("evidencePath")?.to_string();
    let evidence_sha256 = terminal_str("evidenceSha256")?.to_string();
    let evidence_len = terminal_payload
        .get("evidenceLen")
        .and_then(serde_json::Value::as_u64)
        .context("ReportCollectCompleted 缺 evidenceLen")?;
    let control_epoch = terminal_str("controlEpoch")?.to_string();
    let branch_sha = terminal_str("branchSha")?.to_string();
    let claimed = crate::attempt::ClaimedEvidence {
        canonical_path: evidence_path,
        sha256: evidence_sha256.clone(),
        len: evidence_len,
        bytes: Vec::new(),
        control_epoch: control_epoch.clone(),
    };
    let expectation = crate::attempt::DurableActionExpectation {
        round: round.to_string(),
        task_id: task_id.to_string(),
        attempt_id: attempt_id.to_string(),
        attempt_no: dispatch
            .attempt_no
            .context("collect bundle DispatchIssued 缺 attemptNo")?,
        agent: dispatch
            .agent
            .context("collect bundle DispatchIssued 缺 agent")?,
        base_sha: dispatch
            .base_sha
            .context("collect bundle DispatchIssued 缺 baseSha")?,
        go_path: dispatch
            .go_path
            .context("collect bundle DispatchIssued 缺 goPath")?,
        action_id: terminal_str("actionId")?.to_string(),
        evidence_sha256: Some(evidence_sha256),
        evidence_len: Some(evidence_len),
        control_epoch: Some(control_epoch),
        branch_sha: Some(branch_sha),
    };
    validate_collect_gate_receipt(
        root,
        &card,
        &ledger_read.events,
        &expectation,
        &claimed,
        terminal,
        &receipt_ref,
    )?;
    Ok(Some(ValidatedCollectGateBundleV1 {
        candidate_sha: attestation.branch_sha,
        subject_tree_sha: attestation.subject_tree_sha,
        attempt_id: attestation.attempt_id,
        policy_base_sha: attestation.base_sha,
        ir_revision: attestation.ir_revision,
        validation_digest: attestation.validation_digest,
        binding_sha256: attestation.binding_sha256,
        task_card_sha256: attestation.task_card_sha256,
        resolved_command_digest: attestation.resolved_command_digest,
        toolchain_digest: attestation.toolchain_digest,
        environment_digest: attestation.environment_digest,
        gates: attestation
            .gates
            .into_iter()
            .map(|gate| ValidatedCollectGateV1 {
                sequence: gate.sequence,
                source_event_id: gate.gate_event_id,
                gate_run_id: gate.gate_run_id,
                command_ref: gate.command_ref,
                exit_code: gate.exit_code,
                log_sha256: gate.raw_log_cas_sha256,
                log_bytes: gate.raw_log_len,
            })
            .collect(),
    }))
}

/// DispatchWake / ResumeWake 共用的 expectation 构造。
fn wake_expectation(
    round: &str,
    task_id: &str,
    attempt_id: &str,
    attempt_no: usize,
    agent: &str,
    base_sha: &str,
    go_path: &str,
    action_id: &str,
) -> crate::attempt::DurableActionExpectation {
    crate::attempt::DurableActionExpectation {
        round: round.to_string(),
        task_id: task_id.to_string(),
        attempt_id: attempt_id.to_string(),
        attempt_no,
        agent: agent.to_string(),
        base_sha: base_sha.to_string(),
        go_path: go_path.to_string(),
        action_id: action_id.to_string(),
        evidence_sha256: None,
        evidence_len: None,
        control_epoch: None,
        branch_sha: None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReportCollectClaim {
    Owned {
        action_id: String,
        owner: String,
        generation: String,
    },
    Completed,
    RecoveredCompleted,
    Busy,
}

fn report_collect_action_id(
    attempt_id: &str,
    canonical_path: &str,
    digest: &str,
    control_epoch: &str,
) -> String {
    hex::encode(sha2::Sha256::digest(
        format!("report-collect\0{attempt_id}\0{canonical_path}\0{digest}\0{control_epoch}")
            .as_bytes(),
    ))
}

fn append_tierf_cost_sample(
    root: &Path,
    round: &str,
    task_id: &str,
    ctx: &crate::attempt::DispatchContext,
    duration_secs: u64,
) -> Result<()> {
    let attempt_id = ctx
        .attempt_id
        .as_deref()
        .context("CostSampled 缺 attemptId")?;
    let action_id = format!("cost-sampled-{attempt_id}");
    let appended = ledger::append_checked(root, round, |events| {
        if events.iter().any(|event| {
            event.kind == "CostSampled"
                && event.task_id.as_deref() == Some(task_id)
                && event.round.as_deref() == Some(round)
                && event
                    .payload
                    .as_ref()
                    .and_then(|p| p.get("actionId"))
                    .and_then(serde_json::Value::as_str)
                    == Some(action_id.as_str())
        }) {
            return Ok(Vec::new());
        }
        Ok(vec![ledger::event(
            "CostSampled",
            "runtime:orch",
            Some(task_id),
            Some(round),
            serde_json::json!({
                "actionId": action_id,
                "attemptId": attempt_id,
                "agent": ctx.agent,
                "durationSecs": duration_secs,
                "usage": serde_json::Value::Null,
            }),
        )])
    })?;
    if appended > 1 {
        bail!("CostSampled append count mismatch: {appended}");
    }
    Ok(())
}

fn report_collect_payload(
    ctx: &crate::attempt::DispatchContext,
    claimed: &crate::attempt::ClaimedEvidence,
    action_id: &str,
    branch_sha: Option<&str>,
    owner: &str,
    generation: &str,
    gate_receipt: Option<&CollectGateReceiptRef>,
) -> Result<serde_json::Value> {
    // 每个 state event 都携带同一 claim 的 owner + leaseGeneration（fold 必选）。
    let mut payload = serde_json::json!({
        "actionId": action_id,
        "owner": owner,
        "leaseGeneration": generation,
        "attemptId": ctx.attempt_id.as_deref().context("collect 缺 attemptId")?,
        "attemptNo": ctx.attempt_no.context("collect 缺 attemptNo")?,
        "agent": ctx.agent.as_deref().context("collect 缺 agent")?,
        "baseSha": ctx.base_sha.as_deref().context("collect 缺 baseSha")?,
        "goPath": ctx.go_path.as_deref().context("collect 缺 goPath")?,
        "evidencePath": claimed.canonical_path,
        "evidenceSha256": claimed.sha256,
        "evidenceLen": claimed.len,
        "controlEpoch": claimed.control_epoch,
    });
    if let Some(branch_sha) = branch_sha {
        payload["branchSha"] = serde_json::json!(branch_sha);
    }
    if let Some(receipt) = gate_receipt {
        payload["gateReceipt"] = serde_json::json!(receipt.event_id);
        bind_attestation_ref(&mut payload, receipt);
    }
    Ok(payload)
}

/// ReportCollect 的 fold expectation（branch 在 gate 成功前未知，用 None）。
fn report_collect_expectation(
    round: &str,
    task_id: &str,
    ctx: &crate::attempt::DispatchContext,
    claimed: &crate::attempt::ClaimedEvidence,
    action_id: &str,
    branch_sha: Option<&str>,
) -> Result<crate::attempt::DurableActionExpectation> {
    Ok(crate::attempt::DurableActionExpectation {
        round: round.to_string(),
        task_id: task_id.to_string(),
        attempt_id: ctx
            .attempt_id
            .clone()
            .context("collect expectation 缺 attemptId")?,
        attempt_no: ctx.attempt_no.context("collect expectation 缺 attemptNo")?,
        agent: ctx.agent.clone().context("collect expectation 缺 agent")?,
        base_sha: ctx
            .base_sha
            .clone()
            .context("collect expectation 缺 baseSha")?,
        go_path: ctx
            .go_path
            .clone()
            .context("collect expectation 缺 goPath")?,
        action_id: action_id.to_string(),
        evidence_sha256: Some(claimed.sha256.clone()),
        evidence_len: Some(claimed.len),
        control_epoch: Some(claimed.control_epoch.clone()),
        branch_sha: branch_sha.map(str::to_owned),
    })
}

#[allow(clippy::too_many_arguments)]
fn collect_claimed_report(
    root: &Path,
    round: &str,
    task_id: &str,
    card: &card::Card,
    branch: &str,
    report_rel: &str,
    ctx: &crate::attempt::DispatchContext,
    claimed: &crate::attempt::ClaimedEvidence,
    elapsed: &mut dyn FnMut() -> Duration,
    sleeper: &mut dyn FnMut(Duration),
    hook: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<collect::CollectOutcome> {
    // Pin and authenticate the branch/report object before consulting durable
    // terminal state. A forged Completed event cannot bypass object identity.
    //
    // B107（r16 语义恢复）：pin 之前先做 REPORT commit 有界等待——observed 写盘
    // 到 executor commit 之间存在窗口，每 2s 重读 HEAD（不钉死旧 SHA、不预写
    // claim/receipt），字节一致才钉 HEAD；字节不同立即拒绝；到期报「未 commit」。
    let pinned_branch_sha = wait_for_report_commit(
        root,
        branch,
        report_rel,
        claimed,
        REPORT_COMMIT_GRACE,
        REPORT_COMMIT_POLL,
        &mut read_report_at_head,
        elapsed,
        sleeper,
    )?;
    let committed_bytes = gitx::show_bytes(root, &pinned_branch_sha, report_rel)
        .context("读取 pinned task branch 的 committed REPORT 失败")?;
    let committed_digest = hex::encode(sha2::Sha256::digest(&committed_bytes));
    if committed_bytes.len() as u64 != claimed.len
        || committed_digest != claimed.sha256
        || committed_bytes != claimed.bytes
    {
        bail!("pinned task branch 的 committed REPORT 与 claimed evidence 不一致");
    }
    let report_text =
        std::str::from_utf8(&committed_bytes).context("committed REPORT 不是 UTF-8")?;
    let _decl = crate::mech::report_env_decl(report_text).context("REPORT 缺 §0 执行环境自报段")?;
    let action_id = report_collect_action_id(
        ctx.attempt_id.as_deref().context("collect 缺 attemptId")?,
        &claimed.canonical_path,
        &claimed.sha256,
        &claimed.control_epoch,
    );
    // 每次 claim 独立 owner + leaseGeneration；同 action 所有 state event 携带同一代。
    let owner = ulid::Ulid::new().to_string();
    let generation = ulid::Ulid::new().to_string();
    let lease_until =
        humantime::format_rfc3339_seconds(SystemTime::now() + Duration::from_secs(3600))
            .to_string();
    let state_cell = std::cell::RefCell::new(None);
    let appended = ledger::append_checked(root, round, |events| {
        let fresh_ctx = crate::attempt::resolve_current_dispatch(events, task_id, round)?;
        if fresh_ctx.attempt_id != ctx.attempt_id
            || fresh_ctx.attempt_no != ctx.attempt_no
            || fresh_ctx.agent != ctx.agent
            || fresh_ctx.base_sha != ctx.base_sha
            || fresh_ctx.go_path != ctx.go_path
        {
            bail!("REPORT collect dispatch facts changed before owner claim");
        }
        // All durable branches, including terminal Completed/Executed replay,
        // must consume the same immutable model-identity reconciliation before
        // they can return or append recovery state.
        crate::mech::ensure_recorded_identity_allows_collect_in_events(
            root,
            events,
            round,
            task_id,
            ctx.attempt_id.as_deref().context("模型勾稽缺 attemptId")?,
            ctx.agent.as_deref().context("模型勾稽缺 agent")?,
            claimed,
        )?;
        let expectation = report_collect_expectation(
            round,
            task_id,
            ctx,
            claimed,
            &action_id,
            Some(&pinned_branch_sha),
        )?;
        let new_claim_events =
            |claim_owner: &str, claim_generation: &str, claim_lease_until: &str| {
                let mut payload = report_collect_payload(
                    ctx,
                    claimed,
                    &action_id,
                    None,
                    claim_owner,
                    claim_generation,
                    None,
                )?;
                payload["leaseUntil"] = serde_json::json!(claim_lease_until);
                Ok::<_, anyhow::Error>(vec![ledger::event(
                    "ReportCollectClaimed",
                    "runtime:orch",
                    Some(task_id),
                    Some(round),
                    payload,
                )])
            };
        // fold-first：无任何 scope 历史（首次调用）才直接新 claim；否则必须 fold 决策。
        if !crate::attempt::durable_action_scope_has_events(
            events,
            crate::attempt::DurableActionKind::ReportCollect,
            &expectation,
        ) {
            *state_cell.borrow_mut() = Some(ReportCollectClaim::Owned {
                action_id: action_id.clone(),
                owner: owner.clone(),
                generation: generation.clone(),
            });
            return new_claim_events(&owner, &generation, &lease_until);
        }
        let phase = crate::attempt::fold_durable_action(
            events,
            crate::attempt::DurableActionKind::ReportCollect,
            &expectation,
        )?;
        match phase {
            crate::attempt::DurableActionPhase::Completed => {
                // terminal replay：fold 已绑定 evidence/pinned branch；
                // 还需 gate 成功回执在场。
                let completed = latest_durable_event(
                    events,
                    crate::attempt::DurableActionKind::ReportCollect,
                    &expectation,
                )
                .context("ReportCollect Completed replay 缺 terminal 事件")?;
                let completed_payload = completed
                    .payload
                    .as_ref()
                    .context("ReportCollectCompleted 缺 payload")?;
                let receipt =
                    receipt_ref_from_payload(completed_payload, "ReportCollectCompleted")?;
                validate_collect_gate_receipt(
                    root,
                    card,
                    events,
                    &expectation,
                    claimed,
                    completed,
                    &receipt,
                )?;
                *state_cell.borrow_mut() = Some(ReportCollectClaim::Completed);
                Ok(Vec::new())
            }
            crate::attempt::DurableActionPhase::Executed => {
                // completion replay：沿用原 lineage owner/generation，不重跑门。
                let executed = latest_durable_event(
                    events,
                    crate::attempt::DurableActionKind::ReportCollect,
                    &expectation,
                )
                .context("ReportCollect Executed replay 缺 Executed 事件")?;
                let (origin_owner, origin_generation) =
                    durable_anchor_owner_gen(executed, "ReportCollectExecuted")?;
                let payload = executed
                    .payload
                    .as_ref()
                    .context("ReportCollectExecuted 缺 payload")?;
                let branch_sha = payload
                    .get("branchSha")
                    .and_then(serde_json::Value::as_str)
                    .context("ReportCollectExecuted 缺 branchSha")?;
                let receipt = receipt_ref_from_payload(payload, "ReportCollectExecuted")?;
                validate_collect_gate_receipt(
                    root,
                    card,
                    events,
                    &expectation,
                    claimed,
                    executed,
                    &receipt,
                )?;
                *state_cell.borrow_mut() = Some(ReportCollectClaim::RecoveredCompleted);
                Ok(vec![ledger::event(
                    "ReportCollectCompleted",
                    "runtime:orch",
                    Some(task_id),
                    Some(round),
                    report_collect_payload(
                        ctx,
                        claimed,
                        &action_id,
                        Some(branch_sha),
                        &origin_owner,
                        &origin_generation,
                        Some(&receipt),
                    )?,
                )])
            }
            crate::attempt::DurableActionPhase::Executing => {
                let anchor = latest_durable_event(
                    events,
                    crate::attempt::DurableActionKind::ReportCollect,
                    &expectation,
                )
                .context("ReportCollect Executing 缺锚点事件")?;
                if durable_lease_live(anchor, "ReportCollectExecuting")? {
                    *state_cell.borrow_mut() = Some(ReportCollectClaim::Busy);
                    return Ok(Vec::new());
                }
                // 过期 Executing：上一持有者可能在门的任意位置崩溃。用锚点身份
                // 作废且永久标 outcomeUnknown，再以新代 claim 重跑门。
                let recovery = plan_expired_executing_recovery(anchor, SystemTime::now())?;
                let mut release_payload = report_collect_payload(
                    ctx,
                    claimed,
                    &action_id,
                    None,
                    &recovery.release_owner,
                    &recovery.release_generation,
                    None,
                )?;
                release_payload["outcomeUnknown"] = serde_json::json!(recovery.outcome_unknown);
                let released = ledger::event(
                    "ReportCollectReleased",
                    "runtime:orch",
                    Some(task_id),
                    Some(round),
                    release_payload,
                );
                *state_cell.borrow_mut() = Some(ReportCollectClaim::Owned {
                    action_id: action_id.clone(),
                    owner: recovery.reclaim_owner.clone(),
                    generation: recovery.reclaim_generation.clone(),
                });
                let mut events_out = vec![released];
                events_out.extend(new_claim_events(
                    &recovery.reclaim_owner,
                    &recovery.reclaim_generation,
                    &recovery.reclaim_lease_until,
                )?);
                Ok(events_out)
            }
            crate::attempt::DurableActionPhase::Claimed => {
                let anchor = latest_durable_event(
                    events,
                    crate::attempt::DurableActionKind::ReportCollect,
                    &expectation,
                )
                .context("ReportCollect Claimed 缺锚点事件")?;
                if durable_lease_live(anchor, "ReportCollectClaimed")? {
                    *state_cell.borrow_mut() = Some(ReportCollectClaim::Busy);
                    return Ok(Vec::new());
                }
                // 过期 claim：in-lock release CAS（owner/generation 必须等于锚点，
                // 前驱阶段 Claimed 在 kind 表内）+ 新代 reclaim，同事务追加。
                let (anchor_owner, anchor_generation) =
                    durable_anchor_owner_gen(anchor, "ReportCollectClaimed")?;
                let released = ledger::event(
                    "ReportCollectReleased",
                    "runtime:orch",
                    Some(task_id),
                    Some(round),
                    report_collect_payload(
                        ctx,
                        claimed,
                        &action_id,
                        None,
                        &anchor_owner,
                        &anchor_generation,
                        None,
                    )?,
                );
                *state_cell.borrow_mut() = Some(ReportCollectClaim::Owned {
                    action_id: action_id.clone(),
                    owner: owner.clone(),
                    generation: generation.clone(),
                });
                let mut events_out = vec![released];
                events_out.extend(new_claim_events(&owner, &generation, &lease_until)?);
                Ok(events_out)
            }
            crate::attempt::DurableActionPhase::Released => {
                *state_cell.borrow_mut() = Some(ReportCollectClaim::Owned {
                    action_id: action_id.clone(),
                    owner: owner.clone(),
                    generation: generation.clone(),
                });
                new_claim_events(&owner, &generation, &lease_until)
            }
            other => bail!("REPORT collect fold 返回 kind 非法阶段: {other:?}"),
        }
    })?;
    let state = state_cell
        .into_inner()
        .context("REPORT collect state closure did not set result")?;
    let (owner, generation) = match state {
        ReportCollectClaim::Completed => {
            if appended != 0 {
                bail!("completed REPORT collect replay appended events");
            }
            return Ok(collect::CollectOutcome {
                mech_notes: vec!["REPORT collect durable replay ✅".into()],
                gates: Vec::new(),
            });
        }
        ReportCollectClaim::RecoveredCompleted => {
            if appended != 1 {
                bail!("ReportCollectCompleted recovery append count mismatch");
            }
            return Ok(collect::CollectOutcome {
                mech_notes: vec![
                    "REPORT collect completion recovered without rerunning gates ✅".into(),
                ],
                gates: Vec::new(),
            });
        }
        ReportCollectClaim::Busy => {
            if appended != 0 {
                bail!("busy REPORT collect unexpectedly appended events");
            }
            bail!("REPORT collect action is owned by another live caller");
        }
        ReportCollectClaim::Owned { .. } if !(1..=2).contains(&appended) => {
            bail!("ReportCollectClaimed append count mismatch: {appended}")
        }
        ReportCollectClaim::Owned {
            action_id: owned_action_id,
            owner,
            generation,
        } => {
            if owned_action_id != action_id {
                bail!("ReportCollectClaimed action id changed during claim");
            }
            (owner, generation)
        }
    };

    hook("before-report-collect")?;
    let executing = ledger::append_checked(root, round, |events| {
        // fold-first：阶段必须是 Claimed 且 lineage 等于本 claim 的 owner/generation。
        let expectation = report_collect_expectation(
            round,
            task_id,
            ctx,
            claimed,
            &action_id,
            Some(&pinned_branch_sha),
        )?;
        let phase = crate::attempt::fold_durable_action(
            events,
            crate::attempt::DurableActionKind::ReportCollect,
            &expectation,
        )?;
        if phase != crate::attempt::DurableActionPhase::Claimed {
            bail!("REPORT collect execution 前 fold 阶段非法: {phase:?}");
        }
        let anchor = latest_durable_event(
            events,
            crate::attempt::DurableActionKind::ReportCollect,
            &expectation,
        )
        .context("REPORT collect execution lost owner claim")?;
        let (anchor_owner, anchor_generation) =
            durable_anchor_owner_gen(anchor, "ReportCollectClaimed")?;
        if anchor_owner != owner || anchor_generation != generation {
            bail!("REPORT collect execution owner/generation changed");
        }
        let lease = anchor
            .payload
            .as_ref()
            .and_then(|payload| payload.get("leaseUntil"))
            .and_then(serde_json::Value::as_str)
            .context("REPORT collect owner lease missing")?;
        if humantime::parse_rfc3339(lease)? <= SystemTime::now() {
            bail!("REPORT collect owner lease expired before execution fence");
        }
        let mut payload =
            report_collect_payload(ctx, claimed, &action_id, None, &owner, &generation, None)?;
        payload["leaseUntil"] = serde_json::json!(lease);
        Ok(vec![ledger::event(
            "ReportCollectExecuting",
            "runtime:orch",
            Some(task_id),
            Some(round),
            payload,
        )])
    })?;
    if executing != 1 {
        bail!("ReportCollectExecuting append count mismatch: {executing}");
    }
    let collect_result = (|| -> Result<(collect::CollectOutcome, String)> {
        // Recheck immediately before gates as well: an Owned caller may have
        // spent time between its durable claim and execution fence.
        crate::mech::ensure_recorded_identity_allows_collect(
            root,
            round,
            task_id,
            ctx.attempt_id.as_deref().context("模型勾稽缺 attemptId")?,
            ctx.agent.as_deref().context("模型勾稽缺 agent")?,
            claimed,
        )?;
        if gitx::rev_parse(root, branch)? != pinned_branch_sha {
            bail!("task branch 在 collect claim 后发生移动，拒绝运行 gate");
        }
        let committed = pinned_branch_sha.clone();
        let gate_wt =
            root.join(".cowork-temp")
                .join(format!("await-{}-{}", task_id, &claimed.sha256[..16]));
        if std::fs::symlink_metadata(&gate_wt).is_ok() {
            bail!("immutable gate worktree path 已存在: {}", gate_wt.display());
        }
        std::fs::create_dir_all(gate_wt.parent().context("gate worktree 缺 parent")?)?;
        gitx::worktree_add_detached(root, &gate_wt, &committed)?;
        let _gate_guard = DetachedGateWorktree {
            root,
            path: gate_wt.clone(),
        };
        let outcome = collect::check_and_gate(
            root,
            round,
            card,
            &committed,
            report_rel,
            ctx.base_sha.as_deref(),
            &gate_wt,
        )?;
        Ok((outcome, committed))
    })();
    let (outcome, branch_sha) = match collect_result {
        Ok(value) => value,
        Err(error) => {
            // release CAS：账本锁内 fold，前驱阶段必须在 kind 表内
            // （collect: Claimed | Executing），owner/generation 必须等于本 claim。
            let release_result = ledger::append_checked(root, round, |events| {
                let expectation = report_collect_expectation(
                    round,
                    task_id,
                    ctx,
                    claimed,
                    &action_id,
                    Some(&pinned_branch_sha),
                )?;
                let phase = crate::attempt::fold_durable_action(
                    events,
                    crate::attempt::DurableActionKind::ReportCollect,
                    &expectation,
                )?;
                if !matches!(
                    phase,
                    crate::attempt::DurableActionPhase::Claimed
                        | crate::attempt::DurableActionPhase::Executing
                ) {
                    bail!("REPORT collect release 前 fold 阶段非法: {phase:?}");
                }
                let anchor = latest_durable_event(
                    events,
                    crate::attempt::DurableActionKind::ReportCollect,
                    &expectation,
                )
                .context("REPORT collect release 缺锚点事件")?;
                let (anchor_owner, anchor_generation) =
                    durable_anchor_owner_gen(anchor, "ReportCollect release")?;
                if anchor_owner != owner || anchor_generation != generation {
                    bail!("REPORT collect release owner/generation 不等于本 claim");
                }
                Ok(vec![ledger::event(
                    "ReportCollectReleased",
                    "runtime:orch",
                    Some(task_id),
                    Some(round),
                    report_collect_payload(
                        ctx,
                        claimed,
                        &action_id,
                        None,
                        &owner,
                        &generation,
                        None,
                    )?,
                )])
            });
            if let Err(release_error) = release_result {
                return Err(error).context(format!(
                    "REPORT collect failed and owner release also failed: {release_error}"
                ));
            }
            return Err(error);
        }
    };
    let summaries = collect_gate_summaries(root, &outcome)?;
    let executed = ledger::append_checked(root, round, |events| {
        let fresh_ctx = crate::attempt::resolve_current_dispatch(events, task_id, round)?;
        if fresh_ctx.attempt_id != ctx.attempt_id
            || fresh_ctx.attempt_no != ctx.attempt_no
            || fresh_ctx.agent != ctx.agent
            || fresh_ctx.base_sha != ctx.base_sha
            || fresh_ctx.go_path != ctx.go_path
        {
            bail!("REPORT collect dispatch changed before Executed append");
        }
        // fold-first：阶段必须是 Executing 且 lineage 等于本 claim。
        let expectation = report_collect_expectation(
            round,
            task_id,
            ctx,
            claimed,
            &action_id,
            Some(&pinned_branch_sha),
        )?;
        let phase = crate::attempt::fold_durable_action(
            events,
            crate::attempt::DurableActionKind::ReportCollect,
            &expectation,
        )?;
        if phase != crate::attempt::DurableActionPhase::Executing {
            bail!("REPORT collect Executed 前 fold 阶段非法: {phase:?}");
        }
        let anchor = latest_durable_event(
            events,
            crate::attempt::DurableActionKind::ReportCollect,
            &expectation,
        )
        .context("REPORT collect Executed 缺 Executing 锚点")?;
        let (anchor_owner, anchor_generation) =
            durable_anchor_owner_gen(anchor, "ReportCollectExecuting")?;
        if anchor_owner != owner || anchor_generation != generation {
            bail!("REPORT collect Executed owner/generation changed");
        }
        // Dedicated receipt is appended atomically with Executed.  It binds
        // the action lineage, pinned evidence/branch and the complete stable
        // configured gate sequence to the concrete GateExecuted objects.
        let (receipt_event, receipt) = build_collect_gate_receipt(
            root,
            events,
            round,
            task_id,
            card,
            ctx,
            claimed,
            &action_id,
            &branch_sha,
            &owner,
            &generation,
            &anchor.event_id,
            &summaries,
        )?;
        let executed_event = ledger::event(
            "ReportCollectExecuted",
            "runtime:orch",
            Some(task_id),
            Some(round),
            report_collect_payload(
                ctx,
                claimed,
                &action_id,
                Some(&branch_sha),
                &owner,
                &generation,
                Some(&receipt),
            )?,
        );
        Ok(vec![receipt_event, executed_event])
    })?;
    if executed != 2 {
        bail!("ReportCollectExecuted append count mismatch: {executed}");
    }
    hook("after-report-collect-executed")?;
    let completed = ledger::append_checked(root, round, |events| {
        // fold-first：Executed → Completed（幂等 Completed 直接放行），
        // 回执必须在场有效。
        let expectation = report_collect_expectation(
            round,
            task_id,
            ctx,
            claimed,
            &action_id,
            Some(&branch_sha),
        )?;
        let phase = crate::attempt::fold_durable_action(
            events,
            crate::attempt::DurableActionKind::ReportCollect,
            &expectation,
        )?;
        match phase {
            crate::attempt::DurableActionPhase::Executed => {
                let anchor = latest_durable_event(
                    events,
                    crate::attempt::DurableActionKind::ReportCollect,
                    &expectation,
                )
                .context("REPORT collect Completed 缺 Executed 锚点")?;
                let (anchor_owner, anchor_generation) =
                    durable_anchor_owner_gen(anchor, "ReportCollectExecuted")?;
                let anchor_payload = anchor
                    .payload
                    .as_ref()
                    .context("ReportCollectExecuted 缺 payload")?;
                let receipt = receipt_ref_from_payload(anchor_payload, "ReportCollectExecuted")?;
                validate_collect_gate_receipt(
                    root,
                    card,
                    events,
                    &expectation,
                    claimed,
                    anchor,
                    &receipt,
                )?;
                Ok(vec![ledger::event(
                    "ReportCollectCompleted",
                    "runtime:orch",
                    Some(task_id),
                    Some(round),
                    report_collect_payload(
                        ctx,
                        claimed,
                        &action_id,
                        Some(&branch_sha),
                        &anchor_owner,
                        &anchor_generation,
                        Some(&receipt),
                    )?,
                )])
            }
            crate::attempt::DurableActionPhase::Completed => Ok(Vec::new()),
            other => bail!("REPORT collect Completed 前 fold 阶段非法: {other:?}"),
        }
    })?;
    if completed > 1 {
        bail!("ReportCollectCompleted append count mismatch: {completed}");
    }
    // R7：collect 路径消费共同 durable fold——重读账本验证 ReportCollect lineage
    // 及 terminal evidence / pinned branch 绑定。
    {
        let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
        let lr_fresh = read_ledger(&ledger_path)?;
        let expectation = crate::attempt::DurableActionExpectation {
            round: round.to_string(),
            task_id: task_id.to_string(),
            attempt_id: ctx.attempt_id.clone().unwrap_or_default(),
            attempt_no: ctx.attempt_no.unwrap_or_default(),
            agent: ctx.agent.clone().unwrap_or_default(),
            base_sha: ctx.base_sha.clone().unwrap_or_default(),
            go_path: ctx.go_path.clone().unwrap_or_default(),
            action_id: action_id.clone(),
            evidence_sha256: Some(claimed.sha256.clone()),
            evidence_len: Some(claimed.len),
            control_epoch: Some(claimed.control_epoch.clone()),
            branch_sha: Some(branch_sha.clone()),
        };
        let phase = crate::attempt::fold_durable_action(
            &lr_fresh.events,
            crate::attempt::DurableActionKind::ReportCollect,
            &expectation,
        )?;
        if phase != crate::attempt::DurableActionPhase::Completed {
            bail!("REPORT collect final replay fold 未到 Completed: {phase:?}");
        }
        let terminal = latest_durable_event(
            &lr_fresh.events,
            crate::attempt::DurableActionKind::ReportCollect,
            &expectation,
        )
        .context("REPORT collect final replay 缺 Completed terminal")?;
        let terminal_payload = terminal
            .payload
            .as_ref()
            .context("REPORT collect final replay terminal 缺 payload")?;
        let receipt = receipt_ref_from_payload(terminal_payload, "ReportCollectCompleted")?;
        validate_collect_gate_receipt(
            root,
            card,
            &lr_fresh.events,
            &expectation,
            claimed,
            terminal,
            &receipt,
        )?;
    }
    Ok(outcome)
}

/// D5（R-b）陈旧 BLOCKED 作用域守卫：BLOCKED 候选的 mtime 是否严格晚于该 task 最近一次
/// DispatchIssued/NudgeIssued/ResumeIssued 信号。只有严格更新才算本 attempt 的诚实阻塞，
/// 旧文件（来自更早 attempt 且未被清理）不遮蔽新 attempt 的正常轮询。
///
/// 语义：
/// - 无 BLOCKED 文件（`blocked_modified = None`）⇒ false；
/// - 有 BLOCKED、无 marker（`latest_attempt = None`，例如从未派发过信号）⇒ 保守 true
///   （无对照基线时不应错失诚实阻塞）；
/// - 有 marker ⇒ 仅 `blocked_modified > latest_attempt` 为 true；同刻不算新证据（>= 拒绝）。
pub fn blocked_is_current(
    blocked_modified: Option<SystemTime>,
    latest_attempt: Option<SystemTime>,
) -> bool {
    match (blocked_modified, latest_attempt) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some(blocked), Some(marker)) => blocked > marker,
    }
}

/// 双根 BLOCKED current evidence 选择（r42/B87）：按候选顺序返回首个 `blocked_is_current` 的路径。
/// main 旧证据不得遮住 worktree 新证据；REPORT 仍优先于 BLOCKED。
pub fn select_current_blocked_path(
    candidates: &[(PathBuf, Option<SystemTime>)],
    latest_attempt: Option<SystemTime>,
) -> Option<PathBuf> {
    for (path, mtime) in candidates {
        if blocked_is_current(*mtime, latest_attempt) {
            return Some(path.clone());
        }
    }
    None
}

/// 从账本事件流中取该 task 最近一条 `DispatchIssued` / `NudgeIssued` / `ResumeIssued`
/// 信号的可解析时间戳（RFC3339）。忽略其他 task、其他 kind 与不可解析的坏时间戳。
///
/// 多条信号取**事件流顺序**中可解析的最新值（账本为追加语义，最新信号出现在末尾）。
/// 用 `humantime::parse_rfc3339`（与 `ledger::ts_health` 一致，无新增依赖）。解析失败即跳过，
/// 不 panic——账本重读或 metadata 错误必须 fail-closed 且可诊断。
pub fn latest_attempt_signal(events: &[EventRecord], task: &str) -> Option<SystemTime> {
    const CONTROL_KINDS: [&str; 3] = ["DispatchIssued", "NudgeIssued", "ResumeIssued"];
    let mut latest: Option<SystemTime> = None;
    for ev in events {
        if ev.task_id.as_deref() != Some(task) {
            continue;
        }
        if !CONTROL_KINDS.contains(&ev.kind.as_str()) {
            continue;
        }
        if let Ok(t) = humantime::parse_rfc3339(&ev.ts) {
            // 账本为追加顺序，取事件流中可解析时间的最新值：每条候选与当前最大比较，>才替换。
            if latest.map_or(true, |prev| t > prev) {
                latest = Some(t);
            }
        }
    }
    latest
}

/// 判死/判滞时写 POKE-<agent>.txt（render_poke 渲染 pokeHint，O6 mkdir 自愈）。
fn write_poke(root: &Path, agent: &str, round: &str) -> Result<()> {
    let poke_dir = root.join("coordination/runtime");
    fs::create_dir_all(&poke_dir)?;
    let hint = wake::poke_hint_for(root, agent)?.unwrap_or_else(|| {
        format!("r{{round}} 已派发,继续协作:重跑 sh coordination/scripts/wait-dispatch.sh {agent}")
    });
    let poke_text = wake::render_poke(&hint, round);
    let poke_path = poke_dir.join(format!("POKE-{agent}.txt"));
    fs::write(&poke_path, &poke_text)?;
    println!("📋 POKE 已写: {}（{agent} 判死/判滞）", poke_path.display());
    Ok(())
}

/// 该 task 最后一条 DispatchIssued 的 payload（clone）；无则 None。
/// O12（r27/B50）：长命 await-report 进程在 REPORT 收取时刻重读账本，防止持有启动时旧快照。
pub fn latest_dispatch_facts(
    events: &[orch_core::EventRecord],
    task: &str,
) -> Option<serde_json::Value> {
    events
        .iter()
        .rev()
        .find(|e| e.kind == "DispatchIssued" && e.task_id.as_deref() == Some(task))
        .and_then(|e| e.payload.clone())
}

/// 构造执行者诚实阻塞时的固定两条账本事实：先升级给 operator，再改变 task 投影。
pub fn blocked_ledger_events(
    task_id: &str,
    round: &str,
    agent: Option<&str>,
    blocked_rel: &str,
) -> Vec<orch_core::EventRecord> {
    let escalation = ledger::event(
        "EscalationRaised",
        "runtime:orch",
        Some(task_id),
        Some(round),
        serde_json::json!({
            "stage": "executor-blocked",
            "blockedPath": blocked_rel,
            "reason": "执行者提交了本 attempt 的诚实阻塞证据",
            "hint": "处置路径：保留 BLOCKED 证据，由 planner 裁决改卡或重新本地派发",
        }),
    );
    let mut blocked_payload = serde_json::json!({
        "blockedPath": blocked_rel,
    });
    if let Some(agent) = agent {
        blocked_payload
            .as_object_mut()
            .expect("blocked payload is an object")
            .insert(
                "agent".to_string(),
                serde_json::Value::String(agent.to_string()),
            );
    }
    let blocked = ledger::event(
        "AttemptBlocked",
        "runtime:orch",
        Some(task_id),
        Some(round),
        blocked_payload,
    );
    vec![escalation, blocked]
}

/// 同一 attempt 内的阻塞落账幂等判据。
///
/// 只看同 task 的阻塞事实与三个控制信号；从尾部遇到的第一条决定当前 attempt
/// 是否已经记录阻塞。其他事件、其他 task 与时间戳均不参与判定。
pub fn blocked_already_recorded(events: &[orch_core::EventRecord], task_id: &str) -> bool {
    events
        .iter()
        .rev()
        .filter(|event| event.task_id.as_deref() == Some(task_id))
        .find_map(|event| match event.kind.as_str() {
            "AttemptBlocked" => Some(true),
            "DispatchIssued" | "NudgeIssued" | "ResumeIssued" => Some(false),
            _ => None,
        })
        .unwrap_or(false)
}

/// 已被同 task 后续纠正性重派取代的派发事实。
#[derive(Debug, PartialEq)]
pub struct SupersededDispatch {
    pub task_id: String,
    pub dispatch_event_id: String,
    pub agent: Option<String>,
}

/// 纯账本折叠：每个 task 的最后一条 `DispatchIssued` 仍存活，其余均已被取代。
///
/// 返回顺序保持原始账本事件顺序；无 task_id 的派发无法归组，因此忽略。
pub fn superseded_dispatches(events: &[orch_core::EventRecord]) -> Vec<SupersededDispatch> {
    let mut remaining = std::collections::HashMap::<&str, usize>::new();
    for event in events {
        if event.kind == "DispatchIssued" {
            if let Some(task_id) = event.task_id.as_deref() {
                *remaining.entry(task_id).or_default() += 1;
            }
        }
    }

    let mut superseded = Vec::new();
    for event in events {
        if event.kind != "DispatchIssued" {
            continue;
        }
        let Some(task_id) = event.task_id.as_deref() else {
            continue;
        };
        let Some(remaining_for_task) = remaining.get_mut(task_id) else {
            continue;
        };
        *remaining_for_task -= 1;
        if *remaining_for_task == 0 {
            continue;
        }

        superseded.push(SupersededDispatch {
            task_id: task_id.to_owned(),
            dispatch_event_id: event.event_id.clone(),
            agent: event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("agent"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        });
    }
    superseded
}

/// `orch dispatch <task>`：渲染 GO（E5：worktree 命令直接给 SHA）+ DispatchIssued
/// + 唤醒分发（injectable→wake 注入；否则 POKE 备用道）。no_wake=true 跳过注入。
///
/// r44/B97：attempt-aware 派发。先从账本推导当前 attempt（plan_next_attempt 强制
/// takeover 边界——活跃 attempt 拒绝接管），GO/ack 路径按 attemptId 作用域
/// （`GO-<task>-A0001.md`），DispatchIssued payload 带 attemptId。baseSha 永远继承
/// 首个 DispatchIssued 的 concrete SHA，不刷新到推进后的 main。
/// dispatch 闭包返回的三种结果，用闭包外可变槽带回。
/// 只有 Created commit permit + wake；Existing/Repaired 让 permit Drop 且不 wake。
#[derive(Debug)]
pub enum DispatchOutcome {
    /// 新建：写了 GO + 落了 DispatchIssued（+可选 companion）
    Created {
        agent: String,
        base_short: String,
        go_rel: String,
        attempt: crate::attempt::AttemptRef,
        wake_needed: bool,
        expected_append: usize,
    },
    /// 已有 active dispatch 复用：GO 已存在且安全，无需新建
    Existing {
        agent: String,
        go_rel: String,
        attempt: crate::attempt::AttemptRef,
        wake_needed: bool,
    },
    /// 补了缺失的 companion ReassignmentIssued，但 GO/DispatchIssued 已存在
    Repaired {
        agent: String,
        go_rel: String,
        attempt: crate::attempt::AttemptRef,
        wake_needed: bool,
    },
    /// E: 锁内发现 Existing→New 竞态：外层以为 Existing 不取 permit，
    /// 但锁内变 New。需要锁外取 permit 后重试。不在 ledger 锁内取 model-wake 锁。
    RetryNew,
    /// Reassignment rejected before any GO/archive/wake side effect.
    Blocked { reason: String },
}

/// Fail-closed guard for replacing an earlier attempt.
///
/// A timeout classified as stall is not death.  The previous durable provider
/// identity must be positively dead, its exact wake log must not still be
/// advancing, and the process tree must not be known alive.
pub fn reassignment_gate(
    kind_is_dead: bool,
    prev_durable_alive: Option<bool>,
    wake_log_recent_advance: bool,
    process_tree_terminated: Option<bool>,
) -> std::result::Result<(), String> {
    if !kind_is_dead {
        return Err("reassignment blocked: previous attempt is stall/ambiguous, not dead".into());
    }
    if prev_durable_alive != Some(false) {
        return Err("reassignment blocked: durable provider death is not confirmed".into());
    }
    if wake_log_recent_advance {
        return Err("reassignment blocked: previous attempt wake log is still advancing".into());
    }
    if process_tree_terminated == Some(false) {
        return Err("reassignment blocked: previous attempt process tree is still alive".into());
    }
    Ok(())
}

/// Whether a terminal attempt should use the probe-and-archive successor path.
///
/// These terminal states are honest non-death outcomes. They cannot satisfy
/// the confirmed-dead reassignment arm, but their successors still need a
/// fresh durable-session/log probe and must preserve dirty WIP.
pub fn successor_archive_applies(previous_terminal_kind: &str) -> bool {
    matches!(
        previous_terminal_kind,
        "AttemptBlocked" | "AttemptTimedOut" | "AttemptFailed"
    )
}

/// Whether one terminal event belongs on the probe-and-archive successor path.
///
/// The legacy kind-only contract stays unchanged. `VerdictIssued` is
/// payload-sensitive and shares the exact FAIL/BLOCKED classifier used by the
/// attempt takeover planner.
pub fn successor_archive_event_applies(
    previous_terminal_kind: &str,
    payload: Option<&serde_json::Value>,
) -> bool {
    successor_archive_applies(previous_terminal_kind)
        || (previous_terminal_kind == "VerdictIssued"
            && crate::attempt::verdict_is_takeover_terminal(payload))
}



#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockedSuccessorArchive {
    SkipClean,
    ArchiveWip,
}

/// Decide whether an honest `AttemptBlocked` successor needs a WIP archive.
///
/// A fresh probe is sufficient evidence for this path: unlike a crashed/dead
/// reassignment, a blocked attempt does not require termination history when
/// its worktree is clean.  Any current activity still fails closed.
pub fn blocked_successor_archive_plan(
    worktree_clean: bool,
    durable_alive: Option<bool>,
    wake_log_recent_advance: bool,
) -> std::result::Result<BlockedSuccessorArchive, String> {
    if wake_log_recent_advance {
        return Err(
            "blocked successor refused: previous attempt wake log is still advancing".into(),
        );
    }
    if durable_alive == Some(true) {
        return Err("blocked successor refused: previous durable session is still alive".into());
    }
    if worktree_clean {
        Ok(BlockedSuccessorArchive::SkipClean)
    } else {
        Ok(BlockedSuccessorArchive::ArchiveWip)
    }
}

/// Fail-closed guard immediately before archiving a superseded attempt's WIP.
///
/// Managed process groups require positive termination evidence.  A socket
/// proxy has no honestly observable process tree, so it may use `None`, but
/// only after its exact wake log has quiesced.  A known-live tree is illegal
/// for both topologies.
pub fn wip_archive_gate(
    plan_is_log_quiesce: bool,
    process_tree_terminated: Option<bool>,
    wake_log_recent_advance: bool,
) -> std::result::Result<(), String> {
    if wake_log_recent_advance {
        return Err("WIP archive blocked: previous attempt wake log is still advancing".into());
    }
    if process_tree_terminated == Some(false) {
        return Err("WIP archive blocked: previous attempt process tree is still alive".into());
    }
    if !plan_is_log_quiesce && process_tree_terminated != Some(true) {
        return Err(
            "WIP archive blocked: managed process-tree termination is not confirmed".into(),
        );
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct PreparedReassignmentTermination {
    previous_attempt_id: String,
    previous_agent: String,
    plan: Option<wake::TerminationPlan>,
    process_tree_terminated: Option<bool>,
    pgid: Option<u32>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct PreparedBlockedSuccessorArchive {
    previous_attempt_id: String,
    previous_agent: String,
    decision: Option<BlockedSuccessorArchive>,
    plan_is_log_quiesce: bool,
    process_tree_terminated: Option<bool>,
    wake_log_recent_advance: bool,
    error: Option<String>,
}

fn termination_grace_seconds(ir: &crate::plan::RoundIr) -> u64 {
    ir.liveness.termination_grace_seconds.unwrap_or(10)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReassignmentEvidence {
    pub kind_is_dead: bool,
    pub durable_alive: Option<bool>,
    pub wake_log_recent_advance: bool,
    pub process_tree_terminated: Option<bool>,
}

fn event_belongs_to_attempt(event: &EventRecord, attempt_id: &str) -> bool {
    event
        .payload
        .as_ref()
        .and_then(|payload| payload.get("attemptId"))
        .and_then(serde_json::Value::as_str)
        == Some(attempt_id)
}

fn terminal_attempt_uses_successor_archive(
    events: &[EventRecord],
    task_id: &str,
    attempt_id: &str,
) -> bool {
    events
        .iter()
        .rev()
        .filter(|event| event.task_id.as_deref() == Some(task_id))
        .find(|event| {
            matches!(
                event.kind.as_str(),
                "AttemptBlocked"
                    | "AttemptTimedOut"
                    | "AttemptCrashed"
                    | "AttemptFailed"
                    | "VerdictIssued"
                    | "DispatchIssued"
                    | "ReportObserved"
            )
        })
        .is_some_and(|event| {
            successor_archive_event_applies(&event.kind, event.payload.as_ref())
                && (event_belongs_to_attempt(event, attempt_id)
                    || event
                        .payload
                        .as_ref()
                        .is_none_or(|payload| payload.get("attemptId").is_none()))
        })
}

fn terminal_event_confirms_dead(event: &EventRecord) -> Option<bool> {
    match event.kind.as_str() {
        "AttemptCrashed" => Some(true),
        "AttemptTimedOut" => {
            let payload = event.payload.as_ref();
            Some(
                payload
                    .and_then(|value| value.get("kind"))
                    .and_then(serde_json::Value::as_str)
                    == Some("stall")
                    && payload
                        .and_then(|value| value.get("stage"))
                        .and_then(serde_json::Value::as_str)
                        == Some("stall-budget-exceeded")
                    && payload
                        .and_then(|value| value.get("processTreeTerminated"))
                        .and_then(serde_json::Value::as_bool)
                        == Some(true),
            )
        }
        "AttemptBlocked" | "AttemptFailed" => Some(false),
        _ => None,
    }
}

pub fn reassignment_evidence_for_attempt(
    root: &Path,
    events: &[EventRecord],
    task_id: &str,
    previous_agent: &str,
    previous_attempt_id: &str,
    round: &str,
) -> Result<ReassignmentEvidence> {
    let kind_is_dead = events
        .iter()
        .rev()
        .filter(|event| {
            event.task_id.as_deref() == Some(task_id)
                && event_belongs_to_attempt(event, previous_attempt_id)
        })
        .find_map(|event| {
            terminal_event_confirms_dead(event).or_else(|| match event.kind.as_str() {
                "EscalationRaised"
                    if event
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("stage"))
                        .and_then(serde_json::Value::as_str)
                        == Some("liveness-monitor") =>
                {
                    Some(
                        event
                            .payload
                            .as_ref()
                            .and_then(|payload| payload.get("verdict"))
                            .and_then(serde_json::Value::as_str)
                            == Some("dead"),
                    )
                }
                _ => None,
            })
        })
        .unwrap_or(false);

    let mut sample = None;
    let probe = durable_probe_for_attempt(
        root,
        previous_agent,
        events,
        previous_attempt_id,
        &mut sample,
    )?;
    let wake_log_recent_advance = current_attempt_channel_facts(events, previous_attempt_id)
        .is_some_and(|facts| {
            let candidate = PathBuf::from(&facts.log_path);
            let path = if candidate.is_absolute() {
                candidate
            } else {
                root.join(candidate)
            };
            let Ok(metadata) = fs::metadata(path) else {
                return false;
            };
            let monitor_window = crate::plan::require_active_round_ir(root, round, events)
                .map(|active| active.candidate.liveness.monitor_seconds.saturating_mul(2))
                .unwrap_or(30)
                .max(1);
            metadata.len() > facts.probe_end
                && metadata
                    .modified()
                    .ok()
                    .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                    .is_some_and(|age| age <= Duration::from_secs(monitor_window))
        });
    Ok(ReassignmentEvidence {
        kind_is_dead,
        durable_alive: probe.durable_alive,
        wake_log_recent_advance: wake_log_recent_advance || probe.wake_log_advanced,
        process_tree_terminated: probe.process_tree_terminated,
    })
}

fn require_active_tierf_task_from_events(
    root: &Path,
    round: &str,
    task_id: &str,
    events: &[EventRecord],
) -> Result<card::Card> {
    let active = crate::plan::require_active_round_ir(root, round, events)?;
    if !active.candidate.tasks.iter().any(|task| task.id == task_id) {
        bail!("task {task_id} 不在 active ROUND-IR");
    }
    crate::plan::load_bound_task_card(root, round, task_id, &active)
}

fn require_active_tierf_task(root: &Path, round: &str, task_id: &str) -> Result<card::Card> {
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let lr = read_ledger(&ledger_path)?;
    crate::attempt::reject_bad_lines(&lr)?;
    require_active_tierf_task_from_events(root, round, task_id, &lr.events)
}

fn require_active_tierf_round(
    root: &Path,
    round: &str,
) -> Result<crate::plan::ReadonlyIrValidation> {
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let lr = read_ledger(&ledger_path)?;
    crate::attempt::reject_bad_lines(&lr)?;
    crate::plan::require_active_round_ir(root, round, &lr.events)
}

fn require_runtime_dispatch_admission(
    root: &Path,
    round: &str,
    task_id: &str,
    attempt_base_sha: &str,
    events: &[EventRecord],
    agent: &str,
    successor_release: Option<(&str, &str)>,
) -> Result<()> {
    let active = crate::plan::require_active_round_ir(root, round, events)?;
    let task = active
        .candidate
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .with_context(|| format!("task {task_id} 不在 active ROUND-IR"))?;
    let projection = orch_core::fold(events);
    let mut recorded_with_merge = Vec::new();
    let mut recorded_without_merge = BTreeSet::new();
    for dependency in &task.depends_on {
        let Some(projected) = projection.tasks.get(dependency) else {
            continue;
        };
        if projected.state != Some(orch_core::TaskState::Recorded) {
            continue;
        }
        if let Some(merge_sha) = &projected.merge_sha {
            recorded_with_merge.push((dependency.clone(), merge_sha.clone()));
        } else {
            // Historical ledgers and a few runtime fixtures predate MergeExecuted.
            // They still prove Recorded admission, but cannot truthfully support a
            // stale-baseline diagnosis. Preserve their former recorded-only
            // behavior while requiring ancestry for every canonical merge SHA.
            recorded_without_merge.insert(dependency.clone());
            recorded_with_merge.push((dependency.clone(), String::new()));
        }
    }

    let preliminary = crate::plan::dependency_dispatch_admission(
        &task.depends_on,
        &recorded_with_merge,
        Some(attempt_base_sha),
        &|_| true,
    );
    if let crate::plan::DependencyAdmission::BlockedByDependency { blockers } = preliminary {
        bail!(
            "dependency dispatch admission: BlockedByDependency task={task_id} blockers=[{}]",
            blockers.join(", ")
        );
    }

    let mut contained_merges = BTreeSet::new();
    for (dependency, merge_sha) in &recorded_with_merge {
        if recorded_without_merge.contains(dependency) {
            contained_merges.insert(merge_sha.clone());
            continue;
        }
        if gitx::is_ancestor(root, merge_sha, attempt_base_sha).with_context(|| {
            format!(
                "检查 dependency {dependency} merge {merge_sha} 是否在 attempt base {attempt_base_sha} 中失败"
            )
        })? {
            contained_merges.insert(merge_sha.clone());
        }
    }
    match crate::plan::dependency_dispatch_admission(
        &task.depends_on,
        &recorded_with_merge,
        Some(attempt_base_sha),
        &|merge_sha| contained_merges.contains(merge_sha),
    ) {
        crate::plan::DependencyAdmission::Admitted => {}
        crate::plan::DependencyAdmission::BlockedByDependency { blockers } => {
            bail!(
                "dependency dispatch admission: BlockedByDependency task={task_id} blockers=[{}]",
                blockers.join(", ")
            );
        }
        crate::plan::DependencyAdmission::ForwardBaselineRequired {
            dependency,
            merge_sha,
            attempt_base_sha,
        } => {
            bail!(
                "dependency dispatch admission: ForwardBaselineRequired task={task_id} dependency={dependency} merge_sha={merge_sha} attempt_base_sha={attempt_base_sha}"
            );
        }
    }

    if active.candidate.schema_version == crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION {
        if agent != "local" || successor_release.is_some() {
            bail!("schema 3 local dispatch 只接受 agent=local 且不消费 legacy capacity release");
        }
        return Ok(());
    }

    let projected;
    let admission_events = if let Some((task_id, attempt_id)) = successor_release {
        projected = {
            let mut projected = events.to_vec();
            projected.push(ledger::event(
                "AttemptFailed",
                "runtime:orch:capacity-projection",
                Some(task_id),
                Some(round),
                serde_json::json!({"attemptId": attempt_id}),
            ));
            projected
        };
        projected.as_slice()
    } else {
        events
    };
    crate::legacy::scheduling_admits(
        admission_events,
        round,
        &active.candidate.scheduling,
        agent,
        "implement",
    )
    .map_err(anyhow::Error::msg)
}

#[derive(Debug)]
struct CriticalPointDrift(String);

impl std::fmt::Display for CriticalPointDrift {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "active contract drift at critical point: {}",
            self.0
        )
    }
}

impl std::error::Error for CriticalPointDrift {}

fn critical_point<T>(result: Result<T>) -> Result<T> {
    result.map_err(|error| anyhow::Error::new(CriticalPointDrift(format!("{error:#}"))))
}

fn is_critical_point_drift(error: &anyhow::Error) -> bool {
    error.downcast_ref::<CriticalPointDrift>().is_some()
}

fn with_active_tierf_effect<T>(
    root: &Path,
    round: &str,
    task_id: &str,
    operation: &str,
    action: impl FnOnce(&card::Card) -> Result<T>,
) -> Result<T> {
    crate::close::with_protocol_effect(root, operation, || {
        let rebound = critical_point(require_active_tierf_task(root, round, task_id))?;
        action(&rebound)
    })
}

/// Result of one schema-3 local dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDispatchOutcome {
    /// Full attempt identity allocated by the durable ledger.
    pub attempt_id: String,
    /// Full immutable main commit used as the implementation base.
    pub base_sha: String,
    /// Repository-relative worktree leased to the local planner.
    pub worktree_rel: String,
    /// Repository-relative GO artifact bound by `DispatchIssued`.
    pub go_rel: String,
    /// True when the exact current local dispatch already existed.
    pub replayed: bool,
}

fn render_local_attempt_go(
    task_id: &str,
    round: &str,
    base_sha: &str,
    worktree_rel: &str,
    report_rel: &str,
    harness: Option<&str>,
) -> Vec<u8> {
    let execution = match harness {
        Some(alias) => format!(
            "Harness alias `{alias}` implements this task through the unified channel in the already-provisioned worktree."
        ),
        None => "The current root planner implements this task locally in the already-provisioned worktree.".to_string(),
    };
    format!(
        "# GO {task_id} — schema 3 local implementation\n\n\
         Base: `{base_sha}`.\n\
         Worktree: `{worktree_rel}` (already provisioned by orch).\n\
         Card: `coordination/rounds/{round}/tasks/{task_id}.md`.\n\n\
         {execution} Relocate the seed byte-for-byte, \
         capture expected red, turn green, run negative mutations, and write `{report_rel}` as the \
         final action. No provider wake, POKE, heartbeat, or Tier-F wait loop applies.\n"
    )
    .into_bytes()
}

fn validate_local_worktree_repository(root: &Path, worktree: &Path) -> Result<()> {
    let gitfile = worktree.join(".git");
    let metadata = fs::symlink_metadata(&gitfile)
        .with_context(|| format!("local worktree 缺 .git gitfile: {}", gitfile.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("local worktree .git 必须是 regular non-symlink gitfile");
    }
    if gitx::canonical_worktree_common_dir(root)? != gitx::canonical_worktree_common_dir(worktree)?
    {
        bail!("local worktree 属于不同 git common-dir");
    }
    Ok(())
}

/// Dispatch a schema-3 task to the current root planner without a model wake.
pub fn run_dispatch_local(root: &Path, task_id: &str) -> Result<LocalDispatchOutcome> {
    run_dispatch_v3(root, task_id, None)
}

/// Dispatch a schema-3 task through one configured channel alias.
///
/// The task lifecycle owner remains `local`; the alias is recorded only as an
/// invocation address and never becomes a scheduling/capacity entity.
pub fn run_dispatch_harness(
    root: &Path,
    task_id: &str,
    harness: &str,
) -> Result<LocalDispatchOutcome> {
    let outcome = run_dispatch_v3(root, task_id, Some(harness))?;
    crate::wake::run_channel_execute_v3(root, task_id, harness)?;
    Ok(outcome)
}

fn run_dispatch_v3(
    root: &Path,
    task_id: &str,
    harness: Option<&str>,
) -> Result<LocalDispatchOutcome> {
    card::validate_task_id(task_id)?;
    let round = current_round(root)?;
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let initial = read_ledger(&ledger_path)?;
    crate::attempt::reject_bad_lines(&initial)?;
    let active = crate::plan::require_active_round_ir(root, &round, &initial.events)?;
    if active.candidate.schema_version != crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION {
        bail!("dispatch --local 只允许 schema 3 round");
    }
    if !active.candidate.tasks.iter().any(|task| task.id == task_id) {
        bail!("task {task_id} 不在 active ROUND-IR");
    }
    crate::plan::load_bound_task_card(root, &round, task_id, &active)?;
    let concrete_main = gitx::rev_parse(root, "main^{commit}")?;
    crate::close::with_protocol_effect(root, "schema 3 local dispatch", || {
        let _storage_permit = crate::storage::guard_operation(
            root,
            &round,
            Some(task_id),
            None,
            crate::storage::GuardEntry::Dispatch,
            &[
                root.join(".worktrees"),
                root.join("orch/target"),
                root.join(format!("coordination/rounds/{round}/dispatch")),
            ],
        )?;
        let mut outcome = None;
        ledger::append_checked(root, &round, |events| {
            if current_round(root)? != round {
                bail!("local dispatch 锁内 round 漂移");
            }
            let active = crate::plan::require_active_round_ir(root, &round, events)?;
            if active.candidate.schema_version != crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION
                || !active.candidate.tasks.iter().any(|task| task.id == task_id)
            {
                bail!("local dispatch 锁内 active schema/task 漂移");
            }
            crate::plan::load_bound_task_card(root, &round, task_id, &active)?;
            let fresh_main = gitx::rev_parse(root, "main^{commit}")?;
            if fresh_main != concrete_main {
                bail!("local dispatch 锁内 main 漂移: {concrete_main} -> {fresh_main}");
            }
            let plan = crate::attempt::plan_dispatch_locked(
                events,
                task_id,
                "local",
                &concrete_main,
                &round,
            )?;
            match plan {
                crate::attempt::DispatchPlan::Existing { current, .. } => {
                    if current.agent != "local" || current.wake_pending {
                        bail!("current dispatch 不是 canonical local no-wake attempt");
                    }
                    let dispatch_event = events
                        .iter()
                        .rev()
                        .find(|event| {
                            event.kind == "DispatchIssued"
                                && event.task_id.as_deref() == Some(task_id)
                                && event
                                    .payload
                                    .as_ref()
                                    .and_then(|payload| payload.get("attemptId"))
                                    .and_then(serde_json::Value::as_str)
                                    == Some(current.attempt.attempt_id.as_str())
                        })
                        .context("existing schema 3 dispatch event 消失")?;
                    let expected_method = if harness.is_some() {
                        "harness-channel"
                    } else {
                        "local-worktree"
                    };
                    if dispatch_event
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("method"))
                        .and_then(serde_json::Value::as_str)
                        != Some(expected_method)
                        || dispatch_event
                            .payload
                            .as_ref()
                            .and_then(|payload| payload.get("harness"))
                            .and_then(serde_json::Value::as_str)
                            != harness
                    {
                        bail!("existing schema 3 dispatch route 与请求 route 漂移");
                    }
                    let worktree_rel = format!(".worktrees/{task_id}");
                    let worktree = root.join(&worktree_rel);
                    crate::attempt::resolve_go_path_strict(
                        root,
                        &current.go_path,
                        &round,
                        "local",
                        &current.attempt,
                        current.is_legacy,
                    )?;
                    let base_sha = current
                        .base_sha
                        .clone()
                        .context("local DispatchIssued 缺 baseSha")?;
                    validate_local_worktree_repository(root, &worktree)?;
                    if !worktree.is_dir()
                        || gitx::current_branch(&worktree)?.as_deref()
                            != Some(format!("task/{task_id}").as_str())
                    {
                        bail!("existing local dispatch worktree/branch 漂移");
                    }
                    let expected_go = render_local_attempt_go(
                        task_id,
                        &round,
                        &base_sha,
                        &worktree_rel,
                        &format!("coordination/rounds/{round}/reports/{task_id}-REPORT.md"),
                        harness,
                    );
                    crate::attempt::reconcile_or_create_go(
                        &root.join(&current.go_path),
                        &expected_go,
                    )?;
                    outcome = Some(LocalDispatchOutcome {
                        attempt_id: current.attempt.attempt_id,
                        base_sha,
                        worktree_rel,
                        go_rel: current.go_path,
                        replayed: true,
                    });
                    Ok(Vec::new())
                }
                crate::attempt::DispatchPlan::New {
                    next,
                    previous_dispatch,
                } => {
                    let successor = match (&next.previous_attempt, &previous_dispatch) {
                        (None, None) if !next.is_reassignment => false,
                        (Some(previous_attempt), Some(previous))
                            if !next.is_reassignment && previous.agent == "local" =>
                        {
                            if previous.attempt.attempt_id != previous_attempt.attempt_id
                                || !events.iter().any(|event| {
                                    crate::attempt::event_terminates_attempt(
                                        event,
                                        &previous_attempt.attempt_id,
                                    )
                                })
                            {
                                bail!("schema 3 local successor 缺 exact terminal predecessor");
                            }
                            true
                        }
                        _ => bail!("schema 3 local dispatch 不接受 provider reassignment"),
                    };
                    require_runtime_dispatch_admission(
                        root,
                        &round,
                        task_id,
                        &next.base_sha,
                        events,
                        "local",
                        None,
                    )?;
                    let worktree = root.join(&next.worktree_rel);
                    let branch_exists = gitx::branch_exists(root, &next.branch);
                    let worktree_exists = worktree.is_dir();
                    match (branch_exists, worktree_exists) {
                        (false, false) => {
                            gitx::worktree_add(root, &worktree, &next.branch, &next.base_sha)?;
                        }
                        (true, true) => {}
                        _ => bail!("local dispatch branch/worktree 只存在一侧，拒绝修补"),
                    }
                    validate_local_worktree_repository(root, &worktree)?;
                    if gitx::current_branch(&worktree)?.as_deref() != Some(next.branch.as_str()) {
                        bail!("local dispatch worktree 未固定到预期 branch/base");
                    }
                    if successor {
                        if !gitx::porcelain_v2(&worktree)?.trim().is_empty() {
                            bail!("schema 3 local successor 要求 clean worktree");
                        }
                        let previous = previous_dispatch
                            .as_ref()
                            .expect("successor has previous dispatch");
                        let previous_go =
                            crate::attempt::resolve_superseded_go(root, &round, previous)?;
                        crate::attempt::archive_preflight(
                            root,
                            &previous_go,
                            &previous.attempt.attempt_id,
                            "local",
                        )?;
                        crate::attempt::archive_superseded_dispatch(
                            root,
                            &previous_go,
                            &previous.attempt.attempt_id,
                            "local",
                        )?;
                    } else if gitx::rev_parse(&worktree, "HEAD^{commit}")? != next.base_sha {
                        bail!("local dispatch worktree 未固定到预期 base");
                    }
                    let paths = crate::attempt::attempt_paths(&round, "local", &next.attempt);
                    let go_path = root.join(&paths.go_rel);
                    let go_bytes = render_local_attempt_go(
                        task_id,
                        &round,
                        &next.base_sha,
                        &next.worktree_rel,
                        &paths.report_rel,
                        harness,
                    );
                    crate::attempt::reconcile_or_create_go(&go_path, &go_bytes)?;
                    let identity =
                        crate::sites::site_identity(task_id, IMPLEMENT_DISPATCH_SITE_ROLE, "local");
                    let generation = crate::sites::SiteIdentity::next_generation(events, &identity);
                    if generation == 0 {
                        bail!("local implement site generation overflow");
                    }
                    outcome = Some(LocalDispatchOutcome {
                        attempt_id: next.attempt.attempt_id.clone(),
                        base_sha: next.base_sha.clone(),
                        worktree_rel: next.worktree_rel.clone(),
                        go_rel: paths.go_rel.clone(),
                        replayed: false,
                    });
                    let mut dispatch_payload = serde_json::json!({
                        "agent": "local",
                        "method": if harness.is_some() { "harness-channel" } else { "local-worktree" },
                        "baseSha": next.base_sha,
                        "goPath": paths.go_rel,
                        "attemptId": next.attempt.attempt_id,
                        "attemptNo": next.attempt.ordinal,
                        "reassignment": false,
                        "overrideAmbiguousActive": false,
                        "wakePending": false,
                    });
                    if let Some(alias) = harness {
                        dispatch_payload["harness"] = serde_json::json!(alias);
                    }
                    if let Some(previous) = previous_dispatch.as_ref() {
                        dispatch_payload["previousAttemptId"] =
                            serde_json::json!(previous.attempt.attempt_id);
                        dispatch_payload["previousAgent"] = serde_json::json!("local");
                    }
                    Ok(vec![
                        ledger::event(
                            "DispatchIssued",
                            "runtime:orch",
                            Some(task_id),
                            Some(&round),
                            dispatch_payload,
                        ),
                        ledger::event(
                            "WorkspaceLeased",
                            "runtime:orch",
                            Some(task_id),
                            Some(&round),
                            serde_json::json!({
                                "siteId": identity.site_id_for(generation),
                                "generation": generation,
                                "attemptId": next.attempt.attempt_id,
                                "role": crate::sites::SiteRole::Implement.as_str(),
                                "agent": "local",
                                "reviewedHead": next.base_sha,
                                "paths": {
                                    "worktree": next.worktree_rel,
                                    "target": format!("{}/orch/target", next.worktree_rel),
                                },
                            }),
                        ),
                    ])
                }
            }
        })?;
        outcome.context("local dispatch 未生成 outcome")
    })
}

/// Read the current open generation before any legacy-shaped dispatch effect.
/// Refusal is deliberately not recorded as ActionRejected: legacy ledgers and
/// their runtime/GO/storage/worktree state must remain byte-for-byte untouched.
pub(crate) fn require_current_open_schema3_dispatch_generation(
    root: &Path,
    expected_round: Option<&str>,
) -> Result<String> {
    let round = current_round(root)?;
    if expected_round.is_some_and(|expected| expected != round) {
        bail!("dispatch current round changed before effect");
    }
    let read = read_ledger(&root.join(format!("coordination/rounds/{round}/events.jsonl")))?;
    crate::attempt::reject_bad_lines(&read)?;
    if crate::round::contract_schema_from_events(&read.events, &round)?
        != Some(crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION)
        || orch_core::fold(&read.events).round_closed
    {
        bail!("schema 1/2 或 closed round dispatch writer 已退役；仅 open schema 3 explicit local/harness dispatch 可写");
    }
    Ok(round)
}

/// Compatibility dispatch entry; historical generations refuse before effects.
/// Current callers should use the explicit local/harness route APIs.
pub fn run_dispatch(root: &Path, task_id: &str, no_wake: bool) -> Result<(String, String)> {
    run_dispatch_with_override(root, task_id, no_wake, false)
}

/// Legacy-shaped override entry, subject to the same pre-effect generation fence.
pub fn run_dispatch_with_override(
    root: &Path,
    task_id: &str,
    no_wake: bool,
    override_ambiguous_active: bool,
) -> Result<(String, String)> {
    run_dispatch_with_hook_and_override(
        root,
        task_id,
        no_wake,
        override_ambiguous_active,
        |_was_existing, _iteration| Ok(()),
    )
}

/// Deterministic race seam used by integration tests.  The hook runs outside
/// both the model-wake and ledger locks, after readonly admission. Historical
/// generations refuse before the hook or any permit can be requested.
#[doc(hidden)]
pub fn run_dispatch_with_hook<F>(
    root: &Path,
    task_id: &str,
    no_wake: bool,
    after_readonly: F,
) -> Result<(String, String)>
where
    F: FnMut(bool, usize) -> Result<()>,
{
    run_dispatch_with_hook_and_override(root, task_id, no_wake, false, after_readonly)
}

/// Compatibility race/override seam; legacy rejection never acquires an effect lease.
#[doc(hidden)]
pub fn run_dispatch_with_hook_and_override<F>(
    root: &Path,
    task_id: &str,
    no_wake: bool,
    override_ambiguous_active: bool,
    mut after_readonly: F,
) -> Result<(String, String)>
where
    F: FnMut(bool, usize) -> Result<()>,
{
    let round = require_current_open_schema3_dispatch_generation(root, None)?;
    if let Err(error) = require_active_tierf_task(root, &round, task_id) {
        return crate::failure::reject_action_from_ledger_disposition(
            root,
            &round,
            Some(task_id),
            "dispatch",
            &format!("dispatch:{round}:{task_id}"),
            &format!("{error:#}"),
            crate::failure::CliDisposition::Rejected,
        );
    }
    let result = crate::close::with_protocol_effect(root, "tierf dispatch", || {
        critical_point(require_current_open_schema3_dispatch_generation(root, Some(&round)))?;
        critical_point(require_active_tierf_task(root, &round, task_id))?;
        run_dispatch_with_hook_inner(
            root,
            task_id,
            no_wake,
            override_ambiguous_active,
            &round,
            &mut after_readonly,
        )
    });
    match result {
        Ok(outcome) => Ok(outcome),
        Err(error)
            if crate::failure::is_action_rejection(&error) || is_critical_point_drift(&error) =>
        {
            Err(error)
        }
        Err(error) => crate::failure::reject_action_from_ledger_disposition(
            root,
            &round,
            Some(task_id),
            "dispatch",
            &format!("dispatch:{round}:{task_id}"),
            &format!("{error:#}"),
            crate::failure::CliDisposition::Rejected,
        ),
    }
}

fn run_dispatch_with_hook_inner<F>(
    root: &Path,
    task_id: &str,
    no_wake: bool,
    override_ambiguous_active: bool,
    round: &str,
    after_readonly: &mut F,
) -> Result<(String, String)>
where
    F: FnMut(bool, usize) -> Result<()>,
{
    // Storage admission is the first effectful dispatch operation. Keep the permit through GO
    // creation and wake spawn so concurrent dispatches cannot race the same observed free space.
    let _storage_permit = crate::storage::guard_operation(
        root,
        round,
        Some(task_id),
        None,
        crate::storage::GuardEntry::Dispatch,
        &[
            root.join(".worktrees"),
            root.join("orch/target"),
            root.join(format!("coordination/rounds/{round}/dispatch")),
        ],
    )?;
    crate::channel::with_capacity_lock(root, || {
        run_dispatch_with_hook_inner_locked(
            root,
            task_id,
            no_wake,
            override_ambiguous_active,
            round,
            after_readonly,
        )
    })
}

fn run_dispatch_with_hook_inner_locked<F>(
    root: &Path,
    task_id: &str,
    no_wake: bool,
    override_ambiguous_active: bool,
    round: &str,
    after_readonly: &mut F,
) -> Result<(String, String)>
where
    F: FnMut(bool, usize) -> Result<()>,
{
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let c = card::load(root, &round, task_id)?;
    let card_agent = c
        .meta
        .agent
        .clone()
        .context("任务卡缺 agent（Tier F 派发需要）")?;
    let agent = card_agent.clone();
    let concrete_main = gitx::rev_parse(root, "main")?;

    const MAX_REPLANS: usize = 8;
    for iteration in 0..MAX_REPLANS {
        // Every replan starts with a fresh, bad-line-checked ledger read.
        let lr = read_ledger(&ledger_path)?;
        crate::attempt::reject_bad_lines(&lr)?;
        let readonly_plan = crate::attempt::plan_dispatch_locked(
            &lr.events,
            task_id,
            &agent,
            &concrete_main,
            &round,
        )?;
        let successor_capacity_release = match &readonly_plan {
            crate::attempt::DispatchPlan::New {
                next,
                previous_dispatch: Some(previous),
            } if !next.is_reassignment
                && terminal_attempt_uses_successor_archive(
                    &lr.events,
                    task_id,
                    &previous.attempt.attempt_id,
                ) =>
            {
                Some((task_id, previous.attempt.attempt_id.as_str()))
            }
            _ => None,
        };
        let mut is_existing = matches!(
            &readonly_plan,
            crate::attempt::DispatchPlan::Existing { .. }
        );
        if let crate::attempt::DispatchPlan::New { next, .. } = &readonly_plan {
            require_runtime_dispatch_admission(
                root,
                round,
                task_id,
                &next.base_sha,
                &lr.events,
                &agent,
                successor_capacity_release,
            )?;
        }
        after_readonly(is_existing, iteration)?;
        // The deterministic hook may invoke `orch plan` (which must fail the
        // shared→exclusive upgrade) or directly mutate mode/card bytes.  Rebind
        // the signed active task before budget reservation or any GO/archive
        // side effect.
        let rebound = critical_point(require_active_tierf_task(root, round, task_id))?;
        if rebound.meta.agent.as_deref() != Some(agent.as_str()) {
            bail!("dispatch active task agent 漂移");
        }

        let mut wake_permit = if is_existing {
            None
        } else {
            match crate::budget::check_before_model_wake(root, &round) {
                Ok(permit) => Some(permit),
                Err(original_budget_error) => {
                    // A readonly New plan can race with another caller that
                    // publishes the same dispatch before this caller obtains a
                    // permit.  Fresh-replan once: Existing is free and must
                    // proceed; a still-New plan returns the original budget
                    // error without entering the ledger lock.
                    let fresh = read_ledger(&ledger_path)?;
                    crate::attempt::reject_bad_lines(&fresh)?;
                    let fresh_plan = crate::attempt::plan_dispatch_locked(
                        &fresh.events,
                        task_id,
                        &agent,
                        &concrete_main,
                        &round,
                    )?;
                    if matches!(fresh_plan, crate::attempt::DispatchPlan::Existing { .. }) {
                        is_existing = true;
                        None
                    } else {
                        return Err(original_budget_error);
                    }
                }
            }
        };

        // H17: process termination may consume the full configured grace.
        // Capture and execute it before entering ledger::append_checked; the
        // closure below only performs a kill -0 recheck and commits verified
        // evidence.
        let pretermination = {
            let fresh = read_ledger(&ledger_path)?;
            crate::attempt::reject_bad_lines(&fresh)?;
            let fresh_plan = crate::attempt::plan_dispatch_locked(
                &fresh.events,
                task_id,
                &agent,
                &concrete_main,
                round,
            )?;
            match fresh_plan {
                crate::attempt::DispatchPlan::New {
                    next,
                    previous_dispatch,
                } if next.is_reassignment => {
                    let previous =
                        previous_dispatch.context("reassignment 缺 previous dispatch")?;
                    let evidence = reassignment_evidence_for_attempt(
                        root,
                        &fresh.events,
                        task_id,
                        &previous.agent,
                        &previous.attempt.attempt_id,
                        round,
                    )?;
                    let initially_allowed = override_ambiguous_active
                        || reassignment_gate(
                            evidence.kind_is_dead,
                            evidence.durable_alive,
                            evidence.wake_log_recent_advance,
                            evidence.process_tree_terminated,
                        )
                        .is_ok();
                    if !initially_allowed {
                        None
                    } else {
                        let prepared = (|| -> Result<PreparedReassignmentTermination> {
                            let registry = wake::load_registry(root)?;
                            let spec = registry.get(&previous.agent).with_context(|| {
                                format!("termination missing registered agent: {}", previous.agent)
                            })?;
                            let plan =
                                wake::termination_plan(&spec.argv).map_err(anyhow::Error::msg)?;
                            let (process_tree_terminated, pgid) = match plan {
                                wake::TerminationPlan::SignalProcessGroup => {
                                    let facts = current_attempt_channel_facts(
                                        &fresh.events,
                                        &previous.attempt.attempt_id,
                                    )
                                    .context(
                                        "managed termination missing exact attempt channel facts",
                                    )?;
                                    let terminated = if evidence.kind_is_dead
                                        && evidence.durable_alive == Some(false)
                                        && !evidence.wake_log_recent_advance
                                    {
                                        let active = require_active_tierf_round(root, round)?;
                                        let grace = Duration::from_secs(termination_grace_seconds(
                                            &active.candidate,
                                        ));
                                        Some(
                                            terminate_managed_process_group(facts.pid, grace)?
                                                .process_tree_terminated,
                                        )
                                    } else {
                                        evidence.process_tree_terminated
                                    };
                                    (terminated, Some(facts.pid))
                                }
                                wake::TerminationPlan::LogQuiesceOnly => (None, None),
                            };
                            Ok(PreparedReassignmentTermination {
                                previous_attempt_id: previous.attempt.attempt_id.clone(),
                                previous_agent: previous.agent.clone(),
                                plan: Some(plan),
                                process_tree_terminated,
                                pgid,
                                error: None,
                            })
                        })();
                        Some(
                            prepared.unwrap_or_else(|error| PreparedReassignmentTermination {
                                previous_attempt_id: previous.attempt.attempt_id.clone(),
                                previous_agent: previous.agent.clone(),
                                plan: None,
                                process_tree_terminated: None,
                                pgid: None,
                                error: Some(format!("{error:#}")),
                            }),
                        )
                    }
                }
                _ => None,
            }
        };

        // Honest non-dead terminal successors use fresh, outside-lock probes.
        // No sleep or provider observation is allowed inside append_checked.
        let preblocked_archive = {
            let fresh = read_ledger(&ledger_path)?;
            crate::attempt::reject_bad_lines(&fresh)?;
            let fresh_plan = crate::attempt::plan_dispatch_locked(
                &fresh.events,
                task_id,
                &agent,
                &concrete_main,
                round,
            )?;
            match fresh_plan {
                crate::attempt::DispatchPlan::New {
                    next,
                    previous_dispatch: Some(previous),
                } if !next.is_reassignment => {
                    if !terminal_attempt_uses_successor_archive(
                        &fresh.events,
                        task_id,
                        &previous.attempt.attempt_id,
                    ) {
                        None
                    } else {
                        let branch_exists = gitx::branch_exists(root, &next.branch);
                        let wt = root.join(&next.worktree_rel);
                        if !branch_exists || !wt.is_dir() {
                            None
                        } else {
                            let prepared = (|| -> Result<PreparedBlockedSuccessorArchive> {
                                let mut blocked_log_sample = None;
                                let durable_alive = durable_alive_for_attempt(
                                    root,
                                    &previous.agent,
                                    &fresh.events,
                                    &previous.attempt.attempt_id,
                                    &mut blocked_log_sample,
                                )?;
                                let evidence = reassignment_evidence_for_attempt(
                                    root,
                                    &fresh.events,
                                    task_id,
                                    &previous.agent,
                                    &previous.attempt.attempt_id,
                                    round,
                                )?;
                                let worktree_clean = gitx::porcelain_v2(&wt)?.trim().is_empty();
                                let decision = blocked_successor_archive_plan(
                                    worktree_clean,
                                    durable_alive,
                                    evidence.wake_log_recent_advance,
                                )
                                .map_err(anyhow::Error::msg)?;
                                let (plan_is_log_quiesce, process_tree_terminated) = match decision
                                {
                                    BlockedSuccessorArchive::SkipClean => (false, None),
                                    BlockedSuccessorArchive::ArchiveWip => {
                                        let registry = wake::load_registry(root)?;
                                        let spec =
                                            registry.get(&previous.agent).with_context(|| {
                                                format!(
                                                    "blocked successor missing registered agent: {}",
                                                    previous.agent
                                                )
                                            })?;
                                        let plan = wake::termination_plan(&spec.argv)
                                            .map_err(anyhow::Error::msg)?;
                                        (
                                            plan == wake::TerminationPlan::LogQuiesceOnly,
                                            evidence.process_tree_terminated,
                                        )
                                    }
                                };
                                Ok(PreparedBlockedSuccessorArchive {
                                    previous_attempt_id: previous.attempt.attempt_id.clone(),
                                    previous_agent: previous.agent.clone(),
                                    decision: Some(decision),
                                    plan_is_log_quiesce,
                                    process_tree_terminated,
                                    wake_log_recent_advance: evidence.wake_log_recent_advance,
                                    error: None,
                                })
                            })();
                            Some(
                                prepared.unwrap_or_else(|error| PreparedBlockedSuccessorArchive {
                                    previous_attempt_id: previous.attempt.attempt_id.clone(),
                                    previous_agent: previous.agent.clone(),
                                    decision: None,
                                    plan_is_log_quiesce: false,
                                    process_tree_terminated: None,
                                    wake_log_recent_advance: false,
                                    error: Some(format!("{error:#}")),
                                }),
                            )
                        }
                    }
                }
                crate::attempt::DispatchPlan::New {
                    next,
                    previous_dispatch: None,
                } if !next.is_reassignment => None,
                _ => None,
            }
        };

        let mut outcome: Option<DispatchOutcome> = None;
        let root_owned = root.to_path_buf();
        let task_owned = task_id.to_string();
        let agent_owned = agent.clone();
        let round_owned = round.to_string();
        let concrete_main_owned = concrete_main.clone();
        let report_rel_owned = format!("coordination/rounds/{round}/reports/{task_id}-REPORT.md");

        let appended = ledger::append_checked(root, &round, |events| {
            let fresh_round = current_round(&root_owned)?;
            if fresh_round != round_owned {
                bail!("锁内 round 漂移: {round_owned} → {fresh_round}");
            }
            let fresh_card = critical_point(require_active_tierf_task_from_events(
                &root_owned,
                &fresh_round,
                &task_owned,
                events,
            ))?;
            let fresh_card_agent = fresh_card
                .meta
                .agent
                .clone()
                .context("任务卡缺 agent（锁内重读）")?;
            if fresh_card_agent != agent_owned {
                bail!("锁内 card agent 漂移: {agent_owned} → {fresh_card_agent}");
            }
            let fresh_agent = agent_owned.clone();
            let fresh_main = gitx::rev_parse(&root_owned, "main")?;
            if fresh_main != concrete_main_owned {
                bail!("锁内 main SHA 漂移: {concrete_main_owned} → {fresh_main}");
            }

            let plan = crate::attempt::plan_dispatch_locked(
                events,
                &task_owned,
                &fresh_agent,
                &fresh_main,
                &fresh_round,
            )?;
            let successor_capacity_release = match &plan {
                crate::attempt::DispatchPlan::New {
                    next,
                    previous_dispatch: Some(previous),
                } if !next.is_reassignment
                    && terminal_attempt_uses_successor_archive(
                        events,
                        &task_owned,
                        &previous.attempt.attempt_id,
                    ) =>
                {
                    Some((task_owned.as_str(), previous.attempt.attempt_id.as_str()))
                }
                _ => None,
            };
            if let crate::attempt::DispatchPlan::New { next, .. } = &plan {
                require_runtime_dispatch_admission(
                    &root_owned,
                    &fresh_round,
                    &task_owned,
                    &next.base_sha,
                    events,
                    &fresh_agent,
                    successor_capacity_release,
                )?;
            }
            match plan {
                crate::attempt::DispatchPlan::New {
                    next,
                    previous_dispatch,
                } => {
                    if is_existing {
                        outcome = Some(DispatchOutcome::RetryNew);
                        return Ok(Vec::new());
                    }
                    let mut archive_guard_evidence = None;
                    let mut skip_clean_blocked_archive = false;
                    if next.is_reassignment {
                        let previous = previous_dispatch
                            .as_ref()
                            .context("reassignment 缺 previous dispatch")?;
                        let mut evidence = reassignment_evidence_for_attempt(
                            &root_owned,
                            events,
                            &task_owned,
                            &previous.agent,
                            &previous.attempt.attempt_id,
                            &fresh_round,
                        )?;
                        if !override_ambiguous_active {
                            if let Err(reason) = reassignment_gate(
                                evidence.kind_is_dead,
                                evidence.durable_alive,
                                evidence.wake_log_recent_advance,
                                evidence.process_tree_terminated,
                            ) {
                                outcome = Some(DispatchOutcome::Blocked {
                                    reason: reason.clone(),
                                });
                                return Ok(vec![ledger::event(
                                    "EscalationRaised",
                                    "runtime:orch",
                                    Some(&task_owned),
                                    Some(&round_owned),
                                    serde_json::json!({
                                        "stage": "ambiguous-active-blocked",
                                        "reason": reason,
                                        "previousAttemptId": previous.attempt.attempt_id,
                                        "previousAgent": previous.agent,
                                        "override": false,
                                    }),
                                )]);
                            }
                        }

                        let prepared = pretermination.as_ref().filter(|prepared| {
                            prepared.previous_attempt_id == previous.attempt.attempt_id
                                && prepared.previous_agent == previous.agent
                        });
                        let Some(prepared) = prepared else {
                            // The lock-free action was prepared from a
                            // different ledger generation.  Do not perform a
                            // new slow action under this lock; retry from a
                            // fresh outside-lock plan.
                            outcome = Some(DispatchOutcome::RetryNew);
                            return Ok(Vec::new());
                        };
                        if let Some(error) = prepared.error.as_ref() {
                            let reason =
                                format!("WIP archive blocked: termination failed: {error}");
                            outcome = Some(DispatchOutcome::Blocked {
                                reason: reason.clone(),
                            });
                            return Ok(vec![ledger::event(
                                "EscalationRaised",
                                "runtime:orch",
                                Some(&task_owned),
                                Some(&round_owned),
                                serde_json::json!({
                                    "stage": "wip-archive-blocked",
                                    "reason": reason,
                                    "previousAttemptId": previous.attempt.attempt_id,
                                    "previousAgent": previous.agent,
                                }),
                            )]);
                        }
                        let termination_plan = prepared
                            .plan
                            .context("prepared termination missing topology plan")?;
                        let process_tree_terminated = match termination_plan {
                            wake::TerminationPlan::SignalProcessGroup => {
                                let recheck_alive =
                                    prepared.pgid.map(process_group_alive).transpose()?;
                                if let Err(reason) = termination_commit_gate(
                                    prepared.process_tree_terminated == Some(true),
                                    recheck_alive,
                                ) {
                                    let reason = format!(
                                        "WIP archive blocked: termination failed: {reason}"
                                    );
                                    outcome = Some(DispatchOutcome::Blocked {
                                        reason: reason.clone(),
                                    });
                                    return Ok(vec![ledger::event(
                                        "EscalationRaised",
                                        "runtime:orch",
                                        Some(&task_owned),
                                        Some(&round_owned),
                                        serde_json::json!({
                                            "stage": "wip-archive-blocked",
                                            "reason": reason,
                                            "previousAttemptId": previous.attempt.attempt_id,
                                            "previousAgent": previous.agent,
                                        }),
                                    )]);
                                }
                                prepared.process_tree_terminated
                            }
                            wake::TerminationPlan::LogQuiesceOnly => None,
                        };
                        evidence.process_tree_terminated = process_tree_terminated;
                        let plan_is_log_quiesce =
                            termination_plan == wake::TerminationPlan::LogQuiesceOnly;
                        if let Err(reason) = wip_archive_gate(
                            plan_is_log_quiesce,
                            evidence.process_tree_terminated,
                            evidence.wake_log_recent_advance,
                        ) {
                            outcome = Some(DispatchOutcome::Blocked {
                                reason: reason.clone(),
                            });
                            return Ok(vec![ledger::event(
                                "EscalationRaised",
                                "runtime:orch",
                                Some(&task_owned),
                                Some(&round_owned),
                                serde_json::json!({
                                    "stage": "wip-archive-blocked",
                                    "reason": reason,
                                    "previousAttemptId": previous.attempt.attempt_id,
                                    "previousAgent": previous.agent,
                                }),
                            )]);
                        }
                        if !override_ambiguous_active {
                            if let Err(reason) = reassignment_gate(
                                evidence.kind_is_dead,
                                evidence.durable_alive,
                                evidence.wake_log_recent_advance,
                                evidence.process_tree_terminated,
                            ) {
                                outcome = Some(DispatchOutcome::Blocked {
                                    reason: reason.clone(),
                                });
                                return Ok(vec![ledger::event(
                                    "EscalationRaised",
                                    "runtime:orch",
                                    Some(&task_owned),
                                    Some(&round_owned),
                                    serde_json::json!({
                                        "stage": "ambiguous-active-blocked",
                                        "reason": reason,
                                        "previousAttemptId": previous.attempt.attempt_id,
                                        "previousAgent": previous.agent,
                                        "override": false,
                                    }),
                                )]);
                            }
                        }
                        archive_guard_evidence = Some((
                            plan_is_log_quiesce,
                            evidence.process_tree_terminated,
                            evidence.wake_log_recent_advance,
                        ));
                    }
                    let branch_exists = gitx::branch_exists(&root_owned, &next.branch);
                    let worktree_exists = root_owned.join(&next.worktree_rel).is_dir();
                    if !next.is_reassignment
                        && branch_exists
                        && worktree_exists
                        && next
                            .previous_attempt
                            .as_ref()
                            .is_some_and(|previous_attempt| {
                                terminal_attempt_uses_successor_archive(
                                    events,
                                    &task_owned,
                                    &previous_attempt.attempt_id,
                                )
                            })
                    {
                        let previous = previous_dispatch
                            .as_ref()
                            .context("blocked successor 缺 previous dispatch")?;
                        let prepared = preblocked_archive.as_ref().filter(|prepared| {
                            prepared.previous_attempt_id == previous.attempt.attempt_id
                                && prepared.previous_agent == previous.agent
                        });
                        let Some(prepared) = prepared else {
                            outcome = Some(DispatchOutcome::RetryNew);
                            return Ok(Vec::new());
                        };
                        if let Some(error) = prepared.error.as_ref() {
                            let reason = format!(
                                "WIP archive blocked: blocked successor probe failed: {error}"
                            );
                            outcome = Some(DispatchOutcome::Blocked {
                                reason: reason.clone(),
                            });
                            return Ok(vec![ledger::event(
                                "EscalationRaised",
                                "runtime:orch",
                                Some(&task_owned),
                                Some(&round_owned),
                                serde_json::json!({
                                    "stage": "wip-archive-blocked",
                                    "reason": reason,
                                    "previousAttemptId": previous.attempt.attempt_id,
                                    "previousAgent": previous.agent,
                                }),
                            )]);
                        }
                        match prepared
                            .decision
                            .context("blocked successor probe missing archive decision")?
                        {
                            BlockedSuccessorArchive::SkipClean => {
                                skip_clean_blocked_archive = true;
                            }
                            BlockedSuccessorArchive::ArchiveWip => {
                                archive_guard_evidence = Some((
                                    prepared.plan_is_log_quiesce,
                                    prepared.process_tree_terminated,
                                    prepared.wake_log_recent_advance,
                                ));
                            }
                        }
                    }
                    if branch_exists && worktree_exists {
                        let wt = root_owned.join(&next.worktree_rel);
                        let cur_branch = gitx::current_branch(&wt)
                            .context("preflight: 读取 worktree 分支失败")?;
                        if cur_branch.as_deref() != Some(&next.branch) {
                            bail!(
                                "preflight: worktree 当前分支 {cur_branch:?} != 预期 {}",
                                next.branch
                            );
                        }
                    }
                    let worktree_step = crate::attempt::go_worktree_instruction(
                        &task_owned,
                        &next.base_sha,
                        branch_exists,
                        worktree_exists,
                    )?;
                    let base_short = gitx::short(&next.base_sha).to_string();
                    let paths =
                        crate::attempt::attempt_paths(&round_owned, &fresh_agent, &next.attempt);
                    let go_path = root_owned.join(&paths.go_rel);
                    let go_bytes = crate::attempt::render_attempt_go(
                        &task_owned,
                        &fresh_agent,
                        &base_short,
                        &round_owned,
                        &report_rel_owned,
                        &worktree_step,
                    );
                    match std::fs::symlink_metadata(&go_path) {
                        Ok(meta)
                            if meta.file_type().is_file() && !meta.file_type().is_symlink() =>
                        {
                            if std::fs::read(&go_path)? != go_bytes {
                                bail!("GO 已存在但内容冲突: {}", go_path.display());
                            }
                        }
                        Ok(_) => bail!("GO slot 不是 regular non-symlink: {}", go_path.display()),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                    if let Some(prev) = &next.previous_attempt {
                        let previous = previous_dispatch
                            .as_ref()
                            .context("takeover 缺 previous_dispatch")?;
                        if previous.attempt.attempt_id != prev.attempt_id
                            || next.previous_agent.as_deref() != Some(previous.agent.as_str())
                        {
                            bail!("takeover previous_dispatch identity mismatch");
                        }
                        let prev_go_rel = crate::attempt::resolve_superseded_go(
                            &root_owned,
                            &round_owned,
                            previous,
                        )?;
                        crate::attempt::archive_preflight(
                            &root_owned,
                            &prev_go_rel,
                            &prev.attempt_id,
                            &previous.agent,
                        )?;
                        let wt = root_owned.join(&next.worktree_rel);
                        if wt.is_dir() && branch_exists {
                            if !skip_clean_blocked_archive {
                                let (
                                    plan_is_log_quiesce,
                                    process_tree_terminated,
                                    wake_log_advanced,
                                ) = archive_guard_evidence.context(
                                    "takeover WIP snapshot missing termination/archive evidence",
                                )?;
                                wip_archive_gate(
                                    plan_is_log_quiesce,
                                    process_tree_terminated,
                                    wake_log_advanced,
                                )
                                .map_err(anyhow::Error::msg)?;
                                crate::attempt::snapshot_worktree_wip(
                                    &root_owned,
                                    &round_owned,
                                    prev,
                                )
                                .context("takeover WIP 快照失败")?;
                            }
                        }
                        // Re-resolve the strict ledger path immediately before
                        // the mutator; replay never bypasses schema validation.
                        let prev_go_rel = crate::attempt::resolve_superseded_go(
                            &root_owned,
                            &round_owned,
                            previous,
                        )?;
                        crate::attempt::archive_superseded_dispatch(
                            &root_owned,
                            &prev_go_rel,
                            &prev.attempt_id,
                            &previous.agent,
                        )?;
                    }
                    crate::attempt::reconcile_or_create_go(&go_path, &go_bytes)?;

                    let mut dispatch_payload = serde_json::json!({
                        "agent": fresh_agent,
                        "method": "filegate(GO)",
                        "baseSha": next.base_sha,
                        "goPath": paths.go_rel,
                        "attemptId": next.attempt.attempt_id,
                        "attemptNo": next.attempt.ordinal,
                        "reassignment": next.is_reassignment,
                        "overrideAmbiguousActive": next.is_reassignment && override_ambiguous_active,
                        "wakePending": true,
                    });
                    if let Some(prev) = &next.previous_attempt {
                        dispatch_payload["previousAttemptId"] =
                            serde_json::Value::String(prev.attempt_id.clone());
                    }
                    if let Some(previous) = &next.previous_agent {
                        dispatch_payload["previousAgent"] =
                            serde_json::Value::String(previous.clone());
                    }
                    let mut events_to_append = vec![ledger::event(
                        "DispatchIssued",
                        "runtime:orch",
                        Some(&task_owned),
                        Some(&round_owned),
                        dispatch_payload,
                    )];
                    let implement_identity = crate::sites::site_identity(
                        &task_owned,
                        IMPLEMENT_DISPATCH_SITE_ROLE,
                        &fresh_agent,
                    );
                    let implement_generation =
                        crate::sites::SiteIdentity::next_generation(events, &implement_identity);
                    if implement_generation == 0
                        || (implement_generation == u32::MAX
                            && events.iter().any(|event| {
                                event.kind == "WorkspaceLeased"
                                    && event
                                        .payload
                                        .as_ref()
                                        .and_then(|payload| payload.get("siteId"))
                                        .and_then(serde_json::Value::as_str)
                                        == Some(
                                            implement_identity
                                                .site_id_for(implement_generation)
                                                .as_str(),
                                        )
                            }))
                    {
                        bail!("implement site generation overflow");
                    }
                    events_to_append.push(ledger::event(
                        "WorkspaceLeased",
                        "runtime:orch",
                        Some(&task_owned),
                        Some(&round_owned),
                        serde_json::json!({
                            "siteId": implement_identity.site_id_for(implement_generation),
                            "generation": implement_generation,
                            "attemptId": next.attempt.attempt_id,
                            "role": crate::sites::SiteRole::Implement.as_str(),
                            "agent": fresh_agent,
                            "reviewedHead": next.base_sha,
                            "paths": {
                                "worktree": format!(".worktrees/{task_owned}"),
                                "target": format!(".worktrees/{task_owned}/orch/target"),
                            },
                        }),
                    ));
                    if next.is_reassignment {
                        let prev = next
                            .previous_attempt
                            .as_ref()
                            .context("reassignment 缺 previousAttemptId")?;
                        let previous_agent = next
                            .previous_agent
                            .as_ref()
                            .context("reassignment 缺 previousAgent")?;
                        events_to_append.push(ledger::event(
                            "ReassignmentIssued",
                            "runtime:orch",
                            Some(&task_owned),
                            Some(&round_owned),
                            serde_json::json!({
                                "previousAttemptId": prev.attempt_id,
                                "previousAgent": previous_agent,
                                "attemptId": next.attempt.attempt_id,
                                "attemptNo": next.attempt.ordinal,
                                "agent": fresh_agent,
                                "override": override_ambiguous_active,
                            }),
                        ));
                    }
                    let expected_append = events_to_append.len();
                    outcome = Some(DispatchOutcome::Created {
                        agent: fresh_agent,
                        base_short,
                        go_rel: paths.go_rel,
                        attempt: next.attempt,
                        wake_needed: true,
                        expected_append,
                    });
                    Ok(events_to_append)
                }
                crate::attempt::DispatchPlan::Existing {
                    current,
                    needs_companion,
                    previous_attempt_id,
                    previous_agent,
                } => {
                    crate::attempt::resolve_go_path_strict(
                        &root_owned,
                        &current.go_path,
                        &round_owned,
                        &current.agent,
                        &current.attempt,
                        current.is_legacy,
                    )?;
                    let wake_needed = current.wake_pending && !current.wake_completed;
                    if needs_companion {
                        let prev_aid = previous_attempt_id
                            .as_ref()
                            .context("reassignment repair 缺 previousAttemptId")?;
                        let prev_agent = previous_agent
                            .as_ref()
                            .context("reassignment repair 缺 previousAgent")?;
                        let companion = ledger::event(
                            "ReassignmentIssued",
                            "runtime:orch",
                            Some(&task_owned),
                            Some(&round_owned),
                            serde_json::json!({
                                "previousAttemptId": prev_aid,
                                "previousAgent": prev_agent,
                                "attemptId": current.attempt.attempt_id,
                                "attemptNo": current.attempt.ordinal,
                                "agent": current.agent,
                            }),
                        );
                        outcome = Some(DispatchOutcome::Repaired {
                            agent: current.agent,
                            go_rel: current.go_path,
                            attempt: current.attempt,
                            wake_needed,
                        });
                        Ok(vec![companion])
                    } else {
                        outcome = Some(DispatchOutcome::Existing {
                            agent: current.agent,
                            go_rel: current.go_path,
                            attempt: current.attempt,
                            wake_needed,
                        });
                        Ok(Vec::new())
                    }
                }
            }
        })?;

        let outcome = outcome.context("dispatch 闭包未设置 outcome（内部错误）")?;
        validate_dispatch_append_count(&outcome, appended)?;
        match outcome {
            DispatchOutcome::RetryNew => {
                drop(wake_permit);
                // A second outside-lock hook point lets tests deterministically
                // install the next active generation before the fresh reread.
                after_readonly(false, iteration)?;
                continue;
            }
            DispatchOutcome::Blocked { reason } => {
                drop(wake_permit);
                bail!("{reason}");
            }
            DispatchOutcome::Created {
                agent,
                base_short,
                go_rel,
                attempt,
                wake_needed,
                ..
            } => {
                let permit = wake_permit
                    .as_mut()
                    .context("Created dispatch 缺 model-wake permit")?;
                permit.commit();
                finish_dispatch_wake(
                    root,
                    &round,
                    task_id,
                    &agent,
                    &attempt,
                    &go_rel,
                    no_wake,
                    wake_needed,
                )?;
                return Ok((agent, base_short));
            }
            DispatchOutcome::Existing {
                agent,
                go_rel,
                attempt,
                wake_needed,
            }
            | DispatchOutcome::Repaired {
                agent,
                go_rel,
                attempt,
                wake_needed,
            } => {
                drop(wake_permit);
                let fresh = read_ledger(&ledger_path)?;
                crate::attempt::reject_bad_lines(&fresh)?;
                let ctx = crate::attempt::resolve_current_dispatch(&fresh.events, task_id, &round)?;
                let base = ctx
                    .base_sha
                    .as_deref()
                    .context("Existing dispatch 缺 concrete baseSha")?;
                finish_dispatch_wake(
                    root,
                    &round,
                    task_id,
                    &agent,
                    &attempt,
                    &go_rel,
                    no_wake,
                    wake_needed,
                )?;
                return Ok((agent, gitx::short(base).to_string()));
            }
        }
    }
    bail!("dispatch replan exhausted after {MAX_REPLANS} iterations; fail-closed")
}

#[doc(hidden)]
pub fn validate_dispatch_append_count(outcome: &DispatchOutcome, actual: usize) -> Result<()> {
    let expected = match outcome {
        DispatchOutcome::Created {
            expected_append, ..
        } => {
            if !(1..=3).contains(expected_append) {
                bail!("Created expected append 必须在 1..=3");
            }
            *expected_append
        }
        DispatchOutcome::Existing { .. } | DispatchOutcome::RetryNew => 0,
        DispatchOutcome::Repaired { .. } => 1,
        DispatchOutcome::Blocked { .. } => 1,
    };
    if actual != expected {
        bail!("dispatch append count mismatch: outcome expected {expected}, actual {actual}");
    }
    Ok(())
}

fn render_dispatch_wake_message(
    round: &str,
    task_id: &str,
    agent: &str,
    attempt: &crate::attempt::AttemptRef,
    go_rel: &str,
    action_id: Option<&str>,
) -> Result<String> {
    if attempt.ordinal == 0 || attempt.task_id != task_id {
        bail!("dispatch wake attempt/task identity invalid");
    }
    let short_attempt = format!("A{:04}", attempt.ordinal);
    let expected_attempt_id = format!("{task_id}-{short_attempt}");
    if attempt.attempt_id != expected_attempt_id {
        bail!(
            "dispatch wake attemptId {:?} != expected {:?}; refusing unreachable targeted wait",
            attempt.attempt_id,
            expected_attempt_id
        );
    }
    let action_line = action_id
        .map(|action| format!("\nORCH_WAKE_ACTION_ID={action}"))
        .unwrap_or_default();
    Ok(format!(
        "orch 注入唤醒: round={round} taskId={task_id} attemptId={}\n\
         GO={go_rel}\n\
         请只领取上述 attempt，并运行 planner 已预置的定向等待命令：\n\
         coordination/scripts/wait-dispatch.sh {agent} 1800 {task_id} {short_attempt}\
         {action_line}",
        attempt.attempt_id,
    ))
}

fn finish_dispatch_wake(
    root: &Path,
    round: &str,
    task_id: &str,
    agent: &str,
    attempt: &crate::attempt::AttemptRef,
    go_rel: &str,
    no_wake: bool,
    wake_needed: bool,
) -> Result<()> {
    finish_dispatch_wake_with_hook(
        root,
        round,
        task_id,
        agent,
        attempt,
        go_rel,
        no_wake,
        wake_needed,
        &mut |_| Ok(()),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DurableWakeClaim {
    Completed,
    Delivered {
        action_id: String,
    },
    Owned {
        action_id: String,
        owner: String,
        generation: String,
    },
    Busy,
}

fn durable_wake_payload(
    attempt: &crate::attempt::AttemptRef,
    agent: &str,
    base_sha: &str,
    go_rel: &str,
    action_id: &str,
    generation: &str,
    owner: &str,
) -> serde_json::Value {
    // 每个 state event 都携带同一 claim 的 owner + leaseGeneration（fold 必选）。
    serde_json::json!({
        "attemptId": attempt.attempt_id,
        "attemptNo": attempt.ordinal,
        "agent": agent,
        "baseSha": base_sha,
        "goPath": go_rel,
        "actionId": action_id,
        "owner": owner,
        "leaseGeneration": generation,
    })
}

/// B110：spawn 成功后由 wake outcome 构造精确通道事实（fail-closed）。
///
/// POKE 备用道（injected=false，无 pid/log）是显式 [crate::wake::DeliveryState::Pending]，
/// 绝不伪造 Delivered——调用方必须照 spawn 失败处理（release + 中止），
/// 本函数对其直接 bail。logPath 用本次注入的 per-attempt 专属文件
/// （legacy `wake-<agent>.log` 会被相邻 action 重链，串台源，禁用）；
/// probeOffset=0（专属文件从 0 起只含本次注入），probeEnd=投递时刻文件水位。
fn spawn_channel_facts(
    action_id: &str,
    attempt_id: &str,
    attempt_no: usize,
    outcome: &crate::wake::DispatchWakeOutcome,
) -> Result<crate::wake::ActionChannelFacts> {
    if !outcome.injected {
        bail!(
            "wake 未注入（POKE 备用道，无 pid/log）：通道显式 {}",
            crate::wake::DeliveryState::from_spawn(false)
        );
    }
    let pid = outcome.pid.context("wake 注入成功但缺 pid")?;
    let log_path = outcome
        .log_path
        .as_ref()
        .context("wake 注入成功但缺 logPath")?;
    let probe_end = fs::metadata(log_path)
        .with_context(|| format!("读取 wake 日志水位失败: {}", log_path.display()))?
        .len();
    crate::wake::ActionChannelFacts::new(
        action_id,
        attempt_id,
        attempt_no as u64,
        pid,
        &log_path.display().to_string(),
        0,
        probe_end,
    )
    .map_err(anyhow::Error::msg)
}

fn current_attempt_channel_facts(
    events: &[EventRecord],
    attempt_id: &str,
) -> Option<crate::wake::ActionChannelFacts> {
    events.iter().rev().find_map(|event| {
        if !matches!(
            event.kind.as_str(),
            "DispatchWakeCompleted"
                | "DispatchWakeDelivered"
                | "ResumeWakeCompleted"
                | "ResumeWakeDelivered"
                | "NudgeIssued"
        ) {
            return None;
        }
        let payload = event.payload.as_ref()?;
        if payload.get("attemptId").and_then(serde_json::Value::as_str) != Some(attempt_id) {
            return None;
        }
        crate::wake::ActionChannelFacts::from_payload(payload).ok()
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurableAttemptProbe {
    pub durable_alive: Option<bool>,
    pub wake_log_advanced: bool,
    pub process_tree_terminated: Option<bool>,
}

/// Sample the provider identity that belongs to this exact attempt.
///
/// Managed CLI children have a process-group identity.  Socket-backed
/// SmartClaw attempts instead have only their exact wake-log progress; a
/// stagnant or first log sample is ambiguous, never confirmed death.
pub fn durable_probe_for_attempt(
    root: &Path,
    agent: &str,
    events: &[EventRecord],
    attempt_id: &str,
    log_sample: &mut Option<crate::chanhealth::WakeLogSample>,
) -> Result<DurableAttemptProbe> {
    let facts = match current_attempt_channel_facts(events, attempt_id) {
        Some(facts) => facts,
        None => {
            return Ok(DurableAttemptProbe {
                durable_alive: None,
                wake_log_advanced: false,
                process_tree_terminated: None,
            })
        }
    };
    let registry = wake::load_registry(root)?;
    let spec = registry
        .get(agent)
        .with_context(|| format!("durable identity missing registered agent: {agent}"))?;
    match wake::durable_identity_kind(&spec.argv)? {
        wake::DurableIdentityKind::ManagedPidGroup => {
            let alive = crate::chanhealth::pid_alive(facts.pid);
            Ok(DurableAttemptProbe {
                durable_alive: Some(alive),
                wake_log_advanced: false,
                process_tree_terminated: Some(!alive),
            })
        }
        wake::DurableIdentityKind::WakeLogProxy => {
            let candidate = PathBuf::from(&facts.log_path);
            let log_path = if candidate.is_absolute() {
                candidate
            } else {
                root.join(candidate)
            };
            let durable_alive = crate::chanhealth::probe_wake_log_progress(&log_path, log_sample);
            Ok(DurableAttemptProbe {
                durable_alive,
                wake_log_advanced: durable_alive == Some(true),
                process_tree_terminated: None,
            })
        }
    }
}

fn durable_alive_for_attempt(
    root: &Path,
    agent: &str,
    events: &[EventRecord],
    attempt_id: &str,
    log_sample: &mut Option<crate::chanhealth::WakeLogSample>,
) -> Result<Option<bool>> {
    Ok(durable_probe_for_attempt(root, agent, events, attempt_id, log_sample)?.durable_alive)
}

/// B110：Completed 从同 action 的 Delivered 锚点继承精确通道事实——同 action
/// 状态事件身份相同由构造保证（Delivered-replay 路径没有新 spawn，照样一致）；
/// 锚点缺字段/类型错 fail-closed（action facts 缺字段或类型错不得放行）。
fn inherit_channel_facts(
    anchor: &EventRecord,
    payload: &mut serde_json::Value,
    label: &str,
) -> Result<()> {
    let anchor_payload = anchor
        .payload
        .as_ref()
        .with_context(|| format!("{label} 锚点事件缺 payload"))?;
    let facts = crate::wake::ActionChannelFacts::from_payload(anchor_payload)
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("{label} 锚点缺精确通道事实"))?;
    facts.insert_channel_fields(payload);
    Ok(())
}

/// The hook is deliberately after the durable Delivered append and before
/// Completed.  It exercises the recoverable "external spawn succeeded but
/// completion append failed" boundary without weakening the production path.
#[doc(hidden)]
pub fn finish_dispatch_wake_with_hook(
    root: &Path,
    round: &str,
    task_id: &str,
    agent: &str,
    attempt: &crate::attempt::AttemptRef,
    go_rel: &str,
    no_wake: bool,
    wake_needed: bool,
    hook: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<()> {
    crate::close::with_protocol_effect(root, "tierf dispatch wake", || {
        finish_dispatch_wake_with_hook_effect(
            root,
            round,
            task_id,
            agent,
            attempt,
            go_rel,
            no_wake,
            wake_needed,
            hook,
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn finish_dispatch_wake_with_hook_effect(
    root: &Path,
    round: &str,
    task_id: &str,
    agent: &str,
    attempt: &crate::attempt::AttemptRef,
    go_rel: &str,
    no_wake: bool,
    wake_needed: bool,
    hook: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<()> {
    if current_round(root)? != round {
        bail!("dispatch wake round 不再 current");
    }
    critical_point(require_active_tierf_task(root, round, task_id))?;
    let fallback_action_id = format!("dispatch-wake:{round}:{}", attempt.attempt_id);
    match finish_dispatch_wake_with_hook_inner(
        root,
        round,
        task_id,
        agent,
        attempt,
        go_rel,
        no_wake,
        wake_needed,
        hook,
    ) {
        Ok(()) => Ok(()),
        Err(error)
            if crate::failure::is_action_rejection(&error) || is_critical_point_drift(&error) =>
        {
            Err(error)
        }
        Err(error) => crate::failure::reject_action_disposition(
            root,
            round,
            Some(task_id),
            "dispatch",
            &fallback_action_id,
            &format!("{error:#}"),
            crate::failure::CliDisposition::Rejected,
            Some(attempt),
        ),
    }
}

fn finish_dispatch_wake_with_hook_inner(
    root: &Path,
    round: &str,
    task_id: &str,
    agent: &str,
    attempt: &crate::attempt::AttemptRef,
    go_rel: &str,
    no_wake: bool,
    wake_needed: bool,
    hook: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<()> {
    if !wake_needed {
        return Ok(());
    }
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let initial = read_ledger(&ledger_path)?;
    crate::attempt::reject_bad_lines(&initial)?;
    let ctx = crate::attempt::resolve_current_dispatch(&initial.events, task_id, round)?;
    if ctx.attempt_id.as_deref() != Some(attempt.attempt_id.as_str())
        || ctx.attempt_no != Some(attempt.ordinal)
        || ctx.agent.as_deref() != Some(agent)
        || ctx.go_path.as_deref() != Some(go_rel)
    {
        bail!("dispatch wake facts no longer current");
    }
    let base_sha = ctx
        .base_sha
        .as_deref()
        .context("dispatch wake 缺 concrete baseSha")?
        .to_string();
    let go_abs = crate::attempt::resolve_go_path_strict(
        root,
        go_rel,
        round,
        agent,
        attempt,
        ctx.is_legacy.context("dispatch wake 缺 legacy identity")?,
    )?;
    match std::fs::symlink_metadata(&go_abs) {
        Ok(meta) if meta.file_type().is_file() && !meta.file_type().is_symlink() => {}
        Ok(_) => bail!("dispatch wake GO 必须是 regular non-symlink"),
        Err(error) => return Err(error).context("dispatch wake GO stat 失败"),
    }

    // --no-wake remains an explicit deferred delivery.  It may write POKE, but
    // it never claims or completes the injectable action.
    if no_wake {
        let wake_msg = render_dispatch_wake_message(round, task_id, agent, attempt, go_rel, None)?;
        wake::dispatch_wake(root, agent, round, &wake_msg, true)?;
        return Ok(());
    }

    let action_id =
        crate::attempt::dispatch_wake_action_id(round, task_id, attempt, agent, &base_sha, go_rel);
    // 每次 claim 独立 owner + leaseGeneration；同 action 所有 state event 携带同一代。
    let owner = ulid::Ulid::new().to_string();
    let generation = ulid::Ulid::new().to_string();
    let lease_until =
        humantime::format_rfc3339_seconds(SystemTime::now() + Duration::from_secs(30)).to_string();
    let claim_cell = std::cell::RefCell::new(None);
    let task_owned = task_id.to_string();
    let round_owned = round.to_string();
    let agent_owned = agent.to_string();
    let base_owned = base_sha.clone();
    let go_owned = go_rel.to_string();
    let action_owned = action_id.clone();
    let owner_owned = owner.clone();
    let generation_owned = generation.clone();
    let lease_owned = lease_until.clone();
    let claimed_count = ledger::append_checked(root, round, |events| {
        let ctx = crate::attempt::resolve_current_dispatch(events, &task_owned, &round_owned)?;
        if ctx.attempt_id.as_deref() != Some(attempt.attempt_id.as_str())
            || ctx.attempt_no != Some(attempt.ordinal)
            || ctx.agent.as_deref() != Some(agent_owned.as_str())
            || ctx.base_sha.as_deref() != Some(base_owned.as_str())
            || ctx.go_path.as_deref() != Some(go_owned.as_str())
        {
            bail!("dispatch wake attempt changed before owner claim");
        }
        let expectation = wake_expectation(
            &round_owned,
            &task_owned,
            &attempt.attempt_id,
            attempt.ordinal,
            &agent_owned,
            &base_owned,
            &go_owned,
            &action_owned,
        );
        let new_claim_events = || -> Result<Vec<EventRecord>> {
            let mut payload = durable_wake_payload(
                attempt,
                &agent_owned,
                &base_owned,
                &go_owned,
                &action_owned,
                &generation_owned,
                &owner_owned,
            );
            payload["leaseUntil"] = serde_json::json!(lease_owned);
            Ok(vec![ledger::event(
                "DispatchWakeClaimed",
                "runtime:orch",
                Some(&task_owned),
                Some(&round_owned),
                payload,
            )])
        };
        // fold-first：无任何 scope 历史（首次调用）才直接新 claim；否则必须 fold 决策。
        if !crate::attempt::durable_action_scope_has_events(
            events,
            crate::attempt::DurableActionKind::DispatchWake,
            &expectation,
        ) {
            *claim_cell.borrow_mut() = Some(DurableWakeClaim::Owned {
                action_id: action_owned.clone(),
                owner: owner_owned.clone(),
                generation: generation_owned.clone(),
            });
            return new_claim_events();
        }
        let phase = crate::attempt::fold_durable_action(
            events,
            crate::attempt::DurableActionKind::DispatchWake,
            &expectation,
        )?;
        match phase {
            crate::attempt::DurableActionPhase::Completed => {
                *claim_cell.borrow_mut() = Some(DurableWakeClaim::Completed);
                Ok(Vec::new())
            }
            crate::attempt::DurableActionPhase::Delivered => {
                *claim_cell.borrow_mut() = Some(DurableWakeClaim::Delivered {
                    action_id: action_owned.clone(),
                });
                Ok(Vec::new())
            }
            crate::attempt::DurableActionPhase::Launching => {
                let anchor = latest_durable_event(
                    events,
                    crate::attempt::DurableActionKind::DispatchWake,
                    &expectation,
                )
                .context("DispatchWakeLaunching 缺锚点事件")?;
                if durable_lease_live(anchor, "DispatchWakeLaunching")? {
                    *claim_cell.borrow_mut() = Some(DurableWakeClaim::Busy);
                    return Ok(Vec::new());
                }
                bail!(
                    "dispatch wake launch outcome is ambiguous after lease expiry; refusing duplicate external injection"
                );
            }
            crate::attempt::DurableActionPhase::Claimed => {
                let anchor = latest_durable_event(
                    events,
                    crate::attempt::DurableActionKind::DispatchWake,
                    &expectation,
                )
                .context("DispatchWakeClaimed 缺锚点事件")?;
                if durable_lease_live(anchor, "DispatchWakeClaimed")? {
                    *claim_cell.borrow_mut() = Some(DurableWakeClaim::Busy);
                    return Ok(Vec::new());
                }
                // 过期 claim：in-lock release CAS（owner/generation 等于锚点，
                // 前驱阶段 Claimed 在 kind 表内）+ 新代 reclaim，同事务追加。
                let (anchor_owner, anchor_generation) =
                    durable_anchor_owner_gen(anchor, "DispatchWakeClaimed")?;
                let released = ledger::event(
                    "DispatchWakeReleased",
                    "runtime:orch",
                    Some(&task_owned),
                    Some(&round_owned),
                    durable_wake_payload(
                        attempt,
                        &agent_owned,
                        &base_owned,
                        &go_owned,
                        &action_owned,
                        &anchor_generation,
                        &anchor_owner,
                    ),
                );
                *claim_cell.borrow_mut() = Some(DurableWakeClaim::Owned {
                    action_id: action_owned.clone(),
                    owner: owner_owned.clone(),
                    generation: generation_owned.clone(),
                });
                let mut events_out = vec![released];
                events_out.extend(new_claim_events()?);
                Ok(events_out)
            }
            crate::attempt::DurableActionPhase::Released => {
                *claim_cell.borrow_mut() = Some(DurableWakeClaim::Owned {
                    action_id: action_owned.clone(),
                    owner: owner_owned.clone(),
                    generation: generation_owned.clone(),
                });
                new_claim_events()
            }
            other => bail!("dispatch wake fold 返回 kind 非法阶段: {other:?}"),
        }
    })?;
    let claim = claim_cell
        .into_inner()
        .context("dispatch wake owner closure did not set state")?;
    match &claim {
        DurableWakeClaim::Owned { .. } if !(1..=2).contains(&claimed_count) => {
            bail!("DispatchWakeClaimed append count mismatch: {claimed_count}")
        }
        DurableWakeClaim::Completed
        | DurableWakeClaim::Delivered { .. }
        | DurableWakeClaim::Busy
            if claimed_count != 0 =>
        {
            bail!("dispatch wake replay unexpectedly appended {claimed_count} events")
        }
        _ => {}
    }
    if claim == DurableWakeClaim::Completed || claim == DurableWakeClaim::Busy {
        return Ok(());
    }

    if let DurableWakeClaim::Owned {
        action_id: owned_action,
        owner: owned_by,
        generation: owned_generation,
    } = &claim
    {
        let launching = ledger::append_checked(root, round, |events| {
            // fold-first：阶段必须是 Claimed 且 lineage 等于本 claim。
            let expectation = wake_expectation(
                round,
                task_id,
                &attempt.attempt_id,
                attempt.ordinal,
                agent,
                &base_sha,
                go_rel,
                owned_action,
            );
            let phase = crate::attempt::fold_durable_action(
                events,
                crate::attempt::DurableActionKind::DispatchWake,
                &expectation,
            )?;
            if phase != crate::attempt::DurableActionPhase::Claimed {
                bail!("dispatch wake launch 前 fold 阶段非法: {phase:?}");
            }
            let anchor = latest_durable_event(
                events,
                crate::attempt::DurableActionKind::DispatchWake,
                &expectation,
            )
            .context("dispatch wake launch lost owner claim")?;
            let (anchor_owner, anchor_generation) =
                durable_anchor_owner_gen(anchor, "DispatchWakeClaimed")?;
            if anchor_owner != *owned_by || anchor_generation != *owned_generation {
                bail!("dispatch wake launch owner/generation changed");
            }
            let lease = anchor
                .payload
                .as_ref()
                .and_then(|payload| payload.get("leaseUntil"))
                .and_then(serde_json::Value::as_str)
                .context("dispatch wake owner lease missing")?;
            if humantime::parse_rfc3339(lease)? <= SystemTime::now() {
                bail!("dispatch wake owner lease expired before launch fence");
            }
            let mut payload = durable_wake_payload(
                attempt,
                agent,
                &base_sha,
                go_rel,
                owned_action,
                owned_generation,
                owned_by,
            );
            payload["leaseUntil"] = serde_json::json!(lease);
            Ok(vec![ledger::event(
                "DispatchWakeLaunching",
                "runtime:orch",
                Some(task_id),
                Some(round),
                payload,
            )])
        })?;
        if launching != 1 {
            bail!("DispatchWakeLaunching append count mismatch: {launching}");
        }
        // R7：dispatch 路径消费共同 durable fold——重读账本验证 DispatchWake lineage。
        {
            let lr_fresh = read_ledger(&ledger_path)?;
            let expectation = wake_expectation(
                round,
                task_id,
                &attempt.attempt_id,
                attempt.ordinal,
                agent,
                &base_sha,
                go_rel,
                owned_action,
            );
            crate::attempt::fold_durable_action(
                &lr_fresh.events,
                crate::attempt::DurableActionKind::DispatchWake,
                &expectation,
            )?;
        }
        // B110：spawn 成功后立即构造精确通道事实（pid/logPath/probeOffset/probeEnd）。
        // POKE 备用道（injected=false，无 pid/log）是显式 Pending——与 spawn 失败
        // 同走 release + 中止，绝不伪造 DispatchWakeDelivered。
        let request_message =
            render_dispatch_wake_message(round, task_id, agent, attempt, go_rel, None);
        let spawned = render_dispatch_wake_message(
            round,
            task_id,
            agent,
            attempt,
            go_rel,
            Some(owned_action),
        )
        .and_then(|wake_msg| {
            let request_message = request_message?;
            let continuation = format!(
                "implementation:{round}:{task_id}:{}:{agent}",
                attempt.attempt_id
            );
            wake::dispatch_wake_for_continuation_messages(
                root,
                agent,
                round,
                &continuation,
                &request_message,
                &wake_msg,
                false,
            )
        })
        .and_then(|outcome| {
            spawn_channel_facts(owned_action, &attempt.attempt_id, attempt.ordinal, &outcome)
        });
        let facts = match spawned {
            Ok(facts) => facts,
            Err(error) => {
                // release CAS：账本锁内 fold，前驱阶段必须在 kind 表内
                // （wake: Claimed | Launching），owner/generation 必须等于本 claim。
                let release = ledger::append_checked(root, round, |events| {
                    let expectation = wake_expectation(
                        round,
                        task_id,
                        &attempt.attempt_id,
                        attempt.ordinal,
                        agent,
                        &base_sha,
                        go_rel,
                        owned_action,
                    );
                    let phase = crate::attempt::fold_durable_action(
                        events,
                        crate::attempt::DurableActionKind::DispatchWake,
                        &expectation,
                    )?;
                    if !matches!(
                        phase,
                        crate::attempt::DurableActionPhase::Claimed
                            | crate::attempt::DurableActionPhase::Launching
                    ) {
                        bail!("dispatch wake release 前 fold 阶段非法: {phase:?}");
                    }
                    let anchor = latest_durable_event(
                        events,
                        crate::attempt::DurableActionKind::DispatchWake,
                        &expectation,
                    )
                    .context("dispatch wake release 缺锚点事件")?;
                    let (anchor_owner, anchor_generation) =
                        durable_anchor_owner_gen(anchor, "DispatchWake release")?;
                    if anchor_owner != *owned_by || anchor_generation != *owned_generation {
                        bail!("dispatch wake release owner/generation 不等于本 claim");
                    }
                    Ok(vec![ledger::event(
                        "DispatchWakeReleased",
                        "runtime:orch",
                        Some(task_id),
                        Some(round),
                        durable_wake_payload(
                            attempt,
                            agent,
                            &base_sha,
                            go_rel,
                            owned_action,
                            owned_generation,
                            owned_by,
                        ),
                    )])
                });
                let reason = match release {
                    Ok(_) => format!("provider wake launch rejected: {error:#}"),
                    Err(release_error) => format!(
                        "provider wake launch rejected: {error:#}; owner release failed: {release_error:#}"
                    ),
                };
                return crate::failure::reject_action_disposition(
                    root,
                    round,
                    Some(task_id),
                    "dispatch",
                    owned_action,
                    &reason,
                    crate::failure::CliDisposition::Rejected,
                    Some(attempt),
                );
            }
        };
        // This hook marks the honest crash boundary: the external provider has
        // accepted the wake, but no durable Delivered acknowledgement exists
        // yet. Replay remains fenced by DispatchWakeLaunching and must not
        // inject again without provider-side idempotency acknowledgement.
        hook("after-dispatch-wake-spawn-before-delivered")?;
        critical_point(require_active_tierf_task(root, round, task_id))?;
        let delivered = ledger::append_checked(root, round, |events| {
            // fold-first：Launching → Delivered（幂等 Delivered 直接放行）。
            let expectation = wake_expectation(
                round,
                task_id,
                &attempt.attempt_id,
                attempt.ordinal,
                agent,
                &base_sha,
                go_rel,
                owned_action,
            );
            let phase = crate::attempt::fold_durable_action(
                events,
                crate::attempt::DurableActionKind::DispatchWake,
                &expectation,
            )?;
            match phase {
                crate::attempt::DurableActionPhase::Launching => {
                    let anchor = latest_durable_event(
                        events,
                        crate::attempt::DurableActionKind::DispatchWake,
                        &expectation,
                    )
                    .context("dispatch wake delivery lost owner claim")?;
                    let (anchor_owner, anchor_generation) =
                        durable_anchor_owner_gen(anchor, "DispatchWake delivery")?;
                    if anchor_owner != *owned_by || anchor_generation != *owned_generation {
                        bail!("dispatch wake delivery owner/generation changed");
                    }
                    // B110：Delivered 固化精确通道事实——只表示外部 spawn 成功，
                    // 不得冒充 Engaged（咬合由精确日志窗口另判）。
                    let mut payload = durable_wake_payload(
                        attempt,
                        agent,
                        &base_sha,
                        go_rel,
                        owned_action,
                        owned_generation,
                        owned_by,
                    );
                    facts.insert_channel_fields(&mut payload);
                    Ok(vec![ledger::event(
                        "DispatchWakeDelivered",
                        "runtime:orch",
                        Some(task_id),
                        Some(round),
                        payload,
                    )])
                }
                crate::attempt::DurableActionPhase::Delivered => Ok(Vec::new()),
                other => bail!("dispatch wake delivery 前 fold 阶段非法: {other:?}"),
            }
        })?;
        if delivered > 1 {
            bail!("DispatchWakeDelivered append count mismatch: {delivered}");
        }
    }

    hook("after-dispatch-wake-delivered")?;
    critical_point(require_active_tierf_task(root, round, task_id))?;
    let appended = ledger::append_checked(root, round, |events| {
        let ctx = crate::attempt::resolve_current_dispatch(events, task_id, round)?;
        if ctx.attempt_id.as_deref() != Some(attempt.attempt_id.as_str())
            || ctx.attempt_no != Some(attempt.ordinal)
            || ctx.agent.as_deref() != Some(agent)
            || ctx.base_sha.as_deref() != Some(base_sha.as_str())
            || ctx.go_path.as_deref() != Some(go_rel)
        {
            bail!("wake completion attempt changed after delivery");
        }
        // fold-first：Delivered → Completed（沿用 lineage owner/generation；
        // 幂等 Completed 直接放行）。Delivered-replay 与 Owned 路径统一走此处。
        let expectation = wake_expectation(
            round,
            task_id,
            &attempt.attempt_id,
            attempt.ordinal,
            agent,
            &base_sha,
            go_rel,
            &action_id,
        );
        let phase = crate::attempt::fold_durable_action(
            events,
            crate::attempt::DurableActionKind::DispatchWake,
            &expectation,
        )?;
        match phase {
            crate::attempt::DurableActionPhase::Delivered => {
                let anchor = latest_durable_event(
                    events,
                    crate::attempt::DurableActionKind::DispatchWake,
                    &expectation,
                )
                .context("dispatch wake completion 缺 Delivered 锚点")?;
                let (anchor_owner, anchor_generation) =
                    durable_anchor_owner_gen(anchor, "DispatchWake completion")?;
                // B110：Completed 从 Delivered 锚点继承精确通道事实——同 action
                // 状态事件身份相同由构造保证；锚点缺字段 fail-closed。
                let mut payload = durable_wake_payload(
                    attempt,
                    agent,
                    &base_sha,
                    go_rel,
                    &action_id,
                    &anchor_generation,
                    &anchor_owner,
                );
                inherit_channel_facts(anchor, &mut payload, "DispatchWake completion")?;
                Ok(vec![ledger::event(
                    "DispatchWakeCompleted",
                    "runtime:orch",
                    Some(task_id),
                    Some(round),
                    payload,
                )])
            }
            crate::attempt::DurableActionPhase::Completed => Ok(Vec::new()),
            other => bail!("dispatch wake completion 前 fold 阶段非法: {other:?}"),
        }
    })?;
    if appended > 1 {
        bail!("wake completion append count mismatch: {appended}");
    }
    // The externally visible spawn is durably anchored before this fallible IO.
    // A heartbeat write failure cannot release the launch or authorize a
    // duplicate injection.  Completed replay returns above and deliberately
    // does not retry this best-effort projection write.
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let fresh = read_ledger(&ledger_path)?;
    crate::attempt::reject_bad_lines(&fresh)?;
    let facts = current_attempt_channel_facts(&fresh.events, &attempt.attempt_id)
        .context("completed dispatch wake missing exact channel facts")?;
    wake::write_working_heartbeat(root, agent, facts.pid, round, &attempt.attempt_id)?;
    Ok(())
}

fn claim_observed_dispatch_ack(
    root: &Path,
    round: &str,
    task_id: &str,
    attempt_id: &str,
    attempt_no: usize,
    agent: &str,
    ack_evidence: &crate::attempt::EvidenceObservation,
    hook: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<bool> {
    let task_id_owned = task_id.to_string();
    let round_owned = round.to_string();
    let obs = crate::attempt::AttemptObservation {
        attempt_id: Some(attempt_id.to_string()),
        attempt_no: Some(attempt_no),
    };
    let ack_age = SystemTime::now()
        .duration_since(ack_evidence.mtime)
        .ok()
        .map(|age| age.as_secs());
    let outcome = with_active_tierf_effect(
        root,
        round,
        task_id,
        "await ack observation",
        |_active_card| {
            hook("before-ack-claim")?;
            crate::attempt::claim_attempt_action(
                root,
                round,
                task_id,
                &obs,
                Some(ack_evidence),
                &|events, fresh_ctx, _fresh_marker, claimed, _epoch| {
                    let claimed = claimed.context("DispatchAcked claim 缺 evidence")?;
                    let fresh_aid = fresh_ctx
                        .attempt_id
                        .as_deref()
                        .context("DispatchAcked 缺 attemptId")?;
                    let exact_ack = events
                        .iter()
                        .filter(|event| {
                            evidence_event_matches(
                                event,
                                "DispatchAcked",
                                "dispatch-ack",
                                fresh_aid,
                                claimed,
                            )
                        })
                        .count();
                    if exact_ack > 1 {
                        bail!("DispatchAcked exact evidence 重复 ({exact_ack})");
                    }
                    let started = crate::attempt::attempt_event_already_recorded(
                        events,
                        fresh_aid,
                        "AttemptStarted",
                    );
                    let mut evs = Vec::new();
                    if exact_ack == 0 {
                        evs.push(ledger::event(
                            "DispatchAcked",
                            "runtime:orch",
                            Some(&task_id_owned),
                            Some(&round_owned),
                            serde_json::json!({
                                "actionId": "dispatch-ack",
                                "attemptId": fresh_aid,
                                "attemptNo": fresh_ctx.attempt_no,
                                "agent": fresh_ctx.agent,
                                "ackAgeSecs": ack_age,
                                "evidencePath": claimed.canonical_path,
                                "evidenceSha256": claimed.sha256,
                                "evidenceLen": claimed.len,
                                "controlEpoch": claimed.control_epoch,
                            }),
                        ));
                    }
                    if !started {
                        evs.push(ledger::event(
                            "AttemptStarted",
                            "runtime:orch",
                            Some(&task_id_owned),
                            Some(&round_owned),
                            serde_json::json!({
                                "attemptId": fresh_aid,
                                "attemptNo": fresh_ctx.attempt_no,
                                "agent": fresh_ctx.agent,
                            }),
                        ));
                    }
                    if evs.is_empty() {
                        Ok(crate::attempt::ClaimDecision::AlreadyPresent)
                    } else {
                        Ok(crate::attempt::ClaimDecision::Append(evs))
                    }
                },
            )
        },
    )?;

    match outcome {
        crate::attempt::ClaimOutcome::Appended { .. }
        | crate::attempt::ClaimOutcome::AlreadyPresent { .. } => {
            println!("· GO 已被 {agent} 消费（.ack 观察到）");
            Ok(true)
        }
        _ => Ok(false),
    }
}

struct CollectGateObservation<'a> {
    command_ref: &'a str,
    gate_run_id: &'a str,
    exit_code: i32,
    duration_ms: u64,
    subject_tree_sha: &'a str,
    log_sha256: &'a str,
    log_bytes: u64,
    toolchain_digest: &'a str,
    environment_digest: &'a str,
}

const COLLECT_GATE_EXECUTED_KEYS: &[&str] = &[
    "commandRef",
    "phase",
    "gateRunId",
    "exitCode",
    "durationMs",
    "subjectTreeSha",
    "logSha256",
    "logBytes",
    "toolchainDigest",
    "environmentDigest",
];

fn is_collect_gate_event(event: &EventRecord, task_id: &str, round: &str) -> bool {
    event.kind == "GateExecuted"
        && event.task_id.as_deref() == Some(task_id)
        && event.round.as_deref() == Some(round)
        && event
            .payload
            .as_ref()
            .and_then(|payload| payload.get("phase"))
            .and_then(serde_json::Value::as_str)
            == Some("collect")
}

fn collect_gate_observation(event: &EventRecord) -> Result<CollectGateObservation<'_>> {
    let payload = event.payload.as_ref().context("GateExecuted 缺 payload")?;
    let object = payload
        .as_object()
        .context("GateExecuted payload 非 object")?;
    if object.len() != COLLECT_GATE_EXECUTED_KEYS.len()
        || object
            .keys()
            .any(|key| !COLLECT_GATE_EXECUTED_KEYS.contains(&key.as_str()))
    {
        bail!("GateExecuted payload 必须是精确十键 schema");
    }
    let string = |key: &str| {
        payload
            .get(key)
            .and_then(serde_json::Value::as_str)
            .with_context(|| format!("GateExecuted 缺 {key}"))
    };
    let gate_run_id = string("gateRunId")?;
    let toolchain_digest = string("toolchainDigest")?;
    let environment_digest = string("environmentDigest")?;
    if event.actor != "runtime:orch"
        || payload.get("phase").and_then(serde_json::Value::as_str) != Some("collect")
        || gate_run_id.is_empty()
        || !valid_sha256(toolchain_digest)
        || !valid_sha256(environment_digest)
    {
        bail!("GateExecuted actor/phase/run/digest 非 canonical collect observation");
    }
    Ok(CollectGateObservation {
        command_ref: string("commandRef")?,
        gate_run_id,
        exit_code: payload
            .get("exitCode")
            .and_then(serde_json::Value::as_i64)
            .and_then(|value| i32::try_from(value).ok())
            .context("GateExecuted 缺合法 exitCode")?,
        duration_ms: payload
            .get("durationMs")
            .and_then(serde_json::Value::as_u64)
            .context("GateExecuted 缺 durationMs")?,
        subject_tree_sha: string("subjectTreeSha")?,
        log_sha256: string("logSha256")?,
        log_bytes: payload
            .get("logBytes")
            .and_then(serde_json::Value::as_u64)
            .context("GateExecuted 缺 logBytes")?,
        toolchain_digest,
        environment_digest,
    })
}

fn observation_matches_summary(
    observed: &CollectGateObservation<'_>,
    summary: &ActualGateSummary,
    command_ref: &str,
    subject_tree_sha: &str,
) -> bool {
    observed.command_ref == command_ref
        && summary.command_ref == command_ref
        && observed.exit_code == summary.exit_code
        && observed.duration_ms == summary.duration_ms
        && observed.subject_tree_sha == subject_tree_sha
        && observed.log_sha256 == summary.raw_log_cas_sha256
        && observed.log_bytes == summary.raw_log_len
        && summary.exit_code == 0
}

fn bind_shared_digest(slot: &mut Option<String>, observed: &str, label: &str) -> Result<()> {
    match slot.as_deref() {
        Some(expected) if expected != observed => bail!("collect gates 的 {label}Digest 不一致"),
        None => *slot = Some(observed.to_string()),
        _ => {}
    }
    Ok(())
}

fn receipt_payload_matches_attestation(
    payload: &serde_json::Value,
    attestation: &CollectReceiptAttestation,
) -> Result<bool> {
    let common = payload.get("gateCount").and_then(serde_json::Value::as_u64)
        == Some(attestation.configured_gate_count)
        && payload.get("configuredCommandRefs")
            == Some(&serde_json::to_value(&attestation.configured_command_refs)?)
        && payload.get("gates") == Some(&serde_json::to_value(&attestation.gates)?)
        && payload
            .get("toolchainDigest")
            .and_then(serde_json::Value::as_str)
            == Some(attestation.toolchain_digest.as_str())
        && payload
            .get("environmentDigest")
            .and_then(serde_json::Value::as_str)
            == Some(attestation.environment_digest.as_str());
    if !common {
        return Ok(false);
    }
    if attestation.attestation_version == 1 {
        return Ok(true);
    }
    Ok(payload
        .get("subjectTreeSha")
        .and_then(serde_json::Value::as_str)
        == Some(attestation.subject_tree_sha.as_str())
        && payload
            .get("irRevision")
            .and_then(serde_json::Value::as_u64)
            == Some(attestation.ir_revision as u64)
        && payload
            .get("validationDigest")
            .and_then(serde_json::Value::as_str)
            == Some(attestation.validation_digest.as_str())
        && payload
            .get("bindingSha256")
            .and_then(serde_json::Value::as_str)
            == Some(attestation.binding_sha256.as_str())
        && payload
            .get("taskCardSha256")
            .and_then(serde_json::Value::as_str)
            == Some(attestation.task_card_sha256.as_str())
        && payload
            .get("resolvedCommandDigest")
            .and_then(serde_json::Value::as_str)
            == Some(attestation.resolved_command_digest.as_str())
        && payload
            .get("sourceReaderDescriptorSha256")
            .and_then(serde_json::Value::as_str)
            == attestation.source_reader_descriptor_sha256.as_deref()
        && payload
            .get("sourceReaderBaseSha256")
            .and_then(serde_json::Value::as_str)
            == attestation.source_reader_base_sha256.as_deref())
}

fn observation_matches_attestation(
    observed: &CollectGateObservation<'_>,
    gate: &CollectAttestedGate,
    command_ref: &str,
    subject_tree_sha: &str,
    toolchain_digest: &str,
    environment_digest: &str,
) -> bool {
    observed.gate_run_id == gate.gate_run_id
        && gate.command_ref == command_ref
        && gate.exit_code == 0
        && observed.command_ref == command_ref
        && observed.exit_code == 0
        && observed.duration_ms == gate.duration_ms
        && observed.toolchain_digest == toolchain_digest
        && observed.environment_digest == environment_digest
        && observed.subject_tree_sha == subject_tree_sha
        && observed.log_sha256 == gate.raw_log_cas_sha256
        && observed.log_bytes == gate.raw_log_len
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AwaitExpiryDisposition {
    ObserverExpired {
        observer_secs: u64,
        runtime_limit_secs: u64,
    },
    RuntimeDeadlineElapsed {
        runtime_limit_secs: u64,
    },
}

pub fn await_expiry_disposition(
    elapsed_secs: u64,
    observer_timeout_secs: u64,
    runtime_limit_secs: Option<u64>,
) -> AwaitExpiryDisposition {
    match runtime_limit_secs {
        Some(runtime_limit_secs) if elapsed_secs >= runtime_limit_secs => {
            AwaitExpiryDisposition::RuntimeDeadlineElapsed { runtime_limit_secs }
        }
        runtime_limit_secs => AwaitExpiryDisposition::ObserverExpired {
            observer_secs: observer_timeout_secs,
            runtime_limit_secs: runtime_limit_secs.unwrap_or(0),
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AuthenticatedRuntimeDeadline {
    runtime_limit_secs: u64,
    wake_elapsed: Duration,
}

/// Read only the exact current implementation wake. The newest matching event
/// wins before payload parsing, so malformed or unauthenticated replacement
/// facts fail closed to `None` instead of falling back to an older deadline.
/// Runtime elapsed is anchored to that wake's durable timestamp, never to the
/// older `DispatchIssued` timestamp used by liveness.
fn authenticated_runtime_deadline(
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
    agent: &str,
    now_ts: &str,
) -> Option<AuthenticatedRuntimeDeadline> {
    let continuation_id = format!("implementation:{round}:{task_id}:{attempt_id}:{agent}");
    let event = events.iter().rev().find(|event| {
        let payload = event.payload.as_ref();
        event.kind == "WakeIssued"
            && event.actor == "runtime:orch"
            && event.round.as_deref() == Some(round)
            && event.task_id.as_deref() == Some(task_id)
            && payload
                .and_then(|value| value.get("attemptId"))
                .and_then(serde_json::Value::as_str)
                == Some(attempt_id)
            && payload
                .and_then(|value| value.get("agent"))
                .and_then(serde_json::Value::as_str)
                == Some(agent)
            && payload
                .and_then(|value| value.get("continuationId"))
                .and_then(serde_json::Value::as_str)
                == Some(continuation_id.as_str())
    })?;
    let payload = event.payload.as_ref()?;
    let wake_id = payload.get("wakeId")?.as_str()?;
    if payload.get("controlWakeId")?.as_str()? != wake_id {
        return None;
    }
    let runtime_limit: wake::ManagedWakeRuntimeLimit =
        serde_json::from_value(payload.get("runtimeLimit")?.clone()).ok()?;
    let canonical = wake::managed_wake_runtime_limit(runtime_limit.requested_review_deadline_secs);
    if runtime_limit != canonical || runtime_limit.effective_secs == 0 {
        return None;
    }
    let wake_elapsed = liveness::attempt_elapsed_from_dispatch(&event.ts, now_ts).ok()?;
    Some(AuthenticatedRuntimeDeadline {
        runtime_limit_secs: runtime_limit.effective_secs,
        wake_elapsed,
    })
}

fn observer_expiry_event_matches(
    event: &EventRecord,
    attempt_id: &str,
    control_epoch: &str,
    observer_secs: u64,
    runtime_limit_secs: u64,
) -> bool {
    action_event_matches(
        event,
        "ReportAwaitExpired",
        "report-await-expired",
        attempt_id,
        control_epoch,
    ) && event.payload.as_ref().is_some_and(|payload| {
        payload
            .get("observerSecs")
            .and_then(serde_json::Value::as_u64)
            == Some(observer_secs)
            && payload
                .get("runtimeLimitSecs")
                .and_then(serde_json::Value::as_u64)
                == Some(runtime_limit_secs)
    })
}

fn run_await_with_hook_inner_impl(
    root: &Path,
    task_id: &str,
    timeout_secs: u64,
    live: Option<liveness::LivenessOpts>,
    hook: &mut dyn FnMut(&str) -> Result<()>,
    round: &str,
) -> Result<AwaitOutcome> {
    let mut runtime = ProductionAwaitReportRuntime::default();
    run_await_with_hook_inner_with_runtime(
        root,
        task_id,
        timeout_secs,
        live,
        hook,
        round,
        &mut runtime,
    )
}

fn run_await_with_hook_inner_with_runtime<R: AwaitReportRuntime>(
    root: &Path,
    task_id: &str,
    timeout_secs: u64,
    live: Option<liveness::LivenessOpts>,
    hook: &mut dyn FnMut(&str) -> Result<()>,
    round: &str,
    runtime: &mut R,
) -> Result<AwaitOutcome> {
    let c = require_await_entry_card(root, round, task_id)?;
    let branch = format!("task/{task_id}");

    let report_rel = format!("coordination/rounds/{round}/reports/{task_id}-REPORT.md");
    let blocked_rel = format!("coordination/rounds/{round}/reports/{task_id}-BLOCKED.md");

    let candidates: Vec<PathBuf> = vec![
        root.join(&report_rel),
        root.join(".worktrees").join(task_id).join(&report_rel),
    ];
    let blocked_candidates: Vec<PathBuf> = vec![
        root.join(&blocked_rel),
        root.join(".worktrees").join(task_id).join(&blocked_rel),
    ];
    let start = Instant::now();
    let tick = AWAIT_REPORT_RECONCILE_TICK;
    let mut last_probe: Option<Instant> = None;
    let mut candidate_streak: (u8, &'static str) = (0, "");
    let mut durable_log_sample: Option<crate::chanhealth::WakeLogSample> = None;

    let mut tracked_attempt_id: Option<String> = None;
    let mut ack_logged = false;
    // B112-A0004：attempt 时钟 = Some((durable_base, captured_at))。durable_base 从该
    // attempt 最新 DispatchIssued.ts 恢复（账本派生的 Duration 数值），captured_at
    // 为本进程恢复时刻；当前 elapsed 一律由 liveness::attempt_current_elapsed 计算
    // （durable_base + captured_at.elapsed()，timeout 与 liveness 同源）。durable
    // elapsed 可合法大于 monotonic uptime，不得用可能下溢的 Instant 表示——旧实现
    // `Instant::now().checked_sub(elapsed).unwrap_or_else(Instant::now)` 在刚开机
    // 恢复长期 attempt 时把 grace 静默清零，已拆除。None = 账本无 DispatchIssued
    // （或首帧尚未恢复）→ 用 start（本次 await 会话起点）进程内计时。
    let mut attempt_clock: Option<(Duration, Instant)> = None;
    // B112：标记首帧已做 durable 恢复，避免重复读账本前重置。
    let mut attempt_clock_resolved = false;
    // Observer timeout belongs to this `await-report` invocation, not to the
    // attempt's durable age. Keep its deadline expressed on the current
    // attempt clock so the frozen timeout-block source anchor remains useful;
    // on an attempt switch, recompute it from the invocation's remaining time.
    let mut observer_deadline_on_attempt_clock: Option<Duration> = None;

    let watch_plan = await_report_watch_plan(root, round, task_id);
    let (wake_sender, fs_rx) = await_report_wake_channel();
    let callback_plan = watch_plan.clone();
    // A permanently degraded notify backend must only accelerate one final
    // reconciliation. The production loop observes this sticky bit, drops the
    // watcher once, and remains polling-only for the rest of the process.
    let watcher_failed = Arc::new(AtomicBool::new(false));
    let callback_watcher_failed = Arc::clone(&watcher_failed);
    let mut watcher_callback: Option<AwaitReportNotifyCallback> =
        Some(Box::new(move |result: notify::Result<notify::Event>| {
            let relevant = match result {
                Ok(event) if event.need_rescan() => {
                    await_report_event_is_relevant(&callback_plan, AwaitReportWatchNotice::Rescan)
                }
                Ok(event) => await_report_event_is_relevant(
                    &callback_plan,
                    AwaitReportWatchNotice::Paths(&event.paths),
                ),
                Err(_) => {
                    if callback_watcher_failed
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                    {
                        return;
                    }
                    await_report_event_is_relevant(
                        &callback_plan,
                        AwaitReportWatchNotice::WatcherError,
                    )
                }
            };
            if relevant {
                wake_sender.signal();
            }
        }));
    let mut watched = BTreeSet::new();
    let mut watcher_disabled = false;

    // Loop: returns either (found_report_path, observed attempt+mtime, fresh ctx from claim)
    // or a terminal AwaitOutcome.
    'await_poll_loop: loop {
        let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
        let lr_fresh = read_ledger(&ledger_path)
            .with_context(|| format!("轮询时重读账本失败: {}", ledger_path.display()))?;
        crate::attempt::reject_bad_lines(&lr_fresh)?;
        let ctx = crate::attempt::resolve_current_dispatch(&lr_fresh.events, task_id, &round)?;
        let cur_agent = ctx.agent.clone();
        let cur_attempt_id = ctx.attempt_id.clone();
        let cur_attempt_no = ctx.attempt_no;
        let strict_attempt = crate::attempt::AttemptRef {
            task_id: task_id.to_string(),
            ordinal: cur_attempt_no.context("await-report 缺 current attemptNo")?,
            attempt_id: cur_attempt_id
                .clone()
                .context("await-report 缺 current attemptId")?,
        };
        let cur_go_abs = crate::attempt::resolve_go_path_strict(
            root,
            ctx.go_path
                .as_deref()
                .context("await-report 缺 current goPath")?,
            &round,
            ctx.agent
                .as_deref()
                .context("await-report 缺 current agent")?,
            &strict_attempt,
            ctx.is_legacy.context("await-report 缺 legacy identity")?,
        )?;
        let ack_evidence_observation = if crate::attempt::ack_observed_strict(&cur_go_abs)? {
            let ack_path = crate::attempt::resolve_ack_path_strict(&cur_go_abs)?;
            crate::attempt::observe_evidence(&ack_path)?
        } else {
            None
        };
        // A disappearance between strict slot observation and byte capture is
        // a normal "not current yet" poll result, not a fatal IO interruption.
        let ack_observed = ack_evidence_observation.is_some();

        // reset per-attempt liveness state
        if tracked_attempt_id != cur_attempt_id || !attempt_clock_resolved {
            tracked_attempt_id = cur_attempt_id.clone();
            ack_logged = cur_attempt_id.as_ref().is_some_and(|aid| {
                crate::attempt::attempt_event_already_recorded(
                    &lr_fresh.events,
                    aid,
                    "DispatchAcked",
                )
            });
            candidate_streak = (0, "");
            durable_log_sample = None;
            // B112-A0004：从该 attempt 最新 DispatchIssued.ts 恢复 durable_base
            // （Duration 数值，不构造可能下溢的 Instant），captured_at 记录本进程
            // 恢复时刻；此后当前 elapsed = durable_base + captured_at.elapsed()，
            // daemon/await 重启（含刚开机 durable > uptime）不重置 boot/cold grace，
            // 且恢复后继续增长、不冻结为快照。时间倒退/坏 ts →
            // attempt_elapsed_from_dispatch 返回 Err → fail-closed bail（留可诊断
            // 错误）。账本无 DispatchIssued（None）→ attempt_clock=None（无派发则
            // 无 durable elapsed，退化为会话进程内计时）。
            attempt_clock = match crate::attempt::latest_dispatch_ts(&lr_fresh.events, task_id) {
                Some(dispatch_ts) => {
                    let now_ts = crate::ledger::now_rfc3339();
                    let durable_base =
                        liveness::attempt_elapsed_from_dispatch(&dispatch_ts, &now_ts)
                            .with_context(|| {
                                format!(
                                    "await-report 恢复 attempt durable elapsed 失败: \
                             dispatch_ts={dispatch_ts} now_ts={now_ts}"
                                )
                            })?;
                    Some((durable_base, Instant::now()))
                }
                None => None,
            };
            let observer_remaining =
                Duration::from_secs(timeout_secs).saturating_sub(start.elapsed());
            observer_deadline_on_attempt_clock = Some(
                liveness::attempt_current_elapsed(attempt_clock, start)
                    .saturating_add(observer_remaining),
            );
            attempt_clock_resolved = true;
            last_probe = None;
        }

        // ACK durability is part of the await pipeline itself, not an optional
        // liveness side effect. Claim it before watcher setup, filesystem
        // probing, liveness, receive, or sleep edges on every frame.
        // The first exact ledger/dispatch/GO/ACK frame above is the durable
        // bootstrap boundary. Optional watcher construction and filesystem
        // setup happen once, only after that frame has either claimed a fresh
        // ACK or established that no ACK exists yet. Reconciliation also
        // handles dynamic worktree watcher installation without creating a
        // future worktree path; failed registration leaves polling in charge.
        ack_logged = reconcile_await_bootstrap(
            root,
            round,
            task_id,
            &strict_attempt,
            cur_agent.as_deref(),
            ack_evidence_observation.as_ref(),
            ack_observed,
            ack_logged,
            hook,
            runtime,
            &mut watcher_callback,
            &watcher_failed,
            &mut watcher_disabled,
            &watch_plan,
            &mut watched,
        )?;

        // fresh marker (for evidence selection only)
        let sig = crate::attempt::latest_attempt_signal_precise(&lr_fresh.events, task_id);
        let go_mtime = Some(&cur_go_abs).and_then(|full| {
            std::fs::symlink_metadata(full)
                .ok()
                .filter(|m| m.file_type().is_file() && !m.file_type().is_symlink())
                .and_then(|m| m.modified().ok())
        });
        let marker = crate::attempt::compute_evidence_marker(sig, go_mtime);

        // REPORT candidates
        let report_observations: Vec<(PathBuf, Option<crate::attempt::EvidenceObservation>)> =
            candidates
                .iter()
                .map(|path| {
                    crate::attempt::observe_evidence(path).map(|observed| (path.clone(), observed))
                })
                .collect::<Result<_>>()?;
        let report_candidates_with_mtime: Vec<(PathBuf, Option<SystemTime>)> = report_observations
            .iter()
            .map(|(path, observed)| (path.clone(), observed.as_ref().map(|value| value.mtime)))
            .collect();
        let report_found =
            crate::attempt::select_current_evidence_path(&report_candidates_with_mtime, marker);

        // BLOCKED candidates
        let blocked_observations: Vec<(PathBuf, Option<crate::attempt::EvidenceObservation>)> =
            blocked_candidates
                .iter()
                .map(|path| {
                    crate::attempt::observe_evidence(path).map(|observed| (path.clone(), observed))
                })
                .collect::<Result<_>>()?;
        let blocked_candidates_with_mtime: Vec<(PathBuf, Option<SystemTime>)> =
            blocked_observations
                .iter()
                .map(|(path, observed)| (path.clone(), observed.as_ref().map(|value| value.mtime)))
                .collect();
        let blocked_any = blocked_observations
            .iter()
            .any(|(_, observed)| observed.is_some());
        let current_blocked = select_current_blocked_path(&blocked_candidates_with_mtime, marker);
        let report_was_hard_rejected = report_found
            .as_ref()
            .and_then(|found| {
                report_observations
                    .iter()
                    .find(|(path, _)| path == found)
                    .and_then(|(_, observed)| observed.as_ref())
            })
            .is_some_and(|report| {
                report_collect_hard_rejected(
                    &lr_fresh.events,
                    &round,
                    task_id,
                    &strict_attempt,
                    report,
                )
            });

        match await_poll_with_escape(
            report_found.is_some(),
            blocked_any,
            report_was_hard_rejected,
            current_blocked.is_some(),
        ) {
            AwaitPoll::ReportFound => 'report_found: {
                let found = report_found.expect("ReportFound requires candidate");
                let ev_obs = report_observations
                    .iter()
                    .find(|(path, _)| path == &found)
                    .and_then(|(_, observed)| observed.clone())
                    .context("REPORT observation vanished before claim")?;
                let already_recorded = cur_attempt_id
                    .as_deref()
                    .zip(cur_agent.as_deref())
                    .is_some_and(|(attempt_id, agent)| {
                        crate::mech::report_identity_observation_already_recorded(
                            root,
                            &lr_fresh.events,
                            &round,
                            task_id,
                            attempt_id,
                            agent,
                            &ev_obs,
                        )
                    });
                let report_decl = std::str::from_utf8(&ev_obs.bytes)
                    .ok()
                    .and_then(crate::mech::report_env_decl);
                let identity_audit = if already_recorded {
                    None
                } else {
                    match (
                        report_decl.as_ref(),
                        cur_attempt_id.as_deref(),
                        cur_agent.as_deref(),
                    ) {
                        (Some(decl), Some(attempt_id), Some(agent)) => {
                            Some(crate::mech::audit_report_identity(
                                root,
                                &lr_fresh.events,
                                &round,
                                task_id,
                                attempt_id,
                                agent,
                                decl,
                            )?)
                        }
                        _ => None,
                    }
                };
                if identity_audit
                    .as_ref()
                    .is_some_and(|audit| !crate::mech::identity_audit_evidence_ready(audit))
                {
                    break 'report_found;
                }
                // B: claim REPORT before env/mech/collect
                let obs = crate::attempt::AttemptObservation {
                    attempt_id: cur_attempt_id.clone(),
                    attempt_no: cur_attempt_no,
                };
                let task_id_owned = task_id.to_string();
                let round_owned = round.to_string();
                let waited_secs = start.elapsed().as_secs();
                const IDENTITY_AUDIT_RETRY: &str = "ReportObserved identity audit retry";
                let outcome = match with_active_tierf_effect(
                    root,
                    round,
                    task_id,
                    "await report observation",
                    |_active_card| {
                        hook("before-report-claim")?;
                        crate::attempt::claim_attempt_action(
                            root,
                            &round,
                            task_id,
                            &obs,
                            Some(&ev_obs),
                            &|events, fresh_ctx, _fresh_marker, claimed, _epoch| {
                                let claimed =
                                    claimed.context("ReportObserved claim 缺 evidence")?;
                                let aid = fresh_ctx
                                    .attempt_id
                                    .as_deref()
                                    .context("ReportObserved 缺 attemptId")?;
                                let agent = fresh_ctx
                                    .agent
                                    .as_deref()
                                    .context("ReportObserved 模型勾稽缺 agent")?;
                                let matches = events
                                    .iter()
                                    .filter(|event| {
                                        evidence_event_matches(
                                            event,
                                            "ReportObserved",
                                            "report-observed",
                                            aid,
                                            claimed,
                                        )
                                    })
                                    .collect::<Vec<_>>();
                                match matches.len() {
                                    1 => {
                                        if !crate::mech::recorded_identity_audit_still_current(
                                            root,
                                            matches[0],
                                            events,
                                            &round_owned,
                                            &task_id_owned,
                                            aid,
                                            agent,
                                        ) {
                                            bail!("ReportObserved 已存模型勾稽锚点缺失或过期，拒绝 replay")
                                        }
                                        return Ok(crate::attempt::ClaimDecision::AlreadyPresent);
                                    }
                                    0 => {}
                                    count => bail!("ReportObserved exact evidence 重复 ({count})"),
                                }
                                let report_text = std::str::from_utf8(&claimed.bytes)
                                    .context("ReportObserved evidence 不是 UTF-8")?;
                                let report_decl = crate::mech::report_env_decl(report_text);
                                if let Some(audit) = identity_audit.as_ref() {
                                    if !crate::mech::identity_audit_still_current(
                                        root,
                                        audit,
                                        events,
                                        &round_owned,
                                        &task_id_owned,
                                        aid,
                                        agent,
                                    ) {
                                        bail!(IDENTITY_AUDIT_RETRY)
                                    }
                                }
                                let mut payload = serde_json::json!({
                                        "actionId": "report-observed",
                                        "attemptId": aid,
                                        "attemptNo": fresh_ctx.attempt_no,
                                        "reportPath": found.display().to_string(),
                                        "waitedSecs": waited_secs,
                                        "evidencePath": claimed.canonical_path,
                                        "evidenceSha256": claimed.sha256,
                                        "evidenceLen": claimed.len,
                                        "controlEpoch": claimed.control_epoch,
                                });
                                if let Some(decl) = report_decl {
                                    payload["envDecl"] = crate::mech::env_decl_payload(&decl);
                                    if let Some(audit) = identity_audit.as_ref() {
                                        payload["identityReconciliation"] = audit.payload.clone();
                                    }
                                }
                                let report_event = ledger::event(
                                    "ReportObserved",
                                    "runtime:orch",
                                    Some(&task_id_owned),
                                    Some(&round_owned),
                                    payload,
                                );
                                let mut events = vec![report_event];
                                if let Some(failure) = identity_audit.as_ref().and_then(|audit| {
                                    crate::mech::identity_failure_event(
                                        &task_id_owned,
                                        &round_owned,
                                        aid,
                                        fresh_ctx.attempt_no,
                                        audit,
                                    )
                                }) {
                                    events.push(failure);
                                }
                                Ok(crate::attempt::ClaimDecision::Append(events))
                            },
                        )
                    },
                ) {
                    Ok(outcome) => outcome,
                    Err(error)
                        if error
                            .chain()
                            .any(|cause| cause.to_string() == IDENTITY_AUDIT_RETRY) =>
                    {
                        continue 'await_poll_loop;
                    }
                    Err(error) => return Err(error),
                };

                match outcome {
                    crate::attempt::ClaimOutcome::Appended { ctx, evidence, .. }
                    | crate::attempt::ClaimOutcome::AlreadyPresent { ctx, evidence } => {
                        let claimed = evidence.context("successful REPORT claim 缺 evidence")?;
                        println!(
                            "③ REPORT 已收（等待 {}s）: {}",
                            start.elapsed().as_secs(),
                            found.display()
                        );
                        // B107：生产注入真实 clock（Instant 起点）+ thread::sleep；
                        // grace 120s / poll 2s 常量在 wait_for_report_commit 调用点固定。
                        let wait_start = Instant::now();
                        let mut commit_grace_elapsed = move || wait_start.elapsed();
                        let mut commit_grace_sleeper: fn(Duration) = std::thread::sleep;
                        let co = with_active_tierf_effect(
                            root,
                            round,
                            task_id,
                            "await report collect",
                            |active_card| {
                                // ReportObserved is already durable here; collect preserves
                                // its exact evidence and publishes its own terminal boundary.
                                let co = collect_claimed_report(
                                    root,
                                    &round,
                                    task_id,
                                    active_card,
                                    &branch,
                                    &report_rel,
                                    &ctx,
                                    &claimed,
                                    &mut commit_grace_elapsed,
                                    &mut commit_grace_sleeper,
                                    hook,
                                )?;
                                append_tierf_cost_sample(root, &round, task_id, &ctx, waited_secs)?;
                                Ok(co)
                            },
                        )?;
                        return Ok(AwaitOutcome::Collected(co));
                    }
                    // Superseded / EvidenceNotCurrent — continue polling
                    _ => { /* continue loop */ }
                }
            }
            AwaitPoll::Blocked => {
                let selected = current_blocked;
                if let Some(found) = selected {
                    let blocked_rel_found = found
                        .strip_prefix(root)
                        .unwrap_or(&found)
                        .to_string_lossy()
                        .into_owned();
                    let ev_obs = blocked_observations
                        .iter()
                        .find(|(path, _)| path == &found)
                        .and_then(|(_, observed)| observed.clone())
                        .context("BLOCKED observation vanished before claim")?;
                    let task_id_owned = task_id.to_string();
                    let round_owned = round.to_string();
                    let aid_owned = cur_attempt_id.clone();
                    let no_owned = cur_attempt_no;

                    let obs = crate::attempt::AttemptObservation {
                        attempt_id: aid_owned.clone(),
                        attempt_no: no_owned,
                    };
                    let outcome = with_active_tierf_effect(
                        root,
                        round,
                        task_id,
                        "await blocked observation",
                        |_active_card| {
                            hook("before-blocked-claim")?;
                            crate::attempt::claim_attempt_action(
                                root,
                                &round,
                                task_id,
                                &obs,
                                Some(&ev_obs),
                                &|events, fresh_ctx, _fresh_marker, claimed, _epoch| {
                                    let claimed =
                                        claimed.context("AttemptBlocked claim 缺 evidence")?;
                                    let aid = fresh_ctx
                                        .attempt_id
                                        .as_deref()
                                        .context("AttemptBlocked 缺 attemptId")?;
                                    let matches = events
                                        .iter()
                                        .filter(|event| {
                                            evidence_event_matches(
                                                event,
                                                "AttemptBlocked",
                                                "attempt-blocked",
                                                aid,
                                                claimed,
                                            )
                                        })
                                        .count();
                                    match matches {
                                        1 => {
                                            return Ok(
                                                crate::attempt::ClaimDecision::AlreadyPresent,
                                            )
                                        }
                                        0 => {}
                                        count => {
                                            bail!("AttemptBlocked exact evidence 重复 ({count})")
                                        }
                                    }
                                    let fresh_agent = fresh_ctx.agent.as_deref();
                                    let mut evs = blocked_ledger_events(
                                        &task_id_owned,
                                        &round_owned,
                                        fresh_agent.as_deref(),
                                        &blocked_rel_found,
                                    );
                                    for event in evs.iter_mut() {
                                        if event.kind == "AttemptBlocked" {
                                            if let Some(payload) = event.payload.as_mut() {
                                                payload["actionId"] =
                                                    serde_json::json!("attempt-blocked");
                                                payload["attemptId"] = serde_json::json!(aid);
                                                payload["attemptNo"] =
                                                    serde_json::json!(fresh_ctx.attempt_no);
                                                payload["evidencePath"] =
                                                    serde_json::json!(claimed.canonical_path);
                                                payload["evidenceSha256"] =
                                                    serde_json::json!(claimed.sha256);
                                                payload["evidenceLen"] =
                                                    serde_json::json!(claimed.len);
                                                payload["controlEpoch"] =
                                                    serde_json::json!(claimed.control_epoch);
                                            }
                                        }
                                    }
                                    Ok(crate::attempt::ClaimDecision::Append(evs))
                                },
                            )
                        },
                    )?;

                    match outcome {
                        crate::attempt::ClaimOutcome::Appended { .. } => {
                            return Ok(AwaitOutcome::Blocked {
                                report_rel: blocked_rel_found,
                            });
                        }
                        crate::attempt::ClaimOutcome::AlreadyPresent { .. } => {
                            // D: zero notify/poke — just return terminal
                            return Ok(AwaitOutcome::Blocked {
                                report_rel: blocked_rel_found,
                            });
                        }
                        // Superseded / EvidenceNotCurrent — continue polling
                        _ => {}
                    }
                }
            }
            AwaitPoll::Waiting => {}
        }

        // Observer elapsed is scoped to this await invocation. Runtime elapsed
        // is independently anchored to the newest exact authenticated
        // implementation WakeIssued. Whichever deadline arrives first enters
        // the locked classification below.
        let now_ts = ledger::now_rfc3339();
        let runtime_deadline = cur_attempt_id
            .as_deref()
            .zip(cur_agent.as_deref())
            .and_then(|(attempt_id, agent)| {
                authenticated_runtime_deadline(
                    &lr_fresh.events,
                    round,
                    task_id,
                    attempt_id,
                    agent,
                    &now_ts,
                )
            });
        if liveness::attempt_current_elapsed(attempt_clock, start)
            >= observer_deadline_on_attempt_clock.unwrap_or(Duration::MAX)
            || runtime_deadline.is_some_and(|deadline| {
                deadline.wake_elapsed >= Duration::from_secs(deadline.runtime_limit_secs)
            })
        {
            let aid_owned = cur_attempt_id.clone();
            let task_id_owned = task_id.to_string();
            let round_owned = round.to_string();
            let obs = crate::attempt::AttemptObservation {
                attempt_id: aid_owned.clone(),
                attempt_no: cur_attempt_no,
            };
            let claimed_disposition = std::cell::Cell::new(None);
            let outcome = with_active_tierf_effect(
                root,
                round,
                task_id,
                "await expiry",
                |_active_card| {
                    crate::attempt::claim_attempt_action(
                        root,
                        &round,
                        task_id,
                        &obs,
                        None,
                        &|events, fresh_ctx, _fresh_marker, _claimed, epoch| {
                            let aid = fresh_ctx
                                .attempt_id
                                .as_deref()
                                .context("await expiry 缺 attemptId")?;
                            let agent = fresh_ctx
                                .agent
                                .as_deref()
                                .context("await expiry 缺 agent")?;
                            let fresh_now_ts = ledger::now_rfc3339();
                            let fresh_runtime_deadline = authenticated_runtime_deadline(
                                events,
                                round,
                                task_id,
                                aid,
                                agent,
                                &fresh_now_ts,
                            );
                            let observer_elapsed_secs = start.elapsed().as_secs();
                            let runtime_deadline_elapsed =
                                fresh_runtime_deadline.is_some_and(|deadline| {
                                    deadline.wake_elapsed
                                        >= Duration::from_secs(deadline.runtime_limit_secs)
                                });
                            if observer_elapsed_secs < timeout_secs && !runtime_deadline_elapsed {
                                // A same-attempt continuation may have installed a later
                                // authenticated deadline after the outer snapshot. Nothing
                                // is expired in the locked frame, so append no event and
                                // resume polling.
                                return Ok(crate::attempt::ClaimDecision::AlreadyPresent);
                            }
                            let fresh_runtime_limit_secs =
                                fresh_runtime_deadline.map(|deadline| deadline.runtime_limit_secs);
                            let elapsed_secs = fresh_runtime_deadline
                                .map_or(observer_elapsed_secs, |deadline| {
                                    deadline.wake_elapsed.as_secs()
                                });
                            let disposition = await_expiry_disposition(
                                elapsed_secs,
                                timeout_secs,
                                fresh_runtime_limit_secs,
                            );
                            claimed_disposition.set(Some(disposition));

                            match disposition {
                                AwaitExpiryDisposition::ObserverExpired {
                                    observer_secs,
                                    runtime_limit_secs,
                                } => {
                                    let matches = events
                                        .iter()
                                        .filter(|event| {
                                            observer_expiry_event_matches(
                                                event,
                                                aid,
                                                epoch,
                                                observer_secs,
                                                runtime_limit_secs,
                                            )
                                        })
                                        .count();
                                    match matches {
                                        1 => {
                                            return Ok(
                                                crate::attempt::ClaimDecision::AlreadyPresent,
                                            )
                                        }
                                        0 => {}
                                        count => {
                                            bail!("ReportAwaitExpired exact action 重复 ({count})")
                                        }
                                    }
                                    Ok(crate::attempt::ClaimDecision::Append(vec![ledger::event(
                                        "ReportAwaitExpired",
                                        "runtime:orch",
                                        Some(&task_id_owned),
                                        Some(&round_owned),
                                        serde_json::json!({
                                            "actionId": "report-await-expired",
                                            "controlEpoch": epoch,
                                            "observerSecs": observer_secs,
                                            "runtimeLimitSecs": runtime_limit_secs,
                                            "elapsedSecs": elapsed_secs,
                                            "attemptId": aid,
                                            "attemptNo": fresh_ctx.attempt_no,
                                        }),
                                    )]))
                                }
                                AwaitExpiryDisposition::RuntimeDeadlineElapsed {
                                    runtime_limit_secs,
                                } => {
                                    let matches = events
                                        .iter()
                                        .filter(|event| {
                                            action_event_matches(
                                                event,
                                                "AttemptTimedOut",
                                                "await-timeout",
                                                aid,
                                                epoch,
                                            )
                                        })
                                        .count();
                                    match matches {
                                        1 => {
                                            return Ok(
                                                crate::attempt::ClaimDecision::AlreadyPresent,
                                            )
                                        }
                                        0 => {}
                                        count => {
                                            bail!("await-timeout exact action 重复 ({count})")
                                        }
                                    }
                                    Ok(crate::attempt::ClaimDecision::Append(vec![
                                        ledger::event(
                                            "EscalationRaised",
                                            "runtime:orch",
                                            Some(&task_id_owned),
                                            Some(&round_owned),
                                            serde_json::json!({
                                                "stage": "await-report",
                                                "reason": format!(
                                                    "runtime deadline {runtime_limit_secs}s 无 REPORT"
                                                ),
                                                "hint": "核对原生进展与终态证据；由 planner 决定等待或显式接续",
                                            }),
                                        ),
                                        ledger::event(
                                            "AttemptTimedOut",
                                            "runtime:orch",
                                            Some(&task_id_owned),
                                            Some(&round_owned),
                                            serde_json::json!({
                                                "actionId": "await-timeout",
                                                "controlEpoch": epoch,
                                                "timeoutSecs": runtime_limit_secs,
                                                "observerSecs": timeout_secs,
                                                "runtimeLimitSecs": runtime_limit_secs,
                                                "lastSignal": "await-report 整段超时",
                                                "attemptId": aid,
                                                "attemptNo": fresh_ctx.attempt_no,
                                            }),
                                        ),
                                    ]))
                                }
                            }
                        },
                    )
                },
            )?;

            match outcome {
                crate::attempt::ClaimOutcome::Appended { .. }
                | crate::attempt::ClaimOutcome::AlreadyPresent { .. } => {
                    let Some(disposition) = claimed_disposition.get() else {
                        continue 'await_poll_loop;
                    };
                    match disposition {
                        AwaitExpiryDisposition::ObserverExpired { observer_secs, .. } => bail!(
                            "await-report 观察窗口到期（{observer_secs}s）：已落 ReportAwaitExpired（非终态）"
                        ),
                        AwaitExpiryDisposition::RuntimeDeadlineElapsed {
                            runtime_limit_secs,
                        } => bail!(
                            "await-report runtime deadline 到期（{runtime_limit_secs}s）：REPORT 未出现——已有 EscalationRaised + AttemptTimedOut"
                        ),
                    }
                }
                // Superseded / EvidenceNotCurrent — continue polling
                _ => {}
            }
        }

        // liveness probe
        if let (Some(o), Some(agent_str)) = (live.as_ref(), cur_agent.as_deref()) {
            if last_probe.is_none_or(|t| t.elapsed() >= o.probe_every) {
                last_probe = Some(Instant::now());
                // C: liveness probe uses absolute GO path
                let abs_go = Some(cur_go_abs.clone());
                // B112-A0004：durable elapsed 与上方 timeout 同一计算
                // （durable_base + captured_at.elapsed()）；Some 时直接进入快照，
                // 绕过 started.elapsed()；None（无派发）⇒ probe 内部回退
                // start.elapsed() 进程内计时。
                let durable_elapsed = attempt_clock
                    .map(|clock| liveness::attempt_current_elapsed(Some(clock), start));
                let provider_output_age = cur_attempt_id.as_deref().and_then(|attempt_id| {
                    crate::snapshot::provider_output_age(
                        root,
                        &lr_fresh.events,
                        task_id,
                        agent_str,
                        attempt_id,
                    )
                });
                before_await_liveness_probe(hook)?;
                let mut snap = liveness::probe_with_durable_elapsed(
                    root,
                    agent_str,
                    task_id,
                    abs_go.as_deref(),
                    &c.meta.write_set,
                    start,
                    durable_elapsed,
                );
                snap.last_activity_age =
                    liveness::working_idle_age(None, snap.last_activity_age, provider_output_age);
                // B112：解析心跳的现代身份（round/generation/ordinal），供
                // judge_with_identity 与当前现代 dispatch 身份比对。字段缺失/类型
                // 错/无心跳 → None（fail-closed：identity 缺失不得形成 fresh）。
                let hb_identity = liveness::probe_heartbeat_identity(root, agent_str);

                let durable_alive = match cur_attempt_id.as_deref() {
                    Some(aid) => durable_alive_for_attempt(
                        root,
                        agent_str,
                        &lr_fresh.events,
                        aid,
                        &mut durable_log_sample,
                    )?,
                    None => None,
                };
                let wait_pid_alive = liveness::probe_wait_pid_alive(root, agent_str);
                let mut last_signal = liveness::last_signal_json(wait_pid_alive, durable_alive);
                last_signal["hbAgeSecs"] = serde_json::json!(snap.hb_age.map(|a| a.as_secs()));
                last_signal["acked"] = serde_json::json!(ack_observed);
                last_signal["lastActivityAgeSecs"] =
                    serde_json::json!(snap.last_activity_age.map(|a| a.as_secs()));
                last_signal["providerOutputAgeSecs"] =
                    serde_json::json!(provider_output_age.map(|a| a.as_secs()));
                last_signal["hbRound"] =
                    serde_json::json!(hb_identity.as_ref().map(|h| h.round.clone()));
                last_signal["hbGeneration"] =
                    serde_json::json!(hb_identity.as_ref().map(|h| h.generation.clone()));
                // B112：现代 dispatch 身份存在时（is_legacy == Some(false)），
                // judge 调用统一 heartbeat identity 判定——旧轮/旧 attempt/字段
                // 缺失的心跳一律 stale。legacy dispatch 或无身份时走 legacy 回退。
                let modern = match (cur_attempt_id.as_deref(), ctx.is_legacy) {
                    (Some(aid), Some(false)) => Some(liveness::ModernIdentity {
                        round,
                        attempt_id: aid,
                    }),
                    _ => None,
                };
                let storage_suppressed = cur_attempt_id.as_deref().is_some_and(|attempt_id| {
                    crate::storage::storage_interruption_active(
                        &lr_fresh.events,
                        task_id,
                        attempt_id,
                    )
                });
                let judgement = liveness::suppress_dead_for_storage(
                    liveness::judge_with_identity(&snap, o, hb_identity.as_ref(), modern),
                    &lr_fresh.events,
                    task_id,
                    cur_attempt_id.as_deref(),
                );
                match judgement {
                    liveness::Judgement::Healthy(_) => candidate_streak = (0, ""),
                    liveness::Judgement::DeadCandidate(reason) => {
                        candidate_streak = if candidate_streak.1 == "dead" {
                            (candidate_streak.0 + 1, "dead")
                        } else {
                            (1, "dead")
                        };
                        if candidate_streak.0 >= o.confirm {
                            let aid_owned = cur_attempt_id.clone();
                            let no_owned = cur_attempt_no;
                            let task_id_owned = task_id.to_string();
                            let round_owned = round.to_string();
                            let reason_owned = reason.clone();
                            let crash_payload = serde_json::json!({
                                "lastSignal": last_signal,
                                "attemptId": aid_owned,
                                "attemptNo": no_owned,
                            });
                            let obs = crate::attempt::AttemptObservation {
                                attempt_id: cur_attempt_id.clone(),
                                attempt_no: cur_attempt_no,
                            };
                            let outcome = with_active_tierf_effect(
                                root,
                                round,
                                task_id,
                                "await liveness dead",
                                |_active_card| {
                                    crate::attempt::claim_attempt_action(
                                        root,
                                        &round,
                                        task_id,
                                        &obs,
                                        None,
                                        &|events, _fresh_ctx, _fresh_marker, _claimed, _epoch| {
                                            if let Some(aid) = &aid_owned {
                                                if crate::attempt::attempt_event_already_recorded(
                                                    events,
                                                    aid,
                                                    "AttemptCrashed",
                                                ) {
                                                    return Ok(
                                                crate::attempt::ClaimDecision::AlreadyPresent,
                                            );
                                                }
                                            }
                                            Ok(crate::attempt::ClaimDecision::Append(vec![
                                                ledger::event(
                                                    "EscalationRaised",
                                                    "runtime:orch",
                                                    Some(&task_id_owned),
                                                    Some(&round_owned),
                                                    serde_json::json!({"stage": "liveness-dead",
                                                "reason": reason_owned,
                                                "hint": "orch resume <task> 重建现场供新会话，或改派 Tier S"}),
                                                ),
                                                ledger::event(
                                                    "AttemptCrashed",
                                                    "runtime:orch",
                                                    Some(&task_id_owned),
                                                    Some(&round_owned),
                                                    crash_payload.clone(),
                                                ),
                                            ]))
                                        },
                                    )
                                },
                            )?;
                            match outcome {
                                crate::attempt::ClaimOutcome::Appended { .. } => {
                                    // D: only Appended does side effects
                                    with_active_tierf_effect(
                                        root,
                                        round,
                                        task_id,
                                        "await liveness dead notification",
                                        |_active_card| {
                                            liveness::notify_macos(
                                                "orch",
                                                &format!("{task_id} 执行者疑似死亡：{reason}"),
                                            );
                                            write_poke(root, agent_str, &round)?;
                                            Ok(())
                                        },
                                    )?;
                                    return Ok(AwaitOutcome::LivenessDead { reason });
                                }
                                crate::attempt::ClaimOutcome::AlreadyPresent { .. } => {
                                    // D: AlreadyPresent — zero notify/poke
                                    return Ok(AwaitOutcome::LivenessDead { reason });
                                }
                                _ => {} // Superseded — continue polling
                            }
                        }
                    }
                    liveness::Judgement::StalledCandidate(reason) => {
                        candidate_streak = if candidate_streak.1 == "stalled" {
                            (candidate_streak.0 + 1, "stalled")
                        } else {
                            (1, "stalled")
                        };
                        if candidate_streak.0 >= o.confirm {
                            let task_id_owned = task_id.to_string();
                            let round_owned = round.to_string();
                            let reason_owned = reason.clone();
                            let obs = crate::attempt::AttemptObservation {
                                attempt_id: cur_attempt_id.clone(),
                                attempt_no: cur_attempt_no,
                            };
                            let outcome = with_active_tierf_effect(
                                root,
                                round,
                                task_id,
                                "await liveness stalled",
                                |_active_card| {
                                    crate::attempt::claim_attempt_action(
                                        root,
                                        &round,
                                        task_id,
                                        &obs,
                                        None,
                                        &|events, fresh_ctx, _fresh_marker, _claimed, epoch| {
                                            let aid = fresh_ctx
                                                .attempt_id
                                                .as_deref()
                                                .context("liveness-stall 缺 attemptId")?;
                                            let matches = events
                                                .iter()
                                                .filter(|event| {
                                                    action_event_matches(
                                                        event,
                                                        "AttemptTimedOut",
                                                        "liveness-stall",
                                                        aid,
                                                        epoch,
                                                    )
                                                })
                                                .count();
                                            match matches {
                                                1 => return Ok(
                                                    crate::attempt::ClaimDecision::AlreadyPresent,
                                                ),
                                                0 => {}
                                                count => {
                                                    bail!("liveness-stall exact action 重复 ({count})")
                                                }
                                            }
                                            Ok(crate::attempt::ClaimDecision::Append(vec![
                                                ledger::event(
                                                    "EscalationRaised",
                                                    "runtime:orch",
                                                    Some(&task_id_owned),
                                                    Some(&round_owned),
                                                    serde_json::json!({"stage": "liveness-stalled",
                                                "cause": storage_suppressed.then_some("storage-exhaustion"),
                                                "reason": reason_owned,
                                                "hint": "客户端可能仍在长思考——可重跑 await；确认卡死则 orch nudge <agent>"}),
                                                ),
                                                ledger::event(
                                                    "AttemptTimedOut",
                                                    "runtime:orch",
                                                    Some(&task_id_owned),
                                                    Some(&round_owned),
                                                    serde_json::json!({
                                                        "actionId": "liveness-stall",
                                                        "controlEpoch": epoch,
                                                        "lastSignal": last_signal,
                                                        "kind": "stall",
                                                        "cause": storage_suppressed.then_some("storage-exhaustion"),
                                                        "attemptId": aid,
                                                        "attemptNo": fresh_ctx.attempt_no,
                                                    }),
                                                ),
                                            ]))
                                        },
                                    )
                                },
                            )?;
                            match outcome {
                                crate::attempt::ClaimOutcome::Appended { .. } => {
                                    with_active_tierf_effect(
                                        root,
                                        round,
                                        task_id,
                                        "await liveness stalled notification",
                                        |_active_card| {
                                            liveness::notify_macos(
                                                "orch",
                                                &format!("{task_id} 执行者疑似停滞：{reason}"),
                                            );
                                            write_poke(root, agent_str, &round)?;
                                            Ok(())
                                        },
                                    )?;
                                    return Ok(AwaitOutcome::LivenessStalled { reason });
                                }
                                crate::attempt::ClaimOutcome::AlreadyPresent { .. } => {
                                    return Ok(AwaitOutcome::LivenessStalled { reason });
                                }
                                _ => {} // Superseded — continue polling
                            }
                        }
                    }
                }
            }
        }
        wait_for_await_reconcile(runtime, tick, || fs_rx.recv_timeout(tick));
    }
}



#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};


    static TEST_ROOT_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn default_await_timeout_has_legacy_fallback_margin_and_saturates() {
        assert_eq!(default_await_timeout_secs(0), 1_800);
        assert_eq!(default_await_timeout_secs(180), 12_600);
        assert!(default_await_timeout_secs(240) > default_await_timeout_secs(30));
        assert_eq!(default_await_timeout_secs(u64::MAX), u64::MAX);
    }

    #[test]
    fn authenticated_runtime_limit_uses_only_the_exact_canonical_implementation_wake() {
        let implementation_limit = wake::managed_wake_runtime_limit(None);
        let review_limit = wake::managed_wake_runtime_limit(Some(1));
        let mut implementation = ledger::event(
            "WakeIssued",
            "runtime:orch",
            Some("B97"),
            Some("r44"),
            serde_json::json!({
                "agent": "executor-desktop",
                "attemptId": "B97-A0001",
                "continuationId": "implementation:r44:B97:B97-A0001:executor-desktop",
                "wakeId": "implementation-wake",
                "controlWakeId": "implementation-wake",
                "runtimeLimit": implementation_limit,
            }),
        );
        implementation.ts = "2026-08-14T00:00:00Z".to_string();
        let mut same_agent_review = ledger::event(
            "WakeIssued",
            "runtime:orch",
            Some("B97"),
            Some("r44"),
            serde_json::json!({
                "agent": "executor-desktop",
                "attemptId": "B97-A0001",
                "continuationId": "review:r44:B97:B97-A0001:primary:executor-desktop",
                "wakeId": "review-wake",
                "controlWakeId": "review-wake",
                "runtimeLimit": review_limit,
            }),
        );
        same_agent_review.ts = "2026-08-14T00:00:29Z".to_string();

        assert_eq!(
            authenticated_runtime_deadline(
                &[implementation.clone(), same_agent_review],
                "r44",
                "B97",
                "B97-A0001",
                "executor-desktop",
                "2026-08-14T00:00:30Z",
            ),
            Some(AuthenticatedRuntimeDeadline {
                runtime_limit_secs: implementation_limit.effective_secs,
                wake_elapsed: Duration::from_secs(30),
            }),
            "a shorter review deadline must never become the implementation deadline"
        );

        let mut late_replacement = ledger::event(
            "WakeIssued",
            "runtime:orch",
            Some("B97"),
            Some("r44"),
            serde_json::json!({
                "agent": "executor-desktop",
                "attemptId": "B97-A0001",
                "continuationId": "implementation:r44:B97:B97-A0001:executor-desktop",
                "wakeId": "late-wake",
                "controlWakeId": "late-wake",
                "runtimeLimit": implementation_limit,
            }),
        );
        late_replacement.ts = "2026-08-14T00:00:25Z".to_string();
        assert_eq!(
            authenticated_runtime_deadline(
                &[implementation.clone(), late_replacement],
                "r44",
                "B97",
                "B97-A0001",
                "executor-desktop",
                "2026-08-14T00:00:30Z",
            ),
            Some(AuthenticatedRuntimeDeadline {
                runtime_limit_secs: implementation_limit.effective_secs,
                wake_elapsed: Duration::from_secs(5),
            }),
            "a later same-attempt implementation wake must start a fresh runtime clock"
        );

        let mut inconsistent_replacement = ledger::event(
            "WakeIssued",
            "runtime:orch",
            Some("B97"),
            Some("r44"),
            serde_json::json!({
                "agent": "executor-desktop",
                "attemptId": "B97-A0001",
                "continuationId": "implementation:r44:B97:B97-A0001:executor-desktop",
                "wakeId": "inconsistent-wake",
                "controlWakeId": "inconsistent-wake",
                "runtimeLimit": {
                    "requestedReviewDeadlineSecs": null,
                    "effectiveSecs": 1,
                    "source": "ordinary-default",
                },
            }),
        );
        inconsistent_replacement.ts = "2026-08-14T00:00:29Z".to_string();
        assert_eq!(
            authenticated_runtime_deadline(
                &[implementation, inconsistent_replacement],
                "r44",
                "B97",
                "B97-A0001",
                "executor-desktop",
                "2026-08-14T00:00:30Z",
            ),
            None,
            "the newest exact wake must validate canonically; never fall back to an older limit"
        );
    }

    #[test]
    fn observer_expiry_production_path_is_retryable_and_non_terminal() {
        let site = test_root("observer-expiry-non-terminal");
        append_modern_dispatch(&site);

        let error = match run_await(&site.root, "B97", 0, None) {
            Err(error) => error,
            Ok(_) => panic!("observer expiry unexpectedly returned a terminal AwaitOutcome"),
        };
        assert!(
            format!("{error:#}").contains("ReportAwaitExpired（非终态）"),
            "observer expiry must explain its retryable, non-terminal disposition: {error:#}"
        );
        let retry_error = match run_await(&site.root, "B97", 0, None) {
            Err(error) => error,
            Ok(_) => panic!("observer expiry retry unexpectedly returned a terminal outcome"),
        };
        assert!(
            format!("{retry_error:#}").contains("ReportAwaitExpired（非终态）"),
            "observer expiry must remain retryable on replay: {retry_error:#}"
        );

        let events = read_events(&site.root);
        let expiries = events
            .iter()
            .filter(|event| event.kind == "ReportAwaitExpired")
            .collect::<Vec<_>>();
        assert_eq!(expiries.len(), 1);
        let payload = expiries[0].payload.as_ref().unwrap();
        assert_eq!(
            payload.get("attemptId").and_then(serde_json::Value::as_str),
            Some("B97-A0001")
        );
        assert_eq!(
            payload.get("attemptNo").and_then(serde_json::Value::as_u64),
            Some(1)
        );
        assert_eq!(
            payload
                .get("observerSecs")
                .and_then(serde_json::Value::as_u64),
            Some(0)
        );
        assert_eq!(
            payload
                .get("runtimeLimitSecs")
                .and_then(serde_json::Value::as_u64),
            Some(0)
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == "AttemptTimedOut")
                .count(),
            0,
            "an observer without an authenticated runtime limit cannot terminal the attempt"
        );
        assert!(events.iter().any(|event| {
            event.kind == "ActionRejected"
                && event.payload.as_ref().is_some_and(|payload| {
                    payload.get("operation").and_then(serde_json::Value::as_str)
                        == Some("await-report")
                        && payload.get("exitCode").and_then(serde_json::Value::as_i64) == Some(4)
                })
        }));
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == "ActionRejected")
                .count(),
            2,
            "each failed observer command is audited, while the expiry fact stays idempotent"
        );
    }

    struct ValidationTerminationControl {
        times: std::collections::VecDeque<Duration>,
        alive: std::result::Result<bool, String>,
    }

    impl ManagedProcessGroupControl for ValidationTerminationControl {
        fn monotonic_now(&mut self) -> Duration {
            self.times
                .pop_front()
                .expect("unexpected monotonic clock observation")
        }

        fn group_alive(&mut self, _pgid: u32) -> std::result::Result<bool, String> {
            self.alive.clone()
        }

        fn signal_group(
            &mut self,
            _pgid: u32,
            _signal: TerminationSignal,
        ) -> std::result::Result<bool, String> {
            Ok(true)
        }

        fn sleep(&mut self, _duration: Duration) {}
    }

    #[test]
    fn managed_termination_rejects_invalid_policy_clock_regression_and_probe_errors() {
        let zero_poll = ManagedTerminationPolicy {
            term_grace: Duration::ZERO,
            kill_verify: Duration::ZERO,
            poll_interval: Duration::ZERO,
        };
        let mut unused_control = ValidationTerminationControl {
            times: std::collections::VecDeque::new(),
            alive: Ok(true),
        };
        assert!(terminate_managed_process_group_with_control(
            4_000_000_003,
            zero_poll,
            &mut unused_control,
        )
        .unwrap_err()
        .contains("poll interval must be non-zero"));

        let policy = ManagedTerminationPolicy {
            term_grace: Duration::from_millis(20),
            kill_verify: Duration::from_millis(20),
            poll_interval: Duration::from_millis(5),
        };
        let mut backwards = ValidationTerminationControl {
            times: [
                Duration::from_millis(10),
                Duration::from_millis(10),
                Duration::from_millis(5),
            ]
            .into(),
            alive: Ok(true),
        };
        assert!(terminate_managed_process_group_with_control(
            4_000_000_003,
            policy,
            &mut backwards,
        )
        .unwrap_err()
        .contains("monotonic clock moved backwards"));

        let mut probe_error = ValidationTerminationControl {
            times: [Duration::ZERO].into(),
            alive: Err("probe backend failed".into()),
        };
        assert_eq!(
            terminate_managed_process_group_with_control(4_000_000_003, policy, &mut probe_error,)
                .unwrap_err(),
            "probe backend failed"
        );
    }

    #[test]
    fn dispatch_wake_message_is_attempt_scoped_and_directs_exact_wait_claim() {
        let attempt = crate::attempt::AttemptRef {
            task_id: "B110".into(),
            ordinal: 3,
            attempt_id: "B110-A0003".into(),
        };
        let rendered = render_dispatch_wake_message(
            "r47",
            "B110",
            "executor-desktop",
            &attempt,
            "coordination/rounds/r47/dispatch/executor-desktop/GO-B110-A0003.md",
            Some("wake-action-3"),
        )
        .unwrap();
        assert!(rendered.contains("taskId=B110"));
        assert!(rendered.contains("attemptId=B110-A0003"));
        assert!(rendered
            .contains("coordination/scripts/wait-dispatch.sh executor-desktop 1800 B110 A0003"));
        assert!(!rendered.contains("wait-dispatch.sh executor-desktop 1800 B110 B110-A0003"));
        assert!(rendered.contains("ORCH_WAKE_ACTION_ID=wake-action-3"));
        let stable = render_dispatch_wake_message(
            "r47",
            "B110",
            "executor-desktop",
            &attempt,
            "coordination/rounds/r47/dispatch/executor-desktop/GO-B110-A0003.md",
            None,
        )
        .unwrap();
        assert!(!stable.contains("ORCH_WAKE_ACTION_ID"));
        let continuation = "implementation:r47:B110:B110-A0003:executor-desktop";
        assert_ne!(
            crate::wake::wake_request_message_sha256(&stable, continuation).unwrap(),
            crate::wake::wake_request_message_sha256(&rendered, continuation).unwrap(),
            "the caller must pass stable and rendered messages separately so the action nonce cannot enter request identity"
        );
    }

    #[cfg(unix)]
    struct BlockingTerminationControl {
        now: Duration,
        alive: bool,
        entered_sleep: Option<std::sync::mpsc::Sender<()>>,
        release_sleep: std::sync::mpsc::Receiver<()>,
    }

    #[cfg(unix)]
    impl ManagedProcessGroupControl for BlockingTerminationControl {
        fn monotonic_now(&mut self) -> Duration {
            self.now
        }

        fn group_alive(&mut self, _pgid: u32) -> std::result::Result<bool, String> {
            Ok(self.alive)
        }

        fn signal_group(
            &mut self,
            _pgid: u32,
            signal: TerminationSignal,
        ) -> std::result::Result<bool, String> {
            if signal == TerminationSignal::Kill {
                self.alive = false;
            }
            Ok(true)
        }

        fn sleep(&mut self, duration: Duration) {
            if let Some(entered_sleep) = self.entered_sleep.take() {
                entered_sleep.send(()).unwrap();
                self.release_sleep
                    .recv_timeout(Duration::from_secs(2))
                    .expect("test must release the deterministic TERM-grace barrier");
            }
            self.now += duration;
        }
    }

    #[cfg(unix)]
    #[test]
    fn termination_grace_does_not_block_concurrent_ledger_append() {
        use std::sync::mpsc;

        let site = test_root("termination-outside-ledger");
        let root = site.root.clone();
        let (entered_sleep_tx, entered_sleep_rx) = mpsc::channel();
        let (release_sleep_tx, release_sleep_rx) = mpsc::channel();
        let terminator = std::thread::spawn(move || {
            crate::close::with_protocol_effect(&root, "test termination action", || {
                let mut control = BlockingTerminationControl {
                    now: Duration::ZERO,
                    alive: true,
                    entered_sleep: Some(entered_sleep_tx),
                    release_sleep: release_sleep_rx,
                };
                terminate_managed_process_group_with_control(
                    4_000_000_002,
                    ManagedTerminationPolicy {
                        term_grace: Duration::from_millis(500),
                        kill_verify: Duration::from_secs(1),
                        poll_interval: Duration::from_millis(20),
                    },
                    &mut control,
                )
                .map_err(anyhow::Error::msg)
            })
        });
        entered_sleep_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("terminator did not enter deterministic grace barrier");

        let append_root = site.root.clone();
        let (append_done_tx, append_done_rx) = mpsc::channel();
        let appender = std::thread::spawn(move || {
            let result = ledger::append(
                &append_root,
                "r44",
                &[ledger::event(
                    "EscalationRaised",
                    "runtime:test",
                    Some("B134"),
                    Some("r44"),
                    serde_json::json!({"stage": "termination-outside-ledger-lock"}),
                )],
            );
            append_done_tx.send(result).unwrap();
        });
        let append_before_release = append_done_rx.recv_timeout(Duration::from_secs(1));

        // Always release the terminator before asserting so a regression
        // cannot strand a test thread.  The observation above remains the
        // proof that append completed while the slow termination was paused.
        release_sleep_tx.send(()).unwrap();
        appender.join().unwrap();
        append_before_release
            .expect("ledger append was blocked behind termination grace")
            .unwrap();

        let evidence = terminator.join().unwrap().unwrap();
        assert!(evidence.process_tree_terminated);
    }

    struct TierfRoot {
        root: PathBuf,
        wake_marker: PathBuf,
        gate_marker: PathBuf,
    }

    impl Drop for TierfRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn test_root(tag: &str) -> TierfRoot {
        let sequence = TEST_ROOT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = crate::util::test_scratch_dir(&format!(
            "b97-tierf-{tag}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        test_git(&root, &["init", "-q"]);
        std::fs::write(root.join("README.md"), "base\n").unwrap();
        test_git(&root, &["add", "README.md"]);
        test_git(
            &root,
            &[
                "-c",
                "user.name=orch-test",
                "-c",
                "user.email=orch@test.invalid",
                "commit",
                "-q",
                "-m",
                "base",
            ],
        );
        test_git(&root, &["branch", "-M", "main"]);

        let wake_marker = root.join("wake-count.txt");
        let gate_marker = root.join("gate-count.txt");
        std::fs::create_dir_all(root.join("coordination/runtime")).unwrap();
        std::fs::create_dir_all(root.join("coordination/rounds/r44/tasks")).unwrap();
        std::fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r44\n").unwrap();
        std::fs::write(
            root.join("coordination/rounds/r44/tasks/B97.md"),
            "---\ntaskId: B97\nround: r44\nagent: executor-desktop\nwriteSet: []\nfrozenPaths: []\ngates: {fast: [testGate]}\nrequiredReviews:\n  - {role: primary, agent: executor-claw}\nrequiredEvidence:\n  - fixture-evidence\nbudgets: {wallMinutes: 30}\n---\n# B97 test\n",
        )
        .unwrap();
        std::fs::write(
            root.join("coordination/agents.yaml"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "agents": {
                    "executor-desktop": {
                        "injectable": true,
                        "sessionId": "test-session",
                        "wake": {
                            "argv": [
                                "sh",
                                "-c",
                                format!("printf 'wake\\n' >> '{}'", wake_marker.display()),
                                "{session}",
                                "{message}"
                            ]
                        }
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            root.join("coordination/PROJECT-BINDING.yaml"),
            format!(
                "project: {{ecosystems: [rust]}}\nworkspace: {{worktreeRoot: .worktrees}}\ncommands:\n  testGate:\n    argv: [\"sh\", \"-c\", \"printf 'gate\\\\n' >> '{}' && exit 0\"]\n    timeoutSeconds: 30\nscope: {{protectedPaths: []}}\ngit: {{pushPolicy: forbidden}}\noracle: {{dialect: cargo}}\n",
                gate_marker.display()
            ),
        )
        .unwrap();
        // Every Tier F effect entry now rebinds the active signed IR at its
        // critical point, so these fixtures need the same authorization chain
        // production has: a mode config, a compiled ROUND-IR, its canonical
        // production TaskValidated and a user sign-off bound to that exact
        // revision/digest.  Built through the real compiler rather than by
        // hand so the fixture can never drift from the digest contract.
        std::fs::create_dir_all(root.join("coordination/modes")).unwrap();
        std::fs::create_dir_all(root.join(".worktrees")).unwrap();
        std::fs::write(
            root.join(".gitignore"),
            "coordination/rounds/*/dispatch/\ncoordination/runtime/\n.worktrees/\n",
        )
        .unwrap();
        std::fs::write(
            root.join("coordination/modes/test.yaml"),
            "preset: relay\n\
             agents:\n  \
               planner: {adapter: root, tier: none}\n  \
               executor: {adapter: codex-desktop, tier: F, agentId: executor-desktop}\n  \
               verifier: {adapter: root-manual, tier: none}\n\
             hitl: {planSignoff: required, mergeGate: auto}\n\
             verification: {mode: root-manual-fixed-head}\n\
             liveness: {monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}\n\
             scheduling:\n  \
               allowedAgents: [executor-desktop, executor-claw, executor-opencode]\n  \
               capacities:\n    \
                 executor-desktop: {agent: 1, quota: 1, roles: [implement]}\n    \
                 executor-claw: {agent: 1, quota: 1, roles: [implement, primary-review]}\n    \
                 executor-opencode: {agent: 1, quota: 1, roles: [implement, primary-review]}\n\
             budgets: {round: {maxUsd: 12, wallMinutes: 1500, maxModelWakes: 40}}\n\
             git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}\n",
        )
        .unwrap();
        sign_active_ir(&root);

        TierfRoot {
            root,
            wake_marker,
            gate_marker,
        }
    }

    /// Compile + sign the round IR exactly the way production does.
    fn sign_active_ir(root: &Path) {
        crate::plan::materialize_legacy_plan_fixture(root).unwrap();
        let validation = crate::plan::validate_round_ir_readonly(root, "r44").unwrap();
        let payload = crate::plan::plan_signed_off_payload(
            "fixture sign-off",
            validation.persisted_revision,
            &validation.persisted_digest,
        )
        .unwrap();
        crate::ledger::append(
            root,
            "r44",
            &[crate::ledger::event(
                "PlanSignedOff",
                "user",
                None,
                Some("r44"),
                payload,
            )],
        )
        .unwrap();
        test_git(root, &["add", "-A"]);
        if !test_git(root, &["status", "--porcelain=v1"]).is_empty() {
            test_git(
                root,
                &[
                    "-c",
                    "user.name=orch-test",
                    "-c",
                    "user.email=orch@test.invalid",
                    "commit",
                    "-q",
                    "-m",
                    "sign active fixture IR",
                ],
            );
        }
    }


    fn test_git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn read_events(root: &Path) -> Vec<EventRecord> {
        read_ledger(&root.join("coordination/rounds/r44/events.jsonl"))
            .unwrap()
            .events
    }

    fn wait_for_lines(path: &Path, expected: usize) {
        for _ in 0..100 {
            let count = std::fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .count();
            if count >= expected {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("marker {} did not reach {expected} lines", path.display());
    }

    fn append_modern_dispatch(site: &TierfRoot) -> (crate::attempt::AttemptRef, String, String) {
        append_modern_dispatch_at(site, None)
    }

    fn append_modern_dispatch_at(
        site: &TierfRoot,
        dispatch_ts: Option<&str>,
    ) -> (crate::attempt::AttemptRef, String, String) {
        let base_sha = test_git(&site.root, &["rev-parse", "main"]);
        let attempt = crate::attempt::AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        };
        let go_rel =
            "coordination/rounds/r44/dispatch/executor-desktop/GO-B97-A0001.md".to_string();
        let go = site.root.join(&go_rel);
        std::fs::create_dir_all(go.parent().unwrap()).unwrap();
        std::fs::write(&go, "# GO B97\n").unwrap();
        std::fs::write(format!("{}.ack", go.display()), "# ACK B97\n").unwrap();
        let mut dispatch = ledger::event(
            "DispatchIssued",
            "runtime:test",
            Some("B97"),
            Some("r44"),
            serde_json::json!({
                "agent": "executor-desktop",
                "baseSha": base_sha,
                "goPath": go_rel,
                "attemptId": "B97-A0001",
                "attemptNo": 1,
                "wakePending": true
            }),
        );
        if let Some(dispatch_ts) = dispatch_ts {
            dispatch.ts = dispatch_ts.to_string();
        }
        ledger::append(&site.root, "r44", &[dispatch]).unwrap();
        (attempt, go_rel, base_sha)
    }

    #[test]
    fn ack_claim_precedes_a_potentially_stalled_liveness_probe() {
        let site = test_root("ack-before-liveness-probe");
        let (_attempt, go_rel, _base_sha) = append_modern_dispatch(&site);
        let ack_path = site.root.join(format!("{go_rel}.ack"));
        std::thread::sleep(Duration::from_millis(2));
        fs::write(&ack_path, "# current ACK B97\n").unwrap();
        let mut runtime = RecordingAwaitReportRuntime::unavailable_then_main_blocked(&site.root);
        let mut injected = false;

        let error = match run_await_with_hook_inner_with_runtime(
            &site.root,
            "B97",
            30,
            Some(liveness::LivenessOpts {
                probe_every: Duration::ZERO,
                ..liveness::LivenessOpts::default()
            }),
            &mut |point| {
                if point == "before-liveness-probe" {
                    injected = true;
                    anyhow::bail!(
                        "B252 deterministic injection stopped at liveness::probe_with_durable_elapsed"
                    );
                }
                Ok(())
            },
            "r44",
            &mut runtime,
        ) {
            Err(error) => error,
            Ok(_) => panic!("the deterministic liveness probe injection must stop the await loop"),
        };

        assert!(
            injected,
            "the production loop must reach the injected probe boundary"
        );
        assert!(
            ack_path.is_file(),
            "the ACK must already be durable at the injected boundary"
        );
        assert!(
            format!("{error:#}").contains("liveness::probe_with_durable_elapsed"),
            "the injected stop must name the exact production call: {error:#}"
        );
        assert_eq!(
            read_events(&site.root)
                .iter()
                .filter(|event| {
                    event.kind == "DispatchAcked"
                        && event.task_id.as_deref() == Some("B97")
                        && event.payload.as_ref().and_then(|payload| {
                            payload.get("attemptId").and_then(serde_json::Value::as_str)
                        }) == Some("B97-A0001")
                })
                .count(),
            1,
            "ACK is on disk, but its durable claim remained behind the injected liveness probe"
        );
        assert_eq!(
            read_events(&site.root)
                .iter()
                .filter(|event| {
                    event.kind == "AttemptStarted"
                        && event.task_id.as_deref() == Some("B97")
                        && event.payload.as_ref().and_then(|payload| {
                            payload.get("attemptId").and_then(serde_json::Value::as_str)
                        }) == Some("B97-A0001")
                })
                .count(),
            1,
            "the same durable ACK claim must repair AttemptStarted before the probe"
        );
    }

    #[test]
    fn ack_claim_is_independent_of_optional_liveness() {
        let site = test_root("ack-without-liveness");
        let (_attempt, go_rel, _base_sha) = append_modern_dispatch(&site);
        let ack_path = site.root.join(format!("{go_rel}.ack"));
        std::thread::sleep(Duration::from_millis(2));
        fs::write(&ack_path, "# current ACK B97\n").unwrap();
        let mut runtime = RecordingAwaitReportRuntime::unavailable_then_main_blocked(&site.root);

        let outcome = run_await_with_hook_inner_with_runtime(
            &site.root,
            "B97",
            30,
            None,
            &mut |_| Ok(()),
            "r44",
            &mut runtime,
        )
        .unwrap();

        assert!(matches!(outcome, AwaitOutcome::Blocked { .. }));
        let events = read_events(&site.root);
        for kind in ["DispatchAcked", "AttemptStarted"] {
            assert_eq!(
                events
                    .iter()
                    .filter(|event| {
                        event.kind == kind
                            && event.task_id.as_deref() == Some("B97")
                            && event.payload.as_ref().and_then(|payload| {
                                payload.get("attemptId").and_then(serde_json::Value::as_str)
                            }) == Some("B97-A0001")
                    })
                    .count(),
                1,
                "{kind} must be durable even when optional liveness is disabled"
            );
        }
    }

    #[test]
    fn ack_claim_is_durable_before_watcher_installation() {
        let site = test_root("ack-before-watcher-install");
        let (_attempt, go_rel, _base_sha) = append_modern_dispatch(&site);
        let ack_path = site.root.join(format!("{go_rel}.ack"));
        std::thread::sleep(Duration::from_millis(2));
        fs::write(&ack_path, "# current ACK B97\n").unwrap();
        let mut runtime = RecordingAwaitReportRuntime::observe_claim_before_install(&site.root);

        let outcome = run_await_with_hook_inner_with_runtime(
            &site.root,
            "B97",
            30,
            None,
            &mut |_| Ok(()),
            "r44",
            &mut runtime,
        )
        .unwrap();

        assert!(matches!(outcome, AwaitOutcome::Blocked { .. }));
        assert_eq!(runtime.install_calls, 1);
        assert_eq!(
            runtime.claim_counts_at_install,
            Some((1, 1)),
            "fresh ACK must be canonically durable as DispatchAcked + AttemptStarted before optional watcher installation"
        );
    }

    #[test]
    fn ack_absence_preserves_late_watcher_reconciliation() {
        let site = test_root("late-ack-watcher-reconciliation");
        let (_attempt, go_rel, _base_sha) = append_modern_dispatch(&site);
        let ack_path = site.root.join(format!("{go_rel}.ack"));
        fs::remove_file(&ack_path).unwrap();
        let mut runtime =
            RecordingAwaitReportRuntime::late_ack_then_notify_blocked(&site.root, ack_path.clone());
        let mut claim_calls = 0;

        let outcome = run_await_with_hook_inner_with_runtime(
            &site.root,
            "B97",
            30,
            None,
            &mut |point| {
                if point == "before-ack-claim" {
                    claim_calls += 1;
                }
                Ok(())
            },
            "r44",
            &mut runtime,
        )
        .unwrap();

        assert!(matches!(outcome, AwaitOutcome::Blocked { .. }));
        assert!(
            ack_path.is_file(),
            "the watcher frame must create a late ACK"
        );
        assert_eq!(runtime.install_calls, 1);
        assert!(
            runtime.late_ack_wake_received,
            "the relevant BLOCKED notification must wake the bounded receiver"
        );
        assert_eq!(
            runtime.recv_timeouts,
            vec![AWAIT_REPORT_RECONCILE_TICK],
            "one watcher wake must lead directly to the late ACK + BLOCKED reconciliation frame"
        );
        assert_eq!(claim_calls, 1, "late ACK must be claimed exactly once");
        let events = read_events(&site.root);
        for kind in ["DispatchAcked", "AttemptStarted", "AttemptBlocked"] {
            assert_eq!(
                events
                    .iter()
                    .filter(|event| {
                        event.kind == kind
                            && event.task_id.as_deref() == Some("B97")
                            && (kind == "AttemptBlocked"
                                || event.payload.as_ref().and_then(|payload| {
                                    payload.get("attemptId").and_then(serde_json::Value::as_str)
                                }) == Some("B97-A0001"))
                    })
                    .count(),
                1,
                "late reconciliation must durably append {kind} exactly once"
            );
        }
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == "ReportObserved")
                .count(),
            0,
            "the REPORT stat arm must remain absent while the BLOCKED arm wins"
        );
    }

    #[test]
    fn await_entry_guard_drift_is_critical_passthrough_without_outer_rejection() {
        let site = test_root("await-entry-critical-drift");
        let card_path = site.root.join("coordination/rounds/r44/tasks/B97.md");
        let original = fs::read_to_string(&card_path).unwrap();
        fs::write(&card_path, format!("{original}\n# drift mutation\n")).unwrap();
        let rejection_count = |events: &[EventRecord]| {
            events
                .iter()
                .filter(|event| {
                    event.kind == "ActionRejected"
                        && event.payload.as_ref().and_then(|payload| {
                            payload.get("operation").and_then(serde_json::Value::as_str)
                        }) == Some("await-report")
                })
                .count()
        };
        let before = rejection_count(&read_events(&site.root));

        let error = match run_await_with_hook(&site.root, "B97", 30, None, &mut |_| Ok(())) {
            Err(error) => error,
            Ok(_) => panic!("active task-card drift must fail the await entry guard"),
        };

        assert!(
            is_critical_point_drift(&error),
            "the retained inner entry guard must wrap active drift as critical: {error:#}"
        );
        assert_eq!(
            rejection_count(&read_events(&site.root)),
            before,
            "critical await entry drift must pass through without a synthetic ActionRejected"
        );
    }

    #[derive(Clone, Copy)]
    enum AwaitRuntimeScenario {
        Timeout,
        Disconnected,
        WatcherErrorThenTimeout,
        LateAckThenNotifyBlocked,
    }

    struct RecordingAwaitReportRuntime {
        watcher_enabled: bool,
        callback: Option<AwaitReportNotifyCallback>,
        scenario: AwaitRuntimeScenario,
        terminal_path: PathBuf,
        absent_before_first_wait: Option<PathBuf>,
        claim_ledger_root: Option<PathBuf>,
        claim_counts_at_install: Option<(usize, usize)>,
        late_ack_path: Option<PathBuf>,
        late_ack_wake_received: bool,
        install_calls: usize,
        disable_calls: usize,
        watcher_error_emissions: usize,
        created_dirs: Vec<PathBuf>,
        watched: Vec<(PathBuf, bool)>,
        recv_timeouts: Vec<Duration>,
        sleeps: Vec<Duration>,
    }

    impl RecordingAwaitReportRuntime {
        fn late_worktree_blocked(root: &Path) -> Self {
            Self {
                watcher_enabled: true,
                callback: None,
                scenario: AwaitRuntimeScenario::Timeout,
                terminal_path: root
                    .join(".worktrees/B97/coordination/rounds/r44/reports/B97-BLOCKED.md"),
                absent_before_first_wait: Some(root.join(".worktrees/B97")),
                claim_ledger_root: None,
                claim_counts_at_install: None,
                late_ack_path: None,
                late_ack_wake_received: false,
                install_calls: 0,
                disable_calls: 0,
                watcher_error_emissions: 0,
                created_dirs: Vec::new(),
                watched: Vec::new(),
                recv_timeouts: Vec::new(),
                sleeps: Vec::new(),
            }
        }

        fn unavailable_then_main_blocked(root: &Path) -> Self {
            Self {
                watcher_enabled: false,
                callback: None,
                scenario: AwaitRuntimeScenario::Disconnected,
                terminal_path: root.join("coordination/rounds/r44/reports/B97-BLOCKED.md"),
                absent_before_first_wait: None,
                claim_ledger_root: None,
                claim_counts_at_install: None,
                late_ack_path: None,
                late_ack_wake_received: false,
                install_calls: 0,
                disable_calls: 0,
                watcher_error_emissions: 0,
                created_dirs: Vec::new(),
                watched: Vec::new(),
                recv_timeouts: Vec::new(),
                sleeps: Vec::new(),
            }
        }

        fn permanently_degraded_then_main_blocked(root: &Path) -> Self {
            Self {
                watcher_enabled: true,
                callback: None,
                scenario: AwaitRuntimeScenario::WatcherErrorThenTimeout,
                terminal_path: root.join("coordination/rounds/r44/reports/B97-BLOCKED.md"),
                absent_before_first_wait: None,
                claim_ledger_root: None,
                claim_counts_at_install: None,
                late_ack_path: None,
                late_ack_wake_received: false,
                install_calls: 0,
                disable_calls: 0,
                watcher_error_emissions: 0,
                created_dirs: Vec::new(),
                watched: Vec::new(),
                recv_timeouts: Vec::new(),
                sleeps: Vec::new(),
            }
        }

        fn observe_claim_before_install(root: &Path) -> Self {
            let mut runtime = Self::unavailable_then_main_blocked(root);
            runtime.claim_ledger_root = Some(root.to_path_buf());
            runtime
        }

        fn late_ack_then_notify_blocked(root: &Path, ack_path: PathBuf) -> Self {
            let mut runtime = Self::permanently_degraded_then_main_blocked(root);
            runtime.scenario = AwaitRuntimeScenario::LateAckThenNotifyBlocked;
            runtime.late_ack_path = Some(ack_path);
            runtime
        }
    }

    impl AwaitReportRuntime for RecordingAwaitReportRuntime {
        fn install_watcher(&mut self, callback: AwaitReportNotifyCallback) {
            self.install_calls += 1;
            self.claim_counts_at_install = self.claim_ledger_root.as_ref().map(|root| {
                let events = read_events(root);
                let count = |kind: &str| {
                    events
                        .iter()
                        .filter(|event| {
                            event.kind == kind
                                && event.task_id.as_deref() == Some("B97")
                                && event.payload.as_ref().and_then(|payload| {
                                    payload.get("attemptId").and_then(serde_json::Value::as_str)
                                }) == Some("B97-A0001")
                        })
                        .count()
                };
                (count("DispatchAcked"), count("AttemptStarted"))
            });
            if self.watcher_enabled {
                self.callback = Some(callback);
            }
        }

        fn disable_watcher(&mut self) {
            self.disable_calls += 1;
            if let Some(mut callback) = self.callback.take() {
                // A broken backend may race more errors with teardown. The
                // callback's sticky degraded state must suppress all of them.
                for _ in 0..100_000 {
                    callback(Err(notify::Error::generic("permanent watcher failure")));
                    self.watcher_error_emissions += 1;
                }
            }
            self.watcher_enabled = false;
        }

        fn watcher_available(&self) -> bool {
            self.callback.is_some()
        }

        fn create_dir_all(&mut self, path: &Path) -> std::io::Result<()> {
            self.created_dirs.push(path.to_path_buf());
            fs::create_dir_all(path)
        }

        fn is_dir(&mut self, path: &Path) -> bool {
            path.is_dir()
        }

        fn watch(&mut self, path: &Path, mode: notify::RecursiveMode) -> bool {
            self.watched.push((
                path.to_path_buf(),
                matches!(mode, notify::RecursiveMode::Recursive),
            ));
            true
        }

        fn recv_timeout<F>(
            &mut self,
            timeout: Duration,
            native_wait: F,
        ) -> std::result::Result<(), std::sync::mpsc::RecvTimeoutError>
        where
            F: FnOnce() -> std::result::Result<(), std::sync::mpsc::RecvTimeoutError>,
        {
            self.recv_timeouts.push(timeout);
            if matches!(self.scenario, AwaitRuntimeScenario::WatcherErrorThenTimeout)
                && self.recv_timeouts.len() == 1
            {
                let callback = self
                    .callback
                    .as_mut()
                    .expect("degraded scenario requires an installed watcher");
                for _ in 0..100_000 {
                    callback(Err(notify::Error::generic("permanent watcher failure")));
                    self.watcher_error_emissions += 1;
                }
                return native_wait();
            }
            if matches!(
                self.scenario,
                AwaitRuntimeScenario::LateAckThenNotifyBlocked
            ) {
                assert_eq!(
                    self.recv_timeouts.len(),
                    1,
                    "late ACK fixture must converge after its first watcher wake"
                );
                std::thread::sleep(Duration::from_millis(2));
                fs::write(
                    self.late_ack_path
                        .as_ref()
                        .expect("late ACK scenario requires an ACK path"),
                    "# late ACK B97\n",
                )
                .unwrap();
                fs::create_dir_all(self.terminal_path.parent().unwrap()).unwrap();
                fs::write(&self.terminal_path, "# fixture BLOCKED\n").unwrap();
                self.callback
                    .as_mut()
                    .expect("late ACK scenario requires an installed watcher")(Ok(
                    notify::Event::new(notify::EventKind::Any).add_path(self.terminal_path.clone()),
                ));
                let received = native_wait();
                self.late_ack_wake_received = received.is_ok();
                return received;
            }
            let expected_waits =
                if matches!(self.scenario, AwaitRuntimeScenario::WatcherErrorThenTimeout) {
                    2
                } else {
                    1
                };
            assert_eq!(
                self.recv_timeouts.len(),
                expected_waits,
                "production loop failed to converge on the bounded stat scan"
            );
            if let Some(path) = &self.absent_before_first_wait {
                assert!(
                    !path.exists(),
                    "production pre-created future task worktree path {}",
                    path.display()
                );
            }
            // The control marker includes the DispatchIssued ULID millisecond;
            // make this externally-created evidence strictly newer without
            // paying the production two-second interval in a unit test.
            std::thread::sleep(Duration::from_millis(2));
            fs::create_dir_all(self.terminal_path.parent().unwrap()).unwrap();
            fs::write(&self.terminal_path, "# fixture BLOCKED\n").unwrap();
            match self.scenario {
                AwaitRuntimeScenario::Timeout => Err(std::sync::mpsc::RecvTimeoutError::Timeout),
                AwaitRuntimeScenario::Disconnected => {
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
                }
                AwaitRuntimeScenario::WatcherErrorThenTimeout => {
                    assert!(
                        self.callback.is_none(),
                        "the degraded watcher must be dropped before polling resumes"
                    );
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                }
                AwaitRuntimeScenario::LateAckThenNotifyBlocked => unreachable!(
                    "late ACK scenario returns immediately after its synthetic watcher wake"
                ),
            }
        }

        fn sleep(&mut self, duration: Duration) {
            self.sleeps.push(duration);
        }
    }

    #[test]
    fn old_dispatch_age_does_not_consume_a_new_observer_window() {
        let site = test_root("observer-window-is-per-invocation");
        append_modern_dispatch_at(&site, Some("2020-01-01T00:00:00Z"));
        let mut runtime = RecordingAwaitReportRuntime::unavailable_then_main_blocked(&site.root);

        let outcome = run_await_with_hook_inner_with_runtime(
            &site.root,
            "B97",
            30,
            None,
            &mut |_| Ok(()),
            "r44",
            &mut runtime,
        )
        .unwrap();

        assert!(matches!(outcome, AwaitOutcome::Blocked { .. }));
        assert_eq!(
            runtime.recv_timeouts,
            vec![AWAIT_REPORT_RECONCILE_TICK],
            "an old attempt must still enter the new invocation's observer wait"
        );
        let events = read_events(&site.root);
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    matches!(
                        event.kind.as_str(),
                        "ReportAwaitExpired" | "AttemptTimedOut"
                    )
                })
                .count(),
            0,
            "durable attempt age must not expire a fresh observer window"
        );
    }

    #[test]
    fn await_production_loop_mounts_late_worktree_parent_without_precreating_it() {
        let site = test_root("await-late-worktree-runtime");
        append_modern_dispatch(&site);
        let future_worktree = site.root.join(".worktrees/B97");
        assert!(
            !future_worktree.exists(),
            "fixture must begin before the task worktree exists"
        );

        let main_reports = site.root.join("coordination/rounds/r44/reports");
        let task_reports = future_worktree.join("coordination/rounds/r44/reports");
        let mut runtime = RecordingAwaitReportRuntime::late_worktree_blocked(&site.root);
        let outcome = run_await_with_hook_inner_with_runtime(
            &site.root,
            "B97",
            30,
            None,
            &mut |_| Ok(()),
            "r44",
            &mut runtime,
        )
        .unwrap();

        assert!(matches!(outcome, AwaitOutcome::Blocked { .. }));
        assert_eq!(runtime.install_calls, 1);
        assert_eq!(runtime.created_dirs, vec![main_reports.clone()]);
        assert_eq!(
            runtime.watched,
            vec![(main_reports, false), (task_reports, false)],
            "main parent must mount first; late task parent must mount exactly once on the next loop"
        );
        assert_eq!(
            runtime.recv_timeouts,
            vec![AWAIT_REPORT_RECONCILE_TICK],
            "the real production wait must bind the public two-second contract"
        );
        assert!(runtime.sleeps.is_empty());
        assert_eq!(
            read_events(&site.root)
                .iter()
                .filter(|event| event.kind == "AttemptBlocked")
                .count(),
            1,
            "the next real stat scan must converge without a notify event"
        );
    }

    #[test]
    fn await_production_loop_disconnected_channel_sleeps_one_tick_then_stats() {
        let site = test_root("await-disconnected-runtime");
        append_modern_dispatch(&site);
        let main_reports = site.root.join("coordination/rounds/r44/reports");
        let mut runtime = RecordingAwaitReportRuntime::unavailable_then_main_blocked(&site.root);

        let outcome = run_await_with_hook_inner_with_runtime(
            &site.root,
            "B97",
            30,
            None,
            &mut |_| Ok(()),
            "r44",
            &mut runtime,
        )
        .unwrap();

        assert!(matches!(outcome, AwaitOutcome::Blocked { .. }));
        assert_eq!(runtime.install_calls, 1);
        assert!(
            runtime.callback.is_none(),
            "watcher factory must be unavailable"
        );
        assert_eq!(runtime.created_dirs, vec![main_reports]);
        assert!(runtime.watched.is_empty());
        assert_eq!(runtime.recv_timeouts, vec![AWAIT_REPORT_RECONCILE_TICK]);
        assert_eq!(
            runtime.sleeps,
            vec![AWAIT_REPORT_RECONCILE_TICK],
            "each Disconnected result must sleep exactly one reconcile tick before the next stat scan"
        );
        assert_eq!(
            read_events(&site.root)
                .iter()
                .filter(|event| event.kind == "AttemptBlocked")
                .count(),
            1
        );
    }

    #[test]
    fn await_production_loop_disables_permanent_watcher_error_then_polls() {
        let site = test_root("await-permanent-watcher-error-runtime");
        append_modern_dispatch(&site);
        let main_reports = site.root.join("coordination/rounds/r44/reports");
        let mut runtime =
            RecordingAwaitReportRuntime::permanently_degraded_then_main_blocked(&site.root);

        let outcome = run_await_with_hook_inner_with_runtime(
            &site.root,
            "B97",
            30,
            None,
            &mut |_| Ok(()),
            "r44",
            &mut runtime,
        )
        .unwrap();

        assert!(matches!(outcome, AwaitOutcome::Blocked { .. }));
        assert_eq!(runtime.install_calls, 1);
        assert_eq!(runtime.disable_calls, 1, "watcher disable must be one-shot");
        assert_eq!(
            runtime.watcher_error_emissions, 200_000,
            "both the live error train and teardown race must be exercised"
        );
        assert!(runtime.callback.is_none());
        assert_eq!(runtime.created_dirs, vec![main_reports.clone()]);
        assert_eq!(runtime.watched, vec![(main_reports, false)]);
        assert_eq!(
            runtime.recv_timeouts,
            vec![AWAIT_REPORT_RECONCILE_TICK, AWAIT_REPORT_RECONCILE_TICK],
            "one error wake must be followed by exactly one bounded polling tick"
        );
        assert!(runtime.sleeps.is_empty());
        assert_eq!(
            read_events(&site.root)
                .iter()
                .filter(|event| event.kind == "AttemptBlocked")
                .count(),
            1,
            "the polling-only stat scan must still converge"
        );
    }


    fn commit_report(site: &TierfRoot) -> (String, Vec<u8>, String) {
        let report_rel = "coordination/rounds/r44/reports/B97-REPORT.md".to_string();
        let report = b"## 0 execution\nMODEL=test\nDEPTH=high\nCAPTURE=unit-production-path\n\n## 1 changes\nnone\n".to_vec();
        test_git(&site.root, &["checkout", "-q", "-b", "task/B97"]);
        let report_path = site.root.join(&report_rel);
        std::fs::create_dir_all(report_path.parent().unwrap()).unwrap();
        std::fs::write(&report_path, &report).unwrap();
        test_git(&site.root, &["add", &report_rel]);
        test_git(
            &site.root,
            &[
                "-c",
                "user.name=orch-test",
                "-c",
                "user.email=orch@test.invalid",
                "commit",
                "-q",
                "-m",
                "report",
            ],
        );
        let branch_sha = test_git(&site.root, &["rev-parse", "HEAD"]);
        test_git(&site.root, &["checkout", "-q", "main"]);
        (report_rel, report, branch_sha)
    }

    fn claimed_report(
        site: &TierfRoot,
        report_rel: &str,
        bytes: Vec<u8>,
    ) -> crate::attempt::ClaimedEvidence {
        let claimed = crate::attempt::ClaimedEvidence {
            canonical_path: site.root.join(report_rel).to_string_lossy().into_owned(),
            sha256: hex::encode(sha2::Sha256::digest(&bytes)),
            len: bytes.len() as u64,
            bytes,
            control_epoch: "control-epoch-1".into(),
        };
        append_claimed_report_identity(site, &claimed);
        claimed
    }

    fn append_claimed_report_identity(site: &TierfRoot, claimed: &crate::attempt::ClaimedEvidence) {
        // Direct collect fixtures intentionally bypass await_report's normal
        // observation claim.  Once a production-shaped DispatchIssued exists,
        // fill that omitted boundary through the production audit API so both
        // positive and negative collect tests reach their intended durable
        // receipt assertions instead of passing vacuously on a missing audit.
        let events = read_events(&site.root);
        let has_dispatch = events.iter().any(|event| {
            event.kind == "DispatchIssued"
                && event.round.as_deref() == Some("r44")
                && event.task_id.as_deref() == Some("B97")
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("attemptId"))
                    .and_then(serde_json::Value::as_str)
                    == Some("B97-A0001")
        });
        if has_dispatch {
            if let Some(decl) = std::str::from_utf8(&claimed.bytes)
                .ok()
                .and_then(crate::mech::report_env_decl)
            {
                let audit = crate::mech::audit_report_identity(
                    &site.root,
                    &events,
                    "r44",
                    "B97",
                    "B97-A0001",
                    "executor-desktop",
                    &decl,
                )
                .unwrap();
                assert!(crate::mech::identity_audit_evidence_ready(&audit));
                ledger::append(
                    &site.root,
                    "r44",
                    &[ledger::event(
                        "ReportObserved",
                        "runtime:orch",
                        Some("B97"),
                        Some("r44"),
                        serde_json::json!({
                            "actionId": "report-observed",
                            "attemptId": "B97-A0001",
                            "attemptNo": 1,
                            "reportPath": claimed.canonical_path,
                            "evidencePath": claimed.canonical_path,
                            "evidenceSha256": claimed.sha256,
                            "evidenceLen": claimed.len,
                            "controlEpoch": claimed.control_epoch,
                            "envDecl": crate::mech::env_decl_payload(&decl),
                            "identityReconciliation": audit.payload,
                        }),
                    )],
                )
                .unwrap();
            }
        }
    }

    fn modern_ctx(go_rel: &str, base_sha: &str) -> crate::attempt::DispatchContext {
        crate::attempt::DispatchContext {
            attempt_id: Some("B97-A0001".into()),
            attempt_no: Some(1),
            agent: Some("executor-desktop".into()),
            go_path: Some(go_rel.into()),
            base_sha: Some(base_sha.into()),
            is_legacy: Some(false),
        }
    }

    fn append_collect_executing_anchor(
        site: &TierfRoot,
        ctx: &crate::attempt::DispatchContext,
        claimed: &crate::attempt::ClaimedEvidence,
        lease_until: &str,
    ) -> (String, String) {
        let action_id = report_collect_action_id(
            "B97-A0001",
            &claimed.canonical_path,
            &claimed.sha256,
            &claimed.control_epoch,
        );
        let anchor_owner = "crashed-collect-owner".to_string();
        let anchor_generation = "crashed-collect-generation".to_string();
        let mut payload = report_collect_payload(
            ctx,
            claimed,
            &action_id,
            None,
            &anchor_owner,
            &anchor_generation,
            None,
        )
        .unwrap();
        payload["leaseUntil"] = serde_json::json!(lease_until);
        ledger::append(
            &site.root,
            "r44",
            &[
                ledger::event(
                    "ReportCollectClaimed",
                    "runtime:fixture",
                    Some("B97"),
                    Some("r44"),
                    payload.clone(),
                ),
                ledger::event(
                    "ReportCollectExecuting",
                    "runtime:fixture",
                    Some("B97"),
                    Some("r44"),
                    payload,
                ),
            ],
        )
        .unwrap();
        (anchor_owner, anchor_generation)
    }

    #[test]
    fn expired_executing_fixture_self_heals_reruns_gates_and_replan_can_finish() {
        let site = test_root("collect-expired-executing-recovery");
        let (_, go_rel, base_sha) = append_modern_dispatch(&site);
        let (report_rel, report, _) = commit_report(&site);
        let claimed = claimed_report(&site, &report_rel, report);
        let ctx = modern_ctx(&go_rel, &base_sha);
        let (anchor_owner, anchor_generation) =
            append_collect_executing_anchor(&site, &ctx, &claimed, "2000-01-01T00:00:00Z");
        let card = card::load(&site.root, "r44", "B97").unwrap();

        let outcome = collect_claimed_report(
            &site.root,
            "r44",
            "B97",
            &card,
            "task/B97",
            &report_rel,
            &ctx,
            &claimed,
            &mut || Duration::ZERO,
            &mut |_: Duration| {
                panic!("already-committed REPORT must never enter commit-grace sleep")
            },
            &mut |_| Ok(()),
        )
        .unwrap();
        assert_eq!(
            outcome.gates.len(),
            1,
            "reclaimed generation must rerun gates"
        );
        wait_for_lines(&site.gate_marker, 1);

        let events = read_events(&site.root);
        let released_index = events
            .iter()
            .position(|event| event.kind == "ReportCollectReleased")
            .unwrap();
        let released = events[released_index].payload.as_ref().unwrap();
        assert_eq!(released["owner"], anchor_owner);
        assert_eq!(released["leaseGeneration"], anchor_generation);
        assert_eq!(released["outcomeUnknown"], true);
        let reclaimed = events
            .get(released_index + 1)
            .filter(|event| event.kind == "ReportCollectClaimed")
            .expect("release and fresh claim must be appended atomically in order");
        let reclaimed = reclaimed.payload.as_ref().unwrap();
        let reclaimed_owner = reclaimed["owner"].as_str().unwrap();
        let reclaimed_generation = reclaimed["leaseGeneration"].as_str().unwrap();
        assert_ne!(reclaimed_owner, anchor_owner);
        assert_ne!(reclaimed_generation, anchor_generation);
        assert!(
            events.iter().any(|event| {
                event.kind == "ReportCollectExecuting"
                    && event.payload.as_ref().is_some_and(|payload| {
                        payload["owner"] == reclaimed_owner
                            && payload["leaseGeneration"] == reclaimed_generation
                    })
            }),
            "execution fence must use the reclaimed lineage"
        );
        for kind in [
            "CollectGateSuccessReceipt",
            "ReportCollectExecuted",
            "ReportCollectCompleted",
        ] {
            assert_eq!(
                events.iter().filter(|event| event.kind == kind).count(),
                1,
                "recovered collect must reach {kind}"
            );
        }

        // Recovery makes the normal terminal path reachable again. Once the task is
        // recorded, the same fixture no longer blocks the replan invalidation gate.
        let mut ledger_text =
            std::fs::read_to_string(site.root.join("coordination/rounds/r44/events.jsonl"))
                .unwrap();
        let recorded = EventRecord {
            event_id: "fixture-task-recorded".into(),
            ts: "2026-01-01T00:00:00Z".into(),
            actor: "runtime:fixture".into(),
            kind: "TaskRecorded".into(),
            task_id: Some("B97".into()),
            round: Some("r44".into()),
            payload: Some(serde_json::json!({"attemptId": "B97-A0001"})),
            extra: Default::default(),
        };
        ledger_text.push_str(&serde_json::to_string(&recorded).unwrap());
        ledger_text.push('\n');
        assert!(
            crate::plan::replan_invalidation_report(&ledger_text, "r44")
                .unwrap()
                .is_empty(),
            "recovered attempt must not permanently freeze replan after normal terminal state"
        );
    }

    #[test]
    fn live_executing_lease_stays_busy_without_recovery_events() {
        let site = test_root("collect-live-executing-busy");
        let (_, go_rel, base_sha) = append_modern_dispatch(&site);
        let (report_rel, report, _) = commit_report(&site);
        let claimed = claimed_report(&site, &report_rel, report);
        let ctx = modern_ctx(&go_rel, &base_sha);
        append_collect_executing_anchor(&site, &ctx, &claimed, "2999-01-01T00:00:00Z");
        let card = card::load(&site.root, "r44", "B97").unwrap();
        let before = read_events(&site.root);

        let result = collect_claimed_report(
            &site.root,
            "r44",
            "B97",
            &card,
            "task/B97",
            &report_rel,
            &ctx,
            &claimed,
            &mut || Duration::ZERO,
            &mut |_: Duration| {
                panic!("already-committed REPORT must never enter commit-grace sleep")
            },
            &mut |_| Ok(()),
        );
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("live executing lease must remain busy"),
        };
        assert!(error.to_string().contains("owned by another live caller"));
        assert_eq!(read_events(&site.root).len(), before.len());
        assert!(!site.gate_marker.exists());
    }

    fn configure_two_gates(site: &TierfRoot) {
        std::fs::write(
            site.root.join("coordination/rounds/r44/tasks/B97.md"),
            "---\ntaskId: B97\nround: r44\nagent: executor-desktop\nwriteSet: []\nfrozenPaths: []\ngates: {fast: [testGate, testGate2]}\nrequiredReviews:\n  - {role: primary, agent: executor-claw}\nrequiredEvidence: [fixture-evidence]\nbudgets: {wallMinutes: 30}\n---\n# B97 test\n",
        )
        .unwrap();
        std::fs::write(
            site.root.join("coordination/PROJECT-BINDING.yaml"),
            format!(
                "project: {{ecosystems: [rust]}}\nworkspace: {{worktreeRoot: .worktrees}}\ncommands:\n  testGate:\n    argv: [\"sh\", \"-c\", \"printf 'gate-1\\\\n' >> '{}' && exit 0\"]\n    timeoutSeconds: 30\n  testGate2:\n    argv: [\"sh\", \"-c\", \"printf 'gate-2\\\\n' >> '{}' && exit 0\"]\n    timeoutSeconds: 30\nscope: {{protectedPaths: []}}\ngit: {{pushPolicy: forbidden}}\noracle: {{dialect: cargo}}\n",
                site.gate_marker.display(),
                site.gate_marker.display()
            ),
        )
        .unwrap();
        sign_active_ir(&site.root);
    }

    fn append_forged_collect_terminal(
        site: &TierfRoot,
        ctx: &crate::attempt::DispatchContext,
        claimed: &crate::attempt::ClaimedEvidence,
        branch_sha: &str,
        gate_specs: &[(&str, i64)],
        listed_specs: &[(&str, usize, i64)],
    ) {
        let action_id = report_collect_action_id(
            "B97-A0001",
            &claimed.canonical_path,
            &claimed.sha256,
            &claimed.control_epoch,
        );
        let mut claim_payload = report_collect_payload(
            ctx,
            claimed,
            &action_id,
            None,
            "forged-owner",
            "forged-generation",
            None,
        )
        .unwrap();
        claim_payload["leaseUntil"] = serde_json::json!("2999-01-01T00:00:00Z");
        let claim = ledger::event(
            "ReportCollectClaimed",
            "runtime:orch",
            Some("B97"),
            Some("r44"),
            claim_payload.clone(),
        );
        let executing = ledger::event(
            "ReportCollectExecuting",
            "runtime:orch",
            Some("B97"),
            Some("r44"),
            claim_payload,
        );
        let subject_tree_sha =
            gitx::rev_parse(&site.root, &format!("{branch_sha}^{{tree}}")).unwrap();
        let toolchain_digest = "a".repeat(64);
        let environment_digest = "b".repeat(64);
        let gate_events = gate_specs
            .iter()
            .enumerate()
            .map(|(index, (command_ref, exit_code))| {
                let raw = format!("forged-raw-log-{index}\n");
                let raw_sha256 = collect_cas(&site.root).put(raw.as_bytes()).unwrap();
                ledger::event(
                    "GateExecuted",
                    "runtime:orch",
                    Some("B97"),
                    Some("r44"),
                    serde_json::json!({
                        "commandRef": command_ref,
                        "phase": "collect",
                        "gateRunId": format!("01ARZ3NDEKTSV4RRFFQ69G{:04}", index),
                        "exitCode": exit_code,
                        "durationMs": 1,
                        "subjectTreeSha": subject_tree_sha.clone(),
                        "logSha256": raw_sha256,
                        "logBytes": raw.len() as u64,
                        "toolchainDigest": toolchain_digest.clone(),
                        "environmentDigest": environment_digest.clone(),
                    }),
                )
            })
            .collect::<Vec<_>>();
        let mut receipt_payload = report_collect_payload(
            ctx,
            claimed,
            &action_id,
            Some(branch_sha),
            "forged-owner",
            "forged-generation",
            None,
        )
        .unwrap();
        let attested_gates = listed_specs
            .iter()
            .enumerate()
            .map(|(index, (command_ref, event_index, exit_code))| {
                let event_id = gate_events
                    .get(*event_index)
                    .map(|event| event.event_id.as_str())
                    .unwrap_or("missing-gate-event");
                let event_payload = gate_events
                    .get(*event_index)
                    .and_then(|event| event.payload.as_ref());
                let gate_run_id = event_payload
                    .and_then(|payload| payload.get("gateRunId"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("missing-gate-run")
                    .to_string();
                let raw_sha256 = event_payload
                    .and_then(|payload| payload.get("logSha256"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| "c".repeat(64));
                let raw_len = event_payload
                    .and_then(|payload| payload.get("logBytes"))
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                CollectAttestedGate {
                    sequence: (index + 1) as u64,
                    gate_event_id: event_id.to_string(),
                    gate_run_id,
                    command_ref: (*command_ref).to_string(),
                    exit_code: i32::try_from(*exit_code).unwrap(),
                    duration_ms: 1,
                    raw_log_cas_sha256: raw_sha256,
                    raw_log_len: raw_len,
                }
            })
            .collect::<Vec<_>>();
        let configured_command_refs = listed_specs
            .iter()
            .map(|(command_ref, _, _)| (*command_ref).to_string())
            .collect::<Vec<_>>();
        let attestation = CollectReceiptAttestation {
            attestation_version: 1,
            round: "r44".into(),
            task_id: "B97".into(),
            action_id: action_id.clone(),
            owner: "forged-owner".into(),
            lease_generation: "forged-generation".into(),
            attempt_id: "B97-A0001".into(),
            attempt_no: 1,
            agent: ctx.agent.clone().unwrap(),
            base_sha: ctx.base_sha.clone().unwrap(),
            go_path: ctx.go_path.clone().unwrap(),
            executing_event_id: executing.event_id.clone(),
            branch_sha: branch_sha.into(),
            evidence_path: claimed.canonical_path.clone(),
            evidence_sha256: claimed.sha256.clone(),
            evidence_len: claimed.len,
            control_epoch: claimed.control_epoch.clone(),
            subject_tree_sha: String::new(),
            ir_revision: 0,
            validation_digest: String::new(),
            binding_sha256: String::new(),
            task_card_sha256: String::new(),
            resolved_command_digest: String::new(),
            source_reader_descriptor_sha256: None,
            source_reader_base_sha256: None,
            toolchain_digest: toolchain_digest.clone(),
            environment_digest: environment_digest.clone(),
            configured_gate_count: configured_command_refs.len() as u64,
            configured_command_refs,
            gates: attested_gates,
        };
        let missing_bytes = serde_json::to_vec(&attestation).unwrap();
        let missing_attestation = hex::encode(sha2::Sha256::digest(&missing_bytes));
        receipt_payload["receiptVersion"] = serde_json::json!(2);
        receipt_payload["gateCount"] = serde_json::json!(attestation.configured_gate_count);
        receipt_payload["configuredCommandRefs"] =
            serde_json::to_value(&attestation.configured_command_refs).unwrap();
        receipt_payload["gates"] = serde_json::to_value(&attestation.gates).unwrap();
        receipt_payload["toolchainDigest"] = serde_json::json!(attestation.toolchain_digest);
        receipt_payload["environmentDigest"] = serde_json::json!(attestation.environment_digest);
        receipt_payload["receiptDigest"] = serde_json::json!(missing_attestation);
        receipt_payload["gateReceiptDigest"] = serde_json::json!(missing_attestation);
        receipt_payload["receiptAttestationSha256"] = serde_json::json!(missing_attestation);
        receipt_payload["receiptAttestationLen"] = serde_json::json!(missing_bytes.len() as u64);
        let receipt_event = ledger::event(
            "CollectGateSuccessReceipt",
            "runtime:orch",
            Some("B97"),
            Some("r44"),
            receipt_payload,
        );
        let receipt = CollectGateReceiptRef {
            event_id: receipt_event.event_id.clone(),
            attestation_sha256: missing_attestation.to_string(),
            attestation_len: missing_bytes.len() as u64,
        };
        let terminal_payload = report_collect_payload(
            ctx,
            claimed,
            &action_id,
            Some(branch_sha),
            "forged-owner",
            "forged-generation",
            Some(&receipt),
        )
        .unwrap();
        let mut events = vec![claim, executing];
        events.extend(gate_events);
        events.push(receipt_event);
        events.push(ledger::event(
            "ReportCollectExecuted",
            "runtime:orch",
            Some("B97"),
            Some("r44"),
            terminal_payload.clone(),
        ));
        events.push(ledger::event(
            "ReportCollectCompleted",
            "runtime:orch",
            Some("B97"),
            Some("r44"),
            terminal_payload,
        ));
        ledger::append(&site.root, "r44", &events).unwrap();
    }

    fn rewrite_test_ledger(site: &TierfRoot, mutator: impl FnOnce(&mut Vec<EventRecord>)) {
        let mut events = read_events(&site.root);
        mutator(&mut events);
        let mut encoded = Vec::new();
        for event in events {
            serde_json::to_writer(&mut encoded, &event).unwrap();
            encoded.push(b'\n');
        }
        std::fs::write(
            site.root.join("coordination/rounds/r44/events.jsonl"),
            encoded,
        )
        .unwrap();
    }

    fn receipt_ref_from_events(events: &[EventRecord]) -> CollectGateReceiptRef {
        let receipt = events
            .iter()
            .find(|event| event.kind == "CollectGateSuccessReceipt")
            .unwrap();
        let payload = receipt.payload.as_ref().unwrap();
        let (attestation_sha256, attestation_len) =
            receipt_attestation_ref_from_payload(payload, "test receipt").unwrap();
        CollectGateReceiptRef {
            event_id: receipt.event_id.clone(),
            attestation_sha256,
            attestation_len,
        }
    }

    fn rebind_test_receipt_payload(
        payload: &mut serde_json::Value,
        receipt_ref: &CollectGateReceiptRef,
    ) {
        bind_attestation_ref(payload, receipt_ref);
        if payload.get("receiptDigest").is_some() {
            payload["receiptDigest"] = serde_json::json!(receipt_ref.attestation_sha256);
        }
    }

    fn append_cross_action_attestation_reuse(
        site: &TierfRoot,
        ctx: &crate::attempt::DispatchContext,
        claimed: &crate::attempt::ClaimedEvidence,
        branch_sha: &str,
        reused_ref: &CollectGateReceiptRef,
        reused_attestation: &CollectReceiptAttestation,
    ) {
        let action_id = report_collect_action_id(
            "B97-A0001",
            &claimed.canonical_path,
            &claimed.sha256,
            &claimed.control_epoch,
        );
        let owner = "cross-action-owner";
        let generation = "cross-action-generation";
        let mut claim_payload =
            report_collect_payload(ctx, claimed, &action_id, None, owner, generation, None)
                .unwrap();
        claim_payload["leaseUntil"] = serde_json::json!("2999-01-01T00:00:00Z");
        let claim = ledger::event(
            "ReportCollectClaimed",
            "runtime:orch",
            Some("B97"),
            Some("r44"),
            claim_payload.clone(),
        );
        let executing = ledger::event(
            "ReportCollectExecuting",
            "runtime:orch",
            Some("B97"),
            Some("r44"),
            claim_payload,
        );
        let subject_tree_sha =
            gitx::rev_parse(&site.root, &format!("{branch_sha}^{{tree}}")).unwrap();
        let gate_events = reused_attestation
            .gates
            .iter()
            .map(|gate| {
                ledger::event(
                    "GateExecuted",
                    "runtime:orch",
                    Some("B97"),
                    Some("r44"),
                    serde_json::json!({
                        "commandRef": gate.command_ref,
                        "phase": "collect",
                        "gateRunId": gate.gate_run_id,
                        "exitCode": gate.exit_code,
                        "durationMs": gate.duration_ms,
                        "subjectTreeSha": subject_tree_sha,
                        "logSha256": gate.raw_log_cas_sha256,
                        "logBytes": gate.raw_log_len,
                        "toolchainDigest": reused_attestation.toolchain_digest,
                        "environmentDigest": reused_attestation.environment_digest,
                    }),
                )
            })
            .collect::<Vec<_>>();
        let mut receipt_payload = report_collect_payload(
            ctx,
            claimed,
            &action_id,
            Some(branch_sha),
            owner,
            generation,
            None,
        )
        .unwrap();
        receipt_payload["receiptVersion"] = serde_json::json!(2);
        receipt_payload["gateCount"] = serde_json::json!(reused_attestation.configured_gate_count);
        receipt_payload["configuredCommandRefs"] =
            serde_json::to_value(&reused_attestation.configured_command_refs).unwrap();
        receipt_payload["gates"] = serde_json::to_value(&reused_attestation.gates).unwrap();
        receipt_payload["toolchainDigest"] = serde_json::json!(reused_attestation.toolchain_digest);
        receipt_payload["environmentDigest"] =
            serde_json::json!(reused_attestation.environment_digest);
        receipt_payload["receiptDigest"] = serde_json::json!(reused_ref.attestation_sha256);
        bind_attestation_ref(&mut receipt_payload, reused_ref);
        let receipt_event = ledger::event(
            "CollectGateSuccessReceipt",
            "runtime:orch",
            Some("B97"),
            Some("r44"),
            receipt_payload,
        );
        let rebound = CollectGateReceiptRef {
            event_id: receipt_event.event_id.clone(),
            ..reused_ref.clone()
        };
        let terminal_payload = report_collect_payload(
            ctx,
            claimed,
            &action_id,
            Some(branch_sha),
            owner,
            generation,
            Some(&rebound),
        )
        .unwrap();
        let mut events = vec![claim, executing];
        events.extend(gate_events);
        events.push(receipt_event);
        events.push(ledger::event(
            "ReportCollectExecuted",
            "runtime:orch",
            Some("B97"),
            Some("r44"),
            terminal_payload.clone(),
        ));
        events.push(ledger::event(
            "ReportCollectCompleted",
            "runtime:orch",
            Some("B97"),
            Some("r44"),
            terminal_payload,
        ));
        ledger::append(&site.root, "r44", &events).unwrap();
    }

    #[test]
    fn missing_base_sha_key_in_payload_handled_gracefully() {
        // payload 缺 baseSha 键时刷新行为：field("baseSha") 返 None，不 panic
        let ev = orch_core::EventRecord {
            event_id: "x".into(),
            ts: "t".into(),
            actor: "a".into(),
            kind: "DispatchIssued".into(),
            task_id: Some("B47".into()),
            round: Some("r27".into()),
            payload: Some(serde_json::json!({"agent": "x"})), // 无 baseSha
            extra: Default::default(),
        };
        let p = latest_dispatch_facts(&[ev], "B47").expect("有 DispatchIssued 应返 Some");
        assert!(p.get("baseSha").is_none()); // 缺键 → None，不 panic
    }

    #[test]
    fn three_dispatches_returns_last_one() {
        // 同 task 三条派发 → 取第三条（纠正性重派末位优先）
        let mk = |base: &str| orch_core::EventRecord {
            event_id: format!("e{base}"),
            ts: "t".into(),
            actor: "a".into(),
            kind: "DispatchIssued".into(),
            task_id: Some("B47".into()),
            round: Some("r27".into()),
            payload: Some(serde_json::json!({"baseSha": base})),
            extra: Default::default(),
        };
        let events = vec![mk("aaa"), mk("bbb"), mk("ccc")];
        let p = latest_dispatch_facts(&events, "B47").expect("有派发应返 Some");
        assert_eq!(p.get("baseSha").and_then(|v| v.as_str()), Some("ccc"));
    }

    #[test]
    fn superseded_dispatches_preserve_global_ledger_order() {
        let mk = |id: &str, task: &str| orch_core::EventRecord {
            event_id: id.into(),
            ts: "t".into(),
            actor: "a".into(),
            kind: "DispatchIssued".into(),
            task_id: Some(task.into()),
            round: Some("r29".into()),
            payload: Some(serde_json::json!({"agent": format!("agent-{task}")})),
            extra: Default::default(),
        };
        let events = vec![mk("a1", "A"), mk("b1", "B"), mk("a2", "A"), mk("b2", "B")];

        assert_eq!(
            superseded_dispatches(&events),
            vec![
                SupersededDispatch {
                    task_id: "A".into(),
                    dispatch_event_id: "a1".into(),
                    agent: Some("agent-A".into()),
                },
                SupersededDispatch {
                    task_id: "B".into(),
                    dispatch_event_id: "b1".into(),
                    agent: Some("agent-B".into()),
                },
            ]
        );
    }

    #[test]
    fn superseded_dispatch_without_string_agent_returns_none() {
        let mk = |id: &str, agent: serde_json::Value| orch_core::EventRecord {
            event_id: id.into(),
            ts: "t".into(),
            actor: "a".into(),
            kind: "DispatchIssued".into(),
            task_id: Some("B55".into()),
            round: Some("r29".into()),
            payload: Some(serde_json::json!({"agent": agent})),
            extra: Default::default(),
        };
        let events = vec![
            mk("first", serde_json::json!(55)),
            mk("latest", serde_json::json!("A")),
        ];

        assert_eq!(
            superseded_dispatches(&events),
            vec![SupersededDispatch {
                task_id: "B55".into(),
                dispatch_event_id: "first".into(),
                agent: None,
            }]
        );
    }

    #[test]
    fn dispatch_production_first_call_terminal_replay_and_forged_terminal() {
        let site = test_root("dispatch");
        let (attempt, go_rel, _) = append_modern_dispatch(&site);
        finish_dispatch_wake_with_hook(
            &site.root,
            "r44",
            "B97",
            "executor-desktop",
            &attempt,
            &go_rel,
            false,
            true,
            &mut |_| Ok(()),
        )
        .unwrap();
        wait_for_lines(&site.wake_marker, 1);
        let first = read_events(&site.root);
        for kind in [
            "DispatchWakeClaimed",
            "DispatchWakeLaunching",
            "DispatchWakeDelivered",
            "DispatchWakeCompleted",
        ] {
            assert_eq!(first.iter().filter(|event| event.kind == kind).count(), 1);
        }
        let heartbeat_path = site
            .root
            .join("coordination/runtime/heartbeats/executor-desktop.json");
        std::fs::write(&heartbeat_path, b"{\"sentinel\":\"completed-replay\"}").unwrap();

        finish_dispatch_wake_with_hook(
            &site.root,
            "r44",
            "B97",
            "executor-desktop",
            &attempt,
            &go_rel,
            false,
            true,
            &mut |_| Ok(()),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            std::fs::read_to_string(&site.wake_marker)
                .unwrap()
                .lines()
                .count(),
            1,
            "Completed replay must not wake again"
        );
        assert_eq!(read_events(&site.root).len(), first.len());
        assert_eq!(
            std::fs::read(&heartbeat_path).unwrap(),
            b"{\"sentinel\":\"completed-replay\"}",
            "Completed replay must not retry the heartbeat projection write"
        );

        let forged = test_root("dispatch-forged");
        let (attempt, go_rel, base_sha) = append_modern_dispatch(&forged);
        let action_id = crate::attempt::dispatch_wake_action_id(
            "r44",
            "B97",
            &attempt,
            "executor-desktop",
            &base_sha,
            &go_rel,
        );
        ledger::append(
            &forged.root,
            "r44",
            &[ledger::event(
                "DispatchWakeCompleted",
                "runtime:forged",
                Some("B97"),
                Some("r44"),
                durable_wake_payload(
                    &attempt,
                    "executor-desktop",
                    &base_sha,
                    &go_rel,
                    &action_id,
                    "forged-generation",
                    "forged-owner",
                ),
            )],
        )
        .unwrap();
        assert!(
            finish_dispatch_wake_with_hook(
                &forged.root,
                "r44",
                "B97",
                "executor-desktop",
                &attempt,
                &go_rel,
                false,
                true,
                &mut |_| Ok(()),
            )
            .is_err(),
            "forged terminal without legal lineage must fail closed"
        );
        assert!(!forged.wake_marker.exists());
    }

    /// B107：已 commit 的 REPORT 首拍即 Ready，返回的 pin 等于当前 HEAD，零 sleep。
    #[test]
    fn wait_for_report_commit_ready_immediately_pins_current_head() {
        let site = test_root("grace-ready");
        let (report_rel, report, branch_sha) = commit_report(&site);
        let claimed = claimed_report(&site, &report_rel, report);
        let mut elapsed = || Duration::ZERO;
        let mut sleeps = 0usize;
        let mut sleeper = |_: Duration| sleeps += 1;
        let pinned = wait_for_report_commit(
            &site.root,
            "task/B97",
            &report_rel,
            &claimed,
            REPORT_COMMIT_GRACE,
            REPORT_COMMIT_POLL,
            &mut read_report_at_head,
            &mut elapsed,
            &mut sleeper,
        )
        .unwrap();
        assert_eq!(pinned, branch_sha, "Ready 必须钉观察一致时刻的当前 HEAD");
        assert_eq!(sleeps, 0, "首拍 Ready 不得 sleep");
    }

    /// B107：HEAD 缺 REPORT 路径 → Wait；等待期间 commit 后下一拍 Ready。
    /// clock/sleeper 脚本化：sleeper 第一拍执行 commit，确定性零真实等待。
    #[test]
    fn wait_for_report_commit_waits_then_ready_after_commit() {
        let site = test_root("grace-wait");
        let report_rel = "coordination/rounds/r44/reports/B97-REPORT.md".to_string();
        let report = b"## 0 execution\nMODEL=test\nDEPTH=high\nCAPTURE=unit\n".to_vec();
        test_git(&site.root, &["checkout", "-q", "-b", "task/B97"]);
        let report_path = site.root.join(&report_rel);
        std::fs::create_dir_all(report_path.parent().unwrap()).unwrap();
        std::fs::write(&report_path, &report).unwrap();
        test_git(&site.root, &["checkout", "-q", "main"]);
        let claimed = claimed_report(&site, &report_rel, report);
        let polls = std::cell::Cell::new(0u64);
        let mut elapsed = || Duration::from_secs(REPORT_COMMIT_POLL.as_secs() * polls.get());
        let mut committed = false;
        let mut sleeps = Vec::new();
        let mut sleeper = |d: Duration| {
            sleeps.push(d);
            polls.set(polls.get() + 1);
            if !committed {
                committed = true;
                // 第一拍 Wait 期间 executor 完成 commit（与生产时序一致）。
                test_git(&site.root, &["checkout", "-q", "task/B97"]);
                test_git(&site.root, &["add", report_rel.as_str()]);
                test_git(
                    &site.root,
                    &[
                        "-c",
                        "user.name=orch-test",
                        "-c",
                        "user.email=orch@test.invalid",
                        "commit",
                        "-q",
                        "-m",
                        "report",
                    ],
                );
                test_git(&site.root, &["checkout", "-q", "main"]);
            }
        };
        let pinned = wait_for_report_commit(
            &site.root,
            "task/B97",
            &report_rel,
            &claimed,
            REPORT_COMMIT_GRACE,
            REPORT_COMMIT_POLL,
            &mut read_report_at_head,
            &mut elapsed,
            &mut sleeper,
        )
        .unwrap();
        assert_eq!(sleeps, vec![REPORT_COMMIT_POLL], "恰好等一拍 2s");
        assert_eq!(
            pinned,
            test_git(&site.root, &["rev-parse", "task/B97"]),
            "pin 必须等于 commit 后的新 HEAD，不得是等待开始时的旧 SHA"
        );
    }

    // Legacy dispatch IO is retired. Preserve its admission laws as pure
    // tables; exercise the public API family only to prove pre-effect refusal.
    #[test]
    fn legacy_dispatch_family_is_byte_zero_before_hooks_storage_or_provider() {
        let site = test_root("legacy-dispatch-generation");
        fn collect(root: &Path, path: &Path, out: &mut std::collections::BTreeMap<PathBuf, Option<Vec<u8>>>) {
            if !path.exists() { return; }
            let metadata = std::fs::symlink_metadata(path).unwrap();
            assert!(!metadata.file_type().is_symlink());
            out.insert(path.strip_prefix(root).unwrap().to_path_buf(),
                metadata.is_file().then(|| std::fs::read(path).unwrap()));
            if metadata.is_dir() {
                for entry in std::fs::read_dir(path).unwrap() {
                    collect(root, &entry.unwrap().path(), out);
                }
            }
        }
        let snapshot = || {
            let mut tree = std::collections::BTreeMap::new();
            for path in ["coordination/runtime", "coordination/rounds/r44/dispatch", ".worktrees",
                "orch/target", ".cowork-temp", ".orch"] {
                collect(&site.root, &site.root.join(path), &mut tree);
            }
            (
                std::fs::read(site.root.join("coordination/rounds/r44/events.jsonl")).unwrap(),
                test_git(&site.root, &["rev-parse", "HEAD"]),
                test_git(&site.root, &["status", "--porcelain", "--untracked-files=all"]),
                test_git(&site.root, &["worktree", "list", "--porcelain"]),
                test_git(&site.root, &["for-each-ref", "--format=%(refname) %(objectname)"]),
                tree,
                site.wake_marker.exists(),
            )
        };
        let before = snapshot();
        for no_wake in [false, true] {
            for entry in 0..5 {
                let mut hook_ran = false;
                let result = match entry {
                    0 => run_dispatch(&site.root, "B97", no_wake).map(|_| ()),
                    1 => run_dispatch_with_override(&site.root, "B97", no_wake, true).map(|_| ()),
                    2 => run_dispatch_with_hook(&site.root, "B97", no_wake, |_, _| {
                        hook_ran = true; Ok(())
                    }).map(|_| ()),
                    3 => run_dispatch_with_hook_and_override(&site.root, "B97", no_wake, true, |_, _| {
                        hook_ran = true; Ok(())
                    }).map(|_| ()),
                    _ => crate::attempt::run_approved_reattempt_dispatch(
                        &site.root, "B97", no_wake, true, "authorized fixture reason",
                    ).map(|_| ()),
                };
                assert!(format!("{:#}", result.unwrap_err()).contains("dispatch writer 已退役"));
                assert!(!hook_ran, "retired generation reached race hook");
                assert_eq!(snapshot(), before, "entry={entry} no_wake={no_wake}");
            }
        }
    }

    #[test]
    fn dispatch_generation_reader_requires_exact_current_open_schema3() {
        let root = crate::util::test_scratch_dir("b323-dispatch-generation");
        std::fs::create_dir_all(root.join("coordination/runtime")).unwrap();
        std::fs::create_dir_all(root.join("coordination/rounds/r323")).unwrap();
        std::fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r323\n").unwrap();
        let path = root.join("coordination/rounds/r323/events.jsonl");
        let opened = ledger::event("RoundOpened", "runtime:orch", None, Some("r323"),
            serde_json::json!({"contractSchemaVersion":3}));
        let bytes = format!("{}\n", serde_json::to_string(&opened).unwrap());
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(require_current_open_schema3_dispatch_generation(&root, Some("r323")).unwrap(), "r323");
        assert!(require_current_open_schema3_dispatch_generation(&root, Some("r-other")).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bytes);
        let closed = ledger::event("RoundClosed", "runtime:orch", None, Some("r323"), serde_json::json!({}));
        let closed_bytes = format!("{bytes}{}\n", serde_json::to_string(&closed).unwrap());
        std::fs::write(&path, &closed_bytes).unwrap();
        assert!(require_current_open_schema3_dispatch_generation(&root, None).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), closed_bytes);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_reassignment_classifies_stall_unknown_death_and_growing_log_without_writes() {
        assert!(reassignment_gate(false, Some(false), false, Some(true)).unwrap_err().contains("not dead"));
        assert!(reassignment_gate(true, None, false, Some(true)).is_err());
        assert!(reassignment_gate(true, Some(true), false, Some(true)).is_err());
        assert!(reassignment_gate(true, Some(false), true, Some(true)).unwrap_err().contains("still advancing"));
        assert!(reassignment_gate(true, Some(false), false, Some(false)).is_err());
        assert!(reassignment_gate(true, Some(false), false, Some(true)).is_ok());
    }

    #[test]
    fn legacy_override_audit_flags_and_companion_identity_survive_read_only_decode() {
        let base = "a".repeat(40);
        let first = ledger::event("DispatchIssued", "runtime:orch", Some("B97"), Some("r44"),
            serde_json::json!({"attemptId":"B97-A0001","attemptNo":1,"agent":"executor-desktop",
                "baseSha":base,"goPath":"coordination/rounds/r44/dispatch/executor-desktop/GO-B97-A0001.md"}));
        let ended = ledger::event("AttemptCrashed", "runtime:orch", Some("B97"), Some("r44"),
            serde_json::json!({"attemptId":"B97-A0001","attemptNo":1,"agent":"executor-desktop"}));
        let dispatch = ledger::event("DispatchIssued", "runtime:orch", Some("B97"), Some("r44"),
            serde_json::json!({"attemptId":"B97-A0002","attemptNo":2,"agent":"executor-claw",
                "baseSha":base,"goPath":"coordination/rounds/r44/dispatch/executor-claw/GO-B97-A0002.md",
                "previousAttemptId":"B97-A0001","previousAgent":"executor-desktop",
                "reassignment":true,"overrideAmbiguousActive":true}));
        let companion = ledger::event("ReassignmentIssued", "runtime:orch", Some("B97"), Some("r44"),
            serde_json::json!({"previousAttemptId":"B97-A0001","previousAgent":"executor-desktop",
                "attemptId":"B97-A0002","attemptNo":2,"agent":"executor-claw","override":true}));
        let bytes = serde_json::to_vec(&vec![first, ended, dispatch, companion]).unwrap();
        let history: Vec<EventRecord> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(history[2].payload.as_ref().unwrap()["overrideAmbiguousActive"], true);
        assert_eq!(history[3].payload.as_ref().unwrap()["override"], true);
        for key in ["attemptId", "attemptNo", "agent", "previousAttemptId", "previousAgent"] {
            assert_eq!(history[2].payload.as_ref().unwrap()[key], history[3].payload.as_ref().unwrap()[key]);
        }
        let crate::attempt::DispatchPlan::Existing { current, needs_companion, .. } =
            crate::attempt::plan_dispatch_locked(&history, "B97", "executor-claw", &base, "r44").unwrap()
        else { panic!("historical override pair did not decode as the existing attempt"); };
        assert_eq!(current.attempt.attempt_id, "B97-A0002");
        assert!(current.reassignment && current.has_companion_reassignment && !needs_companion);
        assert_eq!(current.previous_attempt_id.as_deref(), Some("B97-A0001"));
        assert_eq!(current.previous_agent.as_deref(), Some("executor-desktop"));
    }

    #[test]
    fn legacy_successor_archive_decisions_keep_clean_dirty_and_quiescence_boundaries() {
        for kind in ["AttemptBlocked", "AttemptTimedOut", "AttemptFailed"] {
            assert!(successor_archive_event_applies(kind, None));
        }
        for verdict in ["FAIL", "BLOCKED"] {
            assert!(successor_archive_event_applies("VerdictIssued", Some(&serde_json::json!({"verdict":verdict}))));
        }
        for verdict in ["PASS", "UNKNOWN"] {
            assert!(!successor_archive_event_applies("VerdictIssued", Some(&serde_json::json!({"verdict":verdict}))));
        }
        assert!(!successor_archive_event_applies("AttemptCrashed", None));
        for clean in [false, true] {
            for alive in [None, Some(false)] {
                assert_eq!(blocked_successor_archive_plan(clean, alive, false).unwrap(),
                    if clean { BlockedSuccessorArchive::SkipClean } else { BlockedSuccessorArchive::ArchiveWip });
            }
            assert!(blocked_successor_archive_plan(clean, Some(true), false).is_err());
            assert!(blocked_successor_archive_plan(clean, Some(false), true).is_err());
        }
        assert!(wip_archive_gate(false, Some(true), false).is_ok());
        assert!(wip_archive_gate(true, None, false).is_ok());
        assert!(wip_archive_gate(false, None, false).is_err());
        assert!(wip_archive_gate(true, Some(false), false).is_err());
        assert!(wip_archive_gate(false, Some(true), true).is_err());
    }

    #[test]
    fn first_attempt_planning_ignores_oracle_history_without_dispatching() {
        let events = [ledger::event("SeedOracleVerified", "runtime:orch", Some("B97"), Some("r44"),
            serde_json::json!({"expectedRed":"compile","measured":{"compileErrors":1}}))];
        let plan = crate::attempt::plan_dispatch_locked(&events, "B97", "legacy-agent", &"a".repeat(40), "r44").unwrap();
        let crate::attempt::DispatchPlan::New { next, .. } = plan else { panic!("expected first attempt"); };
        assert_eq!(next.attempt.attempt_id, "B97-A0001");
        assert_eq!(next.attempt.ordinal, 1);
    }

    #[test]
    fn dependency_admission_keeps_recorded_and_inherited_base_requirements() {
        let dependencies = vec!["B96".to_string()];
        let base = "a".repeat(40);
        let merged = "b".repeat(40);
        assert!(matches!(crate::plan::dependency_dispatch_admission(
            &dependencies, &[], Some(&base), &|_| true,
        ), crate::plan::DependencyAdmission::BlockedByDependency { blockers } if blockers == dependencies));
        let recorded = vec![("B96".to_string(), merged.clone())];
        let result = crate::plan::dependency_dispatch_admission(
            &dependencies, &recorded, Some(&base), &|_| false,
        );
        let crate::plan::DependencyAdmission::ForwardBaselineRequired {
            dependency, merge_sha, attempt_base_sha,
        } = result else { panic!("stale inherited baseline was admitted"); };
        assert_eq!(dependency, "B96");
        assert_eq!(merge_sha, merged);
        assert_eq!(attempt_base_sha, base);
        assert!(matches!(crate::plan::dependency_dispatch_admission(
            &dependencies, &recorded, Some(&base), &|_| true,
        ), crate::plan::DependencyAdmission::Admitted));
    }

    #[test]
    fn legacy_capacity_and_role_classification_remain_read_only() {
        let make = |kind: &str| ledger::event(kind, "runtime:orch", Some("B97"), Some("r44"),
            serde_json::json!({"attemptId":"B97-A0001","attemptNo":1,"agent":"legacy-agent"}));
        let mut events = vec![make("DispatchIssued"), make("ReportObserved")];
        let load = crate::legacy::agent_inflight_load_from_events(&events, "r44").unwrap();
        assert!(crate::legacy::capacity_admits(&load, "legacy-agent", 1).is_err());
        events.push(make("AttemptFailed"));
        let released = crate::legacy::agent_inflight_load_from_events(&events, "r44").unwrap();
        crate::legacy::capacity_admits(&released, "legacy-agent", 1).unwrap();
        let roles = vec!["primary-review".to_string()];
        assert!(crate::legacy::role_admits(&roles, "implement").unwrap_err().contains("runtime role"));
        crate::legacy::role_admits(&roles, "primary-review").unwrap();
    }

    #[test]
    fn verdict_successor_routing_is_payload_and_attempt_scoped() {
        let event = |kind: &str, payload: serde_json::Value| {
            ledger::event(kind, "verifier:root", Some("B97"), Some("r44"), payload)
        };
        for verdict in ["PASS", "UNKNOWN"] {
            let events = vec![event(
                "VerdictIssued",
                serde_json::json!({
                    "attemptId": "B97-A0001",
                    "verdict": verdict,
                }),
            )];
            assert!(!terminal_attempt_uses_successor_archive(
                &events,
                "B97",
                "B97-A0001",
            ));
        }

        let missing_payload = {
            let mut event = event("VerdictIssued", serde_json::Value::Null);
            event.payload = None;
            vec![event]
        };
        assert!(!terminal_attempt_uses_successor_archive(
            &missing_payload,
            "B97",
            "B97-A0001",
        ));
        let null_payload = vec![event("VerdictIssued", serde_json::Value::Null)];
        assert!(!terminal_attempt_uses_successor_archive(
            &null_payload,
            "B97",
            "B97-A0001",
        ));
        let wrong_attempt = vec![event(
            "VerdictIssued",
            serde_json::json!({
                "attemptId": "B97-A9999",
                "verdict": "FAIL",
            }),
        )];
        assert!(!terminal_attempt_uses_successor_archive(
            &wrong_attempt,
            "B97",
            "B97-A0001",
        ));
        let crashed = vec![event(
            "AttemptCrashed",
            serde_json::json!({
                "attemptId": "B97-A0001",
                "verdict": "FAIL",
            }),
        )];
        assert!(!terminal_attempt_uses_successor_archive(
            &crashed,
            "B97",
            "B97-A0001",
        ));
    }

    /// B107：到期仍未 commit → 报「未 commit」（不得误报 writeSet），
    /// 每拍 sleep 恰好 2s，共 60 拍（elapsed 0,2,…,118 Wait；120 拒绝）。
    #[test]
    fn wait_for_report_commit_deadline_reports_uncommitted_not_writeset() {
        let site = test_root("grace-deadline");
        test_git(&site.root, &["branch", "task/B97"]);
        let report_rel = "coordination/rounds/r44/reports/B97-REPORT.md";
        let claimed = claimed_report(&site, report_rel, b"never committed".to_vec());
        let ticks = std::cell::Cell::new(0u64);
        let mut elapsed = || Duration::from_secs(REPORT_COMMIT_POLL.as_secs() * ticks.get());
        let mut sleeps = Vec::new();
        let mut sleeper = |d: Duration| {
            sleeps.push(d);
            ticks.set(ticks.get() + 1);
        };
        let error = wait_for_report_commit(
            &site.root,
            "task/B97",
            report_rel,
            &claimed,
            REPORT_COMMIT_GRACE,
            REPORT_COMMIT_POLL,
            &mut read_report_at_head,
            &mut elapsed,
            &mut sleeper,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("未 commit"),
            "到期必须报未 commit: {message}"
        );
        assert!(
            !message.contains("writeSet"),
            "到期拒绝不得误报 writeSet: {message}"
        );
        assert_eq!(sleeps.len(), 60, "120s/2s 恰好 60 拍等待");
        assert!(sleeps.iter().all(|&d| d == REPORT_COMMIT_POLL));
    }

    /// B107：HEAD 含该路径但字节不同 → 立即拒绝，零 sleep。
    #[test]
    fn wait_for_report_commit_different_bytes_rejects_immediately() {
        let site = test_root("grace-diff");
        let (report_rel, _report, _sha) = commit_report(&site);
        let forged = claimed_report(&site, &report_rel, b"different bytes".to_vec());
        let mut elapsed = || Duration::ZERO;
        let mut sleeps = 0usize;
        let mut sleeper = |_: Duration| sleeps += 1;
        let error = wait_for_report_commit(
            &site.root,
            "task/B97",
            &report_rel,
            &forged,
            REPORT_COMMIT_GRACE,
            REPORT_COMMIT_POLL,
            &mut read_report_at_head,
            &mut elapsed,
            &mut sleeper,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("字节与 observed evidence 不一致"),
            "字节不同必须立即拒绝: {message}"
        );
        assert_eq!(sleeps, 0, "字节不同不得等待");
    }

    /// B107 self-repair：注入 Fault（仓库/对象/读取故障）必须立即传播为 Err，
    /// 零 sleep、不经过决策 clock，不得被误归为 Missing 白等 120s 再误报
    /// 「未 commit」。确定性：reader 闭包直接返回 Err，无真实 git 参与。
    #[test]
    fn wait_for_report_commit_fault_propagates_immediately_without_sleep() {
        let site = test_root("grace-fault");
        test_git(&site.root, &["branch", "task/B97"]);
        let report_rel = "coordination/rounds/r44/reports/B97-REPORT.md";
        let claimed = claimed_report(&site, report_rel, b"unreadable".to_vec());
        // clock 每拍 +2s：绿色路径下 Fault 在决策前传播、elapsed 零调用；
        // 若 Fault 被误归为 Missing（变异），脚本 clock 让其在 60 拍后确定性
        // 到期报「未 commit」，断言照样红（而非悬挂）。
        let ticks = std::cell::Cell::new(0u64);
        let mut elapsed_calls = 0usize;
        let mut elapsed = || {
            elapsed_calls += 1;
            let now = Duration::from_secs(REPORT_COMMIT_POLL.as_secs() * ticks.get());
            ticks.set(ticks.get() + 1);
            now
        };
        let mut sleeps = 0usize;
        let mut sleeper = |_: Duration| sleeps += 1;
        let mut reader = |_: &Path, _: &str, _: &str| -> Result<ReportAtHead> {
            Err(anyhow::anyhow!(
                "git ls-tree 观察 HEAD 失败（模拟仓库故障）"
            ))
        };
        let error = wait_for_report_commit(
            &site.root,
            "task/B97",
            report_rel,
            &claimed,
            REPORT_COMMIT_GRACE,
            REPORT_COMMIT_POLL,
            &mut reader,
            &mut elapsed,
            &mut sleeper,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("模拟仓库故障"),
            "Fault 原文必须原样传播: {message}"
        );
        assert!(
            !message.contains("未 commit"),
            "Fault 不得误报未 commit: {message}"
        );
        assert_eq!(sleeps, 0, "Fault 不得进入等待");
        assert_eq!(elapsed_calls, 0, "Fault 不得经过等待决策 clock");
    }

    /// B107 self-repair：真实 reader 对「真 Missing」的判定——ls-tree 成功且
    /// 精确空输出才 Missing（真实 git 库，路径不在 HEAD 树），未被 Fault 化。
    #[test]
    fn read_report_at_head_genuine_missing_via_real_git() {
        let site = test_root("grace-missing");
        let head = test_git(&site.root, &["rev-parse", "main"]);
        let observed = read_report_at_head(
            &site.root,
            &head,
            "coordination/rounds/r44/reports/B97-REPORT.md",
        )
        .unwrap();
        assert!(
            matches!(observed, ReportAtHead::Missing),
            "ls-tree 精确空输出必须判定为真 Missing"
        );
    }

    /// B107 生产消费回归：REPORT 已写盘但尚未 commit 时，完整 durable collect
    /// 链必须先进 commit-grace 等待（脚本化 clock 零真实等待），executor 在第一个
    /// Wait 拍内 commit，随后 Ready → 钉新 HEAD → 跑门 → 落 terminal。若等待逻辑
    /// 未被生产消费（退化为立即 pin+show_bytes），本用例立即红。
    #[test]
    fn collect_production_consumes_commit_grace_before_durable_claim() {
        let site = test_root("collect-grace");
        let (_, go_rel, base_sha) = append_modern_dispatch(&site);
        let report_rel = "coordination/rounds/r44/reports/B97-REPORT.md".to_string();
        let report = b"## 0 execution\nMODEL=test\nDEPTH=high\nCAPTURE=unit-production-path\n\n## 1 changes\nnone\n".to_vec();
        test_git(&site.root, &["checkout", "-q", "-b", "task/B97"]);
        let report_path = site.root.join(&report_rel);
        std::fs::create_dir_all(report_path.parent().unwrap()).unwrap();
        std::fs::write(&report_path, &report).unwrap();
        test_git(&site.root, &["checkout", "-q", "main"]);
        let claimed = claimed_report(&site, &report_rel, report);
        let ctx = modern_ctx(&go_rel, &base_sha);
        let card = card::load(&site.root, "r44", "B97").unwrap();
        let polls = std::cell::Cell::new(0u64);
        let mut elapsed = || Duration::from_secs(REPORT_COMMIT_POLL.as_secs() * polls.get());
        let mut committed = false;
        let mut sleeps = Vec::new();
        let mut sleeper = |d: Duration| {
            sleeps.push(d);
            polls.set(polls.get() + 1);
            if !committed {
                committed = true;
                test_git(&site.root, &["checkout", "-q", "task/B97"]);
                test_git(&site.root, &["add", report_rel.as_str()]);
                test_git(
                    &site.root,
                    &[
                        "-c",
                        "user.name=orch-test",
                        "-c",
                        "user.email=orch@test.invalid",
                        "commit",
                        "-q",
                        "-m",
                        "report",
                    ],
                );
                test_git(&site.root, &["checkout", "-q", "main"]);
            }
        };
        let outcome = collect_claimed_report(
            &site.root,
            "r44",
            "B97",
            &card,
            "task/B97",
            &report_rel,
            &ctx,
            &claimed,
            &mut elapsed,
            &mut sleeper,
            &mut |_| Ok(()),
        )
        .unwrap();
        assert_eq!(
            sleeps,
            vec![REPORT_COMMIT_POLL],
            "durable claim 前必须恰好等待一拍（未 commit → Wait → commit → Ready）"
        );
        assert_eq!(outcome.gates.len(), 1);
        wait_for_lines(&site.gate_marker, 1);
        let events = read_events(&site.root);
        assert!(events
            .iter()
            .any(|event| event.kind == "ReportCollectCompleted"));
        let committed_sha = test_git(&site.root, &["rev-parse", "task/B97"]);
        assert!(
            events.iter().any(|event| {
                event.kind == "ReportCollectCompleted"
                    && event
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("branchSha"))
                        .and_then(serde_json::Value::as_str)
                        == Some(committed_sha.as_str())
            }),
            "terminal 必须绑定 commit-grace 之后钉到的新 HEAD"
        );
    }

    #[test]
    fn collect_production_first_call_terminal_replay_and_forged_terminal() {
        let site = test_root("collect");
        let (_, go_rel, base_sha) = append_modern_dispatch(&site);
        let (report_rel, report, _) = commit_report(&site);
        let claimed = claimed_report(&site, &report_rel, report);
        let ctx = modern_ctx(&go_rel, &base_sha);
        let card = card::load(&site.root, "r44", "B97").unwrap();
        let first = collect_claimed_report(
            &site.root,
            "r44",
            "B97",
            &card,
            "task/B97",
            &report_rel,
            &ctx,
            &claimed,
            &mut || Duration::ZERO,
            &mut |_: Duration| {
                panic!("already-committed REPORT must never enter commit-grace sleep")
            },
            &mut |_| Ok(()),
        )
        .unwrap();
        assert_eq!(first.gates.len(), 1);
        wait_for_lines(&site.gate_marker, 1);
        let first_events = read_events(&site.root);
        assert!(first_events
            .iter()
            .any(|event| event.kind == "ReportCollectCompleted"));
        let replay = collect_claimed_report(
            &site.root,
            "r44",
            "B97",
            &card,
            "task/B97",
            &report_rel,
            &ctx,
            &claimed,
            &mut || Duration::ZERO,
            &mut |_: Duration| {
                panic!("already-committed REPORT must never enter commit-grace sleep")
            },
            &mut |_| Ok(()),
        )
        .unwrap();
        assert!(replay.gates.is_empty());
        assert_eq!(read_events(&site.root).len(), first_events.len());
        assert_eq!(
            std::fs::read_to_string(&site.gate_marker)
                .unwrap()
                .lines()
                .count(),
            1,
            "Completed replay must not rerun the gate"
        );

        let forged = test_root("collect-forged");
        let (_, forged_go, forged_base) = append_modern_dispatch(&forged);
        let (forged_report_rel, forged_report, branch_sha) = commit_report(&forged);
        let forged_claimed = claimed_report(&forged, &forged_report_rel, forged_report);
        let forged_ctx = modern_ctx(&forged_go, &forged_base);
        let action_id = report_collect_action_id(
            "B97-A0001",
            &forged_claimed.canonical_path,
            &forged_claimed.sha256,
            &forged_claimed.control_epoch,
        );
        let old_gate = ledger::event(
            "GateExecuted",
            "runtime:forged",
            Some("B97"),
            Some("r44"),
            serde_json::json!({"commandRef": "testGate", "exitCode": 0}),
        );
        let receipt = CollectGateReceiptRef {
            event_id: old_gate.event_id.clone(),
            attestation_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .into(),
            attestation_len: 1,
        };
        let mut claim_payload = report_collect_payload(
            &forged_ctx,
            &forged_claimed,
            &action_id,
            None,
            "owner-1",
            "generation-1",
            None,
        )
        .unwrap();
        claim_payload["leaseUntil"] = serde_json::json!("2999-01-01T00:00:00Z");
        let executing_payload = claim_payload.clone();
        let executed_payload = report_collect_payload(
            &forged_ctx,
            &forged_claimed,
            &action_id,
            Some(&branch_sha),
            "owner-1",
            "generation-1",
            Some(&receipt),
        )
        .unwrap();
        ledger::append(
            &forged.root,
            "r44",
            &[
                ledger::event(
                    "ReportCollectClaimed",
                    "runtime:forged",
                    Some("B97"),
                    Some("r44"),
                    claim_payload,
                ),
                ledger::event(
                    "ReportCollectExecuting",
                    "runtime:forged",
                    Some("B97"),
                    Some("r44"),
                    executing_payload,
                ),
                old_gate,
                ledger::event(
                    "ReportCollectExecuted",
                    "runtime:forged",
                    Some("B97"),
                    Some("r44"),
                    executed_payload.clone(),
                ),
                ledger::event(
                    "ReportCollectCompleted",
                    "runtime:forged",
                    Some("B97"),
                    Some("r44"),
                    executed_payload,
                ),
            ],
        )
        .unwrap();
        let forged_card = card::load(&forged.root, "r44", "B97").unwrap();
        assert!(
            collect_claimed_report(
                &forged.root,
                "r44",
                "B97",
                &forged_card,
                "task/B97",
                &forged_report_rel,
                &forged_ctx,
                &forged_claimed,
                &mut || Duration::ZERO,
                &mut |_: Duration| panic!(
                    "already-committed REPORT must never enter commit-grace sleep"
                ),
                &mut |_| Ok(()),
            )
            .is_err(),
            "an old successful gate must not authorize a forged terminal"
        );
        assert!(!forged.gate_marker.exists());
    }



    #[test]
    fn collect_replay_rejects_self_consistent_ledger_when_attestation_cas_is_missing() {
        let site = test_root("collect-missing-action-attestation");
        let (_, go_rel, base_sha) = append_modern_dispatch(&site);
        let (report_rel, report, branch_sha) = commit_report(&site);
        let claimed = claimed_report(&site, &report_rel, report);
        let ctx = modern_ctx(&go_rel, &base_sha);
        append_forged_collect_terminal(
            &site,
            &ctx,
            &claimed,
            &branch_sha,
            &[("testGate", 0)],
            &[("testGate", 0, 0)],
        );
        let events = read_events(&site.root);
        let receipt_ref = receipt_ref_from_events(&events);
        assert!(
            collect_cas(&site.root)
                .get(&receipt_ref.attestation_sha256)
                .unwrap()
                .is_none(),
            "test precondition: canonical action-bound attestation must be absent"
        );
        let receipt = events
            .iter()
            .find(|event| event.kind == "CollectGateSuccessReceipt")
            .unwrap();
        assert_eq!(
            receipt.payload.as_ref().unwrap()["receiptDigest"],
            receipt_ref.attestation_sha256
        );
        let card = card::load(&site.root, "r44", "B97").unwrap();
        assert!(
            collect_claimed_report(
                &site.root,
                "r44",
                "B97",
                &card,
                "task/B97",
                &report_rel,
                &ctx,
                &claimed,
                &mut || Duration::ZERO,
                &mut |_: Duration| panic!(
                    "already-committed REPORT must never enter commit-grace sleep"
                ),
                &mut |_| Ok(()),
            )
            .is_err(),
            "ledger-only self-consistent terminal must not replay without attestation CAS"
        );
        assert!(!site.gate_marker.exists());
    }

    #[test]
    fn collect_replay_rejects_missing_wrong_order_and_red_then_green_gate_receipts() {
        let cases: &[(&str, &[(&str, i64)], &[(&str, usize, i64)])] = &[
            ("missing", &[("testGate", 0)], &[("testGate", 0, 0)]),
            (
                "wrong-order",
                &[("testGate2", 0), ("testGate", 0)],
                &[("testGate2", 0, 0), ("testGate", 1, 0)],
            ),
            (
                "red-then-green",
                &[("testGate", 1), ("testGate2", 0)],
                &[("testGate", 0, 1), ("testGate2", 1, 0)],
            ),
        ];
        for (tag, gate_specs, listed_specs) in cases {
            let site = test_root(tag);
            configure_two_gates(&site);
            let (_, go_rel, base_sha) = append_modern_dispatch(&site);
            let (report_rel, report, branch_sha) = commit_report(&site);
            let claimed = claimed_report(&site, &report_rel, report);
            let ctx = modern_ctx(&go_rel, &base_sha);
            append_forged_collect_terminal(
                &site,
                &ctx,
                &claimed,
                &branch_sha,
                gate_specs,
                listed_specs,
            );
            let card = card::load(&site.root, "r44", "B97").unwrap();
            assert!(
                collect_claimed_report(
                    &site.root,
                    "r44",
                    "B97",
                    &card,
                    "task/B97",
                    &report_rel,
                    &ctx,
                    &claimed,
                    &mut || Duration::ZERO,
                    &mut |_: Duration| panic!(
                        "already-committed REPORT must never enter commit-grace sleep"
                    ),
                    &mut |_| Ok(()),
                )
                .is_err(),
                "{tag} forged receipt must fail through production replay"
            );
            assert!(
                !site.gate_marker.exists(),
                "{tag} replay must not run configured gates"
            );
        }
    }

    #[test]
    fn collect_two_gate_receipt_binds_stable_order_and_replays_without_gates() {
        let site = test_root("collect-two-gate-positive");
        configure_two_gates(&site);
        let (_, go_rel, base_sha) = append_modern_dispatch(&site);
        let (report_rel, report, _) = commit_report(&site);
        let claimed = claimed_report(&site, &report_rel, report);
        let ctx = modern_ctx(&go_rel, &base_sha);
        let card = card::load(&site.root, "r44", "B97").unwrap();
        let first = collect_claimed_report(
            &site.root,
            "r44",
            "B97",
            &card,
            "task/B97",
            &report_rel,
            &ctx,
            &claimed,
            &mut || Duration::ZERO,
            &mut |_: Duration| {
                panic!("already-committed REPORT must never enter commit-grace sleep")
            },
            &mut |_| Ok(()),
        )
        .unwrap();
        assert_eq!(first.gates.len(), 2);
        let events = read_events(&site.root);
        let receipt = events
            .iter()
            .find(|event| event.kind == "CollectGateSuccessReceipt")
            .unwrap();
        let payload = receipt.payload.as_ref().unwrap();
        assert_eq!(payload["gateCount"], 2);
        let gates = payload["gates"].as_array().unwrap();
        assert_eq!(gates[0]["sequence"], 1);
        assert_eq!(gates[0]["commandRef"], "testGate");
        assert_eq!(gates[0]["exitCode"], 0);
        assert!(valid_sha256(gates[0]["rawLogCasSha256"].as_str().unwrap()));
        assert!(gates[0]["rawLogLen"].as_u64().is_some());
        assert_eq!(gates[1]["sequence"], 2);
        assert_eq!(gates[1]["commandRef"], "testGate2");
        assert_eq!(gates[1]["exitCode"], 0);
        let attestation_ref =
            receipt_attestation_ref_from_payload(payload, "positive receipt").unwrap();
        let attestation = read_collect_attestation(
            &site.root,
            &CollectGateReceiptRef {
                event_id: receipt.event_id.clone(),
                attestation_sha256: attestation_ref.0,
                attestation_len: attestation_ref.1,
            },
        )
        .unwrap();
        assert_eq!(
            attestation.configured_command_refs,
            vec!["testGate", "testGate2"]
        );
        verify_attested_raw_logs(&site.root, &attestation.gates).unwrap();

        let replay = collect_claimed_report(
            &site.root,
            "r44",
            "B97",
            &card,
            "task/B97",
            &report_rel,
            &ctx,
            &claimed,
            &mut || Duration::ZERO,
            &mut |_: Duration| {
                panic!("already-committed REPORT must never enter commit-grace sleep")
            },
            &mut |_| Ok(()),
        )
        .unwrap();
        assert!(replay.gates.is_empty());
        assert_eq!(
            std::fs::read_to_string(&site.gate_marker)
                .unwrap()
                .lines()
                .count(),
            2
        );
    }

    #[test]
    fn collect_replay_rejects_cross_action_reuse_of_real_attestation_and_raw_logs() {
        let site = test_root("collect-cross-action-cas-reuse");
        let (_, go_rel, base_sha) = append_modern_dispatch(&site);
        let (report_rel, report, branch_sha) = commit_report(&site);
        let claimed = claimed_report(&site, &report_rel, report);
        let ctx = modern_ctx(&go_rel, &base_sha);
        let card = card::load(&site.root, "r44", "B97").unwrap();
        collect_claimed_report(
            &site.root,
            "r44",
            "B97",
            &card,
            "task/B97",
            &report_rel,
            &ctx,
            &claimed,
            &mut || Duration::ZERO,
            &mut |_: Duration| {
                panic!("already-committed REPORT must never enter commit-grace sleep")
            },
            &mut |_| Ok(()),
        )
        .unwrap();
        let legitimate_events = read_events(&site.root);
        let legitimate_ref = receipt_ref_from_events(&legitimate_events);
        let legitimate_attestation = read_collect_attestation(&site.root, &legitimate_ref).unwrap();
        verify_attested_raw_logs(&site.root, &legitimate_attestation.gates).unwrap();

        let mut second_claim = claimed.clone();
        second_claim.control_epoch = "control-epoch-2".into();
        append_claimed_report_identity(&site, &second_claim);
        append_cross_action_attestation_reuse(
            &site,
            &ctx,
            &second_claim,
            &branch_sha,
            &legitimate_ref,
            &legitimate_attestation,
        );
        let error = collect_claimed_report(
            &site.root,
            "r44",
            "B97",
            &card,
            "task/B97",
            &report_rel,
            &ctx,
            &second_claim,
            &mut || Duration::ZERO,
            &mut |_: Duration| {
                panic!("already-committed REPORT must never enter commit-grace sleep")
            },
            &mut |_| Ok(()),
        )
        .err()
        .expect("another action must not reuse a real attestation/raw-log CAS set");
        let message = format!("{error:#}");
        assert!(
            !message.contains("模型勾稽") && !message.contains("ReportObserved"),
            "cross-action fixture must reach the attestation/CAS guard: {message}"
        );
        assert_eq!(
            std::fs::read_to_string(&site.gate_marker)
                .unwrap()
                .lines()
                .count(),
            1,
            "forged replay must not rerun gates"
        );
    }

    #[test]
    fn collect_replay_rejects_attestation_content_and_length_mismatch() {
        for mismatch in ["content", "length"] {
            let site = test_root(&format!("collect-attestation-{mismatch}"));
            let (_, go_rel, base_sha) = append_modern_dispatch(&site);
            let (report_rel, report, _) = commit_report(&site);
            let claimed = claimed_report(&site, &report_rel, report);
            let ctx = modern_ctx(&go_rel, &base_sha);
            let card = card::load(&site.root, "r44", "B97").unwrap();
            collect_claimed_report(
                &site.root,
                "r44",
                "B97",
                &card,
                "task/B97",
                &report_rel,
                &ctx,
                &claimed,
                &mut || Duration::ZERO,
                &mut |_: Duration| {
                    panic!("already-committed REPORT must never enter commit-grace sleep")
                },
                &mut |_| Ok(()),
            )
            .unwrap();
            let receipt_ref = receipt_ref_from_events(&read_events(&site.root));
            if mismatch == "content" {
                std::fs::write(
                    collect_cas(&site.root).object_path(&receipt_ref.attestation_sha256),
                    b"{}",
                )
                .unwrap();
            } else {
                let wrong_len = receipt_ref.attestation_len + 1;
                rewrite_test_ledger(&site, |events| {
                    for event in events.iter_mut().filter(|event| {
                        matches!(
                            event.kind.as_str(),
                            "CollectGateSuccessReceipt"
                                | "ReportCollectExecuted"
                                | "ReportCollectCompleted"
                        )
                    }) {
                        event.payload.as_mut().unwrap()["receiptAttestationLen"] =
                            serde_json::json!(wrong_len);
                    }
                });
            }
            assert!(
                collect_claimed_report(
                    &site.root,
                    "r44",
                    "B97",
                    &card,
                    "task/B97",
                    &report_rel,
                    &ctx,
                    &claimed,
                    &mut || Duration::ZERO,
                    &mut |_: Duration| panic!(
                        "already-committed REPORT must never enter commit-grace sleep"
                    ),
                    &mut |_| Ok(()),
                )
                .is_err(),
                "{mismatch} mismatch must fail through production replay"
            );
        }
    }

    #[test]
    fn collect_replay_rejects_raw_log_cas_content_mismatch() {
        let site = test_root("collect-raw-log-cas-mismatch");
        let (_, go_rel, base_sha) = append_modern_dispatch(&site);
        let (report_rel, report, _) = commit_report(&site);
        let claimed = claimed_report(&site, &report_rel, report);
        let ctx = modern_ctx(&go_rel, &base_sha);
        let card = card::load(&site.root, "r44", "B97").unwrap();
        collect_claimed_report(
            &site.root,
            "r44",
            "B97",
            &card,
            "task/B97",
            &report_rel,
            &ctx,
            &claimed,
            &mut || Duration::ZERO,
            &mut |_: Duration| {
                panic!("already-committed REPORT must never enter commit-grace sleep")
            },
            &mut |_| Ok(()),
        )
        .unwrap();
        let receipt_ref = receipt_ref_from_events(&read_events(&site.root));
        let attestation = read_collect_attestation(&site.root, &receipt_ref).unwrap();
        let raw = &attestation.gates[0];
        std::fs::write(
            collect_cas(&site.root).object_path(&raw.raw_log_cas_sha256),
            b"tampered-raw-log",
        )
        .unwrap();
        assert!(
            collect_claimed_report(
                &site.root,
                "r44",
                "B97",
                &card,
                "task/B97",
                &report_rel,
                &ctx,
                &claimed,
                &mut || Duration::ZERO,
                &mut |_: Duration| panic!(
                    "already-committed REPORT must never enter commit-grace sleep"
                ),
                &mut |_| Ok(()),
            )
            .is_err(),
            "raw-log CAS content mismatch must fail through production replay"
        );
    }

    #[test]
    fn collect_replay_rejects_gate_actor_and_global_event_id_reuse() {
        for mutation in ["actor", "duplicate-event-id"] {
            let site = test_root(&format!("collect-gate-{mutation}"));
            configure_two_gates(&site);
            let (_, go_rel, base_sha) = append_modern_dispatch(&site);
            let (report_rel, report, _) = commit_report(&site);
            let claimed = claimed_report(&site, &report_rel, report);
            let ctx = modern_ctx(&go_rel, &base_sha);
            let card = card::load(&site.root, "r44", "B97").unwrap();
            collect_claimed_report(
                &site.root,
                "r44",
                "B97",
                &card,
                "task/B97",
                &report_rel,
                &ctx,
                &claimed,
                &mut || Duration::ZERO,
                &mut |_: Duration| {
                    panic!("already-committed REPORT must never enter commit-grace sleep")
                },
                &mut |_| Ok(()),
            )
            .unwrap();
            if mutation == "actor" {
                rewrite_test_ledger(&site, |events| {
                    events
                        .iter_mut()
                        .find(|event| event.kind == "GateExecuted")
                        .unwrap()
                        .actor = "runtime:forged".into();
                });
            } else {
                let old_events = read_events(&site.root);
                let old_ref = receipt_ref_from_events(&old_events);
                let mut attestation = read_collect_attestation(&site.root, &old_ref).unwrap();
                let reused_id = attestation.gates[0].gate_event_id.clone();
                attestation.gates[1].gate_event_id = reused_id.clone();
                let mut new_ref = write_collect_attestation(&site.root, &attestation).unwrap();
                new_ref.event_id = old_ref.event_id.clone();
                rewrite_test_ledger(&site, |events| {
                    let mut gate_events = events
                        .iter_mut()
                        .filter(|event| event.kind == "GateExecuted")
                        .collect::<Vec<_>>();
                    gate_events[1].event_id = reused_id;
                    for event in events.iter_mut().filter(|event| {
                        matches!(
                            event.kind.as_str(),
                            "CollectGateSuccessReceipt"
                                | "ReportCollectExecuted"
                                | "ReportCollectCompleted"
                        )
                    }) {
                        let payload = event.payload.as_mut().unwrap();
                        rebind_test_receipt_payload(payload, &new_ref);
                        if event.kind == "CollectGateSuccessReceipt" {
                            payload["gates"] = serde_json::to_value(&attestation.gates).unwrap();
                        }
                    }
                });
            }
            assert!(
                collect_claimed_report(
                    &site.root,
                    "r44",
                    "B97",
                    &card,
                    "task/B97",
                    &report_rel,
                    &ctx,
                    &claimed,
                    &mut || Duration::ZERO,
                    &mut |_: Duration| panic!(
                        "already-committed REPORT must never enter commit-grace sleep"
                    ),
                    &mut |_| Ok(()),
                )
                .is_err(),
                "{mutation} must fail through production replay"
            );
            assert_eq!(
                std::fs::read_to_string(&site.gate_marker)
                    .unwrap()
                    .lines()
                    .count(),
                2,
                "forged replay must not rerun gates"
            );
        }
    }



    #[test]
    fn tierf_cost_sample_is_idempotent_per_attempt() {
        let root = std::env::temp_dir().join(format!("b101-cost-{}", ulid::Ulid::new()));
        let ledger_dir = root.join("coordination/rounds/r45");
        std::fs::create_dir_all(&ledger_dir).unwrap();
        std::fs::write(ledger_dir.join("events.jsonl"), "").unwrap();
        let ctx = crate::attempt::DispatchContext {
            attempt_id: Some("B101-A0001".into()),
            attempt_no: Some(1),
            agent: Some("executor-desktop".into()),
            go_path: Some(
                "coordination/rounds/r45/dispatch/executor-desktop/GO-B101-A0001.md".into(),
            ),
            base_sha: Some("base".into()),
            is_legacy: Some(false),
        };
        append_tierf_cost_sample(&root, "r45", "B101", &ctx, 3).unwrap();
        append_tierf_cost_sample(&root, "r45", "B101", &ctx, 3).unwrap();
        let read = orch_core::read_ledger(&ledger_dir.join("events.jsonl")).unwrap();
        assert_eq!(
            read.events
                .iter()
                .filter(|event| event.kind == "CostSampled")
                .count(),
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn report_observed_source_restores_env_decl_payload() {
        let source = include_str!("tierf.rs");
        let run_await = source
            .split_once("pub fn run_await(")
            .unwrap()
            .1
            .split_once("#[cfg(test)]")
            .unwrap()
            .0;
        let report_claim = run_await.split_once("ReportObserved").unwrap().1;
        assert!(report_claim.contains("env_decl_payload"));
    }

    #[test]
    fn historical_terminated_stall_is_confirmed_dead_for_existing_reassignment_gate() {
        let confirmed = ledger::event(
            "AttemptTimedOut",
            "runtime:orch",
            Some("B142"),
            Some("r50"),
            serde_json::json!({
                "attemptId": "B142-A0002",
                "kind": "stall",
                "stage": "stall-budget-exceeded",
                "processTreeTerminated": true,
            }),
        );
        assert_eq!(terminal_event_confirms_dead(&confirmed), Some(true));

        let ordinary_stall = ledger::event(
            "AttemptTimedOut",
            "runtime:orch",
            Some("B142"),
            Some("r50"),
            serde_json::json!({
                "attemptId": "B142-A0002",
                "kind": "stall",
            }),
        );
        assert_eq!(terminal_event_confirms_dead(&ordinary_stall), Some(false));
    }
}
