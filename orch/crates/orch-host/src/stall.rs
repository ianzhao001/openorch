//! 停滞体检（H121① / B247）——`orch stall-check` 的判定内核。
//!
//! **planner 预置 stub（r68/B247）**：本文件与 `lib.rs` 的 `pub mod stall;` 由 planner
//! 在开轮前一个原子提交预置，使 B247 的落位种子红在 `E0432: unresolved import`
//! 而不是 `E0583: file not found for module`。
//!
//! 判定表由 `coordination/scripts/stall-check.sh`（212 行手工先行版）定义，
//! 实现须逐格移植。三条必须钉死的语义：
//!   1. **活动是否压过产物，取决于产物属于哪个 attempt**（shell 的 `art_matches_current`）：
//!      产物属于**当前** attempt ⇒ 它已交付 ⇒ `awaiting-collection`，**即使仍有活动**；
//!      产物属于**旧** attempt 且有活动 ⇒ `executor-working`。
//!      「活动永远压过产物」是**错的**，会把已交付的当前 attempt 误判成还在干活。
//!   2. **`collect-inflight` 绝不可 kill**（H117）：收取持 1 小时 durable 租约，纯时间判活。
//!   3. **`planner-idle` 是轮级判定**，属独立的 `classify_round`，不进 `classify_attempt`
//!      ——单个 attempt 的入参里没有全轮信息。
//!
//! 在 B247 的 seed-only commit 中，本 stub **刻意不含任何 pub 项**；以下实现由
//! B247 的执行者在 seeded-red 取证后填充。

use anyhow::{Context, Result};
use std::path::Path;

/// The durable stage that the IO layer has projected for one attempt.
///
/// Activity and branch artifacts deliberately stay outside this enum: their
/// precedence is the subtle part of the decision table and must remain
/// visible to [`classify_attempt`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptProjection {
    Recorded,
    TerminalFailure,
    CollectingNow,
    ReadyForReview,
    CollectRejected,
    CollectInflight,
    Dispatched,
}

/// A canonical executor artifact observed on `task/<ID>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactOnBranch {
    None,
    Report,
    Blocked,
}

impl ArtifactOnBranch {
    fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Report => "REPORT",
            Self::Blocked => "BLOCKED",
        }
    }

    fn is_present(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Branch artifact sampled from one immutable task-branch tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactObservation {
    pub artifact: ArtifactOnBranch,
    pub from_current_attempt: bool,
}

/// Pin `task/<ID>` once, then inspect canonical REPORT/BLOCKED paths in that
/// immutable tree. Missing paths are ordinary; object/read failures propagate.
pub fn observe_branch_artifact(
    root: &Path,
    round: &str,
    task: &str,
    current_attempt: &str,
) -> Result<ArtifactObservation> {
    let branch = format!("task/{task}");
    if !crate::gitx::branch_exists(root, &branch) {
        return Ok(ArtifactObservation {
            artifact: ArtifactOnBranch::None,
            from_current_attempt: false,
        });
    }
    let head = crate::gitx::rev_parse(root, &format!("refs/heads/{branch}"))
        .with_context(|| format!("stall-check 无法钉住 {branch}"))?;
    let candidates = [
        (
            ArtifactOnBranch::Report,
            format!("coordination/rounds/{round}/reports/{task}-REPORT.md"),
        ),
        (
            ArtifactOnBranch::Blocked,
            format!("coordination/rounds/{round}/reports/{task}-BLOCKED.md"),
        ),
    ];
    let mut first = None;
    for (artifact, path) in candidates {
        if !crate::gitx::tree_path_exists(root, &head, &path)? {
            continue;
        }
        let bytes = crate::gitx::show_bytes(root, &head, &path)
            .with_context(|| format!("stall-check 读取 {head}:{path} 失败"))?;
        let observation = ArtifactObservation {
            artifact,
            from_current_attempt: artifact_attempt_id(&bytes).as_deref() == Some(current_attempt),
        };
        if observation.from_current_attempt {
            return Ok(observation);
        }
        first.get_or_insert(observation);
    }
    Ok(first.unwrap_or(ArtifactObservation {
        artifact: ArtifactOnBranch::None,
        from_current_attempt: false,
    }))
}

fn artifact_attempt_id(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    text.lines().take(16).find_map(|line| {
        let value = line.strip_prefix("attemptId:")?;
        let value = value.trim().trim_matches(['\'', '"']);
        (!value.is_empty()).then(|| value.to_string())
    })
}

/// Already-observed facts for one attempt. This type performs no IO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StallInputs {
    pub task_id: String,
    pub attempt_id: String,
    pub projection: AttemptProjection,
    pub artifact: ArtifactOnBranch,
    /// Mirrors the shell prototype's `art_matches_current` predicate.
    pub artifact_from_current_attempt: bool,
    pub executor_alive: bool,
    pub wake_log_growing: bool,
}

