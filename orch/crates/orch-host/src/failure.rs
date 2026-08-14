//! Typed adapter/executor failure classification seam (r43/B92).
//!
//! The planner pre-places this module so the executor can implement it without
//! touching the shared `lib.rs` hot spot.
//!
//! 契约(见 coordination/rounds/r43/tasks/B92.md):
//! 分类只使用 adapter/runtime 提供的结构化字段与明确的 provider terminal error;
//! 不得扫描普通模型 stdout/stderr 文本推断 quota/auth/permission。
//! 优先级:timeout flag > provider quota > provider auth > provider permission
//! > HTTP 429/503 > protocol_error > clean-exit-0 Success;其余 Unknown(不可重试)。

/// 结构化失败类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailureClass {
    QuotaExhausted,
    RateLimited,
    ServiceUnavailable,
    Authentication,
    Permission,
    Timeout,
    Protocol,
    Unknown,
}

/// adapter/runtime 提供的结构化证据(不含有模型普通输出文本)。
#[derive(Debug, Clone, Copy, Default)]
pub struct FailureEvidence<'a> {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub http_status: Option<u16>,
    pub protocol_error: Option<&'a str>,
    pub provider_error: Option<&'a str>,
}

/// 适配器调用结局:干净成功,或带类别与可重试标记的失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterOutcome {
    Success,
    Failure {
        class: FailureClass,
        retryable: bool,
    },
}

fn failure(class: FailureClass, retryable: bool) -> AdapterOutcome {
    AdapterOutcome::Failure { class, retryable }
}

/// 依据结构化证据分类 adapter 结局;不 panic、不读取模型普通输出。
pub fn classify_adapter_outcome(evidence: &FailureEvidence<'_>) -> AdapterOutcome {
    // 1. timeout 是结构事实,优先于一切其他字段。
    if evidence.timed_out {
        return failure(FailureClass::Timeout, true);
    }
    // 2. provider 终态错误:quota 不得降格为普通 429;auth/permission fail-closed。
    if let Some(provider) = evidence.provider_error {
        let message = provider.to_lowercase();
        if message.contains("quota") || message.contains("credit") {
            return failure(FailureClass::QuotaExhausted, false);
        }
        if message.contains("authentication")
            || message.contains("invalid api key")
            || message.contains("invalid-key")
            || message.contains("unauthorized")
        {
            return failure(FailureClass::Authentication, false);
        }
        if message.contains("permission") || message.contains("sandbox") {
            return failure(FailureClass::Permission, false);
        }
    }
    // 3. 瞬时 provider 故障可重试。
    match evidence.http_status {
        Some(429) => return failure(FailureClass::RateLimited, true),
        Some(503) => return failure(FailureClass::ServiceUnavailable, true),
        _ => {}
    }
    // 4. 协议错误即便 exit 0 也优先于 Success。
    if evidence.protocol_error.is_some() {
        return failure(FailureClass::Protocol, false);
    }
    // 5. 仅 clean exit 0 且无任何显式失败字段才是 Success;其余 Unknown 不可重试。
    let clean_exit = evidence.exit_code == Some(0)
        && evidence.http_status.is_none()
        && evidence.provider_error.is_none()
        && evidence.protocol_error.is_none();
    if clean_exit {
        AdapterOutcome::Success
    } else {
        failure(FailureClass::Unknown, false)
    }
}

// ─────────────────────────── action-scoped 拒绝（B113） ───────────────────────────
//
// 契约(见 coordination/rounds/r47/tasks/B113.md):
//   wake/nudge/dispatch/run-task/daemon 的所有运行前拒绝与 provider launch 拒绝统一落
//   `ActionRejected` 事件，带 actionId/operation/reason/exitCode/attempt identity。
//   attempt identity 来自 `attempt::current_attempt` 的只读调用（attempt.rs 冻结）：
//   有 current attempt 时用其 attemptId/attemptNo；run-task pre-attempt（尚无
//   DispatchIssued）时用确定性 actionId=`run-task:<round>:<taskId>`，attempt identity
//   留 None。幂等判定（同 actionId 已拒绝则不再重复告警）放在本模块，不进 ledger.rs。

use crate::attempt::{current_attempt, AttemptRef};
use crate::ledger;
use anyhow::{bail, Context, Result};
use orch_core::EventRecord;
use std::fmt;

