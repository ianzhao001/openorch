//! orch-core · 协议 v0 域模型（design/01 状态机 · design/02 事件字典/目录布局）
//!
//! M1 首切片范围：事件账本容错读取（errata R6）、Task 状态投影折叠、coordination/ 布局体检。
//! 纪律：本 crate 零平台代码；路径一律由调用方传入绝对根（errata E6）。

pub mod observation;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ─────────────────────────── 事件（容错） ───────────────────────────

/// events.jsonl 的一行。未知字段/未知事件类型**原样保留**（design/05 风险 R6：
/// 「解析容错——未知事件原样存账本不丢弃」）。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EventRecord {
    #[serde(rename = "eventId")]
    pub event_id: String,
    pub ts: String,
    pub actor: String,
    /// 事件类型（design/02 §4 字典；容错起见建模为字符串）
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "taskId", default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    /// 兜底：schema 演进产生的未知字段
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// 账本读取结果：坏行不丢弃、带行号记录（可 doctor 报警）
pub struct LedgerRead {
    pub events: Vec<EventRecord>,
    pub bad_lines: Vec<(usize, String)>,
}

/// 逐行容错读取 events.jsonl
pub fn read_ledger(path: &Path) -> std::io::Result<LedgerRead> {
    let text = fs::read_to_string(path)?;
    let mut events = Vec::new();
    let mut bad_lines = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<EventRecord>(line) {
            Ok(ev) => events.push(ev),
            Err(e) => bad_lines.push((i + 1, e.to_string())),
        }
    }
    Ok(LedgerRead { events, bad_lines })
}

/// 导出 EventRecord 的 JSON Schema（design/05：schemars 出协议 v0 机读规范）
pub fn event_schema_json() -> String {
    let schema = schemars::schema_for!(EventRecord);
    serde_json::to_string_pretty(&schema).expect("schema serializes")
}

// ─────────────────────────── 事件目录（单一事实源） ───────────────────────────

/// Review 槽位生命周期事件；它们是已知事实，但不改变 Task 状态投影。
pub const REVIEW_LIFECYCLE_EVENT_KINDS: [&str; 2] = ["ReviewRequested", "ReviewDelivered"];

// B303 quorum facts are kept separate so the frozen B202 lifecycle seam stays
// byte- and meaning-stable while the aggregate event catalog remains complete.
const REVIEW_QUORUM_EVENT_KINDS: [&str; 3] = [
    "ReviewFallbackSelected",
    "NongateReviewDelivered",
    "ReviewSeatSubstituted",
];

// B310 freezes the event vocabulary consumed by the dynamic review pool and
// the later gate-lane cards.  Keeping the set here makes projection, doctor,
// archive replay, and every future producer share one catalog entry point.
const RUNTIME_POLICY_EVENT_KINDS: [&str; 10] = [
    "RuntimePolicyActivated",
    "RuntimePolicyDeactivated",
    "ReviewSpoolPromoted",
    "ReviewPanelSelected",
    "ReviewSeatRouted",
    "ReviewSeatTerminated",
    "ReviewPanelClosed",
    "GateLaneEscalated",
    "GateReused",
    "GateReuseMiss",
];