/// Deterministic attempt-level decision consumed by the CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StallVerdict {
    pub verdict: &'static str,
    pub need_action: bool,
    pub hint: String,
}

impl StallVerdict {
    fn new(verdict: &'static str, need_action: bool, hint: impl Into<String>) -> Self {
        Self {
            verdict,
            need_action,
            hint: hint.into(),
        }
    }
}

/// Classify one attempt using the nine-row table from the shell prototype.
pub fn classify_attempt(input: &StallInputs) -> StallVerdict {
    match input.projection {
        AttemptProjection::Recorded => StallVerdict::new("recorded", false, "—"),
        AttemptProjection::TerminalFailure => {
            StallVerdict::new("terminal-failure", true, "planner 处置：改派 / 新 attempt")
        }
        AttemptProjection::CollectingNow => StallVerdict::new(
            "collecting-now",
            false,
            "本地收取门在跑（不产生模型请求，属正常）",
        ),
        AttemptProjection::ReadyForReview => {
            StallVerdict::new("ready-for-review", true, "planner 派审查")
        }
        AttemptProjection::CollectRejected => StallVerdict::new(
            "collect-rejected",
            true,
            format!(
                "上次收取被拒；查原因后可重收：orch await-report {}",
                input.task_id
            ),
        ),
        AttemptProjection::CollectInflight => StallVerdict::new(
            "collect-inflight",
            false,
            "收取持有 durable 租约；绝不可 kill（H117），等待租约持有者或到期恢复",
        ),
        AttemptProjection::Dispatched => classify_dispatched(input),
    }
}

fn classify_dispatched(input: &StallInputs) -> StallVerdict {
    let artifact_present = input.artifact.is_present();
    let active = input.executor_alive || input.wake_log_growing;

    // A current-attempt artifact is delivery evidence. It must outrank both
    // activity signals; otherwise a still-running wrapper can hide INC-001.
    if artifact_present && input.artifact_from_current_attempt {
        return awaiting_collection(input);
    }

    // Activity only outranks a stale artifact left by a prior attempt.
    if active {
        return StallVerdict::new(
            "executor-working",
            false,
            format!(
                "执行者在跑（{}；进程或 wake 日志增长为证）",
                input.attempt_id
            ),
        );
    }

    if artifact_present {
        return awaiting_collection(input);
    }

    StallVerdict::new(
        "executor-gone-no-artifact",
        true,
        "进程没了且分支无产物；查 wake 日志后决定恢复或改派",
    )
}

fn awaiting_collection(input: &StallInputs) -> StallVerdict {
    StallVerdict::new(
        "awaiting-collection",
        true,
        format!(
            "分支上已有 {}，执行者已交付但无人收；跑 orch await-report {}",
            input.artifact.label(),
            input.task_id
        ),
    )
}

/// Round-wide signals. These facts cannot be honestly inferred from one
/// attempt, so planner idleness has a separate input and return type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoundInputs {
    pub local_orch_processes: usize,
    pub agent_processes: usize,
    pub live_wake_logs: usize,
    pub ledger_silence_secs: u64,
}