/// CLI 退出码只描述「本命令声明的效果推进到了哪一步」。未分类错误必须保守地
/// 返回 `EffectUnknown`，只有 durable `ActionRejection` 才能声明零效果拒绝。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CliDisposition {
    EffectAchieved,
    EffectPartial,
    Rejected,
    EffectUnknown,
}

impl CliDisposition {
    pub const ALL: [Self; 4] = [
        Self::EffectAchieved,
        Self::EffectPartial,
        Self::Rejected,
        Self::EffectUnknown,
    ];

    pub const fn code(self) -> u8 {
        match self {
            Self::EffectAchieved => 0,
            Self::EffectPartial => 1,
            Self::Rejected => 2,
            Self::EffectUnknown => 5,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::EffectAchieved => "EffectAchieved",
            Self::EffectPartial => "EffectPartial",
            Self::Rejected => "Rejected",
            Self::EffectUnknown => "EffectUnknown",
        }
    }
}

/// Command-specific non-terminal outcomes intentionally live outside the four-code
/// `CliDisposition` table. The private numeric mapping prevents production callers
/// from laundering a bare integer while preserving the frozen durable codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandOutcome {
    PendingReceipt,
    PendingConsumption,
}

impl CommandOutcome {
    const fn code(self) -> u8 {
        match self {
            Self::PendingReceipt => 3,
            Self::PendingConsumption => 4,
        }
    }
}

/// Outermost command-level effect classification. It deliberately wraps the leaf
/// error so final CLI classification can prefer command progress over a nested
/// pre-attempt rejection that happened earlier in the same command.
#[derive(Debug)]
struct CliDispositionError {
    disposition: CliDisposition,
    source: anyhow::Error,
}

impl fmt::Display for CliDispositionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "command effect {:?} (exit {}): {}",
            self.disposition,
            self.disposition.code(),
            self.source
        )
    }
}

impl std::error::Error for CliDispositionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Bind a command-level disposition outside an existing cause chain.
pub fn with_cli_disposition(source: anyhow::Error, disposition: CliDisposition) -> anyhow::Error {
    anyhow::Error::new(CliDispositionError {
        disposition,
        source,
    })
}

/// Final CLI classifier: the outermost typed command disposition wins; only when
/// none exists may a durable leaf `ActionRejection` supply its command-specific
/// code. Every other error is honestly classified as effect-unknown.
pub fn cli_error_exit_code(err: &anyhow::Error) -> u8 {
    for source in err.chain() {
        if let Some(bound) = source.downcast_ref::<CliDispositionError>() {
            let code = bound.disposition.code();
            return if code == 0 {
                CliDisposition::EffectUnknown.code()
            } else {
                code
            };
        }
    }
    rejection_exit_code(err)
        .and_then(|code| u8::try_from(code).ok())
        .filter(|code| *code != 0)
        .unwrap_or_else(|| CliDisposition::EffectUnknown.code())
}

/// 供 CLI 传递 `ActionRejection` 的退出码：包装为 `std::error::Error`，使其能
/// 穿过 `anyhow::Error` 链被 `main.rs` 提取（B113：CLI 非零退出/snapshot 告警）。
#[derive(Debug)]
pub struct ActionRejectionError {
    pub rejection: ActionRejection,
}

impl fmt::Display for ActionRejectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} 拒绝（exit {}）：{}",
            self.rejection.operation, self.rejection.exit_code, self.rejection.reason
        )
    }
}

impl std::error::Error for ActionRejectionError {}

impl ActionRejection {
    /// 构造一个可穿过 `anyhow::Error` 链的拒绝错误，供 CLI 提取退出码。
    pub fn into_error(self) -> anyhow::Error {
        anyhow::Error::new(ActionRejectionError { rejection: self })
    }
}

/// 从 `anyhow::Error` 链提取 `ActionRejection` 的退出码（供 main.rs CLI）。
/// 无 `ActionRejection` 时返回 None，调用方回退默认非零退出码。
pub fn rejection_exit_code(err: &anyhow::Error) -> Option<i32> {
    for source in err.chain() {
        if let Some(ar_err) = source.downcast_ref::<ActionRejectionError>() {
            return Some(ar_err.rejection.exit_code);
        }
    }
    None
}