/// 已知事件类型目录（单一事实源，design/02 §4 事件字典 + B105 durable action 闭合）。
/// `fold` 必须消费本目录判定「已知但不改态」事件，不得维护第二份白名单。
/// B303 的 quorum 事实在不扩张冻结 lifecycle seam 的前提下由此目录统一暴露。
/// 顺序稳定（声明序）、无重复。
pub fn known_event_kinds() -> Vec<&'static str> {
    [
        // —— 改 Task 投影状态的事件 ——
        "DispatchIssued",
        "AttemptBlocked",
        "ReportObserved",
        "VerdictIssued",
        "MechCheckFailed",
        "MergeExecuted",
        "TaskRecorded",
        "TaskReopened",
        "PlanSignedOff",
        "RoundClosed",
        // —— 已知但不改 Task 投影状态的事件 ——
        "RoundOpened",
        "TaskValidated",
        "SeedOracleVerified",
        "DispatchAcked",
        "AgentSelected",
        "WorkspaceLeased",
        "WorkspaceReleased",
        "SiteRetired",
        // B270: an authorized frozen-contract replacement is a durable audit
        // fact.  It deliberately does not change the task projection: the
        // adjacent canonical TaskRecorded remains the sole Recorded edge.
        "FrozenContractSuperseded",
        "AgentEventReceived",
        "SeedRelocated",
        "RedProven",
        "GateExecuted",
        "MutationProven",
        "PermissionRequested",
        "PermissionDecided",
        "EscalationRaised",
        "NudgeIssued",
        "ResumeIssued",
        "ReassignmentApproved",
        "BudgetThresholdCrossed",
        "AttemptTimedOut",
        "AttemptCrashed",
        "ReconciliationNeeded",
        "AttemptStarted",
        "AttemptFailed",
        "ReassignmentIssued",
        "RouteCooldownSet",
        "QualityAssessed",
        "ReconciliationResolved",
        "CostSampled",
        "InjectionIssued",
        "InjectionConsumed",
        "PlannerTurnCompleted",
        "PlannerTurnLost",
        "WakeIssued",
        "ManagedWakeTerminated",
        "ManagedWakeAttachState",
        "VerifyStarted",
        "MergeStarted",
        // B114 session overlay：durable，但不改变 TaskState 投影。
        "SessionOverlayApplied",
        "SessionOverlayCleared",
        // —— durable action 事件（B105：目录闭合，原属 unknown 容错）——
        "DispatchWakeClaimed",
        "DispatchWakeLaunching",
        "DispatchWakeDelivered",
        "DispatchWakeCompleted",
        "DispatchWakeReleased",
        "ResumeWakeClaimed",
        "ResumeWakeLaunching",
        "ResumeWakeDelivered",
        "ResumeWakeCompleted",
        "ResumeWakeReleased",
        "ReportCollectClaimed",
        "ReportCollectExecuting",
        "ReportCollectExecuted",
        "ReportCollectCompleted",
        "ReportCollectReleased",
        "CollectGateSuccessReceipt",
        // B113：action-scoped 运行前拒绝（不改 TaskState 投影，但必须登记以防
        // fold 把它推入 unknown_kinds——catalog 漂移）。
        "ActionRejected",
    ]
    .into_iter()
    .chain(REVIEW_LIFECYCLE_EVENT_KINDS)
    .chain(REVIEW_QUORUM_EVENT_KINDS)
    .chain(RUNTIME_POLICY_EVENT_KINDS)
    .collect()
}

/// 判定事件类型是否在已知目录内（B105）。
pub fn is_known_event_kind(kind: &str) -> bool {
    known_event_kinds().iter().any(|k| *k == kind)
}

// ─────────────────────────── Task 状态投影 ───────────────────────────

/// Task 层状态（design/01 §3 的账本可观测子集）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub enum TaskState {
    Dispatched,
    ReadyForVerification,
    ChangesRequested,
    Blocked,
    Approved,
    Merged,
    Recorded,
    Reopened,
}

impl std::fmt::Display for TaskState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            TaskState::Dispatched => "dispatched",
            TaskState::ReadyForVerification => "ready_for_verification",
            TaskState::ChangesRequested => "changes_requested",
            TaskState::Blocked => "blocked",
            TaskState::Approved => "approved",
            TaskState::Merged => "merged",
            TaskState::Recorded => "recorded",
            TaskState::Reopened => "reopened",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Default, Clone)]
pub struct TaskProjection {
    pub state: Option<TaskState>,
    pub last_ts: String,
    pub merge_sha: Option<String>,
    pub event_count: usize,
}

#[derive(Debug, Default)]
pub struct RoundProjection {
    pub tasks: BTreeMap<String, TaskProjection>,
    pub total_events: usize,
    pub plan_signed_off: bool,
    pub round_closed: bool,
    pub unknown_kinds: Vec<String>,
}

/// 事件折叠为投影——「事件是事实，状态是投影」（design/02 §6）。
/// 注意：这里只消费账本可见事件；SeedRelocated/RedProven 等门控事件出现时同样计入。
/// B105：已知事件判定统一消费 `known_event_kinds()` 目录，不再维护第二份 match 白名单。
pub fn fold(events: &[EventRecord]) -> RoundProjection {
    let catalog = known_event_kinds();
    let mut p = RoundProjection::default();
    for ev in events {
        p.total_events += 1;
        let state = match ev.kind.as_str() {
            "DispatchIssued" => Some(TaskState::Dispatched),
            "AttemptBlocked" => Some(TaskState::Blocked),
            "ReportObserved" => Some(TaskState::ReadyForVerification),
            "VerdictIssued" => match ev
                .payload
                .as_ref()
                .and_then(|v| v.get("verdict"))
                .and_then(|v| v.as_str())
            {
                Some("PASS") => Some(TaskState::Approved),
                Some("FAIL") => Some(TaskState::ChangesRequested),
                Some("BLOCKED") => Some(TaskState::Blocked),
                _ => None,
            },
            // 机检失败=返工态（r12 step 学费缺口①：机检 FAIL 不落账→投影失真误 spawn verifier）
            "MechCheckFailed" => Some(TaskState::ChangesRequested),
            "MergeExecuted" => Some(TaskState::Merged),
            "TaskRecorded" => Some(TaskState::Recorded),
            "TaskReopened" => Some(TaskState::Reopened),
            "PlanSignedOff" => {
                p.plan_signed_off = true;
                None
            }
            "RoundClosed" => {
                p.round_closed = true;
                None
            }
            // B105：其余已知事件（含 durable action 全目录）不改 Task 投影，
            // 判定统一走目录，未知类型才记 unknown（容错，不报错）
            other => {
                if !catalog.iter().any(|k| *k == other)
                    && !p.unknown_kinds.iter().any(|k| k == other)
                {
                    p.unknown_kinds.push(other.to_string());
                }
                None
            }
        };
        if let Some(task_id) = &ev.task_id {
            let t = p.tasks.entry(task_id.clone()).or_default();
            t.event_count += 1;
            t.last_ts = ev.ts.clone();
            if let Some(s) = state {
                t.state = Some(s);
                if s == TaskState::Merged {
                    t.merge_sha = ev
                        .payload
                        .as_ref()
                        .and_then(|v| v.get("mergeSha"))
                        .and_then(|v| v.as_str())
                        .map(String::from);
                }
            }
        }
    }
    p
}