/// Deterministic round-level decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundVerdict {
    pub planner_idle: bool,
    pub need_action: bool,
    pub hint: String,
}

/// Report planner idleness only when all four round-wide conditions hold.
pub fn classify_round(input: &RoundInputs) -> RoundVerdict {
    let planner_idle = input.local_orch_processes == 0
        && input.agent_processes == 0
        && input.live_wake_logs == 0
        && input.ledger_silence_secs > 600;
    RoundVerdict {
        planner_idle,
        need_action: planner_idle,
        hint: if planner_idle {
            format!(
                "无本地动作、无 agent、无活跃 wake 日志，账本已静默 {} 秒",
                input.ledger_silence_secs
            )
        } else {
            "—".to_string()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn base() -> StallInputs {
        StallInputs {
            task_id: "B900".into(),
            attempt_id: "B900-A0001".into(),
            projection: AttemptProjection::Dispatched,
            artifact: ArtifactOnBranch::None,
            artifact_from_current_attempt: false,
            executor_alive: false,
            wake_log_growing: false,
        }
    }

    #[test]
    fn canonical_vectors_cover_every_attempt_verdict_row() {
        let vectors = [
            (
                AttemptProjection::Recorded,
                ArtifactOnBranch::None,
                false,
                false,
                "recorded",
                false,
            ),
            (
                AttemptProjection::TerminalFailure,
                ArtifactOnBranch::None,
                false,
                false,
                "terminal-failure",
                true,
            ),
            (
                AttemptProjection::CollectingNow,
                ArtifactOnBranch::None,
                false,
                false,
                "collecting-now",
                false,
            ),
            (
                AttemptProjection::ReadyForReview,
                ArtifactOnBranch::None,
                false,
                false,
                "ready-for-review",
                true,
            ),
            (
                AttemptProjection::CollectRejected,
                ArtifactOnBranch::None,
                false,
                false,
                "collect-rejected",
                true,
            ),
            (
                AttemptProjection::CollectInflight,
                ArtifactOnBranch::None,
                false,
                false,
                "collect-inflight",
                false,
            ),
            (
                AttemptProjection::Dispatched,
                ArtifactOnBranch::None,
                false,
                true,
                "executor-working",
                false,
            ),
            (
                AttemptProjection::Dispatched,
                ArtifactOnBranch::Report,
                true,
                true,
                "awaiting-collection",
                true,
            ),
            (
                AttemptProjection::Dispatched,
                ArtifactOnBranch::None,
                false,
                false,
                "executor-gone-no-artifact",
                true,
            ),
        ];
        let mut observed = BTreeSet::new();
        for (projection, artifact, current, alive, expected, need_action) in vectors {
            let verdict = classify_attempt(&StallInputs {
                projection,
                artifact,
                artifact_from_current_attempt: current,
                executor_alive: alive,
                ..base()
            });
            eprintln!(
                "canonical-attempt verdict={} need_action={}",
                verdict.verdict, verdict.need_action
            );
            assert_eq!(verdict.verdict, expected);
            assert_eq!(verdict.need_action, need_action);
            observed.insert(verdict.verdict);
        }
        assert_eq!(
            observed,
            BTreeSet::from([
                "awaiting-collection",
                "collect-inflight",
                "collect-rejected",
                "collecting-now",
                "executor-gone-no-artifact",
                "executor-working",
                "ready-for-review",
                "recorded",
                "terminal-failure",
            ])
        );
        assert!(!observed.contains("planner-idle"));
    }

    #[test]
    fn canonical_round_vector_is_separate_from_attempt_domain() {
        let verdict = classify_round(&RoundInputs {
            local_orch_processes: 0,
            agent_processes: 0,
            live_wake_logs: 0,
            ledger_silence_secs: 601,
        });
        eprintln!(
            "canonical-round verdict=planner-idle need_action={}",
            verdict.need_action
        );
        assert!(verdict.planner_idle);
        assert!(verdict.need_action);
    }
}