/// Whether an error has already crossed the durable ActionRejected boundary.
/// Wrappers use this to avoid appending a second rejection for the same action.
pub fn is_action_rejection(err: &anyhow::Error) -> bool {
    rejection_exit_code(err).is_some()
}

/// 一个 action-scoped 的运行前拒绝。`exit_code` 必须非零（CLI 以此退出）。
///
/// `attempt_id`/`attempt_no` 为 None 表示拒绝发生在 current attempt 建立之前
/// （如 run-task 的 worktree 创建失败、wake 的 argv 校验失败）；为 Some 表示拒绝
/// 发生在已有 current attempt 之后（如 bad ledger、active ambiguity、spawn fault），
/// 由生产调用点通过 `with_attempt` 注入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionRejection {
    pub operation: String,
    pub action_id: String,
    pub reason: String,
    pub exit_code: i32,
    pub attempt_id: Option<String>,
    pub attempt_no: Option<usize>,
    /// 是否触发 snapshot action-scoped alert。seed 断言为 true；普通校验错误不得
    /// 吞掉原始原因——只有显式构造的拒绝才告警。
    pub alert: bool,
}

impl ActionRejection {
    /// seed 纯函数契约：operation/action_id/reason 非空，exit_code != 0。
    /// attempt identity 由生产调用点在构造后通过 `with_attempt` 注入。
    #[doc(hidden)]
    pub fn new(operation: &str, action_id: &str, reason: &str, exit_code: i32) -> Result<Self> {
        if operation.is_empty() {
            bail!("ActionRejection.operation 不能为空");
        }
        if action_id.is_empty() {
            bail!("ActionRejection.action_id 不能为空");
        }
        if reason.is_empty() {
            bail!("ActionRejection.reason 不能为空");
        }
        if !(1..=u8::MAX as i32).contains(&exit_code) {
            bail!("ActionRejection.exit_code 必须在 1..=255（且必须与 CLI 实退一致）");
        }
        Ok(Self {
            operation: operation.to_string(),
            action_id: action_id.to_string(),
            reason: reason.to_string(),
            exit_code,
            attempt_id: None,
            attempt_no: None,
            alert: true,
        })
    }

    /// Typed production constructor for one of the four public CLI dispositions.
    pub fn from_disposition(
        operation: &str,
        action_id: &str,
        reason: &str,
        disposition: CliDisposition,
    ) -> Result<Self> {
        Self::new(operation, action_id, reason, i32::from(disposition.code()))
    }

    /// Typed production constructor for command-specific codes outside the public table.
    pub fn from_command_outcome(
        operation: &str,
        action_id: &str,
        reason: &str,
        outcome: CommandOutcome,
    ) -> Result<Self> {
        Self::new(operation, action_id, reason, i32::from(outcome.code()))
    }

    /// 生产调用点在 `current_attempt` 返回 Some 后调用，填入 attempt identity。
    /// attempt.rs 冻结——这里只读 `AttemptRef`，不编辑 attempt.rs。
    pub fn with_attempt(mut self, attempt: &AttemptRef) -> Self {
        self.attempt_id = Some(attempt.attempt_id.clone());
        self.attempt_no = Some(attempt.ordinal);
        self
    }
}

/// 把拒绝投影为 `EventRecord`（kind=`ActionRejected`）。seed 纯函数契约：
/// payload 含 actionId/operation/reason/exitCode/alert（seed 断言这 5 字段）+
/// attemptId/attemptNo（seed 不断言，生产集成回归断言）。actor=`runtime:orch`。
///
/// `ledger::event` 接受任意 `kind: &str`，不改 ledger.rs。
pub fn rejection_event(
    round: &str,
    task_id: Option<&str>,
    rejection: &ActionRejection,
) -> EventRecord {
    let payload = serde_json::json!({
        "actionId": rejection.action_id,
        "operation": rejection.operation,
        "reason": rejection.reason,
        "exitCode": rejection.exit_code,
        "alert": rejection.alert,
        "attemptId": rejection.attempt_id,
        "attemptNo": rejection.attempt_no,
    });
    ledger::event(
        "ActionRejected",
        "runtime:orch",
        task_id,
        Some(round),
        payload,
    )
}