// ─────────────────────────── doctor 布局体检 ───────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug)]
pub struct Check {
    pub name: &'static str,
    pub status: CheckStatus,
    pub detail: String,
}

fn file_contains_line(path: &Path, needle: &str) -> bool {
    fs::read_to_string(path)
        .map(|t| t.lines().any(|l| l.trim() == needle))
        .unwrap_or(false)
}

/// coordination/ 布局体检（design/02 §1/§3；`orch doctor`）
pub fn doctor(root: &Path) -> Vec<Check> {
    let mut out = Vec::new();
    let coord = root.join("coordination");

    // 1. coordination/ 存在
    out.push(if coord.is_dir() {
        Check {
            name: "coordination/ 目录",
            status: CheckStatus::Pass,
            detail: coord.display().to_string(),
        }
    } else {
        Check {
            name: "coordination/ 目录",
            status: CheckStatus::Fail,
            detail: "不存在".into(),
        }
    });

    // 2. gitignore 三行（design/02 §3）
    let gi = root.join(".gitignore");
    let required = [
        "coordination/rounds/*/dispatch/",
        "coordination/runtime/",
        ".worktrees/",
    ];
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|l| !file_contains_line(&gi, l))
        .collect();
    out.push(if missing.is_empty() {
        Check {
            name: "gitignore 三行",
            status: CheckStatus::Pass,
            detail: "齐".into(),
        }
    } else {
        Check {
            name: "gitignore 三行",
            status: CheckStatus::Fail,
            detail: format!("缺: {}", missing.join(" | ")),
        }
    });

    // 3. gitattributes union merge（errata 防账本冲突）
    let ga = root.join(".gitattributes");
    let union_ok = file_contains_line(&ga, "coordination/rounds/*/events.jsonl merge=union");
    out.push(if union_ok {
        Check {
            name: "events union-merge",
            status: CheckStatus::Pass,
            detail: "齐".into(),
        }
    } else {
        Check {
            name: "events union-merge",
            status: CheckStatus::Warn,
            detail: ".gitattributes 缺 union 规则".into(),
        }
    });

    // 4. wait-dispatch.sh 存在且可执行
    let script = coord.join("scripts/wait-dispatch.sh");
    let exec_ok = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::metadata(&script)
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            script.is_file()
        }
    };
    out.push(if exec_ok {
        Check {
            name: "wait-dispatch.sh",
            status: CheckStatus::Pass,
            detail: "存在且可执行".into(),
        }
    } else if script.is_file() {
        Check {
            name: "wait-dispatch.sh",
            status: CheckStatus::Fail,
            detail: "存在但不可执行".into(),
        }
    } else {
        Check {
            name: "wait-dispatch.sh",
            status: CheckStatus::Fail,
            detail: "缺失".into(),
        }
    });

    // 5. BOARD.md
    let board = coord.join("BOARD.md");
    out.push(if board.is_file() {
        Check {
            name: "BOARD.md",
            status: CheckStatus::Pass,
            detail: "存在".into(),
        }
    } else {
        Check {
            name: "BOARD.md",
            status: CheckStatus::Warn,
            detail: "缺失（人读账本）".into(),
        }
    });

    // 6. CURRENT-ROUND 与账本可解析性
    let cr = coord.join("runtime/CURRENT-ROUND");
    match fs::read_to_string(&cr) {
        Ok(round) => {
            let round = round.trim().to_string();
            let ledger = coord.join(format!("rounds/{round}/events.jsonl"));
            match read_ledger(&ledger) {
                Ok(r) if r.bad_lines.is_empty() => out.push(Check {
                    name: "当前轮账本",
                    status: CheckStatus::Pass,
                    detail: format!("round={round}，{} 事件，0 坏行", r.events.len()),
                }),
                Ok(r) => out.push(Check {
                    name: "当前轮账本",
                    status: CheckStatus::Warn,
                    detail: format!(
                        "round={round}，{} 事件，{} 坏行（首坏行 #{}）",
                        r.events.len(),
                        r.bad_lines.len(),
                        r.bad_lines[0].0
                    ),
                }),
                Err(e) => out.push(Check {
                    name: "当前轮账本",
                    status: CheckStatus::Fail,
                    detail: format!("round={round}，events.jsonl 不可读: {e}"),
                }),
            }
        }
        Err(_) => out.push(Check {
            name: "当前轮账本",
            status: CheckStatus::Warn,
            detail: "runtime/CURRENT-ROUND 缺失（无活动轮——runtime/ 可重建属正常）".into(),
        }),
    }

    out
}