/// 幂等判定（供 snapshot.rs 调用）：同 actionId 的 `ActionRejected` 是否已存在于
/// 事件流。放在 failure.rs（writeSet 内），不进 ledger.rs。
pub fn action_already_rejected(events: &[EventRecord], action_id: &str) -> bool {
    events.iter().any(|e| {
        e.kind == "ActionRejected"
            && e.payload
                .as_ref()
                .and_then(|p| p.get("actionId"))
                .and_then(serde_json::Value::as_str)
                == Some(action_id)
    })
}

/// 构造一个 run-task pre-attempt 拒绝：actionId=`run-task:<round>:<taskId>`，
/// attempt identity 留 None（尚无 current attempt）。卡正文 line 47-49 裁定。
pub fn run_task_pre_attempt_rejection(
    round: &str,
    task_id: &str,
    reason: &str,
) -> Result<ActionRejection> {
    let action_id = format!("run-task:{}:{}", round, task_id);
    ActionRejection::from_disposition("run-task", &action_id, reason, CliDisposition::Rejected)
}

/// 在已有 current attempt 的路径上构造拒绝：actionId 用 attempt_id，attempt identity
/// 由 `current_attempt(events, task)` 读取（attempt.rs 冻结，只读 pub fn）。
/// 若无 current attempt（返回 None），回退到 `fallback_action_id`（如 wakeId）。
#[doc(hidden)]
pub fn rejection_with_current_attempt(
    events: &[EventRecord],
    task: &str,
    operation: &str,
    fallback_action_id: &str,
    reason: &str,
    exit_code: i32,
) -> Result<ActionRejection> {
    let mut rejection = ActionRejection::new(operation, fallback_action_id, reason, exit_code)?;
    match current_attempt(events, task) {
        Ok(Some(attempt)) => {
            rejection.action_id = attempt.attempt_id.clone();
            rejection = rejection.with_attempt(&attempt);
        }
        Ok(None) => {}
        Err(error) => {
            // Identity ambiguity/malformed attempt data is itself material rejection evidence.
            // Preserve it in the durable reason instead of silently degrading to "no attempt".
            rejection.reason =
                format!("{reason}; current attempt identity unavailable (fail-closed): {error:#}");
        }
    }
    Ok(rejection)
}

/// 落账一个拒绝事件到当前轮账本（`ledger::append` 是 pub fn，不改 ledger.rs）。
/// 返回原 `rejection` 以便调用方继续 bail!/返回退出码。
pub fn append_rejection(
    root: &std::path::Path,
    round: &str,
    task_id: Option<&str>,
    rejection: &ActionRejection,
) -> Result<ActionRejection> {
    let ev = rejection_event(round, task_id, rejection);
    ledger::append(root, round, &[ev])?;
    Ok(rejection.clone())
}

/// Construct, durably append, and return an action-scoped rejection error.
/// Construction and append failures are deliberately propagated with the original reason;
/// callers must never replace them with the operation's earlier error.
#[doc(hidden)]
pub fn reject_action<T>(
    root: &std::path::Path,
    round: &str,
    task_id: Option<&str>,
    operation: &str,
    action_id: &str,
    reason: &str,
    exit_code: i32,
    attempt: Option<&AttemptRef>,
) -> Result<T> {
    let mut rejection = ActionRejection::new(operation, action_id, reason, exit_code)
        .with_context(|| format!("构造 ActionRejected 失败；原拒绝原因: {reason}"))?;
    if let Some(attempt) = attempt {
        rejection = rejection.with_attempt(attempt);
    }
    let rejection = append_rejection(root, round, task_id, &rejection).with_context(|| {
        format!(
            "追加 ActionRejected 失败（operation={operation}, actionId={action_id}）；原拒绝原因: {reason}"
        )
    })?;
    Err(rejection.into_error())
}