// ─────────────────────────── 测试 ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实干跑账本行（r0 原文）必须可解析
    #[test]
    fn parses_real_dryrun_line() {
        let line = r#"{"eventId":"evt-1784658185-d","ts":"2026-07-21T18:23:05Z","actor":"reviewer","type":"MergeExecuted","taskId":"B1","payload":{"mergeSha":"be48f48","policy":"no-ff"}}"#;
        let ev: EventRecord = serde_json::from_str(line).unwrap();
        assert_eq!(ev.kind, "MergeExecuted");
        assert_eq!(ev.task_id.as_deref(), Some("B1"));
    }

    /// 未知事件类型与未知字段不报错（R6 容错）
    #[test]
    fn tolerates_unknown_kind_and_fields() {
        let line = r#"{"eventId":"x","ts":"t","actor":"a","type":"FutureEvent","weird":123}"#;
        let ev: EventRecord = serde_json::from_str(line).unwrap();
        let p = fold(&[ev]);
        assert_eq!(p.unknown_kinds, vec!["FutureEvent".to_string()]);
    }

    #[test]
    fn frozen_contract_superseded_is_known_and_projection_inert() {
        let event = |kind: &str| EventRecord {
            event_id: format!("event-{kind}"),
            ts: "t".into(),
            actor: "runtime:orch".into(),
            kind: kind.into(),
            task_id: Some("B270".into()),
            round: Some("r71".into()),
            payload: Some(serde_json::json!({})),
            extra: Default::default(),
        };
        let before = fold(&[event("DispatchIssued")]);
        let after = fold(&[event("DispatchIssued"), event("FrozenContractSuperseded")]);
        assert!(is_known_event_kind("FrozenContractSuperseded"));
        assert!(after.unknown_kinds.is_empty());
        assert_eq!(after.tasks["B270"].state, before.tasks["B270"].state);
    }

    /// B1 生命周期折叠：dispatched → approved → merged → recorded
    #[test]
    fn mech_check_failed_folds_to_changes_requested() {
        // r12 缺口①修复语义：机检失败→返工态（不再停留 ReadyForVerification 误导 runloop）
        let evs: Vec<EventRecord> = [
            r#"{"eventId":"1","ts":"2026-07-22T00:00:00Z","actor":"runtime:orch","type":"ReportObserved","taskId":"BX"}"#,
            r#"{"eventId":"2","ts":"2026-07-22T00:00:01Z","actor":"runtime:orch","type":"MechCheckFailed","taskId":"BX","payload":{"reason":"protected"}}"#,
        ].iter().map(|l| serde_json::from_str(l).unwrap()).collect();
        let p = fold(&evs);
        assert_eq!(p.tasks["BX"].state, Some(TaskState::ChangesRequested));
    }

    #[test]
    fn folds_task_lifecycle() {
        let mk = |kind: &str, payload: serde_json::Value| EventRecord {
            event_id: "e".into(),
            ts: "t".into(),
            actor: "a".into(),
            kind: kind.into(),
            task_id: Some("B1".into()),
            round: None,
            payload: Some(payload),
            extra: Default::default(),
        };
        let events = vec![
            mk("DispatchIssued", serde_json::json!({})),
            mk("VerdictIssued", serde_json::json!({"verdict":"PASS"})),
            mk("MergeExecuted", serde_json::json!({"mergeSha":"abc1234"})),
            mk("TaskRecorded", serde_json::json!({})),
        ];
        let p = fold(&events);
        let b1 = &p.tasks["B1"];
        assert_eq!(b1.state, Some(TaskState::Recorded));
        assert_eq!(b1.merge_sha.as_deref(), Some("abc1234"));
        assert_eq!(b1.event_count, 4);
    }
}