/// Same durable rejection boundary, deriving attempt identity from already-read events.
/// A malformed/ambiguous attempt is preserved in `reason` and does not suppress the event.
#[doc(hidden)]
pub fn reject_action_with_events<T>(
    root: &std::path::Path,
    round: &str,
    task_id: Option<&str>,
    operation: &str,
    action_id: &str,
    reason: &str,
    exit_code: i32,
    events: &[EventRecord],
) -> Result<T> {
    let mut rejection = ActionRejection::new(operation, action_id, reason, exit_code)?;
    if let Some(task) = task_id {
        match current_attempt(events, task) {
            Ok(Some(attempt)) => rejection = rejection.with_attempt(&attempt),
            Ok(None) => {}
            Err(error) => {
                rejection.reason = format!(
                    "{reason}; current attempt identity unavailable (fail-closed): {error:#}"
                );
            }
        }
    }
    let rejection = append_rejection(root, round, task_id, &rejection).with_context(|| {
        format!(
            "追加 ActionRejected 失败（operation={operation}, actionId={action_id}）；原拒绝原因: {reason}"
        )
    })?;
    Err(rejection.into_error())
}

/// Durable boundary for errors discovered before a trustworthy ledger snapshot exists.
/// Read failures are included in the event reason; bad JSON lines are retained in the read
/// result and therefore do not prevent appending the rejection after the corrupt line.
#[doc(hidden)]
pub fn reject_action_from_ledger<T>(
    root: &std::path::Path,
    round: &str,
    task_id: Option<&str>,
    operation: &str,
    action_id: &str,
    reason: &str,
    exit_code: i32,
) -> Result<T> {
    let path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    match orch_core::read_ledger(&path) {
        Ok(read) => {
            let durable_reason = if let Some((line, error)) = read.bad_lines.first() {
                format!(
                    "{reason}; rejection ledger contains bad line {line} (fail-closed): {error}"
                )
            } else {
                reason.to_string()
            };
            reject_action_with_events(
                root,
                round,
                task_id,
                operation,
                action_id,
                &durable_reason,
                exit_code,
                &read.events,
            )
        }
        Err(read_error) => {
            let durable_reason =
                format!("{reason}; rejection ledger read failed (fail-closed): {read_error:#}");
            reject_action(
                root,
                round,
                task_id,
                operation,
                action_id,
                &durable_reason,
                exit_code,
                None,
            )
        }
    }
}

/// Durable boundary for operations whose contract explicitly defines the current attempt as
/// the action identity (not merely attached metadata), notably post-attempt `run-task`.
#[doc(hidden)]
pub fn reject_attempt_action_from_ledger<T>(
    root: &std::path::Path,
    round: &str,
    task_id: &str,
    operation: &str,
    fallback_action_id: &str,
    reason: &str,
    exit_code: i32,
) -> Result<T> {
    let path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    match orch_core::read_ledger(&path) {
        Ok(read) => {
            let durable_reason = if let Some((line, error)) = read.bad_lines.first() {
                format!(
                    "{reason}; rejection ledger contains bad line {line} (fail-closed): {error}"
                )
            } else {
                reason.to_string()
            };
            let rejection = rejection_with_current_attempt(
                &read.events,
                task_id,
                operation,
                fallback_action_id,
                &durable_reason,
                exit_code,
            )?;
            let rejection = append_rejection(root, round, Some(task_id), &rejection)
                .with_context(|| {
                    format!(
                        "追加 ActionRejected 失败（operation={operation}, actionId={}）；原拒绝原因: {reason}",
                        rejection.action_id
                    )
                })?;
            Err(rejection.into_error())
        }
        Err(read_error) => {
            let durable_reason =
                format!("{reason}; rejection ledger read failed (fail-closed): {read_error:#}");
            reject_action(
                root,
                round,
                Some(task_id),
                operation,
                fallback_action_id,
                &durable_reason,
                exit_code,
                None,
            )
        }
    }
}

/// Typed production companion for `rejection_with_current_attempt`.
pub fn rejection_with_current_attempt_disposition(
    events: &[EventRecord],
    task: &str,
    operation: &str,
    fallback_action_id: &str,
    reason: &str,
    disposition: CliDisposition,
) -> Result<ActionRejection> {
    rejection_with_current_attempt(
        events,
        task,
        operation,
        fallback_action_id,
        reason,
        i32::from(disposition.code()),
    )
}

/// Typed durable rejection entry for one of the four public dispositions.
#[allow(clippy::too_many_arguments)]
pub fn reject_action_disposition<T>(
    root: &std::path::Path,
    round: &str,
    task_id: Option<&str>,
    operation: &str,
    action_id: &str,
    reason: &str,
    disposition: CliDisposition,
    attempt: Option<&AttemptRef>,
) -> Result<T> {
    reject_action(
        root,
        round,
        task_id,
        operation,
        action_id,
        reason,
        i32::from(disposition.code()),
        attempt,
    )
}

/// Typed durable rejection entry for a command-specific non-terminal outcome.
#[allow(clippy::too_many_arguments)]
pub fn reject_action_command_outcome<T>(
    root: &std::path::Path,
    round: &str,
    task_id: Option<&str>,
    operation: &str,
    action_id: &str,
    reason: &str,
    outcome: CommandOutcome,
    attempt: Option<&AttemptRef>,
) -> Result<T> {
    reject_action(
        root,
        round,
        task_id,
        operation,
        action_id,
        reason,
        i32::from(outcome.code()),
        attempt,
    )
}

/// Typed ledger-reading rejection entry for one of the public dispositions.
#[allow(clippy::too_many_arguments)]
pub fn reject_action_from_ledger_disposition<T>(
    root: &std::path::Path,
    round: &str,
    task_id: Option<&str>,
    operation: &str,
    action_id: &str,
    reason: &str,
    disposition: CliDisposition,
) -> Result<T> {
    reject_action_from_ledger(
        root,
        round,
        task_id,
        operation,
        action_id,
        reason,
        i32::from(disposition.code()),
    )
}

/// Typed ledger-reading rejection entry for a command-specific outcome.
#[allow(clippy::too_many_arguments)]
pub fn reject_action_from_ledger_command_outcome<T>(
    root: &std::path::Path,
    round: &str,
    task_id: Option<&str>,
    operation: &str,
    action_id: &str,
    reason: &str,
    outcome: CommandOutcome,
) -> Result<T> {
    reject_action_from_ledger(
        root,
        round,
        task_id,
        operation,
        action_id,
        reason,
        i32::from(outcome.code()),
    )
}

/// Typed post-attempt rejection entry whose action identity comes from the ledger.
#[allow(clippy::too_many_arguments)]
pub fn reject_attempt_action_from_ledger_disposition<T>(
    root: &std::path::Path,
    round: &str,
    task_id: &str,
    operation: &str,
    fallback_action_id: &str,
    reason: &str,
    disposition: CliDisposition,
) -> Result<T> {
    reject_attempt_action_from_ledger(
        root,
        round,
        task_id,
        operation,
        fallback_action_id,
        reason,
        i32::from(disposition.code()),
    )
}

#[cfg(test)]
mod tests {
    //! B92 边界补测(空证据 / exit None → Unknown;种子落位文件保持 byte-identical)。
    use super::*;

    fn evidence<'a>(
        exit_code: Option<i32>,
        timed_out: bool,
        http_status: Option<u16>,
        protocol_error: Option<&'a str>,
        provider_error: Option<&'a str>,
    ) -> FailureEvidence<'a> {
        FailureEvidence {
            exit_code,
            timed_out,
            http_status,
            protocol_error,
            provider_error,
        }
    }

    #[test]
    fn empty_evidence_is_unknown() {
        assert_eq!(
            classify_adapter_outcome(&evidence(None, false, None, None, None)),
            AdapterOutcome::Failure {
                class: FailureClass::Unknown,
                retryable: false
            }
        );
    }

    #[test]
    fn missing_exit_code_is_unknown() {
        assert_eq!(
            classify_adapter_outcome(&evidence(None, false, None, None, Some("something novel"))),
            AdapterOutcome::Failure {
                class: FailureClass::Unknown,
                retryable: false
            }
        );
    }

    #[test]
    fn outer_command_disposition_overrides_a_rejected_leaf() {
        let leaf = ActionRejection::from_disposition(
            "close",
            "close-r1",
            "leaf guard",
            CliDisposition::Rejected,
        )
        .unwrap()
        .into_error();
        let bound = with_cli_disposition(leaf, CliDisposition::EffectUnknown);
        assert_eq!(rejection_exit_code(&bound), Some(2));
        assert_eq!(cli_error_exit_code(&bound), 5);
    }

    #[test]
    fn plain_errors_default_to_effect_unknown() {
        assert_eq!(
            cli_error_exit_code(&anyhow::anyhow!("unclassified")),
            CliDisposition::EffectUnknown.code()
        );
    }
}
