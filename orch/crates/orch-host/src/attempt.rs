//! Attempt identity, safe Tier-F takeover, and non-destructive WIP snapshot.
//!
//! B97 (r44) — attempt-scoped dispatch/takeover 契约：
//! - 一次 attempt 用 `<task>-A0001` 这种单调递增标识（按 task，不按 agent）；
//! - takeover 只允许在最近 attempt 已终态（Blocked/TimedOut/Crashed/Failed）或
//!   verdict FAIL/BLOCKED 之后；活跃 attempt（仅 ReportObserved 或仍在飞行）禁止接管；
//! - baseSha 永远继承首个 DispatchIssued 的 concrete SHA，不刷新到推进后的 main；
//! - 复用固定 `task/<ID>` 分支与 `.worktrees/<ID>` worktree；新 attempt 更换 agent 落
//!   ReassignmentIssued，同 agent 不伪装 reassignment；
//! - GO/ack 按 attemptId 作用域（`GO-<task>-A0001.md[.ack]`），REPORT/BLOCKED 保持 canonical
//!   `<task>-REPORT.md` / `<task>-BLOCKED.md`；ack 与 terminal 幂等按 attemptId，不按 task；
//! - takeover 前若 worktree dirty，用 git plumbing（临时 index/read-tree/add/write-tree/
//!   commit-tree）捕 staged+unstaged+untracked 到 `archive/<round>-<task>-<attemptId>-wip`
//!   本地 ref，绝不触碰真实 index/HEAD/worktree 字节；clean worktree 返 None。
//!
//! 纯账本推理函数（attempt 事件折叠、路径推导、evidence 严格性、attempt-scoped 幂等）
//! 与 IO 函数（WIP snapshot）都在本模块；real `run_dispatch`/`run_await` 消费这些 API。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use orch_core::EventRecord;
use sha2::{Digest, Sha256};

use crate::gitx;

/// 检查账本是否有坏行。bad_lines 非空 ⇒ Err（fail-closed）。
/// 用于 run_dispatch/run_await/run_resume 在任何决策前显式拒绝坏账本。
pub fn reject_bad_lines(lr: &orch_core::LedgerRead) -> Result<()> {
    if !lr.bad_lines.is_empty() {
        bail!(
            "账本有 {} 坏行（首坏行 #{}: {}），拒绝操作 fail-closed",
            lr.bad_lines.len(),
            lr.bad_lines[0].0,
            lr.bad_lines[0].1
        );
    }
    Ok(())
}

// ─────────────────────────── attempt identity ───────────────────────────

/// 一个 attempt 的稳定标识。格式 `<task>-A0001`，按 task 单调递增。
/// legacy（无 attemptId 的 DispatchIssued）按该 task 追加顺序推导。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptRef {
    pub task_id: String,
    pub ordinal: usize,
    pub attempt_id: String,
}

/// 终态事件：最近 attempt 可被接管的前置条件（非活跃）。除了四个 Attempt* 终态，
/// 还接受最近一条 `VerdictIssued` 的 verdict 为 FAIL/BLOCKED（PASS 不算——Approved 后
/// 不应再有 takeover；下一棒应是新派发而非接管）。ReportObserved 永远不算终态。
const TERMINAL_KINDS: [&str; 4] = [
    "AttemptBlocked",
    "AttemptTimedOut",
    "AttemptCrashed",
    "AttemptFailed",
];

/// `VerdictIssued` 的 verdict 值是否构成「可接管」终态（FAIL/BLOCKED）。
/// PASS/Approved 后不接管；未知 verdict 不视作终态（fail-closed，不静默放行）。
pub(crate) fn verdict_is_takeover_terminal(payload: Option<&serde_json::Value>) -> bool {
    let Some(p) = payload else { return false };
    matches!(
        p.get("verdict").and_then(serde_json::Value::as_str),
        Some("FAIL") | Some("BLOCKED")
    )
}

/// 从账本事件流推导某 task 的「当前 attempt」：取该 task 最后一条 DispatchIssued
/// 所属的 attempt 序号。legacy（无 attemptId 字段完全缺失）的 DispatchIssued 按追加
/// 顺序计数，即第 N 条 DispatchIssued 推导为 ordinal=N、attemptId=`<task>-A{N:04}`。
///
/// **混合 explicit→legacy**：每条 DispatchIssued 的显式 attemptId 仅与该条自己的
/// 序号校验（不与最后一条的 derived 比对）。这样 A0001(explicit)→A0002(legacy) 不会
/// 用过期的 A0001 与 A0002 比对产生误判。
///
/// **Strict identity validation (audit item 2)**：attemptId 字段存在但非 string、
/// 为 null、或与序号不匹配 ⇒ Err（不当 legacy）。attemptNo 若存在必须为正整数且
/// 与序号一致，类型/数值不符 ⇒ Err。legacy 仅指字段完全缺失。
///
/// 无该 task 的任何 DispatchIssued ⇒ None。
pub fn current_attempt(events: &[EventRecord], task: &str) -> Result<Option<AttemptRef>> {
    let mut count = 0usize;
    for ev in events {
        if ev.task_id.as_deref() != Some(task) {
            continue;
        }
        if ev.kind != "DispatchIssued" {
            continue;
        }
        count += 1;
        let derived = AttemptRef {
            task_id: task.to_string(),
            ordinal: count,
            attempt_id: format_attempt_id(task, count),
        };
        // Identity is a three-field generation: all present and exact, or all
        // absent (true legacy).  null/wrong-type/empty and every partial
        // combination are malformed rather than legacy.
        dispatch_identity(ev, &derived)?;
    }
    if count == 0 {
        return Ok(None);
    }
    Ok(Some(AttemptRef {
        task_id: task.to_string(),
        ordinal: count,
        attempt_id: format_attempt_id(task, count),
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchIdentity {
    Modern,
    Legacy,
}

fn dispatch_identity(ev: &EventRecord, derived: &AttemptRef) -> Result<DispatchIdentity> {
    let payload = ev.payload.as_ref().context("DispatchIssued 缺 payload")?;
    let attempt_id_opt = payload.get("attemptId");
    let attempt_no_opt = payload.get("attemptNo");
    let go_path_opt = payload.get("goPath");
    if attempt_id_opt.is_none() && attempt_no_opt.is_none() && go_path_opt.is_none() {
        return Ok(DispatchIdentity::Legacy);
    }
    if let Some(attempt_id_val) = attempt_id_opt {
        let attempt_id = attempt_id_val
            .as_str()
            .filter(|value| !value.is_empty())
            .context("DispatchIssued attemptId 必须是非空 string")?;
        if attempt_id != derived.attempt_id {
            bail!(
                "DispatchIssued attemptId {attempt_id:?} != derived {:?}",
                derived.attempt_id
            );
        }
        if let Some(attempt_no_val) = attempt_no_opt {
            let attempt_no = attempt_no_val
                .as_u64()
                .context("DispatchIssued attemptNo 必须是正整数")?;
            if attempt_no as usize != derived.ordinal {
                bail!(
                    "DispatchIssued attemptNo {attempt_no} != derived {}",
                    derived.ordinal
                );
            }
        }
        return Ok(DispatchIdentity::Modern);
    }
    bail!("DispatchIssued identity partial: attemptId 缺失但 attemptNo/goPath 存在");
}

/// Pure attempt inference deliberately accepts the historical attemptId-only
/// shape used by `current_attempt` and takeover reasoning.  Any code that
/// materializes or replays a production dispatch must instead go through
/// `dispatch_record_for_attempt`, which requires the complete modern
/// attemptId/attemptNo/goPath tuple.
fn dispatch_record_for_attempt_inference(
    ev: &EventRecord,
    derived: &AttemptRef,
) -> Result<DispatchRecord> {
    dispatch_record_for_attempt_mode(ev, derived, false)
}

/// 解析当前 attempt 的完整派发上下文：attempt_id、attempt_no、agent、goPath、baseSha。
/// 用 current_attempt 推导/校验 identity（legacy 兼容），从同一条最新 DispatchIssued
/// payload 取 agent/goPath/baseSha。run_await 每轮调用此函数绑定当前 attempt。
#[derive(Debug, Clone)]
pub struct DispatchContext {
    pub attempt_id: Option<String>,
    pub attempt_no: Option<usize>,
    pub agent: Option<String>,
    pub go_path: Option<String>,
    pub base_sha: Option<String>,
    pub is_legacy: Option<bool>,
}

/// claim 尝试的结果。
#[derive(Debug, Clone)]
pub enum ClaimOutcome {
    /// 新追加了事件（count > 0），事件已落账。count 严格等于 events.len()。
    Appended {
        ctx: DispatchContext,
        count: usize,
        evidence: Option<ClaimedEvidence>,
    },
    /// 同 attempt 幂等：事件已存在，无需追加（count == 0 但幂等合法）。
    /// ctx 携带 fresh dispatch context 供调用方使用（如 collect 的 baseSha/agent）。
    AlreadyPresent {
        ctx: DispatchContext,
        evidence: Option<ClaimedEvidence>,
    },
    /// attempt 在锁内切换：observed attempt != fresh attempt，不追加，回轮询。
    Superseded { fresh_attempt_id: Option<String> },
    /// 证据文件不再 current：mtime != observed 或 <= fresh marker，不追加，回轮询。
    EvidenceNotCurrent,
}

/// 轮询时观察到的 attempt identity。
#[derive(Debug, Clone)]
pub struct AttemptObservation {
    pub attempt_id: Option<String>,
    pub attempt_no: Option<usize>,
}

/// 轮询时观察到的 evidence 文件。
#[derive(Debug, Clone)]
pub struct EvidenceObservation {
    /// Exact absolute path observed by the poller.
    pub path: std::path::PathBuf,
    /// Canonical absolute path, captured before entering the ledger lock.
    pub canonical_path: std::path::PathBuf,
    pub mtime: std::time::SystemTime,
    pub len: u64,
    pub sha256: String,
    pub bytes: Vec<u8>,
}

/// Immutable evidence returned by a successful claim.  Callers must consume
/// these bytes instead of reopening the mutable worktree path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedEvidence {
    pub canonical_path: String,
    pub sha256: String,
    pub len: u64,
    pub bytes: Vec<u8>,
    pub control_epoch: String,
}

/// 闭包在锁内的返回决策。
#[derive(Debug)]
pub enum ClaimDecision {
    /// 追加这些事件
    Append(Vec<EventRecord>),
    /// 幂等：已存在，不追加
    AlreadyPresent,
}

fn metadata_same(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    if a.len() != b.len() || a.modified().ok() != b.modified().ok() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        a.dev() == b.dev() && a.ino() == b.ino()
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn read_regular_evidence(path: &std::path::Path) -> Result<Option<EvidenceObservation>> {
    let before = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("evidence stat 失败: {}", path.display()))
        }
    };
    if !before.file_type().is_file() || before.file_type().is_symlink() {
        bail!("evidence 必须是 regular non-symlink: {}", path.display());
    }
    let canonical_path = path
        .canonicalize()
        .with_context(|| format!("evidence canonicalize 失败: {}", path.display()))?;
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("evidence read 失败: {}", path.display()))
        }
    };
    let after = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("evidence restat 失败: {}", path.display()))
        }
    };
    if !after.file_type().is_file()
        || after.file_type().is_symlink()
        || !metadata_same(&before, &after)
        || after.len() != bytes.len() as u64
    {
        return Ok(None);
    }
    Ok(Some(EvidenceObservation {
        path: path.to_path_buf(),
        canonical_path,
        mtime: after
            .modified()
            .with_context(|| format!("evidence mtime 失败: {}", path.display()))?,
        len: bytes.len() as u64,
        sha256: hex::encode(Sha256::digest(&bytes)),
        bytes,
    }))
}

/// Poll-side immutable observation. Missing/disappearing files are `None`;
/// malformed filesystem objects are fail-closed errors.
pub fn observe_evidence(path: &std::path::Path) -> Result<Option<EvidenceObservation>> {
    read_regular_evidence(path)
}

fn current_control_epoch(events: &[EventRecord], task: &str) -> Result<String> {
    events
        .iter()
        .rev()
        .find(|event| {
            event.task_id.as_deref() == Some(task)
                && matches!(
                    event.kind.as_str(),
                    "DispatchIssued" | "NudgeIssued" | "ResumeIssued"
                )
        })
        .map(|event| {
            if event.event_id.is_empty() {
                bail!("control event 缺 eventId")
            } else {
                Ok(event.event_id.clone())
            }
        })
        .transpose()?
        .context("active attempt 缺 control epoch")
}

/// 统一的 claim 机械位：所有 BLOCKED/timeout/crash/stall/REPORT/ack 都走这一函数。
///
/// 在 ledger.lock 内：
/// 1. fresh resolve_current_dispatch — 要求 active Some attempt
/// 2. fresh attempt_id != observed ⇒ Superseded
/// 3. fresh control marker 从 fresh events + root-resolved GO mtime 重算
/// 4. evidence 提供：symlink_metadata 验证 regular 非 symlink、
///    mtime == observed mtime、mtime > fresh control marker（freshness 在 idempotency 前）
/// 5. 调用 decide 闭包返回 ClaimDecision::Append(events) 或 AlreadyPresent
/// 6. Append: append_checked 返回 actual count，必须 == events.len()，否则 Err
pub fn claim_attempt_action(
    root: &std::path::Path,
    round: &str,
    task: &str,
    observed: &AttemptObservation,
    evidence: Option<&EvidenceObservation>,
    decide: &dyn Fn(
        &[EventRecord],
        &DispatchContext,
        Option<std::time::SystemTime>,
        Option<&ClaimedEvidence>,
        &str,
    ) -> Result<ClaimDecision>,
) -> Result<ClaimOutcome> {
    use std::cell::RefCell;

    let outcome_cell: RefCell<Option<ClaimOutcome>> = RefCell::new(None);
    let expected_count_cell: RefCell<Option<usize>> = RefCell::new(None);

    let actual_count = crate::ledger::append_checked(root, round, |events| {
        // 1. fresh resolve
        let fresh_ctx = resolve_current_dispatch(events, task, round)?;

        // 2. check attempt identity — must be active Some attempt, both
        //    attemptId and attemptNo must be Some and exactly match observed.
        //    No task-level fallback for None==None.
        let (fresh_aid, fresh_no) = (&fresh_ctx.attempt_id, &fresh_ctx.attempt_no);
        match (fresh_aid, &observed.attempt_id) {
            (Some(fresh), Some(obs)) if fresh == obs => {}
            _ => {
                *outcome_cell.borrow_mut() = Some(ClaimOutcome::Superseded {
                    fresh_attempt_id: fresh_aid.clone(),
                });
                return Ok(Vec::new());
            }
        }
        match (fresh_no, &observed.attempt_no) {
            (Some(fresh), Some(obs)) if fresh == obs => {}
            _ => {
                *outcome_cell.borrow_mut() = Some(ClaimOutcome::Superseded {
                    fresh_attempt_id: fresh_aid.clone(),
                });
                return Ok(Vec::new());
            }
        }

        // 3. fresh control marker
        let strict_attempt = AttemptRef {
            task_id: task.to_string(),
            ordinal: fresh_ctx.attempt_no.context("claim 缺 fresh attemptNo")?,
            attempt_id: fresh_ctx
                .attempt_id
                .clone()
                .context("claim 缺 fresh attemptId")?,
        };
        let go_abs = resolve_go_path_strict(
            root,
            fresh_ctx
                .go_path
                .as_deref()
                .context("claim 缺 fresh goPath")?,
            round,
            fresh_ctx.agent.as_deref().context("claim 缺 fresh agent")?,
            &strict_attempt,
            fresh_ctx.is_legacy.context("claim 缺 legacy identity")?,
        )?;
        let _ = ack_observed_strict(&go_abs)?;
        let sig = latest_attempt_signal_precise(events, task);
        let go_mtime = std::fs::symlink_metadata(&go_abs)
            .ok()
            .and_then(|meta| meta.modified().ok());
        let fresh_marker = compute_evidence_marker(sig, go_mtime);
        let control_epoch = current_control_epoch(events, task)?;

        // 4. evidence validation (if provided) — freshness before idempotency
        let claimed = if let Some(ev_obs) = evidence {
            let fresh = match read_regular_evidence(&ev_obs.path) {
                Ok(Some(fresh)) => fresh,
                Ok(None) | Err(_) => {
                    *outcome_cell.borrow_mut() = Some(ClaimOutcome::EvidenceNotCurrent);
                    return Ok(Vec::new());
                }
            };
            if fresh.canonical_path != ev_obs.canonical_path
                || fresh.mtime != ev_obs.mtime
                || fresh.len != ev_obs.len
                || fresh.sha256 != ev_obs.sha256
                || fresh.bytes != ev_obs.bytes
                || !evidence_is_current(Some(fresh.mtime), fresh_marker)
            {
                *outcome_cell.borrow_mut() = Some(ClaimOutcome::EvidenceNotCurrent);
                return Ok(Vec::new());
            }
            Some(ClaimedEvidence {
                canonical_path: fresh.canonical_path.to_string_lossy().into_owned(),
                sha256: fresh.sha256,
                len: fresh.len,
                bytes: fresh.bytes,
                control_epoch: control_epoch.clone(),
            })
        } else {
            None
        };

        // 4½. B144 接线：REPORT evidence 的 frontmatter 机检改经 report_head_fields。
        // 解析失败（非 UTF-8 / 无 frontmatter / 双缺 / 矛盾）→ Err 融入 claim 既有
        // 错误路径；解析成功仅与分支实际提交做一致性比对，不一致只打印提示行进
        // collect 输出（不改变任何既有判定结果——存量 REPORT 兼容是硬约束）。
        if let Some(claimed_ev) = &claimed {
            if is_report_evidence_path(&claimed_ev.canonical_path) {
                if let Some(note) = report_head_claim_check(root, task, &claimed_ev.bytes)? {
                    println!("③½ {note}");
                }
            }
        }

        // 5. call decide closure
        let decision = decide(
            events,
            &fresh_ctx,
            fresh_marker,
            claimed.as_ref(),
            &control_epoch,
        )?;
        match decision {
            ClaimDecision::AlreadyPresent => {
                *outcome_cell.borrow_mut() = Some(ClaimOutcome::AlreadyPresent {
                    ctx: fresh_ctx_clone(&fresh_ctx),
                    evidence: claimed,
                });
                *expected_count_cell.borrow_mut() = Some(0);
                Ok(Vec::new())
            }
            ClaimDecision::Append(evs) => {
                let expected = evs.len();
                if expected == 0 {
                    bail!("claim: decide returned Append with 0 events");
                }
                *outcome_cell.borrow_mut() = Some(ClaimOutcome::Appended {
                    ctx: fresh_ctx_clone(&fresh_ctx),
                    count: expected,
                    evidence: claimed,
                });
                *expected_count_cell.borrow_mut() = Some(expected);
                Ok(evs)
            }
        }
    })?;

    // 6. strict count verification: actual append count must == expected
    let outcome = outcome_cell
        .borrow_mut()
        .take()
        .unwrap_or(ClaimOutcome::EvidenceNotCurrent);
    match &outcome {
        ClaimOutcome::Appended { count, .. } => {
            let expected = expected_count_cell.borrow().unwrap_or(0);
            if actual_count != *count {
                bail!(
                    "claim: append count mismatch: actual={} but expected={}",
                    actual_count,
                    count
                );
            }
            if actual_count != expected {
                bail!(
                    "claim: append count mismatch: actual={} but expected_from_closure={}",
                    actual_count,
                    expected
                );
            }
        }
        _ => {
            // Non-Appended: actual_count must be 0
            if actual_count != 0 {
                bail!(
                    "claim: non-Appended outcome but actual append count={}",
                    actual_count
                );
            }
        }
    }
    Ok(outcome)
}

/// Clone a DispatchContext (helper since DispatchContext fields are all owned/Clone).
fn fresh_ctx_clone(ctx: &DispatchContext) -> DispatchContext {
    DispatchContext {
        attempt_id: ctx.attempt_id.clone(),
        attempt_no: ctx.attempt_no,
        agent: ctx.agent.clone(),
        go_path: ctx.go_path.clone(),
        base_sha: ctx.base_sha.clone(),
        is_legacy: ctx.is_legacy,
    }
}

// ─────────────────────────── strict GO/ack resolver ───────────────────────────

/// 唯一的 strict GO/ack path resolver。
/// 接受 ledger 相对或绝对路径，但必须 byte-exact 等于
/// `coordination/rounds/<round>/dispatch/<agent>/GO-<attempt.attempt_id>.md`（现代）
/// 或仅当 DispatchIssued 确实 legacy 时接受 `GO-<task>.md`（legacy）。
/// `is_legacy` 标记该 DispatchIssued 是否确实无 attemptId 字段。
/// 拒绝 outside absolute、..、nested、wrong filename、wrong ordinal、symlink、非普通文件。
/// 返回 absolute GO path。
pub fn resolve_go_path_strict(
    root: &std::path::Path,
    go_path_str: &str,
    round: &str,
    agent: &str,
    attempt: &AttemptRef,
    is_legacy_dispatch: bool,
) -> Result<std::path::PathBuf> {
    let abs =
        resolve_go_source_schema(root, go_path_str, round, agent, attempt, is_legacy_dispatch)?;
    let meta = std::fs::symlink_metadata(&abs)
        .with_context(|| format!("GO file does not exist or stat failed: {}", abs.display()))?;
    if meta.file_type().is_symlink() {
        bail!("GO path is a symlink (rejected): {go_path_str}");
    }
    if !meta.file_type().is_file() {
        bail!("GO path is not a regular file: {go_path_str}");
    }
    Ok(abs)
}

/// Validate only the ledger path schema, without requiring the source file to
/// exist.  Archive replay must perform this exact validation before it is
/// allowed to inspect the destination slot.
fn resolve_go_source_schema(
    root: &std::path::Path,
    go_path_str: &str,
    round: &str,
    agent: &str,
    attempt: &AttemptRef,
    is_legacy_dispatch: bool,
) -> Result<std::path::PathBuf> {
    if go_path_str.is_empty() {
        bail!("GO path 为空");
    }
    // Path::components normalizes embedded `.` on Unix, so reject raw
    // components first.  Byte-exact comparison below also rejects duplicate
    // separators, trailing slashes and every basename downgrade.
    if go_path_str
        .split('/')
        .any(|component| component == "." || component == "..")
    {
        bail!("GO path contains ./ or ../ component: {go_path_str}");
    }
    for (label, value) in [
        ("round", round),
        ("agent", agent),
        ("task", attempt.task_id.as_str()),
        ("attemptId", attempt.attempt_id.as_str()),
    ] {
        if value.is_empty()
            || value == "."
            || value == ".."
            || value.contains('/')
            || value.contains('\\')
        {
            bail!("GO {label} 不是安全的单路径组件: {value:?}");
        }
    }
    let filename = if is_legacy_dispatch {
        format!("GO-{}.md", attempt.task_id)
    } else {
        format!("GO-{}.md", attempt.attempt_id)
    };
    let expected_rel = format!("coordination/rounds/{round}/dispatch/{agent}/{filename}");
    let expected_abs = root.join(&expected_rel);
    let exact = if std::path::Path::new(go_path_str).is_absolute() {
        go_path_str == expected_abs.to_string_lossy()
    } else {
        go_path_str == expected_rel
    };
    if !exact {
        bail!(
            "GO path schema mismatch: got {go_path_str:?}, expected {:?} or {:?}",
            expected_rel,
            expected_abs.display().to_string()
        );
    }
    validate_parent_chain(root, &expected_abs)?;
    Ok(expected_abs)
}

fn validate_parent_chain(root: &std::path::Path, path: &std::path::Path) -> Result<()> {
    let rel = path
        .strip_prefix(root)
        .with_context(|| format!("path 不在 root 内: {}", path.display()))?;
    let mut current = root.to_path_buf();
    let components = rel.components().collect::<Vec<_>>();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_dir() && !meta.file_type().is_symlink() => {}
            Ok(_) => bail!(
                "path parent 必须是真实目录（不得 symlink）: {}",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("stat path parent 失败: {}", current.display()))
            }
        }
    }
    Ok(())
}

/// Resolve ack path as exact GO + ".ack". Must exist, be regular, non-symlink.
pub fn resolve_ack_path_strict(go_abs: &std::path::Path) -> Result<std::path::PathBuf> {
    let mut ack_str = go_abs.as_os_str().to_os_string();
    ack_str.push(".ack");
    let ack_path = std::path::PathBuf::from(ack_str);
    // must exist, regular, non-symlink — not optional
    let meta = std::fs::symlink_metadata(&ack_path).with_context(|| {
        format!(
            "ack file does not exist or stat failed: {}",
            ack_path.display()
        )
    })?;
    if meta.file_type().is_symlink() {
        bail!("ack path is a symlink (rejected): {}", ack_path.display());
    }
    if !meta.file_type().is_file() {
        bail!("ack path is not a regular file: {}", ack_path.display());
    }
    Ok(ack_path)
}

/// Observe the exact ack slot. Missing is the normal waiting state; every
/// symlink (including broken), directory, or other non-regular entry is an
/// error rather than "not acked".
pub fn ack_observed_strict(go_abs: &std::path::Path) -> Result<bool> {
    let mut ack_os = go_abs.as_os_str().to_os_string();
    ack_os.push(".ack");
    let ack = std::path::PathBuf::from(ack_os);
    match std::fs::symlink_metadata(&ack) {
        Ok(meta) if meta.file_type().is_file() && !meta.file_type().is_symlink() => Ok(true),
        Ok(meta) => bail!(
            "ack slot 必须是 regular non-symlink，实际 type={:?}: {}",
            meta.file_type(),
            ack.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("ack stat 失败: {}", ack.display())),
    }
}

/// 从账本事件流解析该 task 当前 attempt 的完整派发上下文。
/// identity 用 current_attempt 推导（legacy 兼容、malformed fail-closed）。
/// agent/goPath/baseSha 取最新 DispatchIssued payload（与 current_attempt 推导的序号一致的那条）。
pub fn resolve_current_dispatch(
    events: &[EventRecord],
    task: &str,
    expected_round: &str,
) -> Result<DispatchContext> {
    let current = current_attempt(events, task)?;
    let Some(cur) = current else {
        return Ok(DispatchContext {
            attempt_id: None,
            attempt_no: None,
            agent: None,
            go_path: None,
            base_sha: None,
            is_legacy: None,
        });
    };
    let latest_dispatch = events
        .iter()
        .rev()
        .find(|ev| ev.kind == "DispatchIssued" && ev.task_id.as_deref() == Some(task));
    let Some(dispatch_ev) = latest_dispatch else {
        // current_attempt 返回 Some 但无 DispatchIssued — 不可能，但 fail-closed
        bail!("resolve_current_dispatch: current_attempt 有序号但无 DispatchIssued 事件");
    };
    if dispatch_ev.round.as_deref() != Some(expected_round) {
        bail!(
            "current DispatchIssued round {:?} != current round {:?}",
            dispatch_ev.round,
            expected_round
        );
    }
    let record = dispatch_record_for_attempt(dispatch_ev, &cur)?;
    if !record.is_legacy {
        match dispatch_ev
            .payload
            .as_ref()
            .and_then(|payload| payload.get("baseSha"))
        {
            Some(value) if value.as_str().is_some_and(|text| !text.is_empty()) => {}
            _ => bail!("modern DispatchIssued baseSha 必须是非空 string"),
        }
        // P1-2：生产路径 modern dispatch 必须内联完整身份：attemptId（dispatch_identity
        // 已校验精确相等）、attemptNo（存在且等于推导序号）、goPath（存在且等于
        // canonical scoped 路径）。不允许 partial modern 合成为 legacy GO；
        // dispatch_record_for_attempt 的宽松推导仅用于 test 兼容。
        match dispatch_ev
            .payload
            .as_ref()
            .and_then(|payload| payload.get("attemptNo"))
            .and_then(serde_json::Value::as_u64)
        {
            Some(no) if no as usize == cur.ordinal => {}
            Some(no) => bail!(
                "modern DispatchIssued attemptNo {no} != derived {}",
                cur.ordinal
            ),
            None => bail!("modern DispatchIssued 生产路径缺 attemptNo（partial modern 不允许）"),
        }
        let canonical_go = format!(
            "coordination/rounds/{expected_round}/dispatch/{}/GO-{}.md",
            record.agent, cur.attempt_id
        );
        match dispatch_ev
            .payload
            .as_ref()
            .and_then(|payload| payload.get("goPath"))
            .and_then(serde_json::Value::as_str)
            .filter(|text| !text.is_empty())
        {
            Some(go_path) if go_path == canonical_go => {}
            Some(go_path) => {
                bail!("modern DispatchIssued goPath {go_path:?} != canonical {canonical_go:?}")
            }
            None => bail!("modern DispatchIssued 生产路径缺 goPath（partial modern 不允许）"),
        }
    }
    Ok(DispatchContext {
        attempt_id: Some(cur.attempt_id),
        attempt_no: Some(cur.ordinal),
        agent: Some(record.agent),
        go_path: Some(record.go_path),
        base_sha: record.base_sha,
        is_legacy: Some(record.is_legacy),
    })
}

/// 取该 task **首个** DispatchIssued 的 baseSha（原始 concrete SHA，不刷新）。
/// 返回 None 表示首个 DispatchIssued 无 baseSha（账本不完整）。
fn first_dispatch_base_sha(events: &[EventRecord], task: &str) -> Option<String> {
    events
        .iter()
        .find(|ev| ev.kind == "DispatchIssued" && ev.task_id.as_deref() == Some(task))
        .and_then(|ev| ev.payload.as_ref())
        .and_then(|p| p.get("baseSha"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// 最近 attempt 是否已终态（可接管前置）。从后往前扫描该 task 的事件，遇到的第一条
/// 「决定性」事件给出答案：
/// - `DispatchIssued` ⇒ 活跃 attempt（最近派发未终态）⇒ false；
/// - `AttemptBlocked/TimedOut/Crashed/Failed` ⇒ true；
/// - `VerdictIssued` 且 verdict ∈ {FAIL, BLOCKED} ⇒ true（PASS/Approved 不接管）；
/// - `ReportObserved` ⇒ 永远 false（已观察 REPORT 的 attempt 不可接管，必须等终态/裁决）；
/// - 其他事件（DispatchAcked/AttemptStarted/NudgeIssued 等）不影响判定，继续向前。
/// 该事件是否终结了点名的 attempt。
///
/// r62 收轮非门审查 P1-2：`TERMINAL_KINDS` 此前只在本模块内与 orch-cli 的只读视图里
/// 被消费，合并授权路径从不查询它——于是「attempt 终态后其 root PASS 随之作废」这条
/// 语义在 Rust 侧无人执行（`.githooks/reference-transaction` 的第五条清除路径却以它
/// 为依据，构成 H94 型双实现漂移）。这个谓词是两侧共用的判据，`verify.rs` 的合并授权
/// 用它 fail-closed。
///
/// 只认精确点名本 attempt 的终态：payload.attemptId 必须逐字相等。缺 attemptId 的
/// 终态事件不算（宁可漏判也不误杀——误杀会拦住一次合法合并）。
pub(crate) fn event_terminates_attempt(event: &EventRecord, attempt_id: &str) -> bool {
    let names_attempt = || {
        event
            .payload
            .as_ref()
            .and_then(|payload| payload.get("attemptId"))
            .and_then(serde_json::Value::as_str)
            == Some(attempt_id)
    };
    if TERMINAL_KINDS.contains(&event.kind.as_str()) {
        return names_attempt();
    }
    if event.kind == "VerdictIssued" && verdict_is_takeover_terminal(event.payload.as_ref()) {
        return names_attempt();
    }
    false
}

fn latest_attempt_is_terminal(events: &[EventRecord], task: &str) -> bool {
    for ev in events.iter().rev() {
        if ev.task_id.as_deref() != Some(task) {
            continue;
        }
        if ev.kind == "DispatchIssued" {
            return false; // 活跃 attempt：最近派发未终态
        }
        if TERMINAL_KINDS.contains(&ev.kind.as_str()) {
            return true;
        }
        if ev.kind == "VerdictIssued" {
            return verdict_is_takeover_terminal(ev.payload.as_ref());
        }
        if ev.kind == "ReportObserved" {
            return false; // 已观察 REPORT 的 attempt 不可接管
        }
        // 其他事件不影响判定，继续向前
    }
    // 无派发 ⇒ 无活跃 attempt ⇒ 视为「可派发」（非活跃），但 plan_next_attempt 仍要另判
    true
}

/// 最近一条「可接管终态」事件的 attemptId（用于 previous_attempt）。从尾部扫描该 task
/// 的事件，遇到的首条决定性事件给出答案：
/// - `AttemptBlocked/TimedOut/Crashed/Failed` ⇒ 取其 payload.attemptId；
/// - `VerdictIssued` 且 verdict ∈ {FAIL, BLOCKED} ⇒ 取其 payload.attemptId；
/// - `DispatchIssued` / `ReportObserved` ⇒ None（活跃或不可接管，无 previous terminal）；
/// - 其他事件继续向前。
///
/// **Strict terminal identity (audit item 4)**：attemptId 字段只有完全缺失才 legacy，
/// present null/non-string/empty/mismatch 都 Err。
/// **Legacy 兼容**：若终态事件本身未带 attemptId（旧账本），回退到该 task 当前
/// attempt 的推导 attemptId（按 DispatchIssued 追加顺序）。
fn latest_terminal_attempt_id(events: &[EventRecord], task: &str) -> Result<Option<String>> {
    // 先找到决定性终态事件
    let mut terminal_idx: Option<usize> = None;
    for (i, ev) in events.iter().enumerate().rev() {
        if ev.task_id.as_deref() != Some(task) {
            continue;
        }
        if TERMINAL_KINDS.contains(&ev.kind.as_str()) {
            terminal_idx = Some(i);
            break;
        }
        if ev.kind == "VerdictIssued" && verdict_is_takeover_terminal(ev.payload.as_ref()) {
            terminal_idx = Some(i);
            break;
        }
        if ev.kind == "DispatchIssued" || ev.kind == "ReportObserved" {
            return Ok(None);
        }
    }
    let idx = match terminal_idx {
        Some(i) => i,
        None => return Ok(None),
    };
    let terminal_ev = &events[idx];
    // 优先从终态事件 payload 取 attemptId
    if let Some(p) = terminal_ev.payload.as_ref() {
        if let Some(aid_val) = p.get("attemptId") {
            // 字段存在但 null/non-string/empty ⇒ Err, not legacy
            if aid_val.is_null() {
                bail!(
                    "终态事件 {:?} 的 attemptId 为 null（应为 string 或缺失）",
                    terminal_ev.kind
                );
            }
            let aid = aid_val.as_str().ok_or_else(|| {
                anyhow::anyhow!(
                    "终态事件 {:?} 的 attemptId 非 string: {aid_val:?}",
                    terminal_ev.kind
                )
            })?;
            if aid.is_empty() {
                bail!("终态事件 {:?} 的 attemptId 为空串", terminal_ev.kind);
            }
            return Ok(Some(aid.to_string()));
        }
    }
    // Legacy 回退：字段完全缺失，按 DispatchIssued 追加顺序推导
    let dispatch_count = events[..idx]
        .iter()
        .filter(|ev| ev.kind == "DispatchIssued" && ev.task_id.as_deref() == Some(task))
        .count();
    if dispatch_count > 0 {
        Ok(Some(format_attempt_id(task, dispatch_count)))
    } else {
        Ok(None)
    }
}

/// 最近终态 attempt 的 agent。DispatchIssued.agent 是唯一权威；terminal
/// payload.agent 若存在只做严格一致性校验，绝不能覆盖派发身份。
fn latest_terminal_attempt_agent(events: &[EventRecord], task: &str) -> Result<Option<String>> {
    let terminal_aid = match latest_terminal_attempt_id(events, task)? {
        Some(aid) => aid,
        None => return Ok(None),
    };
    let ordinal =
        parse_ordinal_from_attempt_id(&terminal_aid).context("terminal attemptId 格式无效")?;
    let dispatch_ev = events
        .iter()
        .filter(|event| event.kind == "DispatchIssued" && event.task_id.as_deref() == Some(task))
        .nth(ordinal.saturating_sub(1))
        .context("terminal attempt 无对应 DispatchIssued")?;
    let derived = AttemptRef {
        task_id: task.to_string(),
        ordinal,
        attempt_id: terminal_aid,
    };
    let dispatch = dispatch_record_for_attempt_inference(dispatch_ev, &derived)?;

    let terminal = events.iter().rev().find(|event| {
        event.task_id.as_deref() == Some(task)
            && (TERMINAL_KINDS.contains(&event.kind.as_str())
                || (event.kind == "VerdictIssued"
                    && verdict_is_takeover_terminal(event.payload.as_ref())))
    });
    if let Some(value) = terminal
        .and_then(|event| event.payload.as_ref())
        .and_then(|payload| payload.get("agent"))
    {
        let terminal_agent = value
            .as_str()
            .filter(|agent| !agent.is_empty())
            .context("terminal payload.agent 必须是非空 string")?;
        if terminal_agent != dispatch.agent {
            bail!(
                "terminal payload.agent {terminal_agent:?} 与 DispatchIssued.agent {:?} 不一致",
                dispatch.agent
            );
        }
    }
    Ok(Some(dispatch.agent))
}

/// 从 attemptId（格式 `<task>-A0001`）解析序号。
fn parse_ordinal_from_attempt_id(aid: &str) -> Option<usize> {
    let dash = aid.rfind('-')?;
    let num_part = &aid[dash + 1..];
    if !num_part.starts_with('A') {
        return None;
    }
    num_part[1..].parse::<usize>().ok()
}

pub struct NextAttemptPlan {
    pub attempt: AttemptRef,
    pub base_sha: String,
    pub branch: String,
    pub worktree_rel: String,
    pub previous_attempt: Option<AttemptRef>,
    /// 上一 attempt 的 agent（用于 ReassignmentIssued 判定与归档路径）。
    pub previous_agent: Option<String>,
    /// 新 attempt 是否构成 reassignment（previous agent 与新 agent 不同）。
    /// 同 agent fresh retry 不伪装 reassignment。
    pub is_reassignment: bool,
}

/// 规划下一个 attempt。活跃 attempt（最近 DispatchIssued 无终态后续，或最近事件为
/// ReportObserved）⇒ Err（禁接管）。首个 attempt：base_sha = 传入的 concrete_main；
/// 后续 attempt：base_sha = 首个 DispatchIssued 的 baseSha（不刷新到推进后的 main）。
/// 分支/worktree 固定复用 `task/<ID>`、`.worktrees/<ID>`。
/// `is_reassignment` 由 previous_agent 与传入的 agent 比较得出；同 agent fresh retry
/// 不伪装 reassignment。
pub fn plan_next_attempt(
    events: &[EventRecord],
    task: &str,
    agent: &str,
    concrete_main: &str,
    _round: &str,
) -> Result<NextAttemptPlan> {
    let current = current_attempt(events, task)?;
    let Some(cur) = current else {
        // 首个 attempt：无前置派发
        return Ok(NextAttemptPlan {
            attempt: AttemptRef {
                task_id: task.to_string(),
                ordinal: 1,
                attempt_id: format_attempt_id(task, 1),
            },
            base_sha: concrete_main.to_string(),
            branch: format!("task/{task}"),
            worktree_rel: format!(".worktrees/{task}"),
            previous_attempt: None,
            previous_agent: None,
            is_reassignment: false,
        });
    };

    // 已有派发：必须最近 attempt 已终态才允许接管
    if !latest_attempt_is_terminal(events, task) {
        bail!("最近 attempt 仍活跃（未终态），禁止 takeover");
    }

    let prev_aid = latest_terminal_attempt_id(events, task)?;
    // C: validate that the decisive terminal attemptId equals the current/latest
    // attempt. A stale or malformed A0001 terminal after current A0002 must not
    // authorize A0003. If terminal has explicit attemptId != current, fail closed.
    if let Some(ref terminal_aid) = prev_aid {
        if terminal_aid != &cur.attempt_id {
            bail!(
                "终态事件 attemptId {terminal_aid:?} 与当前 attempt {} 不一致，拒绝接管 fail-closed",
                cur.attempt_id
            );
        }
    }
    let previous_attempt = prev_aid.map(|aid| AttemptRef {
        task_id: task.to_string(),
        ordinal: cur.ordinal,
        attempt_id: aid,
    });
    let previous_agent = latest_terminal_attempt_agent(events, task)?;
    // 安全接管要求能解析上一 attempt 的 agent（用于归档路径与 reassignment 判定）。
    // 无法解析时 fail-closed——不得静默继续，也不得在 previous_agent=None 时
    // 把 is_reassignment 设为 true（那会伪装 reassignment）。
    let is_reassignment = match &previous_agent {
        Some(prev) => prev.as_str() != agent,
        None => {
            bail!("takeover 无法解析上一 attempt 的 agent（账本不完整？），拒绝接管 fail-closed");
        }
    };

    // C: base_sha 永远继承首个 DispatchIssued 的 concrete SHA。
    // 缺失首个 baseSha ⇒ fail-closed（不 fall back 到当前 main）。
    let base_sha = first_dispatch_base_sha(events, task)
        .context("takeover 缺失首个 DispatchIssued 的 baseSha，拒绝接管 fail-closed")?;

    Ok(NextAttemptPlan {
        attempt: AttemptRef {
            task_id: task.to_string(),
            ordinal: cur.ordinal + 1,
            attempt_id: format_attempt_id(task, cur.ordinal + 1),
        },
        base_sha,
        branch: format!("task/{task}"),
        worktree_rel: format!(".worktrees/{task}"),
        previous_attempt,
        previous_agent,
        is_reassignment,
    })
}

fn format_attempt_id(task: &str, ordinal: usize) -> String {
    format!("{task}-A{:04}", ordinal)
}

/// Pure approved-attempt successor predicate locked by the B204 seed.
pub fn approved_reattempt_next(
    task_id: &str,
    attempt_id: &str,
    explicit: bool,
    merge_started: bool,
    reason: &str,
) -> Result<String> {
    crate::card::validate_task_id(task_id)?;
    if !explicit {
        bail!("approved reattempt requires explicit --new-attempt");
    }
    if merge_started {
        bail!("approved reattempt is forbidden after MergeStarted");
    }
    if reason.trim().is_empty() {
        bail!("approved reattempt requires a non-empty reason");
    }
    let prefix = format!("{task_id}-A");
    let digits = attempt_id
        .strip_prefix(&prefix)
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .with_context(|| format!("attemptId {attempt_id:?} does not bind task {task_id:?}"))?;
    let ordinal = digits
        .parse::<usize>()
        .with_context(|| format!("attemptId ordinal overflow: {attempt_id}"))?;
    if ordinal == 0 || attempt_id != format_attempt_id(task_id, ordinal) {
        bail!("attemptId is not canonical: {attempt_id}");
    }
    let next = ordinal
        .checked_add(1)
        .context("approved reattempt ordinal overflow")?;
    Ok(format_attempt_id(task_id, next))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedReattemptOutcome {
    pub previous_attempt_id: String,
    pub next_attempt_id: String,
    pub appended: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ApprovedReattemptExplicitContext {
    task_id: String,
    reason: String,
}

thread_local! {
    /// Capability token for the one explicit `dispatch --new-attempt`
    /// orchestration.  Merely finding the durable approved terminal in the
    /// ledger is intentionally insufficient authority to mint its successor:
    /// after a crash, a plain `orch dispatch` must remain fenced until the
    /// operator repeats the explicit flag and exact reason.
    static APPROVED_REATTEMPT_EXPLICIT_CONTEXT:
        std::cell::RefCell<Option<ApprovedReattemptExplicitContext>> =
        const { std::cell::RefCell::new(None) };
}

struct ApprovedReattemptExplicitScope {
    entered: ApprovedReattemptExplicitContext,
}

impl ApprovedReattemptExplicitScope {
    fn enter(task_id: &str, reason: &str) -> Result<Self> {
        let entered = ApprovedReattemptExplicitContext {
            task_id: task_id.to_string(),
            reason: reason.to_string(),
        };
        APPROVED_REATTEMPT_EXPLICIT_CONTEXT.with(|context| {
            let mut context = context.borrow_mut();
            if let Some(active) = context.as_ref() {
                bail!(
                    "nested approved reattempt context rejected: active task={} requested task={}",
                    active.task_id,
                    entered.task_id
                );
            }
            *context = Some(entered.clone());
            Ok(())
        })?;
        Ok(Self { entered })
    }
}

impl Drop for ApprovedReattemptExplicitScope {
    fn drop(&mut self) {
        APPROVED_REATTEMPT_EXPLICIT_CONTEXT.with(|context| {
            let current = context.borrow().clone();
            assert_eq!(
                current.as_ref(),
                Some(&self.entered),
                "unbalanced approved reattempt explicit context"
            );
            *context.borrow_mut() = None;
        });
    }
}

fn approved_reattempt_context_matches(task_id: &str, reason: &str) -> bool {
    APPROVED_REATTEMPT_EXPLICIT_CONTEXT.with(|context| {
        context
            .borrow()
            .as_ref()
            .is_some_and(|active| active.task_id == task_id && active.reason == reason)
    })
}

/// An `AttemptBlocked(stage=approved-reattempt)` is a crash-recovery marker,
/// not ambient takeover authority.  Bind its successor to the lexical
/// explicit CLI capability and to the exact audited reason.
fn require_explicit_approved_reattempt_successor(
    events: &[EventRecord],
    task_id: &str,
) -> Result<()> {
    let decisive = events
        .iter()
        .rev()
        .filter(|event| event.task_id.as_deref() == Some(task_id))
        .find(|event| {
            event.kind == "DispatchIssued"
                || event.kind == "ReportObserved"
                || TERMINAL_KINDS.contains(&event.kind.as_str())
                || (event.kind == "VerdictIssued"
                    && verdict_is_takeover_terminal(event.payload.as_ref()))
        });
    let Some(terminal) = decisive.filter(|event| event.kind == "AttemptBlocked") else {
        return Ok(());
    };
    let Some(stage) = terminal
        .payload
        .as_ref()
        .and_then(|payload| payload.get("stage"))
    else {
        return Ok(());
    };
    let stage = stage
        .as_str()
        .context("AttemptBlocked stage must be a string when present")?;
    if stage != "approved-reattempt" {
        return Ok(());
    }
    let reason = terminal
        .payload
        .as_ref()
        .and_then(|payload| payload.get("reason"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .context("approved-reattempt terminal lacks a non-empty reason")?;
    if !approved_reattempt_context_matches(task_id, reason) {
        bail!("approved-reattempt successor requires explicit --new-attempt with the exact reason");
    }
    Ok(())
}

enum ApprovedReattemptDecision {
    Append {
        previous: AttemptRef,
        next_attempt_id: String,
        agent: String,
        verdict_event_id: String,
    },
    AlreadyPrepared {
        previous_attempt_id: String,
        next_attempt_id: String,
    },
}

fn payload_str<'a>(event: &'a EventRecord, key: &str) -> Option<&'a str> {
    event.payload.as_ref()?.get(key)?.as_str()
}

fn ensure_no_inflight_durable_action(
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    dispatch_position: usize,
) -> Result<()> {
    let scoped = &events[dispatch_position + 1..];
    for kind in [
        DurableActionKind::DispatchWake,
        DurableActionKind::ResumeWake,
        DurableActionKind::ReportCollect,
    ] {
        let prefix = kind.prefix();
        let mut action_ids = BTreeSet::new();
        for event in scoped.iter().filter(|event| {
            event.task_id.as_deref() == Some(task_id) && event.kind.starts_with(prefix)
        }) {
            if event.round.as_deref() != Some(round) {
                bail!(
                    "approved reattempt durable {} round mismatch: {:?}",
                    event.kind,
                    event.round
                );
            }
            let action_id = payload_str(event, "actionId")
                .filter(|value| !value.is_empty())
                .with_context(|| format!("{} lacks durable actionId", event.kind))?;
            action_ids.insert(action_id.to_string());
        }
        for action_id in action_ids {
            let selected = scoped
                .iter()
                .filter(|event| {
                    event.task_id.as_deref() == Some(task_id)
                        && event.kind.starts_with(prefix)
                        && payload_str(event, "actionId") == Some(action_id.as_str())
                })
                .collect::<Vec<_>>();
            let first = selected
                .first()
                .copied()
                .context("durable action identity disappeared")?;
            let payload = first
                .payload
                .as_ref()
                .context("durable action lacks payload")?;
            let required_str = |key: &str| -> Result<String> {
                payload
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .with_context(|| format!("{} lacks durable {key}", first.kind))
            };
            let attempt_no = payload
                .get("attemptNo")
                .and_then(serde_json::Value::as_u64)
                .filter(|value| *value > 0)
                .context("durable action lacks positive attemptNo")?
                .try_into()
                .context("durable attemptNo overflow")?;
            let completed = selected
                .iter()
                .rev()
                .copied()
                .find(|event| event.kind == format!("{prefix}Completed"));
            let terminal_value = |key: &str| -> Option<String> {
                completed
                    .and_then(|event| event.payload.as_ref())
                    .and_then(|payload| payload.get(key))
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            };
            let terminal_len = completed
                .and_then(|event| event.payload.as_ref())
                .and_then(|payload| payload.get("evidenceLen"))
                .and_then(serde_json::Value::as_u64);
            if kind == DurableActionKind::ReportCollect && completed.is_some() {
                if terminal_value("branchSha").is_none()
                    || terminal_value("evidenceSha256").is_none()
                    || terminal_len.is_none()
                    || terminal_value("controlEpoch").is_none()
                {
                    bail!("approved reattempt sees malformed ReportCollectCompleted");
                }
            }
            let expectation = DurableActionExpectation {
                round: round.to_string(),
                task_id: task_id.to_string(),
                attempt_id: required_str("attemptId")?,
                attempt_no,
                agent: required_str("agent")?,
                base_sha: required_str("baseSha")?,
                go_path: required_str("goPath")?,
                action_id: action_id.clone(),
                evidence_sha256: terminal_value("evidenceSha256"),
                evidence_len: terminal_len,
                control_epoch: terminal_value("controlEpoch"),
                branch_sha: terminal_value("branchSha"),
            };
            let phase = fold_durable_action(scoped, kind, &expectation)?;
            if !matches!(
                phase,
                DurableActionPhase::Completed | DurableActionPhase::Released
            ) {
                bail!(
                    "approved reattempt refuses in-flight durable action {prefix}:{action_id} phase={phase:?}"
                );
            }
        }
    }
    Ok(())
}

fn ensure_no_inflight_managed_review(
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
) -> Result<()> {
    let formal_wake_ids = events
        .iter()
        .filter(|event| {
            event.kind == "ReviewRequested"
                && event.task_id.as_deref() == Some(task_id)
                && event.round.as_deref() == Some(round)
                && payload_str(event, "attemptId") == Some(attempt_id)
        })
        .filter_map(|event| payload_str(event, "wakeId"))
        .collect::<BTreeSet<_>>();

    let mut managed_wakes = BTreeMap::<String, &EventRecord>::new();
    for wake in events.iter().filter(|event| {
        event.kind == "WakeIssued"
            && event.round.as_deref() == Some(round)
            && (formal_wake_ids.contains(payload_str(event, "wakeId").unwrap_or_default())
                || (event.task_id.as_deref() == Some(task_id)
                    && payload_str(event, "attemptId") == Some(attempt_id)))
    }) {
        let Some(control_wake_id) = payload_str(wake, "controlWakeId") else {
            continue;
        };
        let wake_id = payload_str(wake, "wakeId")
            .filter(|value| !value.is_empty())
            .context("managed review WakeIssued lacks wakeId")?;
        if control_wake_id != wake_id {
            bail!("managed review WakeIssued controlWakeId mismatch");
        }
        if managed_wakes.insert(wake_id.to_string(), wake).is_some() {
            bail!("managed review WakeIssued is duplicated for wakeId={wake_id}");
        }
    }
    for (wake_id, wake) in managed_wakes {
        let terminals = events
            .iter()
            .filter(|event| {
                event.kind == "ManagedWakeTerminated"
                    && payload_str(event, "wakeId") == Some(wake_id.as_str())
            })
            .collect::<Vec<_>>();
        if terminals.len() != 1 {
            bail!(
                "approved reattempt refuses managed review wake {wake_id}: expected one terminal, found {}",
                terminals.len()
            );
        }
        let terminal = terminals[0];
        if terminal.task_id.as_deref() != wake.task_id.as_deref()
            || terminal.round.as_deref() != Some(round)
            || payload_str(terminal, "agent") != payload_str(wake, "agent")
            || terminal
                .payload
                .as_ref()
                .and_then(|payload| payload.get("managedScopeTerminated"))
                .and_then(serde_json::Value::as_bool)
                != Some(true)
        {
            bail!("approved reattempt managed review wake terminal is not exact/fully terminated");
        }
    }

    let mut attach_actions = BTreeMap::<String, Vec<&EventRecord>>::new();
    for event in events.iter().filter(|event| {
        event.kind == "ManagedWakeAttachState"
            && event.task_id.as_deref() == Some(task_id)
            && event.round.as_deref() == Some(round)
            && payload_str(event, "attemptId") == Some(attempt_id)
    }) {
        let action_id = payload_str(event, "actionId")
            .filter(|value| !value.is_empty())
            .context("ManagedWakeAttachState lacks actionId")?;
        attach_actions
            .entry(action_id.to_string())
            .or_default()
            .push(event);
    }
    let legal_transition = |previous: &str, next: &str| {
        matches!(
            (previous, next),
            (
                "claimed",
                "source-stop-requested" | "source-terminal" | "failed"
            ) | ("source-stop-requested", "source-terminal" | "failed")
                | ("source-terminal", "launching" | "released" | "failed")
                | ("launching", "delivered" | "failed")
                | ("delivered", "completed")
        )
    };
    for (action_id, chain) in attach_actions {
        let phases = chain
            .iter()
            .map(|event| {
                payload_str(event, "phase")
                    .filter(|value| !value.is_empty())
                    .context("ManagedWakeAttachState lacks phase")
            })
            .collect::<Result<Vec<_>>>()?;
        if phases.as_slice() == ["session-death-declared"] {
            continue;
        }
        if phases.first().copied() != Some("claimed")
            || phases
                .windows(2)
                .any(|pair| !legal_transition(pair[0], pair[1]))
        {
            bail!("managed review attach {action_id} has an invalid phase chain");
        }
        if !matches!(
            phases.last().copied(),
            Some("completed" | "released" | "failed")
        ) {
            bail!(
                "approved reattempt refuses in-flight managed review attach {action_id} phase={:?}",
                phases.last()
            );
        }
    }
    Ok(())
}

fn approved_reattempt_decision(
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    reason: &str,
) -> Result<ApprovedReattemptDecision> {
    let reason = reason.trim();
    if reason.is_empty() {
        bail!("approved reattempt requires a non-empty reason");
    }
    let current = current_attempt(events, task_id)?
        .with_context(|| format!("approved reattempt task {task_id} has no current attempt"))?;
    let dispatches = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "DispatchIssued" && event.task_id.as_deref() == Some(task_id)
        })
        .collect::<Vec<_>>();
    let (dispatch_position, dispatch_event) = *dispatches
        .last()
        .context("approved reattempt current dispatch disappeared")?;
    if dispatch_event.round.as_deref() != Some(round) {
        bail!("approved reattempt current dispatch is outside the active round");
    }
    let dispatch = dispatch_record_for_attempt(dispatch_event, &current)?;
    let full_oid = |value: &str| {
        value.len() == 40
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    let current_base = dispatch
        .base_sha
        .as_deref()
        .filter(|value| full_oid(value))
        .context("approved reattempt current dispatch lacks concrete lowercase baseSha")?;

    // Validate every historical approved terminal independently. Multiple
    // generations are legal (A1->A2, later A2->A3); duplicates for the same
    // predecessor are not.
    let prepared_events = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "AttemptBlocked"
                && event.actor == "runtime:orch"
                && event.task_id.as_deref() == Some(task_id)
                && event.round.as_deref() == Some(round)
                && payload_str(event, "stage") == Some("approved-reattempt")
        })
        .collect::<Vec<_>>();
    let mut prepared = BTreeMap::<String, (usize, String, String, String, String, usize)>::new();
    for (blocked_position, blocked) in prepared_events {
        let payload = blocked
            .payload
            .as_ref()
            .and_then(serde_json::Value::as_object)
            .context("approved-reattempt AttemptBlocked payload must be an object")?;
        const KEYS: &[&str] = &[
            "attemptId",
            "attemptNo",
            "agent",
            "stage",
            "verdictEventId",
            "reason",
        ];
        if payload.len() != KEYS.len() || KEYS.iter().any(|key| !payload.contains_key(*key)) {
            bail!("approved-reattempt AttemptBlocked payload shape is not exact");
        }
        let previous_attempt_id = payload_str(blocked, "attemptId")
            .context("approved-reattempt AttemptBlocked lacks attemptId")?;
        let next_attempt_id = approved_reattempt_next(
            task_id,
            previous_attempt_id,
            true,
            false,
            payload_str(blocked, "reason").unwrap_or_default(),
        )?;
        let previous_ordinal = previous_attempt_id
            .strip_prefix(&format!("{task_id}-A"))
            .and_then(|digits| digits.parse::<usize>().ok())
            .context("approved-reattempt previous attempt ordinal is invalid")?;
        let blocked_attempt_no = blocked
            .payload
            .as_ref()
            .and_then(|payload| payload.get("attemptNo"))
            .and_then(serde_json::Value::as_u64)
            .context("approved-reattempt AttemptBlocked lacks attemptNo")?;
        if blocked_attempt_no != previous_ordinal as u64 {
            bail!("approved-reattempt AttemptBlocked attemptNo mismatch");
        }
        let blocked_agent = payload_str(blocked, "agent")
            .filter(|value| !value.is_empty())
            .context("approved-reattempt AttemptBlocked lacks agent")?;
        let blocked_reason = payload_str(blocked, "reason")
            .filter(|value| !value.trim().is_empty())
            .context("approved-reattempt AttemptBlocked lacks reason")?;
        let verdict_event_id = payload_str(blocked, "verdictEventId")
            .filter(|value| !value.is_empty())
            .context("approved-reattempt AttemptBlocked lacks verdictEventId")?;
        let attempt_verdicts = events
            .iter()
            .enumerate()
            .filter(|event| {
                event.1.kind == "VerdictIssued"
                    && event.1.actor == "verifier:root"
                    && event.1.task_id.as_deref() == Some(task_id)
                    && event.1.round.as_deref() == Some(round)
                    && payload_str(event.1, "attemptId") == Some(previous_attempt_id)
            })
            .collect::<Vec<_>>();
        if attempt_verdicts.len() != 1
            || attempt_verdicts[0].1.event_id != verdict_event_id
            || attempt_verdicts[0].0 >= blocked_position
        {
            bail!("approved-reattempt terminal must bind the sole root verdict for its attempt");
        }
        let verdict_payload: crate::verify::RootVerdictPayload = serde_json::from_value(
            attempt_verdicts[0]
                .1
                .payload
                .clone()
                .context("approved-reattempt root verdict lacks payload")?,
        )
        .context("approved-reattempt root verdict payload is malformed")?;
        if verdict_payload.verdict != "PASS"
            || verdict_payload.attempt_id != previous_attempt_id
            || verdict_payload.attempt_no != previous_ordinal
            || verdict_payload.implementer_agent != blocked_agent
        {
            bail!("approved-reattempt terminal root PASS tuple mismatch");
        }
        if prepared
            .insert(
                previous_attempt_id.to_string(),
                (
                    blocked_position,
                    next_attempt_id,
                    blocked_agent.to_string(),
                    blocked_reason.to_string(),
                    verdict_event_id.to_string(),
                    previous_ordinal,
                ),
            )
            .is_some()
        {
            bail!("approved-reattempt AttemptBlocked is duplicated for {previous_attempt_id}");
        }
    }

    // Crash after the terminal fact but before the successor DispatchIssued.
    if let Some((blocked_position, next_attempt_id, _, blocked_reason, _, _)) =
        prepared.get(&current.attempt_id)
    {
        if blocked_reason != reason || *blocked_position <= dispatch_position {
            bail!("existing approved-reattempt terminal fact conflicts with this request");
        }
        return Ok(ApprovedReattemptDecision::AlreadyPrepared {
            previous_attempt_id: current.attempt_id.clone(),
            next_attempt_id: next_attempt_id.clone(),
        });
    }

    let root_verdicts = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "VerdictIssued"
                && event.actor == "verifier:root"
                && event.task_id.as_deref() == Some(task_id)
                && event.round.as_deref() == Some(round)
                && payload_str(event, "attemptId") == Some(current.attempt_id.as_str())
        })
        .collect::<Vec<_>>();

    // Crash/replay after the exact successor dispatch. The reason is the
    // stable operator action key for this CLI shape: if the response is lost
    // and A(n+1) later obtains its own PASS, replaying the original reason
    // must still resolve A(n)->A(n+1), never silently reinterpret the retry as
    // a new A(n+1)->A(n+2) request. A genuinely new generation therefore
    // needs a fresh reason.
    let predecessors = prepared
        .iter()
        .filter(|(_, (_, next, _, _, _, _))| next == &current.attempt_id)
        .collect::<Vec<_>>();
    if predecessors.len() > 1 {
        bail!("approved-reattempt successor has multiple predecessor terminals");
    }
    if let Some((
        previous_attempt_id,
        (blocked_position, next, previous_agent, blocked_reason, _, previous_ordinal),
    )) = predecessors.first().copied()
    {
        if *blocked_position >= dispatch_position
            || dispatch.previous_attempt_id.as_deref() != Some(previous_attempt_id.as_str())
            || dispatch.previous_agent.as_deref() != Some(previous_agent.as_str())
            || current.ordinal != previous_ordinal + 1
            || next != &current.attempt_id
            || dispatch.reassignment != (dispatch.agent != *previous_agent)
        {
            bail!("approved-reattempt successor dispatch lineage conflicts with replay");
        }
        let previous = AttemptRef {
            task_id: task_id.to_string(),
            attempt_id: previous_attempt_id.to_string(),
            ordinal: *previous_ordinal,
        };
        let previous_dispatch = events[..*blocked_position]
            .iter()
            .rev()
            .find(|event| {
                event.kind == "DispatchIssued"
                    && event.task_id.as_deref() == Some(task_id)
                    && payload_str(event, "attemptId") == Some(previous_attempt_id)
            })
            .context("approved-reattempt predecessor dispatch disappeared")?;
        let previous_dispatch = dispatch_record_for_attempt(previous_dispatch, &previous)?;
        let previous_base = previous_dispatch
            .base_sha
            .as_deref()
            .filter(|value| full_oid(value))
            .context("approved-reattempt predecessor lacks concrete lowercase baseSha")?;
        if current_base != previous_base {
            bail!("approved-reattempt successor did not inherit concrete baseSha");
        }
        if blocked_reason == reason {
            return Ok(ApprovedReattemptDecision::AlreadyPrepared {
                previous_attempt_id: previous_attempt_id.to_string(),
                next_attempt_id: current.attempt_id.clone(),
            });
        }
        if root_verdicts.is_empty() {
            bail!("approved-reattempt successor dispatch lineage conflicts with replay reason");
        }
    }

    // A reason already bound to some older generation is never reusable. This
    // turns arbitrarily delayed command replay into a refusal rather than a
    // fresh successor when the task has advanced beyond the original A(n+1).
    if prepared
        .iter()
        .any(|(_, (_, _, _, blocked_reason, _, _))| blocked_reason == reason)
    {
        bail!("approved-reattempt reason is already bound to a historical transition");
    }

    ensure_no_inflight_durable_action(events, round, task_id, dispatch_position)?;
    if root_verdicts.len() != 1 {
        bail!(
            "approved reattempt requires exactly one current root verdict, found {}",
            root_verdicts.len()
        );
    }
    let (verdict_position, verdict) = root_verdicts[0];
    let verdict_payload: crate::verify::RootVerdictPayload = serde_json::from_value(
        verdict
            .payload
            .clone()
            .context("approved reattempt current root verdict lacks payload")?,
    )
    .context("approved reattempt current root verdict payload is malformed")?;
    if verdict_position <= dispatch_position
        || verdict_payload.verdict != "PASS"
        || verdict_payload.attempt_id != current.attempt_id
        || verdict_payload.attempt_no != current.ordinal
        || verdict_payload.implementer_agent != dispatch.agent
    {
        bail!("approved reattempt root verdict is not the exact current PASS tuple");
    }
    for event in events.iter().skip(verdict_position + 1).filter(|event| {
        event.task_id.as_deref() == Some(task_id) && event.round.as_deref() == Some(round)
    }) {
        match event.kind.as_str() {
            "MergeStarted" => bail!("approved reattempt is forbidden after MergeStarted"),
            "MergeExecuted" | "TaskRecorded" => {
                bail!("approved reattempt is forbidden after merge/record")
            }
            kind if TERMINAL_KINDS.contains(&kind) => {
                bail!("approved reattempt current attempt is already terminal")
            }
            _ => {}
        }
    }
    let next_attempt_id =
        approved_reattempt_next(task_id, &current.attempt_id, true, false, reason)?;
    Ok(ApprovedReattemptDecision::Append {
        previous: current,
        next_attempt_id,
        agent: dispatch.agent,
        verdict_event_id: verdict.event_id.clone(),
    })
}

/// Terminate an approved pre-merge attempt while the caller holds both the
/// global protocol transition and Tier-F capacity leases.  This is private on
/// purpose: publishing the terminal outside the dispatch critical section
/// would recreate the crash window in which an unrelated plain dispatch could
/// consume it.
fn prepare_approved_reattempt_locked(
    root: &Path,
    task_id: &str,
    reason: &str,
    was_existing: bool,
) -> Result<ApprovedReattemptOutcome> {
    let reason = reason.trim().to_string();
    if !approved_reattempt_context_matches(task_id, &reason) {
        bail!("approved reattempt preparation requires its explicit dispatch context");
    }
    let round = crate::current_round(root)?;
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let read = orch_core::read_ledger(&ledger_path)?;
    reject_bad_lines(&read)?;
    let (previous, next_attempt_id, agent, verdict_event_id) =
        match approved_reattempt_decision(&read.events, &round, task_id, &reason)? {
            ApprovedReattemptDecision::AlreadyPrepared {
                previous_attempt_id,
                next_attempt_id,
            } => {
                ensure_no_inflight_managed_review(
                    &read.events,
                    &round,
                    task_id,
                    &previous_attempt_id,
                )?;
                let current = current_attempt(&read.events, task_id)?
                    .context("approved reattempt replay lost current attempt")?;
                if current.attempt_id == previous_attempt_id {
                    let dispatch_position = read
                        .events
                        .iter()
                        .rposition(|event| {
                            event.kind == "DispatchIssued"
                                && event.task_id.as_deref() == Some(task_id)
                                && payload_str(event, "attemptId")
                                    == Some(previous_attempt_id.as_str())
                        })
                        .context("approved reattempt replay lost predecessor dispatch")?;
                    ensure_no_inflight_durable_action(
                        &read.events,
                        &round,
                        task_id,
                        dispatch_position,
                    )?;
                }
                return Ok(ApprovedReattemptOutcome {
                    previous_attempt_id,
                    next_attempt_id,
                    appended: false,
                });
            }
            ApprovedReattemptDecision::Append {
                previous,
                next_attempt_id,
                agent,
                verdict_event_id,
            } => (previous, next_attempt_id, agent, verdict_event_id),
        };
    if !was_existing {
        bail!("fresh approved reattempt must begin from Tier-F Existing plan");
    }
    ensure_no_inflight_managed_review(&read.events, &round, task_id, &previous.attempt_id)?;
    let authorization =
        crate::verify::validate_root_merge_authorization(root, &round, task_id, &read.events)?;
    if authorization.attempt_id != previous.attempt_id
        || authorization.attempt_no != previous.ordinal
        || authorization.implementer_agent != agent
        || authorization.verdict_event_id != verdict_event_id
    {
        bail!("approved reattempt root authorization tuple drifted");
    }

    let terminal_event = crate::ledger::event(
        "AttemptBlocked",
        "runtime:orch",
        Some(task_id),
        Some(&round),
        serde_json::json!({
            "attemptId": previous.attempt_id,
            "attemptNo": previous.ordinal,
            "agent": agent,
            "stage": "approved-reattempt",
            "verdictEventId": verdict_event_id,
            "reason": reason,
        }),
    );
    let mut projected = read.events.clone();
    projected.push(terminal_event);
    let concrete_main = gitx::rev_parse(root, "main")?;
    let (next, previous_dispatch) =
        match plan_dispatch_locked(&projected, task_id, &agent, &concrete_main, &round)? {
            DispatchPlan::New {
                next,
                previous_dispatch: Some(previous_dispatch),
            } => (next, previous_dispatch),
            _ => bail!("approved reattempt projected plan is not one terminal successor"),
        };
    if next.attempt.attempt_id != next_attempt_id
        || next.previous_attempt.as_ref() != Some(&previous)
        || next.previous_agent.as_deref() != Some(agent.as_str())
        || next.is_reassignment
        || previous_dispatch.attempt != previous
        || previous_dispatch.agent != agent
    {
        bail!("approved reattempt projected successor tuple drifted");
    }
    let active = crate::plan::require_active_round_ir(root, &round, &projected)?;
    crate::scheduler::scheduling_admits(
        &projected,
        &round,
        &active.candidate.scheduling,
        &agent,
        "implement",
    )
    .map_err(anyhow::Error::msg)?;
    let mut wake_permit = crate::budget::check_before_model_wake(root, &round)?;

    let branch_exists = gitx::branch_exists(root, &next.branch);
    let worktree = root.join(&next.worktree_rel);
    if !branch_exists || !worktree.is_dir() {
        bail!("approved reattempt requires the root-authorized branch/worktree to remain present");
    }
    if gitx::current_branch(&worktree)?.as_deref() != Some(next.branch.as_str())
        || gitx::rev_parse(&worktree, "HEAD")? != authorization.head_sha
        || !gitx::porcelain_v2(&worktree)?.trim().is_empty()
    {
        bail!("approved reattempt requires the exact clean root-authorized worktree HEAD");
    }
    let worktree_step = go_worktree_instruction(task_id, &next.base_sha, true, true)?;
    let paths = attempt_paths(&round, &agent, &next.attempt);
    let report_rel = format!("coordination/rounds/{round}/reports/{task_id}-REPORT.md");
    let go_bytes = render_attempt_go(
        task_id,
        &agent,
        gitx::short(&next.base_sha),
        &round,
        &report_rel,
        &worktree_step,
    );
    let previous_go = resolve_superseded_go(root, &round, &previous_dispatch)?;
    archive_preflight(
        root,
        &previous_go,
        &previous.attempt_id,
        &previous_dispatch.agent,
    )?;
    archive_superseded_dispatch(
        root,
        &previous_go,
        &previous.attempt_id,
        &previous_dispatch.agent,
    )?;
    let go_path = root.join(&paths.go_rel);
    reconcile_or_create_go(&go_path, &go_bytes)?;

    let expected_previous_id = previous.attempt_id.clone();
    let expected_previous_no = previous.ordinal;
    let expected_next_id = next.attempt.attempt_id.clone();
    let expected_next_no = next.attempt.ordinal;
    let expected_base = next.base_sha.clone();
    let expected_branch = next.branch.clone();
    let expected_verdict_id = verdict_event_id.clone();
    let expected_agent = agent.clone();
    let expected_go_rel = paths.go_rel.clone();
    let appended = crate::ledger::append_checked(root, &round, |events| {
        let fresh_round = crate::current_round(root)?;
        if fresh_round != round || gitx::rev_parse(root, "main")? != concrete_main {
            bail!("approved reattempt round/main drifted before atomic batch");
        }
        let (fresh_previous, fresh_next_id, fresh_agent, fresh_verdict_id) =
            match approved_reattempt_decision(events, &round, task_id, &reason)? {
                ApprovedReattemptDecision::Append {
                    previous,
                    next_attempt_id,
                    agent,
                    verdict_event_id,
                } => (previous, next_attempt_id, agent, verdict_event_id),
                ApprovedReattemptDecision::AlreadyPrepared { .. } => {
                    bail!("approved reattempt batch raced with an existing preparation")
                }
            };
        if fresh_previous.attempt_id != expected_previous_id
            || fresh_previous.ordinal != expected_previous_no
            || fresh_next_id != expected_next_id
            || fresh_agent != expected_agent
            || fresh_verdict_id != expected_verdict_id
        {
            bail!("approved reattempt fresh decision tuple drifted");
        }
        ensure_no_inflight_managed_review(events, &round, task_id, &fresh_previous.attempt_id)?;
        let fresh_authorization =
            crate::verify::validate_root_merge_authorization(root, &round, task_id, events)?;
        if fresh_authorization.attempt_id != expected_previous_id
            || fresh_authorization.attempt_no != expected_previous_no
            || fresh_authorization.implementer_agent != expected_agent
            || fresh_authorization.verdict_event_id != expected_verdict_id
            || fresh_authorization.head_sha != authorization.head_sha
        {
            bail!("approved reattempt fresh root authorization drifted");
        }
        if gitx::current_branch(&worktree)?.as_deref() != Some(expected_branch.as_str())
            || gitx::rev_parse(&worktree, "HEAD")? != fresh_authorization.head_sha
            || !gitx::porcelain_v2(&worktree)?.trim().is_empty()
        {
            bail!("approved reattempt worktree drifted before atomic batch");
        }
        let metadata = std::fs::symlink_metadata(&go_path)?;
        if !metadata.file_type().is_file()
            || metadata.file_type().is_symlink()
            || std::fs::read(&go_path)? != go_bytes
        {
            bail!("approved reattempt successor GO drifted before atomic batch");
        }

        let terminal = crate::ledger::event(
            "AttemptBlocked",
            "runtime:orch",
            Some(task_id),
            Some(&round),
            serde_json::json!({
                "attemptId": expected_previous_id,
                "attemptNo": expected_previous_no,
                "agent": expected_agent,
                "stage": "approved-reattempt",
                "verdictEventId": expected_verdict_id,
                "reason": reason,
            }),
        );
        let mut fresh_projected = events.to_vec();
        fresh_projected.push(terminal.clone());
        let fresh_plan = plan_dispatch_locked(
            &fresh_projected,
            task_id,
            &expected_agent,
            &concrete_main,
            &round,
        )?;
        match fresh_plan {
            DispatchPlan::New { next, .. }
                if next.attempt.attempt_id == expected_next_id
                    && next.attempt.ordinal == expected_next_no
                    && next.base_sha == expected_base
                    && !next.is_reassignment => {}
            _ => bail!("approved reattempt fresh projected dispatch plan drifted"),
        }
        let active = crate::plan::require_active_round_ir(root, &round, &fresh_projected)?;
        crate::scheduler::scheduling_admits(
            &fresh_projected,
            &round,
            &active.candidate.scheduling,
            &expected_agent,
            "implement",
        )
        .map_err(anyhow::Error::msg)?;
        let dispatch = crate::ledger::event(
            "DispatchIssued",
            "runtime:orch",
            Some(task_id),
            Some(&round),
            serde_json::json!({
                "agent": expected_agent,
                "method": "filegate(GO)",
                "baseSha": expected_base,
                "goPath": expected_go_rel,
                "attemptId": expected_next_id,
                "attemptNo": expected_next_no,
                "reassignment": false,
                "overrideAmbiguousActive": false,
                "wakePending": true,
                "previousAttemptId": expected_previous_id,
                "previousAgent": expected_agent,
            }),
        );
        Ok(vec![terminal, dispatch])
    })?;
    if appended != 2 {
        bail!("approved reattempt atomic batch append count mismatch: {appended}");
    }
    wake_permit.commit();
    Ok(ApprovedReattemptOutcome {
        previous_attempt_id: previous.attempt_id,
        next_attempt_id,
        appended: true,
    })
}

/// Fresh post-dispatch fence for the atomic CLI orchestration. The terminal
/// and successor are one ledger batch; this readback proves Tier-F then
/// observed that exact generation rather than a drifted replay.
fn validate_approved_reattempt_successor(
    root: &Path,
    task_id: &str,
    reason: &str,
    expected: &ApprovedReattemptOutcome,
) -> Result<()> {
    let round = crate::current_round(root)?;
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let read = orch_core::read_ledger(&ledger_path)?;
    reject_bad_lines(&read)?;
    let current = current_attempt(&read.events, task_id)?
        .context("approved reattempt successor vanished during postcheck")?;
    if current.attempt_id != expected.next_attempt_id {
        bail!(
            "approved reattempt successor drifted: expected {}, current {}",
            expected.next_attempt_id,
            current.attempt_id
        );
    }
    match approved_reattempt_decision(&read.events, &round, task_id, reason)? {
        ApprovedReattemptDecision::AlreadyPrepared {
            previous_attempt_id,
            next_attempt_id,
        } if previous_attempt_id == expected.previous_attempt_id
            && next_attempt_id == expected.next_attempt_id =>
        {
            Ok(())
        }
        ApprovedReattemptDecision::AlreadyPrepared {
            previous_attempt_id,
            next_attempt_id,
        } => bail!(
            "approved reattempt lineage drifted: expected {}->{}, found {}->{}",
            expected.previous_attempt_id,
            expected.next_attempt_id,
            previous_attempt_id,
            next_attempt_id
        ),
        ApprovedReattemptDecision::Append { .. } => {
            bail!("approved reattempt successor advanced again before postcheck")
        }
    }
}

/// Execute the complete approved-reattempt transition under one protocol
/// exclusive lease. Tier-F owns the nested capacity lease; its deterministic
/// hook publishes `AttemptBlocked + DispatchIssued` as one batch, after which
/// the ordinary Existing path performs wake completion and this wrapper
/// validates the exact successor.
pub fn run_approved_reattempt_dispatch(
    root: &Path,
    task_id: &str,
    no_wake: bool,
    override_ambiguous_active: bool,
    reason: &str,
) -> Result<(String, String, ApprovedReattemptOutcome)> {
    crate::card::validate_task_id(task_id)?;
    let reason = reason.trim().to_string();
    if reason.is_empty() {
        bail!("approved reattempt requires a non-empty reason");
    }
    crate::close::with_protocol_transition_named(
        root,
        "orch dispatch --new-attempt",
        Some(task_id),
        || {
            let _explicit = ApprovedReattemptExplicitScope::enter(task_id, &reason)?;
            let prepared = std::cell::RefCell::new(None::<ApprovedReattemptOutcome>);
            let (agent, base) = crate::tierf::run_dispatch_with_hook_and_override(
                root,
                task_id,
                no_wake,
                override_ambiguous_active,
                |was_existing, _iteration| {
                    let observed =
                        prepare_approved_reattempt_locked(root, task_id, &reason, was_existing)?;
                    let mut prepared = prepared.borrow_mut();
                    if let Some(expected) = prepared.as_ref() {
                        if expected.previous_attempt_id != observed.previous_attempt_id
                            || expected.next_attempt_id != observed.next_attempt_id
                        {
                            bail!(
                                "approved reattempt changed lineage inside dispatch: {}->{} became {}->{}",
                                expected.previous_attempt_id,
                                expected.next_attempt_id,
                                observed.previous_attempt_id,
                                observed.next_attempt_id
                            );
                        }
                    } else {
                        *prepared = Some(observed);
                    }
                    Ok(())
                },
            )?;
            let prepared = prepared
                .into_inner()
                .context("approved reattempt dispatch never reached its capacity-locked hook")?;
            validate_approved_reattempt_successor(root, task_id, &reason, &prepared)?;
            Ok((agent, base, prepared))
        },
    )
}

// ─────────────────────────── dispatch locked plan (single-flight) ───────────────────────────

/// 从一条 DispatchIssued 事件的 payload 单行自身解析完整派发记录。
/// 不依赖 task 最后一条，而是按 event_id 精确定位。
pub struct DispatchRecord {
    pub attempt: AttemptRef,
    pub agent: String,
    pub base_sha: Option<String>,
    pub go_path: String,
    pub is_legacy: bool,
    pub previous_attempt_id: Option<String>,
    pub previous_agent: Option<String>,
    pub reassignment: bool,
    pub has_companion_reassignment: bool,
    pub wake_pending: bool,
    pub wake_completed: bool,
}

/// Stable idempotency identity for one dispatch wake action.  It deliberately
/// excludes mutable filesystem facts and includes every immutable dispatch
/// fact that the delivery/completion replay validates.
pub fn dispatch_wake_action_id(
    round: &str,
    task: &str,
    attempt: &AttemptRef,
    agent: &str,
    base_sha: &str,
    go_path: &str,
) -> String {
    let material = format!(
        "dispatch-wake\0{round}\0{task}\0{}\0{}\0{agent}\0{base_sha}\0{go_path}",
        attempt.attempt_id, attempt.ordinal
    );
    hex::encode(Sha256::digest(material.as_bytes()))
}

/// 从一条 DispatchIssued 事件解析其派发记录。该 event 必须是 DispatchIssued。
pub fn dispatch_record_for_attempt(
    ev: &EventRecord,
    derived: &AttemptRef,
) -> Result<DispatchRecord> {
    dispatch_record_for_attempt_mode(ev, derived, true)
}

fn dispatch_record_for_attempt_mode(
    ev: &EventRecord,
    derived: &AttemptRef,
    require_complete_modern: bool,
) -> Result<DispatchRecord> {
    if ev.kind != "DispatchIssued" {
        bail!(
            "dispatch_record_for_attempt: 事件 {:?} 不是 DispatchIssued",
            ev.kind
        );
    }
    let task = ev.task_id.as_ref().context("DispatchIssued 缺 task_id")?;
    if task != &derived.task_id {
        bail!(
            "DispatchIssued task {:?} != derived task {:?}",
            task,
            derived.task_id
        );
    }
    let p = ev.payload.as_ref().context("DispatchIssued 缺 payload")?;
    let agent = p
        .get("agent")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(String::from)
        .context("DispatchIssued 缺非空 agent")?;
    let base_sha = match p.get("baseSha") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .filter(|text| !text.is_empty())
                .context("DispatchIssued baseSha 必须是非空 string")?
                .to_string(),
        ),
    };
    let identity = dispatch_identity(ev, derived)?;
    let round = ev.round.as_deref().context("DispatchIssued 缺 round")?;
    let (attempt, go_path, is_legacy) = match identity {
        DispatchIdentity::Modern => {
            let attempt_no = p.get("attemptNo").and_then(serde_json::Value::as_u64);
            let go_path = p
                .get("goPath")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty());
            if require_complete_modern {
                if attempt_no != Some(derived.ordinal as u64) {
                    bail!(
                        "modern DispatchIssued attemptNo {:?} != derived {}",
                        attempt_no,
                        derived.ordinal
                    );
                }
                let canonical = format!(
                    "coordination/rounds/{round}/dispatch/{agent}/GO-{}.md",
                    derived.attempt_id
                );
                if go_path != Some(canonical.as_str()) {
                    bail!(
                        "modern DispatchIssued goPath {:?} != canonical {:?}",
                        go_path,
                        canonical
                    );
                }
            }
            (
                derived.clone(),
                go_path.map(str::to_owned).unwrap_or_else(|| {
                    format!("coordination/rounds/{round}/dispatch/{agent}/GO-{task}.md")
                }),
                false,
            )
        }
        DispatchIdentity::Legacy => (
            derived.clone(),
            format!("coordination/rounds/{round}/dispatch/{agent}/GO-{task}.md"),
            true,
        ),
    };
    let optional_nonempty_string = |key: &str| -> Result<Option<String>> {
        match p.get(key) {
            None => Ok(None),
            Some(value) => value
                .as_str()
                .filter(|text| !text.is_empty())
                .map(|text| Some(text.to_string()))
                .with_context(|| format!("DispatchIssued {key} 必须是非空 string")),
        }
    };
    let previous_attempt_id = optional_nonempty_string("previousAttemptId")?;
    let previous_agent = optional_nonempty_string("previousAgent")?;
    let reassignment = match p.get("reassignment") {
        None => false,
        Some(value) => value
            .as_bool()
            .context("DispatchIssued reassignment 必须是 bool")?,
    };
    let wake_pending = match p.get("wakePending") {
        None => false,
        Some(value) => value
            .as_bool()
            .context("DispatchIssued wakePending 必须是 bool")?,
    };
    Ok(DispatchRecord {
        attempt,
        agent,
        base_sha,
        go_path,
        is_legacy,
        previous_attempt_id,
        previous_agent,
        reassignment,
        has_companion_reassignment: false, // filled by caller
        wake_pending,
        wake_completed: false, // filled by caller
    })
}

/// 锁内 dispatch 计划。要么新建（New），要么复用已有 active dispatch（Existing）。
pub enum DispatchPlan {
    /// 需要新建 attempt：写 GO、落 DispatchIssued（+可选 companion）。
    New {
        next: NextAttemptPlan,
        /// 上一 attempt 的 DispatchRecord（如有，用于 archive）
        previous_dispatch: Option<DispatchRecord>,
    },
    /// 已有 active DispatchIssued 与请求 agent 一致：复用 GO，可能补 companion。
    Existing {
        current: DispatchRecord,
        /// 缺失 companion ReassignmentIssued 但 DispatchIssued 标记了 reassignment=true
        needs_companion: bool,
        previous_attempt_id: Option<String>,
        previous_agent: Option<String>,
    },
}

/// 在 ledger.lock 内调用的纯账本推理：决定是 New dispatch 还是 Existing 复用。
/// 并发第二个调用进锁后只能看到已有 DispatchIssued，走 Existing 路径。
/// Existing 仅接受当前 active（未终态）DispatchIssued 与请求 agent 一致。
pub fn plan_dispatch_locked(
    events: &[EventRecord],
    task: &str,
    requested_agent: &str,
    concrete_main: &str,
    round: &str,
) -> Result<DispatchPlan> {
    let current = current_attempt(events, task)?;
    let Some(current_attempt) = current else {
        // 无任何 DispatchIssued — 首次派发
        let plan = NextAttemptPlan {
            attempt: AttemptRef {
                task_id: task.to_string(),
                ordinal: 1,
                attempt_id: format_attempt_id(task, 1),
            },
            base_sha: concrete_main.to_string(),
            branch: format!("task/{task}"),
            worktree_rel: format!(".worktrees/{task}"),
            previous_attempt: None,
            previous_agent: None,
            is_reassignment: false,
        };
        return Ok(DispatchPlan::New {
            next: plan,
            previous_dispatch: None,
        });
    };

    // 找到当前 attempt 的 DispatchIssued 事件（最后一条）及其 ledger index。
    let (last_dispatch_index, last_dispatch_ev) = events
        .iter()
        .enumerate()
        .rev()
        .find(|(_, ev)| ev.kind == "DispatchIssued" && ev.task_id.as_deref() == Some(task))
        .context("current_attempt 有序号但无 DispatchIssued 事件")?;
    if last_dispatch_ev.round.as_deref() != Some(round) {
        bail!(
            "DispatchIssued top-level round {:?} != current round {round:?}",
            last_dispatch_ev.round
        );
    }

    // 如果最近 attempt 仍活跃（未终态）⇒ Existing 复用
    if !latest_attempt_is_terminal(events, task) {
        let mut rec = dispatch_record_for_attempt(last_dispatch_ev, &current_attempt)?;
        // agent 必须一致
        if rec.agent != requested_agent {
            bail!(
                "已有 active DispatchIssued 的 agent {} 与请求 agent {} 不一致，拒绝并发派发 fail-closed",
                rec.agent, requested_agent
            );
        }
        // C: 每次枚举当前 attempt 的 ReassignmentIssued companion
        // Enumerate the whole current attempt segment first.  Do not filter by
        // task/round/payload, because that would turn malformed companions into
        // an apparent zero-companion repair case.
        let segment = &events[last_dispatch_index + 1..];
        let companions: Vec<&EventRecord> = segment
            .iter()
            .filter(|event| {
                event.kind == "ReassignmentIssued"
                    && (event.task_id.as_deref() == Some(task)
                        || event
                            .payload
                            .as_ref()
                            .and_then(|payload| payload.get("attemptId"))
                            .and_then(serde_json::Value::as_str)
                            == Some(rec.attempt.attempt_id.as_str()))
            })
            .collect();
        // C: >1 companion = fail
        if companions.len() > 1 {
            bail!(
                "Existing dispatch: companion 重复 ({} 条)，fail-closed",
                companions.len()
            );
        }
        // C: 1 companion must verify all 7 fields exact match
        if companions.len() == 1 {
            let companion = companions[0];
            if companion.task_id.as_deref() != Some(task) {
                bail!("Existing: companion top-level task mismatch");
            }
            if companion.round.as_deref() != Some(round) {
                bail!("Existing: companion top-level round mismatch");
            }
            let ep = companion
                .payload
                .as_ref()
                .context("Existing: companion 缺 payload")?;
            let ex_prev_aid = ep
                .get("previousAttemptId")
                .and_then(serde_json::Value::as_str);
            let ex_prev_ag = ep.get("previousAgent").and_then(serde_json::Value::as_str);
            let ex_aid = ep.get("attemptId").and_then(serde_json::Value::as_str);
            let ex_ag = ep.get("agent").and_then(serde_json::Value::as_str);
            let ex_no = ep.get("attemptNo").and_then(serde_json::Value::as_u64);
            if ex_aid != Some(&rec.attempt.attempt_id) {
                bail!("Existing: companion attemptId mismatch");
            }
            if ex_ag != Some(&rec.agent) {
                bail!("Existing: companion agent mismatch");
            }
            if ex_no.map(|n| n as usize) != Some(rec.attempt.ordinal) {
                bail!("Existing: companion attemptNo mismatch");
            }
            if ex_prev_aid != rec.previous_attempt_id.as_deref() {
                bail!("Existing: companion previousAttemptId mismatch");
            }
            if ex_prev_ag != rec.previous_agent.as_deref() {
                bail!("Existing: companion previousAgent mismatch");
            }
        }
        if !rec.reassignment && !companions.is_empty() {
            bail!("Existing: 非 reassignment attempt 出现 companion");
        }
        if rec.reassignment && (rec.previous_attempt_id.is_none() || rec.previous_agent.is_none()) {
            bail!("Existing: reassignment=true 但 previous identity 不完整");
        }
        let needs_companion = rec.reassignment && companions.is_empty();
        let prev_aid = rec.previous_attempt_id.clone();
        let prev_agent = rec.previous_agent.clone();
        rec.has_companion_reassignment = !needs_companion && rec.reassignment;
        let wake_events: Vec<&EventRecord> = segment
            .iter()
            .filter(|event| {
                event.kind == "DispatchWakeCompleted" && event.task_id.as_deref() == Some(task)
            })
            .collect();
        if wake_events.len() > 1 {
            bail!("Existing: DispatchWakeCompleted 重复");
        }
        if let Some(event) = wake_events.first() {
            if event.task_id.as_deref() != Some(task) || event.round.as_deref() != Some(round) {
                bail!("Existing: DispatchWakeCompleted top-level identity mismatch");
            }
            let payload = event
                .payload
                .as_ref()
                .context("DispatchWakeCompleted 缺 payload")?;
            let base_sha = rec
                .base_sha
                .as_deref()
                .context("DispatchWakeCompleted 对应 dispatch 缺 baseSha")?;
            let expected_action = dispatch_wake_action_id(
                round,
                task,
                &rec.attempt,
                &rec.agent,
                base_sha,
                &rec.go_path,
            );
            if payload.get("attemptId").and_then(serde_json::Value::as_str)
                != Some(rec.attempt.attempt_id.as_str())
                || payload.get("attemptNo").and_then(serde_json::Value::as_u64)
                    != Some(rec.attempt.ordinal as u64)
                || payload.get("agent").and_then(serde_json::Value::as_str)
                    != Some(rec.agent.as_str())
                || payload.get("baseSha").and_then(serde_json::Value::as_str) != Some(base_sha)
                || payload.get("goPath").and_then(serde_json::Value::as_str)
                    != Some(rec.go_path.as_str())
                || payload.get("actionId").and_then(serde_json::Value::as_str)
                    != Some(expected_action.as_str())
            {
                bail!("Existing: DispatchWakeCompleted payload identity mismatch");
            }
            rec.wake_completed = true;
        }
        return Ok(DispatchPlan::Existing {
            current: rec,
            needs_companion,
            previous_attempt_id: prev_aid,
            previous_agent: prev_agent,
        });
    }

    // 最近 attempt 已终态 ⇒ takeover，规划下一个
    require_explicit_approved_reattempt_successor(events, task)?;
    let plan = plan_next_attempt(events, task, requested_agent, concrete_main, round)?;
    // 解析上一 attempt 的 DispatchRecord（用于 archive）
    let previous_attempt = plan
        .previous_attempt
        .as_ref()
        .context("terminal takeover 缺 previous attempt")?;
    let previous_ev = events
        .iter()
        .filter(|event| event.kind == "DispatchIssued" && event.task_id.as_deref() == Some(task))
        .nth(previous_attempt.ordinal.saturating_sub(1))
        .context("terminal takeover 缺 previous DispatchIssued")?;
    let previous_dispatch = Some(dispatch_record_for_attempt(previous_ev, previous_attempt)?);
    Ok(DispatchPlan::New {
        next: plan,
        previous_dispatch,
    })
}

// ─────────────────────────── superseded GO path resolution ───────────────────────────

/// 从上一条 DispatchIssued 的 payload.goPath 解析实际归档源路径。
/// 使用 resolve_go_path_strict 进行 byte-exact 验证。
/// 支持 legacy GO-<task>.md（仅当 DispatchIssued 确实 legacy 无 attemptId）和现代 GO-<attemptId>.md。
/// 缺 goPath 时在 scoped/legacy 候选中 exactly-one；0/2 fail。
/// 返回 absolute GO path (root.join)。
pub fn resolve_superseded_go(
    root: &std::path::Path,
    round: &str,
    previous: &DispatchRecord,
) -> Result<String> {
    let abs = if previous.is_legacy {
        // 历史 legacy 行不携带 goPath。按 schema（legacy basename / scoped basename）
        // 各自折叠 live source + archived destination 为一个 logical candidate，
        // 再做全局 exactly-one：0 个 fail；2 个（跨 schema 共存）fail-closed；
        // 唯一 candidate 返回该 schema 的 canonical source rel——destination-only
        // 时 archive 流程判定 AlreadyDone，scoped-destination-only 必须返回
        // scoped basename；同 schema live+destination 并存时返回 source rel 由
        // archive 状态机判 GoCollision fail-closed。
        let legacy_rel = format!(
            "coordination/rounds/{round}/dispatch/{}/GO-{}.md",
            previous.agent, previous.attempt.task_id
        );
        let scoped_rel = format!(
            "coordination/rounds/{round}/dispatch/{}/GO-{}.md",
            previous.agent, previous.attempt.attempt_id
        );
        let mut logical: Vec<String> = Vec::new();
        for (candidate, legacy) in [(&legacy_rel, true), (&scoped_rel, false)] {
            let path = resolve_go_source_schema(
                root,
                candidate,
                round,
                &previous.agent,
                &previous.attempt,
                legacy,
            )?;
            let live = match std::fs::symlink_metadata(&path) {
                Ok(meta) if meta.file_type().is_file() && !meta.file_type().is_symlink() => true,
                Ok(_) => bail!(
                    "legacy archive candidate 必须是 regular non-symlink: {}",
                    path.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(error.into()),
            };
            let destination = path
                .parent()
                .context("legacy archive candidate 无父目录")?
                .join("superseded")
                .join(previous.attempt.attempt_id.as_str())
                .join(path.file_name().unwrap_or_default());
            let archived = match std::fs::symlink_metadata(&destination) {
                Ok(meta) if meta.file_type().is_file() && !meta.file_type().is_symlink() => true,
                Ok(_) => bail!(
                    "legacy archive destination 必须是 regular non-symlink: {}",
                    destination.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(error.into()),
            };
            if live || archived {
                logical.push(candidate.clone());
            }
        }
        match logical.len() {
            1 => root.join(&logical[0]),
            0 => bail!(
                "legacy logical GO candidate 0 个（live source 与 archived destination 均缺）"
            ),
            _ => bail!(
                "legacy/scoped 跨 schema GO candidate 共存（{} 个），拒绝消歧 fail-closed",
                logical.len()
            ),
        }
    } else {
        resolve_go_source_schema(
            root,
            &previous.go_path,
            round,
            &previous.agent,
            &previous.attempt,
            false,
        )?
    };
    Ok(abs
        .strip_prefix(root)
        .unwrap_or(&abs)
        .to_string_lossy()
        .to_string())
}

// ─────────────────────────── GO render + reconcile ───────────────────────────

/// 渲染 GO 文本为 bytes。纯函数，便于测试。
pub fn render_attempt_go(
    task_id: &str,
    agent: &str,
    base_short: &str,
    round: &str,
    report_rel: &str,
    worktree_step: &str,
) -> Vec<u8> {
    format!(
        "# GO {id} —— 已放行（{agent}）\n\n\
         基线：base = `{short}`（继承首个 DispatchIssued 的 concrete SHA）。\n\
         任务卡：coordination/rounds/{round}/tasks/{id}.md ——先完整读卡。\n\n\
         要点：\n\
         {wt_step}\n\
         2. 第一个 commit 只做种子逐字节搬运；随后取红证据 → 实现转绿 → 快门 → 负向变异。\n\
         3. REPORT 是最后一个动作：写（worktree 内的）{report} 并 commit。\n\
         4. 写完后重新运行 coordination/scripts/wait-dispatch.sh {agent} 等待下一指令。\n",
        id = task_id,
        agent = agent,
        short = base_short,
        round = round,
        report = report_rel,
        wt_step = worktree_step,
    )
    .into_bytes()
}

/// 用 create_new 创建 GO 文件。如果文件已存在：
/// - 逐字节与 expected 对比：相同是 append 失败的 orphan，可复用且不重写/不改 mtime；
/// - 不同则 fail。
/// 绝对禁止删除 orphan GO。
pub fn reconcile_or_create_go(go_path: &Path, expected: &[u8]) -> Result<()> {
    use std::io::Write;
    let go_dir = go_path.parent().context("GO 路径无父目录")?;
    std::fs::create_dir_all(go_dir)?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(go_path)
    {
        Ok(mut f) => {
            f.write_all(expected)?;
            f.sync_all()?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // GO 已存在——逐字节对比
            let existing = std::fs::read(go_path)
                .with_context(|| format!("读取已存在 GO 失败: {}", go_path.display()))?;
            if existing.as_slice() == expected {
                // orphan GO（append 失败的残留），可复用，不重写/不改 mtime
                Ok(())
            } else {
                bail!(
                    "GO 已存在但内容不同（冲突），拒绝覆盖: {}",
                    go_path.display()
                )
            }
        }
        Err(e) => Err(e).with_context(|| format!("创建 GO 失败: {}", go_path.display())),
    }
}

// ─────────────────────────── archive: shared 4-path state machine ───────────────────────────

/// 归档路径状态机：4 个布尔值（go_src, ack_src, go_dst, ack_dst）映射到唯一 action。
/// preflight 和 mutator 共用同一分类逻辑。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveAction {
    /// GO src 存在, dst 不存在 → 正常归档 GO + 可选 ack
    MoveGoAndAck,
    /// GO src 存在、dst 不存在，而 ack 已在 destination 或不存在。
    MoveGoOnly,
    /// GO src 缺失, dst 存在, ack src 存在, ack dst 不存在 → 只补搬 ack
    MoveAckOnly,
    /// GO src 缺失, dst 存在, ack 不存在或已归档 → 幂等已完成
    AlreadyDone,
    /// hard-link 已创建但 source unlink 未完成；安全重验 inode 后补 unlink。
    RecoverSameInodePartial,
    /// GO 两端都缺 → fail
    BothGoMissing,
    /// GO 缺失但 ack 存在 → fail
    GoMissingAckPresent,
    /// GO source + dst 同时存在 → collision
    GoCollision,
    /// ack source + dst 同时存在 → collision
    AckCollision,
}

#[derive(Debug)]
struct ArchivePaths {
    go_src: std::path::PathBuf,
    ack_src: std::path::PathBuf,
    go_dst: std::path::PathBuf,
    ack_dst: std::path::PathBuf,
    archive_dir: std::path::PathBuf,
    action: ArchiveAction,
    ack_at_source: bool,
    ack_same_inode: bool,
}

fn regular_slot(path: &std::path::Path, label: &str) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() && !meta.file_type().is_symlink() => Ok(true),
        Ok(meta) => bail!(
            "archive {label} 必须是 regular non-symlink，实际 type={:?}: {}",
            meta.file_type(),
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("archive stat {label} 失败: {}", path.display()))
        }
    }
}

#[cfg(unix)]
fn same_regular_object(a: &std::path::Path, b: &std::path::Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let a_meta = std::fs::symlink_metadata(a)?;
    let b_meta = std::fs::symlink_metadata(b)?;
    Ok(a_meta.file_type().is_file()
        && !a_meta.file_type().is_symlink()
        && b_meta.file_type().is_file()
        && !b_meta.file_type().is_symlink()
        && a_meta.dev() == b_meta.dev()
        && a_meta.ino() == b_meta.ino())
}

#[cfg(not(unix))]
fn same_regular_object(_a: &std::path::Path, _b: &std::path::Path) -> Result<bool> {
    Ok(false)
}

/// ack 授权的稳定对象身份：dev/ino（unix）+ 长度 + 内容 digest。
/// partial recovery 在 unlink source 前捕获；GO mutation 前重验 destination
/// 仍是同一对象（未变 → 放行；被替换或消失 → fail-closed，GO 保持 live）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectFingerprint {
    dev: u64,
    ino: u64,
    len: u64,
    sha256: String,
}

fn fingerprint_regular(path: &std::path::Path) -> Result<ObjectFingerprint> {
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("fingerprint stat 失败: {}", path.display()))?;
    if !meta.file_type().is_file() || meta.file_type().is_symlink() {
        bail!(
            "fingerprint 目标必须是 regular non-symlink: {}",
            path.display()
        );
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("fingerprint read 失败: {}", path.display()))?;
    #[cfg(unix)]
    let (dev, ino) = {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino())
    };
    #[cfg(not(unix))]
    let (dev, ino) = (0u64, 0u64);
    Ok(ObjectFingerprint {
        dev,
        ino,
        len: meta.len(),
        sha256: hex::encode(Sha256::digest(&bytes)),
    })
}

fn unlink_same_inode_source(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    if !same_regular_object(src, dst)? {
        bail!(
            "archive partial recovery inode changed: {} != {}",
            src.display(),
            dst.display()
        );
    }
    std::fs::remove_file(src)
        .with_context(|| format!("archive partial source unlink 失败: {}", src.display()))
}

/// 归档预检（只读）：在 mutate 前验证 4 路径状态。
pub fn archive_preflight(
    root: &std::path::Path,
    go_rel: &str,
    previous_attempt_id: &str,
    previous_agent: &str,
) -> Result<()> {
    let paths = archive_compute_paths(root, go_rel, previous_attempt_id, previous_agent)?;
    match paths.action {
        ArchiveAction::MoveGoAndAck
        | ArchiveAction::MoveGoOnly
        | ArchiveAction::MoveAckOnly
        | ArchiveAction::AlreadyDone
        | ArchiveAction::RecoverSameInodePartial => Ok(()),
        ArchiveAction::BothGoMissing => {
            bail!("archive preflight: GO 两端都缺（显式 goPath 但文件不存在），fail-closed")
        }
        ArchiveAction::GoMissingAckPresent => {
            bail!("archive preflight: GO 缺失但 ack 存在（不可能态），fail-closed")
        }
        ArchiveAction::GoCollision => {
            bail!("archive preflight: GO source 和 destination 同时存在 (collision)，fail-closed")
        }
        ArchiveAction::AckCollision => {
            bail!("archive preflight: ack source 和 destination 同时存在 (collision)，fail-closed")
        }
    }
}

/// 计算 archive 路径和 action（共享逻辑）。
fn archive_compute_paths(
    root: &std::path::Path,
    go_rel: &str,
    previous_attempt_id: &str,
    previous_agent: &str,
) -> Result<ArchivePaths> {
    archive_compute_paths_recovery(root, go_rel, previous_attempt_id, previous_agent, false)
}

fn archive_compute_paths_recovery(
    root: &std::path::Path,
    go_rel: &str,
    previous_attempt_id: &str,
    previous_agent: &str,
    allow_verified_ack_destination: bool,
) -> Result<ArchivePaths> {
    // 从 go_rel 解析 round（格式 coordination/rounds/<round>/dispatch/...），
    // 避免依赖 CURRENT-ROUND 文件（archive 流程可独立于 runtime 当前轮）。
    let round = go_rel
        .split('/')
        .nth(2)
        .context("archive go_rel 缺 round 段")?;
    let (task, suffix) = previous_attempt_id
        .rsplit_once("-A")
        .context("archive previousAttemptId 格式无效")?;
    if task.is_empty() || suffix.len() != 4 || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("archive previousAttemptId 格式无效: {previous_attempt_id}");
    }
    for (label, value) in [
        ("round", round),
        ("agent", previous_agent),
        ("task", task),
        ("attemptId", previous_attempt_id),
    ] {
        if value.is_empty()
            || value == "."
            || value == ".."
            || value.contains('/')
            || value.contains('\\')
        {
            bail!("archive {label} 不是安全的单路径组件: {value:?}");
        }
    }
    let modern_rel = format!(
        "coordination/rounds/{round}/dispatch/{previous_agent}/GO-{previous_attempt_id}.md"
    );
    let legacy_rel = format!("coordination/rounds/{round}/dispatch/{previous_agent}/GO-{task}.md");
    let modern_abs = root.join(&modern_rel);
    let legacy_abs = root.join(&legacy_rel);
    let is_modern = go_rel == modern_rel || go_rel == modern_abs.to_string_lossy();
    let is_legacy = go_rel == legacy_rel || go_rel == legacy_abs.to_string_lossy();
    if usize::from(is_modern) + usize::from(is_legacy) != 1 {
        bail!("archive GO path 非 byte-exact modern/legacy expected source: {go_rel:?}");
    }
    let go_src = if is_modern { modern_abs } else { legacy_abs };
    validate_parent_chain(root, &go_src)?;
    let mut ack_os = go_src.as_os_str().to_os_string();
    ack_os.push(".ack");
    let ack_src = std::path::PathBuf::from(ack_os);
    let archive_dir = root
        .join("coordination/rounds")
        .join(&round)
        .join("dispatch")
        .join(previous_agent)
        .join("superseded")
        .join(previous_attempt_id);
    validate_parent_chain(root, &archive_dir.join("slot"))?;
    match std::fs::symlink_metadata(&archive_dir) {
        Ok(meta) if meta.file_type().is_dir() && !meta.file_type().is_symlink() => {}
        Ok(_) => bail!(
            "archive destination directory 必须是真实目录: {}",
            archive_dir.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "stat archive destination directory 失败: {}",
                    archive_dir.display()
                )
            })
        }
    }
    let go_name = go_src
        .file_name()
        .with_context(|| format!("GO 路径无文件名: {go_rel}"))?;
    let go_dst = archive_dir.join(go_name);
    let ack_dst = archive_dir.join(format!("{}.ack", go_name.to_string_lossy()));
    let go_src_regular = regular_slot(&go_src, "GO source")?;
    let go_dst_regular = regular_slot(&go_dst, "GO destination")?;
    let ack_src_regular = regular_slot(&ack_src, "ack source")?;
    let ack_dst_regular = regular_slot(&ack_dst, "ack destination")?;
    let go_same = go_src_regular && go_dst_regular && same_regular_object(&go_src, &go_dst)?;
    let ack_same = ack_src_regular && ack_dst_regular && same_regular_object(&ack_src, &ack_dst)?;
    let action = if go_src_regular && go_dst_regular && !go_same {
        ArchiveAction::GoCollision
    } else if ack_src_regular && ack_dst_regular && !ack_same {
        ArchiveAction::AckCollision
    } else if go_same || ack_same {
        ArchiveAction::RecoverSameInodePartial
    } else {
        match (go_src_regular, go_dst_regular) {
            (false, false) => ArchiveAction::BothGoMissing,
            (true, true) => unreachable!("same/different inode handled above"),
            (true, false) => {
                if ack_src_regular {
                    ArchiveAction::MoveGoAndAck
                } else if ack_dst_regular && !allow_verified_ack_destination {
                    // A destination-only ack is not proof that it belongs to
                    // this source.  Only the local continuation immediately
                    // after verifying and unlinking a same-inode partial may
                    // finish the GO move.
                    ArchiveAction::AckCollision
                } else {
                    ArchiveAction::MoveGoOnly
                }
            }
            (false, true) => {
                if ack_src_regular {
                    ArchiveAction::MoveAckOnly
                } else {
                    ArchiveAction::AlreadyDone
                }
            }
        }
    };
    Ok(ArchivePaths {
        go_src,
        ack_src,
        go_dst,
        ack_dst,
        archive_dir,
        action,
        ack_at_source: ack_src_regular,
        ack_same_inode: ack_same,
    })
}

fn move_regular_no_clobber(
    src: &std::path::Path,
    dst: &std::path::Path,
    hook: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<()> {
    // hard_link is create-new/no-clobber on the same filesystem.  Removing the
    // source only after the link succeeds prevents rename(2)'s overwrite
    // behaviour from destroying an independently-created destination.
    // This hook is at the actual fresh-check -> hard_link no-clobber window.
    hook("before-hard-link")?;
    move_regular_no_clobber_after_hook(src, dst)
}

fn move_regular_no_clobber_after_hook(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    std::fs::hard_link(src, dst).with_context(|| {
        format!(
            "archive no-clobber link 失败: {} -> {}",
            src.display(),
            dst.display()
        )
    })?;
    if let Err(error) = std::fs::remove_file(src) {
        // Keep the same-inode two-link partial state. A retry recognizes it
        // and safely finishes the source unlink; deleting dst here would make
        // a cleanup failure ambiguous and potentially lose the recoverable
        // destination.
        return Err(error)
            .with_context(|| format!("archive source remove 失败: {}", src.display()));
    }
    Ok(())
}

/// 归档旧 attempt 的 GO（与可选 ack）到 `superseded/<previousAttemptId>/` 下。
/// 使用与 archive_preflight 相同的 4 路径状态机。
pub fn archive_superseded_dispatch(
    root: &std::path::Path,
    go_rel: &str,
    previous_attempt_id: &str,
    previous_agent: &str,
) -> Result<()> {
    archive_superseded_dispatch_with_hook(
        root,
        go_rel,
        previous_attempt_id,
        previous_agent,
        &mut |_| Ok(()),
    )
}

#[doc(hidden)]
pub fn archive_superseded_dispatch_with_hook(
    root: &std::path::Path,
    go_rel: &str,
    previous_attempt_id: &str,
    previous_agent: &str,
    hook: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<()> {
    archive_superseded_dispatch_inner(
        root,
        go_rel,
        previous_attempt_id,
        previous_agent,
        None,
        hook,
    )
}

fn archive_superseded_dispatch_inner(
    root: &std::path::Path,
    go_rel: &str,
    previous_attempt_id: &str,
    previous_agent: &str,
    verified_ack: Option<ObjectFingerprint>,
    hook: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<()> {
    // Revalidate immediately before mutation.  No invalid type/collision has
    // created the destination directory at this point.
    let pre = archive_compute_paths_recovery(
        root,
        go_rel,
        previous_attempt_id,
        previous_agent,
        verified_ack.is_some(),
    )?;
    match pre.action {
        ArchiveAction::AlreadyDone => Ok(()),
        ArchiveAction::RecoverSameInodePartial => {
            hook("before-partial-recovery")?;
            let fresh = archive_compute_paths_recovery(
                root,
                go_rel,
                previous_attempt_id,
                previous_agent,
                verified_ack.is_some(),
            )?;
            if fresh.action != ArchiveAction::RecoverSameInodePartial {
                bail!("archive state changed before partial recovery");
            }
            // R6：在 unlink source 前捕获 ack 对象的稳定身份（dev/ino/len/sha256），
            // 授权后续 MoveGoOnly 在 GO mutation 前重验 destination ack 未被替换。
            let recovered_ack_fingerprint = if fresh.ack_same_inode {
                Some(fingerprint_regular(&fresh.ack_src)?)
            } else {
                None
            };
            if regular_slot(&fresh.go_src, "GO source")?
                && regular_slot(&fresh.go_dst, "GO destination")?
            {
                unlink_same_inode_source(&fresh.go_src, &fresh.go_dst)?;
            }
            if regular_slot(&fresh.ack_src, "ack source")?
                && regular_slot(&fresh.ack_dst, "ack destination")?
            {
                unlink_same_inode_source(&fresh.ack_src, &fresh.ack_dst)?;
            }
            archive_superseded_dispatch_inner(
                root,
                go_rel,
                previous_attempt_id,
                previous_agent,
                recovered_ack_fingerprint,
                hook,
            )
        }
        ArchiveAction::MoveAckOnly => {
            std::fs::create_dir_all(&pre.archive_dir)?;
            hook("before-move")?;
            let fresh = archive_compute_paths_recovery(
                root,
                go_rel,
                previous_attempt_id,
                previous_agent,
                verified_ack.is_some(),
            )?;
            if fresh.action != ArchiveAction::MoveAckOnly {
                bail!("archive state changed before MoveAckOnly");
            }
            move_regular_no_clobber(&fresh.ack_src, &fresh.ack_dst, hook)?;
            Ok(())
        }
        ArchiveAction::MoveGoAndAck => {
            std::fs::create_dir_all(&pre.archive_dir)?;
            hook("before-move")?;
            let fresh = archive_compute_paths_recovery(
                root,
                go_rel,
                previous_attempt_id,
                previous_agent,
                verified_ack.is_some(),
            )?;
            if fresh.action != ArchiveAction::MoveGoAndAck {
                bail!("archive state changed before MoveGoAndAck");
            }
            move_regular_no_clobber(&fresh.go_src, &fresh.go_dst, hook)?;
            if fresh.ack_at_source {
                if let Err(error) = move_regular_no_clobber(&fresh.ack_src, &fresh.ack_dst, hook) {
                    let rollback = move_regular_no_clobber(&fresh.go_dst, &fresh.go_src, hook);
                    if let Err(rollback_error) = rollback {
                        bail!(
                            "归档 ack 失败: {error}; GO rollback 也失败: {rollback_error}（GO 残留在 {}）",
                            fresh.go_dst.display()
                        );
                    }
                    return Err(anyhow::anyhow!("归档 ack 失败（GO 已 rollback）: {error}")
                        .context(format!(
                            "ack: {} -> {}",
                            fresh.ack_src.display(),
                            fresh.ack_dst.display()
                        )));
                }
            }
            Ok(())
        }
        ArchiveAction::MoveGoOnly => {
            std::fs::create_dir_all(&pre.archive_dir)?;
            hook("before-move")?;
            let fresh = archive_compute_paths_recovery(
                root,
                go_rel,
                previous_attempt_id,
                previous_agent,
                verified_ack.is_some(),
            )?;
            if fresh.action != ArchiveAction::MoveGoOnly {
                bail!("archive state changed before MoveGoOnly");
            }
            // R6：ack same-inode 授权绑定稳定对象身份（dev/ino/len/sha256）。
            // GO mutation 前重验 destination-only ack 仍是 partial recovery 捕获的
            // 同一对象：未变 → 放行（positive-success）；被替换 / 消失 → 授权失效
            // fail-closed，GO 必须保持 live（replacement-failure）。
            if let Some(ref expected_fingerprint) = verified_ack {
                if !regular_slot(&fresh.ack_dst, "ack destination")? {
                    bail!(
                        "archive 授权的 ack destination 在 GO mutation 前消失，\
                         GO 保持 live fail-closed"
                    );
                }
                let actual_fingerprint = fingerprint_regular(&fresh.ack_dst)?;
                if &actual_fingerprint != expected_fingerprint {
                    bail!(
                        "archive ack 对象身份在 GO mutation 前改变（授权失效），\
                         GO 保持 live fail-closed"
                    );
                }
            }
            // The audit seam is deliberately *inside* MoveGoOnly: the final
            // ack authorization check must happen after the last externally
            // observable hook and immediately before GO's no-clobber link.
            hook("before-hard-link")?;
            if let Some(ref expected_fingerprint) = verified_ack {
                if !regular_slot(&fresh.ack_dst, "ack destination")? {
                    bail!(
                        "archive 授权的 ack destination 在 before-hard-link 后消失，\
                         GO 保持 live fail-closed"
                    );
                }
                let actual_fingerprint = fingerprint_regular(&fresh.ack_dst)?;
                if &actual_fingerprint != expected_fingerprint {
                    bail!(
                        "archive ack 对象身份在 before-hard-link 后改变（授权失效），\
                         GO 保持 live fail-closed"
                    );
                }
            }
            move_regular_no_clobber_after_hook(&fresh.go_src, &fresh.go_dst)
        }
        ArchiveAction::BothGoMissing => {
            bail!("归档失败：GO 两端都缺（显式 goPath 但文件不存在），fail-closed")
        }
        ArchiveAction::GoMissingAckPresent => {
            bail!("归档不安全：GO 缺失但 ack 存在（不可能态），拒绝归档 fail-closed")
        }
        ArchiveAction::GoCollision => {
            bail!("归档冲突：GO source 和 destination 同时存在，拒绝覆盖（保留首份）")
        }
        ArchiveAction::AckCollision => {
            bail!("归档冲突：ack source 和 destination 同时存在")
        }
    }
}

// ─────────────────────────── attempt-scoped paths ───────────────────────────

/// attempt 作用域的路径集。GO/ack 按 attemptId（`GO-<task>-A0001.md[.ack]`），
/// REPORT/BLOCKED 保持 canonical（不按 attempt 区分，REPORT 优先于 BLOCKED）。
pub struct AttemptPaths {
    pub go_rel: String,
    pub ack_rel: String,
    pub report_rel: String,
    pub blocked_rel: String,
}

/// 推导 attempt 作用域路径。round/agent 决定 GO 落点；attempt_id 决定 GO/ack 文件名。
/// REPORT/BLOCKED 保持 canonical，不按 attempt 分文件。
pub fn attempt_paths(round: &str, agent: &str, attempt: &AttemptRef) -> AttemptPaths {
    let go_rel = format!(
        "coordination/rounds/{round}/dispatch/{agent}/GO-{aid}.md",
        aid = attempt.attempt_id,
    );
    let ack_rel = format!("{go_rel}.ack");
    let report_rel = format!(
        "coordination/rounds/{round}/reports/{task}-REPORT.md",
        task = attempt.task_id,
    );
    let blocked_rel = format!(
        "coordination/rounds/{round}/reports/{task}-BLOCKED.md",
        task = attempt.task_id,
    );
    AttemptPaths {
        go_rel,
        ack_rel,
        report_rel,
        blocked_rel,
    }
}

// ─────────────────────────── evidence strictness ───────────────────────────

/// 证据（REPORT/BLOCKED 文件 mtime）是否严格 current：
/// 只有 evidence > marker 才 true；同刻（>=）或更早都 false；
/// evidence 存在但 marker=None（从未派发信号）⇒ false（无对照基线不视为新证据）。
/// 这与 tierf::blocked_is_current 的「marker=None ⇒ 保守 true」不同：本 attempt 契约
/// 要求 attempt-scoped 的严格性，旧证据不得冒充新 attempt 的诚实产物。
pub fn evidence_is_current(
    evidence_modified: Option<std::time::SystemTime>,
    latest_attempt_marker: Option<std::time::SystemTime>,
) -> bool {
    match (evidence_modified, latest_attempt_marker) {
        (Some(ev), Some(marker)) => ev > marker,
        _ => false,
    }
}

/// 计算当前 attempt 的 control marker（亚秒精度，audit item 5）。
/// 账本 ts 截整秒，不能单独用它拒绝同秒旧 REPORT/BLOCKED。取
/// `max(latest_attempt_signal, GO 文件 mtime)` 作为 marker：
/// - latest_attempt_signal 来自最新 DispatchIssued/NudgeIssued/ResumeIssued 的 ts
///   （截整秒，是下界）；
/// - GO 文件真实 mtime 是纳秒精度，是更精确的派发时刻；
/// - 取 max 确保旧证据的 mtime 即使晚于截整秒 event ts、但早于新 GO 文件 mtime
///   时仍被拒绝。
pub fn compute_evidence_marker(
    latest_attempt_signal: Option<std::time::SystemTime>,
    go_file_mtime: Option<std::time::SystemTime>,
) -> Option<std::time::SystemTime> {
    match (latest_attempt_signal, go_file_mtime) {
        (Some(sig), Some(go_mt)) => Some(sig.max(go_mt)),
        (Some(sig), None) => Some(sig),
        (None, Some(go_mt)) => Some(go_mt),
        (None, None) => None,
    }
}

/// 从 ULID event_id 解码毫秒精度时间戳。
/// 使用项目已有的 `ulid` crate 的完整 `Ulid::from_string()` 解析，
/// 拒绝非 26 位 / 非法高位 / 合法前缀垃圾。解析失败返回 None（调用方 fallback 到 RFC3339 ts）。
fn ulid_timestamp(event_id: &str) -> Option<std::time::SystemTime> {
    let ulid = ulid::Ulid::from_string(event_id).ok()?;
    let ms = ulid.timestamp_ms();
    if ms == 0 {
        return None;
    }
    std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_millis(ms))
}

/// 解析 control event (DispatchIssued/NudgeIssued/ResumeIssued) 的高精度时间戳。
/// 优先解码 event_id 的 ULID 时间戳（毫秒精度），解析失败 fallback 到 RFC3339 ts
/// （截整秒，是下界）。这样同秒的 Nudge/Resume 也有高精度。
fn control_event_timestamp(ev: &EventRecord) -> Option<std::time::SystemTime> {
    if let Some(t) = ulid_timestamp(&ev.event_id) {
        return Some(t);
    }
    humantime::parse_rfc3339(&ev.ts).ok()
}

/// 从账本事件流取该 task 最新 control signal (DispatchIssued/NudgeIssued/ResumeIssued)
/// 的高精度时间戳。用 control_event_timestamp（ULID 优先，RFC3339 fallback）。
pub fn latest_attempt_signal_precise(
    events: &[EventRecord],
    task: &str,
) -> Option<std::time::SystemTime> {
    const CONTROL_KINDS: [&str; 3] = ["DispatchIssued", "NudgeIssued", "ResumeIssued"];
    let mut latest: Option<std::time::SystemTime> = None;
    for ev in events {
        if ev.task_id.as_deref() != Some(task) {
            continue;
        }
        if !CONTROL_KINDS.contains(&ev.kind.as_str()) {
            continue;
        }
        if let Some(t) = control_event_timestamp(ev) {
            if latest.map_or(true, |prev| t > prev) {
                latest = Some(t);
            }
        }
    }
    latest
}

// ─────────────────────────── B112：durable attempt elapsed ───────────────────────────

/// 取该 task 最新一条 `DispatchIssued` 事件的 RFC3339 `ts` 字符串（账本原始值，
/// 非重格式化）。`run_await` 用它与 `liveness::attempt_elapsed_from_dispatch`
/// 恢复当前 attempt 的 durable elapsed，使 daemon/await 重启不重置 boot/cold grace。
///
/// 取事件流顺序中最后一条 DispatchIssued（账本为追加语义）。无则 None。
/// 不做 ULID 高精度解码——durable elapsed 用账本 ts（截整秒）作为下界已足够，
/// 与 `latest_attempt_signal_precise` 的 ULID 优先路径互补：后者用于 control marker。
pub fn latest_dispatch_ts(events: &[EventRecord], task: &str) -> Option<String> {
    events
        .iter()
        .rev()
        .find(|ev| ev.kind == "DispatchIssued" && ev.task_id.as_deref() == Some(task))
        .map(|ev| ev.ts.clone())
}

// ─────────────────────────── attempt-scoped idempotency ───────────────────────────

/// 某 attempt 是否已记录指定 kind 事件（ack/terminal 幂等按 attemptId，不按 task）。
/// 只看事件 payload 的 attemptId 字段；未标 attemptId 的事件不参与判定。
pub fn attempt_event_already_recorded(
    events: &[EventRecord],
    attempt_id: &str,
    kind: &str,
) -> bool {
    events.iter().any(|ev| {
        ev.kind == kind
            && ev
                .payload
                .as_ref()
                .and_then(|p| p.get("attemptId"))
                .and_then(serde_json::Value::as_str)
                == Some(attempt_id)
    })
}

// ─────────────────────────── conditional GO worktree instruction (M7) ───────────────────────────

/// GO 提示词中的 worktree 建立指令。固定复用 `task/<ID>` 与 `.worktrees/<ID>`：
/// - 都不存在 ⇒ `git worktree add .worktrees/<ID> -b task/<ID> <baseSha>`（首次建立，-b）；
/// - branch 存在、worktree 不存在 ⇒ `git worktree add .worktrees/<ID> task/<ID>`（已有分支，不加 -b）；
/// - 都存在 ⇒ 接续指令（直接进入 worktree，勿重建）；
/// - worktree 存在但 branch 不存在 ⇒ **fail-closed Err**（worktree 不在预期分支上，不安全）。
///
/// 这是 M7 的真实注入点：条件化渲染「已有 branch/worktree 不再 -b」。纯函数便于
/// 直接单测（不依赖文件系统），run_dispatch 据此渲染 GO 文本。
pub fn go_worktree_instruction(
    task_id: &str,
    base_sha: &str,
    branch_exists: bool,
    worktree_exists: bool,
) -> Result<String> {
    match (branch_exists, worktree_exists) {
        (false, false) => Ok(format!(
            "1. 仓库根执行 `git worktree add .worktrees/{id} -b task/{id} {full}`，之后在 .worktrees/{id} 内工作。",
            id = task_id,
            full = base_sha,
        )),
        (true, false) => Ok(format!(
            "1. 仓库根执行 `git worktree add .worktrees/{id} task/{id}`（分支已存在，不加 -b），之后在 .worktrees/{id} 内工作。",
            id = task_id,
        )),
        (true, true) => Ok(format!(
            "仓库根已存在分支 task/{id} 与 worktree .worktrees/{id}——直接进入 .worktrees/{id} 接续工作（勿重建、勿 -b）。",
            id = task_id,
        )),
        (false, true) => bail!(
            "worktree .worktrees/{id} 存在但分支 task/{id} 不存在——worktree 可能不在预期分支上，拒绝派发 fail-closed",
            id = task_id,
        ),
    }
}

// ─────────────────────────── strict evidence selection ───────────────────────────

/// 在双根候选（main 仓 + worktree）中选首个严格 current 的证据路径。
/// 严格 current：evidence mtime > latest_attempt_marker（同刻/更早/无 marker 都不算）。
/// 与 tierf::blocked_is_current 的「marker=None ⇒ 保守 true」不同——本 attempt 契约
/// 要求 attempt-scoped 严格性，旧证据不得冒充新 attempt 的诚实产物。
/// 无候选 current ⇒ None（调用方应继续轮询，不得用旧证据抵新 attempt）。
pub fn select_current_evidence_path(
    candidates: &[(std::path::PathBuf, Option<std::time::SystemTime>)],
    latest_attempt_marker: Option<std::time::SystemTime>,
) -> Option<std::path::PathBuf> {
    for (path, mtime) in candidates {
        if evidence_is_current(*mtime, latest_attempt_marker) {
            return Some(path.clone());
        }
    }
    None
}

// ─────────────────────────── atomic ack+start event construction ───────────────────────────

/// 在 append_checked 的锁内、基于 fresh 账本快照构造 DispatchAcked + AttemptStarted 事件。
/// 幂等：若该 attemptId 已有 DispatchAcked **且** AttemptStarted ⇒ 返回空 vec（完全幂等）。
/// 若 DispatchAcked 已有但 AttemptStarted 缺失（partial legacy 状态）⇒ 补一个 AttemptStarted，
/// 不静默留空（修复 partial 状态而非忽略）。
/// 旧 attempt 的 ack 不得触发新 attempt 的 Started（按 attemptId 作用域判定）。
///
/// 返回的 vec 可直接交给 ledger::append_checked 的 decide 闭包返回值。
pub fn build_ack_start_events(
    events: &[EventRecord],
    task_id: &str,
    round: &str,
    attempt_id: &str,
    attempt_no: usize,
    agent: &str,
    ack_age_secs: Option<u64>,
) -> Vec<EventRecord> {
    use crate::ledger;
    let acked = attempt_event_already_recorded(events, attempt_id, "DispatchAcked");
    let started = attempt_event_already_recorded(events, attempt_id, "AttemptStarted");
    match (acked, started) {
        (true, true) => Vec::new(), // 完全幂等
        (true, false) => {
            // partial legacy：补 AttemptStarted
            vec![ledger::event(
                "AttemptStarted",
                "runtime:orch",
                Some(task_id),
                Some(round),
                serde_json::json!({
                    "attemptId": attempt_id,
                    "attemptNo": attempt_no,
                    "agent": agent,
                }),
            )]
        }
        (false, _) => {
            // 正常首见 ack：DispatchAcked + AttemptStarted（若 started 已有则不重复）
            let mut events_to_append = vec![ledger::event(
                "DispatchAcked",
                "runtime:orch",
                Some(task_id),
                Some(round),
                serde_json::json!({
                    "agent": agent,
                    "ackAgeSecs": ack_age_secs,
                    "attemptId": attempt_id,
                    "attemptNo": attempt_no,
                }),
            )];
            if !started {
                events_to_append.push(ledger::event(
                    "AttemptStarted",
                    "runtime:orch",
                    Some(task_id),
                    Some(round),
                    serde_json::json!({
                        "attemptId": attempt_id,
                        "attemptNo": attempt_no,
                        "agent": agent,
                    }),
                ));
            }
            events_to_append
        }
    }
}

// ─────────────────────────── non-destructive WIP snapshot ───────────────────────────

/// worktree 的非破坏快照结果。dirty=false 时其余字段无意义（调用方应先判 dirty）。
pub struct WipSnapshot {
    pub dirty: bool,
    pub archive_ref: String,
    pub snapshot_sha: String,
}

/// 协作式 snapshot 锁：create_new 独占，drop 释放。锁定整个捕获-发布窗口，
/// 防止遵守同一锁的 snapshot/writer 交错。它是 advisory/cooperative lock，
/// 不能阻止任意外部进程直接改 worktree 或 index。
struct SnapshotLock(std::path::PathBuf);

impl SnapshotLock {
    fn acquire(root: &Path, task: &str) -> Result<Self> {
        if task.is_empty() || task.contains('/') || task.contains('\\') || task == ".." {
            bail!("snapshot lock task 不是安全的单路径组件: {task:?}");
        }
        let lock_path = root.join(".git").join(format!("orch-snapshot-{task}.lock"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(_) => Ok(Self(lock_path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => bail!(
                "另一 snapshot 正在进行（lock 存在）: {}",
                lock_path.display()
            ),
            Err(error) => Err(error)
                .with_context(|| format!("snapshot lock 获取失败: {}", lock_path.display())),
        }
    }
}

impl Drop for SnapshotLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Test/audit hook，phase 为 "between-captures"（tree1 后、tree2 前）、
/// "before-final-capture"（postchecks 后、final capture 前）与
/// "after-final-capture"（final capture 后、第四次捕获前）与
/// "before-ref-publish"（commit object 已建、紧邻 ref publish 的最终 fence）。
/// Production callers 传恒 Ok 闭包；same-module `#[cfg(test)]` tests 在对应
/// phase 变更 worktree/index，证明 tree2/tree3 与 postcheck 捕获它且
/// 不发布 archive ref。Never exposed outside this crate.
/// 内部实现：snapshot_worktree_wip 的 hook 版本。always-compiled private。
/// production wrapper 传 None；同模块 #[cfg(test)] 可传 Some。
#[doc(hidden)]
pub fn snapshot_worktree_wip_with_hook(
    root: &Path,
    round: &str,
    attempt: &AttemptRef,
    hook: &mut dyn FnMut(&str, &Path, &Path) -> Result<()>,
) -> Result<Option<WipSnapshot>> {
    let wt = root.join(&format!(".worktrees/{}", attempt.task_id));
    let expected_branch = format!("task/{}", attempt.task_id);

    // 前置：worktree 必须在 task/<ID> 分支
    let cur = gitx::current_branch(&wt).context("读取 worktree 当前分支失败")?;
    if cur.as_deref() != Some(&expected_branch) {
        bail!(
            "worktree 不在 {expected_branch} 分支（当前 {:?}），拒绝快照",
            cur
        );
    }

    // 协作锁：覆盖整个捕获-发布窗口（before 留证 → final capture → CAS publish）。
    let _snapshot_lock = SnapshotLock::acquire(root, &attempt.task_id)?;

    // 现场留证（快照前）：porcelain、HEAD、真实 index 字节
    let before_status = gitx::porcelain_v2(&wt).context("快照前 status 失败")?;
    let before_head = gitx::rev_parse(&wt, "HEAD").context("快照前 HEAD 失败")?;
    let index_path_str = gitx::rev_parse_path(&wt, "index")?;
    let index_path = std::path::PathBuf::from(&index_path_str);
    // B: propagate index read errors (not .ok())
    let before_index = fs::read(&index_path)
        .with_context(|| format!("快照前读取 index 失败: {}", index_path.display()))?;

    // clean 判定：porcelain 为空 ⇒ 无 dirty，返 None
    if before_status.trim().is_empty() {
        return Ok(None);
    }

    // 用临时索引在 worktree 侧建 tree（GIT_INDEX_FILE 隔离真实 index；work-tree 指向 wt）
    let temp_index_one = gitx::temp_index_path(root, &format!("{}-tree1", attempt.attempt_id));
    let _cleanup_one = TempFileGuard(temp_index_one.clone());
    let tree_one =
        gitx::snapshot_tree(root, &wt, &temp_index_one).context("第一临时索引建树失败")?;

    // The hook is intentionally between the two independent captures. A
    // same-status/same-mtime content replacement must change tree2 and abort
    // before any archive ref is published.
    hook("between-captures", root, &wt).context("snapshot hook 失败")?;

    let temp_index_two = gitx::temp_index_path(root, &format!("{}-tree2", attempt.attempt_id));
    let _cleanup_two = TempFileGuard(temp_index_two.clone());
    let tree_two =
        gitx::snapshot_tree(root, &wt, &temp_index_two).context("第二临时索引建树失败")?;
    if tree_two != tree_one {
        bail!(
            "WIP snapshot changed between independent captures: tree1={tree_one} tree2={tree_two}"
        );
    }

    // 现场一致性复核（快照后必须逐字节一致）——**先复核再发布 archive ref**：
    // 若先 update_ref 再复核，复核失败时 ref 已发布，违反「changed site ⇒ no ref」。
    let after_status = gitx::porcelain_v2(&wt).context("快照后 status 失败")?;
    let after_head = gitx::rev_parse(&wt, "HEAD").context("快照后 HEAD 失败")?;
    // B: propagate index read errors (not .ok())
    let after_index = fs::read(&index_path)
        .with_context(|| format!("快照后读取 index 失败: {}", index_path.display()))?;
    if after_status != before_status {
        bail!("快照改动 worktree status：before={before_status:?} after={after_status:?}");
    }
    if after_head != before_head {
        bail!("快照改动 HEAD：before={before_head} after={after_head}");
    }
    if after_index != before_index {
        bail!("快照改动真实 index 字节");
    }

    // P1-4：CAS 发布前再次捕获 tree 并要求一致——防止同状态内容竞态
    // （dirty 文件在 tree_two 后改变字节但 status/HEAD/index 形状不变）。
    // final capture 在锁内紧邻 CAS publish。
    hook("before-final-capture", root, &wt).context("snapshot hook 失败")?;
    let temp_index_three = gitx::temp_index_path(root, &format!("{}-tree3", attempt.attempt_id));
    let _cleanup_three = TempFileGuard(temp_index_three.clone());
    let tree_three =
        gitx::snapshot_tree(root, &wt, &temp_index_three).context("第三临时索引建树失败")?;
    if tree_three != tree_two {
        bail!(
            "WIP snapshot changed between CAS publish: tree2={tree_two} tree3={tree_three}（同状态内容竞态）"
        );
    }

    // final capture 后到 ref CAS 前仍可能有非协作 writer 改变 dirty bytes。
    // 用 deterministic hook 覆盖该窗口，再做紧邻 publish 的第四次捕获。
    hook("after-final-capture", root, &wt).context("snapshot hook 失败")?;
    let temp_index_four = gitx::temp_index_path(root, &format!("{}-tree4", attempt.attempt_id));
    let _cleanup_four = TempFileGuard(temp_index_four.clone());
    let tree_four =
        gitx::snapshot_tree(root, &wt, &temp_index_four).context("第四临时索引建树失败")?;
    if tree_four != tree_three {
        bail!(
            "WIP snapshot changed after final capture: tree3={tree_three} tree4={tree_four}（发布前竞态）"
        );
    }

    // 仅当三项 postcheck + 四次 tree 一致全过才构造 commit object。commit-tree
    // 尚未发布任何 ref，因此随后 fence 失败仍不会留下 archive ref。
    let commit_sha = gitx::commit_tree(root, &tree_one, &before_head, "orch-b97-wip-snapshot")
        .context("commit-tree 失败")?;
    hook("before-ref-publish", root, &wt).context("snapshot publish fence hook 失败")?;
    let temp_index_five = gitx::temp_index_path(root, &format!("{}-tree5", attempt.attempt_id));
    let _cleanup_five = TempFileGuard(temp_index_five.clone());
    let tree_five =
        gitx::snapshot_tree(root, &wt, &temp_index_five).context("第五临时索引建树失败")?;
    if tree_five != tree_four {
        bail!("WIP snapshot changed at ref publish fence: tree4={tree_four} tree5={tree_five}");
    }
    let archive_ref_field = format!("archive/{round}-{}-wip", attempt.attempt_id);
    let archive_ref_name = format!("refs/{archive_ref_field}");
    let actual_sha = gitx::update_ref_cas_create(
        root,
        &archive_ref_name,
        &commit_sha,
        &tree_one,
        &before_head,
    )
    .context("update-ref archive CAS 失败")?;

    Ok(Some(WipSnapshot {
        dirty: true,
        archive_ref: archive_ref_field,
        snapshot_sha: actual_sha,
    }))
}

/// 捕获 worktree 的 staged+unstaged+untracked 全部改动到游离 commit 与 archive ref，
/// 绝不改变真实 index、HEAD 或 worktree 字节。clean worktree 返 None（不制造空 archive）。
///
/// 前置：worktree 必须在 `task/<ID>` 分支上（detached 或错分支 ⇒ fail-closed Err）。
/// 前后 porcelain-v2 / HEAD / index 字节必须逐位一致，否则 fail-closed 不发布 archive ref。
///
/// Strong consistency requires that arbitrary writers have stopped, or that
/// every writer participates in this function's cooperative snapshot lock.
/// The advisory lock alone cannot prevent a non-cooperating external writer;
/// repeated captures and the final publish fence only detect mutations that
/// overlap one of those observations.
pub fn snapshot_worktree_wip(
    root: &Path,
    round: &str,
    attempt: &AttemptRef,
) -> Result<Option<WipSnapshot>> {
    snapshot_worktree_wip_with_hook(root, round, attempt, &mut |_phase, _root, _worktree| Ok(()))
}

/// 临时文件清理守卫：drop 时删除临时索引文件。
struct TempFileGuard(std::path::PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

// ─────────────────────────── durable action fold ───────────────────────────
//
// revision 7 契约：三类 durable action（DispatchWake / ResumeWake / ReportCollect）
// 必须有 kind-specific 合法状态迁移、owner + leaseGeneration lineage、foreign/missing
// Released fail-closed；REPORT terminal 绑定 evidence digest / length / control epoch
// 和 pinned branch SHA；三条真实 Tier-F 生产路径（dispatch / collect / resume）必须
// 共同消费此 fold，只留纯 seam 判未完成。

/// 三类 durable action。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurableActionKind {
    DispatchWake,
    ResumeWake,
    ReportCollect,
}

impl DurableActionKind {
    /// 事件 kind 前缀（`DispatchWake` / `ResumeWake` / `ReportCollect`）。
    pub fn prefix(self) -> &'static str {
        match self {
            DurableActionKind::DispatchWake => "DispatchWake",
            DurableActionKind::ResumeWake => "ResumeWake",
            DurableActionKind::ReportCollect => "ReportCollect",
        }
    }
}

/// fold 的阶段结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurableActionPhase {
    Claimed,
    Launching,
    Delivered,
    Executing,
    Executed,
    Completed,
    Released,
}

/// fold 期望的不可变事实集（runtime 调用方持有，校验事件 lineage）。
#[derive(Debug, Clone)]
pub struct DurableActionExpectation {
    pub round: String,
    pub task_id: String,
    pub attempt_id: String,
    pub attempt_no: usize,
    pub agent: String,
    pub base_sha: String,
    pub go_path: String,
    pub action_id: String,
    pub evidence_sha256: Option<String>,
    pub evidence_len: Option<u64>,
    pub control_epoch: Option<String>,
    pub branch_sha: Option<String>,
}

/// fold 内部提取的 lineage 与可选绑定字段。
///
/// owner / leaseGeneration 是每个 state event 的必选字段（缺失 / 类型错误 /
/// 为空一律 fail-closed 报错）；branch / evidence / control epoch 仅在
/// terminal 阶段与 expectation 精确比对。
struct DurableEventFields {
    owner: String,
    generation: String,
    branch_sha: Option<String>,
    evidence_sha256: Option<String>,
    evidence_len: Option<u64>,
    control_epoch: Option<String>,
}

/// 必选字符串身份字段：缺失 / 非字符串 / 为空 / 值不匹配全部 fail-closed。
fn strict_identity_str(
    payload: &serde_json::Value,
    key: &str,
    expected: &str,
    prefix: &str,
) -> Result<()> {
    match payload.get(key) {
        Some(serde_json::Value::String(actual)) if !actual.is_empty() => {
            if actual != expected {
                bail!("durable {prefix} event {key} 值不匹配：{actual:?} != {expected:?}");
            }
            Ok(())
        }
        _ => bail!("durable {prefix} event {key} 缺失 / 类型错误 / 为空"),
    }
}

/// 必选 u64 身份字段：缺失 / 类型错误 / 值不匹配全部 fail-closed。
fn strict_identity_u64(
    payload: &serde_json::Value,
    key: &str,
    expected: u64,
    prefix: &str,
) -> Result<()> {
    match payload.get(key).and_then(serde_json::Value::as_u64) {
        Some(actual) if actual == expected => Ok(()),
        Some(actual) => bail!("durable {prefix} event {key} 值不匹配：{actual} != {expected}"),
        None => bail!("durable {prefix} event {key} 缺失 / 类型错误"),
    }
}

/// 必选 lineage 字段（owner / leaseGeneration）：缺失 / 非字符串 / 为空 fail-closed。
fn mandatory_state_str(payload: &serde_json::Value, key: &str, prefix: &str) -> Result<String> {
    match payload.get(key) {
        Some(serde_json::Value::String(value)) if !value.is_empty() => Ok(value.clone()),
        _ => bail!("durable {prefix} event {key} 缺失 / 类型错误 / 为空"),
    }
}

/// 可选绑定字段：缺失 → None；存在但类型错误 → fail-closed。
fn optional_state_str(
    payload: &serde_json::Value,
    key: &str,
    prefix: &str,
) -> Result<Option<String>> {
    match payload.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(value)) => Ok(Some(value.clone())),
        _ => bail!("durable {prefix} event {key} 类型错误"),
    }
}

impl DurableEventFields {
    fn extract(ev: &EventRecord, prefix: &str) -> Result<Self> {
        let payload = ev.payload.as_ref().context("durable event 缺 payload")?;
        let evidence_len =
            match payload.get("evidenceLen") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::Number(n)) => Some(n.as_u64().ok_or_else(|| {
                    anyhow::anyhow!("durable {prefix} event evidenceLen 类型错误")
                })?),
                _ => bail!("durable {prefix} event evidenceLen 类型错误"),
            };
        Ok(Self {
            owner: mandatory_state_str(payload, "owner", prefix)?,
            generation: mandatory_state_str(payload, "leaseGeneration", prefix)?,
            branch_sha: optional_state_str(payload, "branchSha", prefix)?,
            evidence_sha256: optional_state_str(payload, "evidenceSha256", prefix)?,
            evidence_len,
            control_epoch: optional_state_str(payload, "controlEpoch", prefix)?,
        })
    }
}

/// 判断是否存在属于本 fold 精确 scope（top-level round + task + kind prefix +
/// payload actionId 完全相等）的事件。供 tierf 区分「首次调用（无任何 scope
/// 事件，直接走新 claim）」与「已有历史（必须 fold 决策）」。
pub fn durable_action_scope_has_events(
    events: &[EventRecord],
    kind: DurableActionKind,
    expectation: &DurableActionExpectation,
) -> bool {
    let prefix = kind.prefix();
    events.iter().any(|ev| {
        ev.task_id.as_deref() == Some(expectation.task_id.as_str())
            && ev.kind.starts_with(prefix)
            && match ev.payload.as_ref().and_then(|p| p.get("actionId")) {
                Some(serde_json::Value::String(action_id)) if !action_id.is_empty() => {
                    action_id == &expectation.action_id
                }
                // A same-task/kind malformed action cannot be treated as a
                // pristine first call. Route it through the common fold so
                // missing/wrong-type identity fails closed.
                _ => true,
            }
    })
}

/// 折叠一个 durable action 的完整事件序列，校验合法状态迁移、owner /
/// leaseGeneration lineage 与 terminal 证据绑定。
///
/// scope 选择：top-level task_id + kind prefix + payload actionId。
/// actionId 为不等非空字符串 → foreign，跳过；actionId 缺失 / 类型错误 / 为空
/// → fail-closed 报错（区分 foreign 与 malformed）。被选事件的 top-level
/// round 必须等于 expectation.round；attemptId / attemptNo / agent / baseSha /
/// goPath 必须存在、类型正确、值 byte-exact；owner / leaseGeneration 必须存在
/// 且非空。
///
/// 合法迁移图（kind-specific）：
/// - DispatchWake / ResumeWake: Claimed → Launching → (Delivered) → Completed；
///   Released 仅允许从 Claimed | Launching。
/// - ReportCollect: Claimed → Executing → Executed → Completed；
///   Released 仅允许从 Claimed | Executing。
/// - 多代 lease：Claimed 仅允许从初始状态或 Released 之后出现（重新锚定
///   owner / generation）；旧 generation 的任何后续事件因 lineage 不等而失败。
///
/// 校验：
/// - 每条非 Claimed 事件的 owner / leaseGeneration 必须等于当前锚点；
/// - Released 必须处于 kind 对应的前驱阶段且 owner / generation 精确相等；
/// - ReportCollect 的 Executed / Completed 的 branchSha 必须等于
///   expectation.branch_sha（当其存在）；
/// - Completed（ReportCollect）必须绑定 evidence digest / length / control epoch。
pub fn fold_durable_action(
    events: &[EventRecord],
    kind: DurableActionKind,
    expectation: &DurableActionExpectation,
) -> Result<DurableActionPhase> {
    let prefix = kind.prefix();
    let mut phase: Option<DurableActionPhase> = None;
    let mut current_owner: Option<String> = None;
    let mut current_generation: Option<String> = None;

    for ev in events {
        // 精确筛选：top-level task + kind prefix。
        if ev.task_id.as_deref() != Some(expectation.task_id.as_str()) {
            continue;
        }
        if !ev.kind.starts_with(prefix) {
            continue;
        }
        // actionId 区分 foreign 与 malformed：不等非空字符串 → foreign 跳过；
        // 缺失 / 类型错误 / 为空 → fail-closed。
        let payload = ev.payload.as_ref().context("durable event 缺 payload")?;
        match payload.get("actionId") {
            Some(serde_json::Value::String(action_id)) if !action_id.is_empty() => {
                if action_id != &expectation.action_id {
                    continue; // foreign action
                }
            }
            _ => bail!("durable {prefix} event actionId 缺失 / 类型错误 / 为空"),
        }
        // 被选事件的 top-level round 必须精确等于 expectation.round。
        if ev.round.as_deref() != Some(expectation.round.as_str()) {
            bail!(
                "durable {prefix} event round {:?} != expectation {:?}",
                ev.round,
                expectation.round
            );
        }
        // 完整身份字段：全部必选、类型正确、值 byte-exact。
        strict_identity_str(payload, "attemptId", &expectation.attempt_id, prefix)?;
        strict_identity_u64(payload, "attemptNo", expectation.attempt_no as u64, prefix)?;
        strict_identity_str(payload, "agent", &expectation.agent, prefix)?;
        strict_identity_str(payload, "baseSha", &expectation.base_sha, prefix)?;
        strict_identity_str(payload, "goPath", &expectation.go_path, prefix)?;
        let suffix = &ev.kind[prefix.len()..];
        let fields = DurableEventFields::extract(ev, prefix)?;

        // lineage：非 Claimed 事件的 owner / generation 必须等于当前锚点。
        if suffix != "Claimed" {
            match (&current_owner, &current_generation) {
                (Some(established_owner), Some(established_generation)) => {
                    if established_owner != &fields.owner {
                        bail!(
                            "durable {prefix} owner lineage 违反：{established_owner} != {}",
                            fields.owner
                        );
                    }
                    if established_generation != &fields.generation {
                        bail!(
                            "durable {prefix} leaseGeneration lineage 违反：\
                             {established_generation} != {}",
                            fields.generation
                        );
                    }
                }
                _ => bail!("durable {prefix} {suffix} 缺合法前驱 Claimed（lineage 未建立）"),
            }
        }

        match suffix {
            "Claimed" => {
                // 多代 lease：仅允许从初始状态或 Released 之后重新 Claim。
                match phase {
                    None | Some(DurableActionPhase::Released) => {}
                    _ => bail!("durable {prefix} Claimed 缺合法前驱（未 Released 的重复 Claim）"),
                }
                current_owner = Some(fields.owner.clone());
                current_generation = Some(fields.generation.clone());
                phase = Some(DurableActionPhase::Claimed);
            }
            "Launching" => {
                if kind == DurableActionKind::ReportCollect {
                    bail!("durable {prefix} 不支持 Launching 阶段");
                }
                if phase != Some(DurableActionPhase::Claimed) {
                    bail!("durable {prefix} Launching 缺合法前驱 Claimed");
                }
                phase = Some(DurableActionPhase::Launching);
            }
            "Delivered" => {
                if kind == DurableActionKind::ReportCollect {
                    bail!("durable {prefix} 不支持 Delivered 阶段");
                }
                if phase != Some(DurableActionPhase::Launching) {
                    bail!("durable {prefix} Delivered 缺合法前驱 Launching");
                }
                phase = Some(DurableActionPhase::Delivered);
            }
            "Executing" => {
                if kind != DurableActionKind::ReportCollect {
                    bail!("durable {prefix} 不支持 Executing 阶段");
                }
                if phase != Some(DurableActionPhase::Claimed) {
                    bail!("durable {prefix} Executing 缺合法前驱 Claimed");
                }
                phase = Some(DurableActionPhase::Executing);
            }
            "Executed" => {
                if kind != DurableActionKind::ReportCollect {
                    bail!("durable {prefix} 不支持 Executed 阶段");
                }
                if phase != Some(DurableActionPhase::Executing) {
                    bail!("durable {prefix} Executed 缺合法前驱 Executing");
                }
                // Executed 必须绑定 pinned branchSha。
                if let Some(ref expected) = expectation.branch_sha {
                    let actual = fields.branch_sha.as_deref();
                    if actual != Some(expected.as_str()) {
                        bail!(
                            "durable {prefix} Executed branchSha 不匹配：{actual:?} != {expected:?}"
                        );
                    }
                }
                phase = Some(DurableActionPhase::Executed);
            }
            "Completed" => {
                if kind == DurableActionKind::ReportCollect {
                    if phase != Some(DurableActionPhase::Executed) {
                        bail!("durable {prefix} Completed 缺合法前驱 Executed");
                    }
                    // terminal 绑定 evidence + pinned branch。
                    if let Some(ref expected) = expectation.branch_sha {
                        let actual = fields.branch_sha.as_deref();
                        if actual != Some(expected.as_str()) {
                            bail!(
                                "durable {prefix} Completed branchSha 不匹配：\
                                 {actual:?} != {expected:?}"
                            );
                        }
                    }
                    if let Some(ref expected) = expectation.evidence_sha256 {
                        let actual = fields.evidence_sha256.as_deref();
                        if actual != Some(expected.as_str()) {
                            bail!(
                                "durable {prefix} Completed evidenceSha256 不匹配：\
                                 {actual:?} != {expected:?}"
                            );
                        }
                    }
                    if let Some(expected_len) = expectation.evidence_len {
                        if fields.evidence_len != Some(expected_len) {
                            bail!(
                                "durable {prefix} Completed evidenceLen 不匹配：\
                                 {:?} != {expected_len}",
                                fields.evidence_len
                            );
                        }
                    }
                    if let Some(ref expected_epoch) = expectation.control_epoch {
                        let actual = fields.control_epoch.as_deref();
                        if actual != Some(expected_epoch.as_str()) {
                            bail!(
                                "durable {prefix} Completed controlEpoch 不匹配：\
                                 {actual:?} != {expected_epoch:?}"
                            );
                        }
                    }
                } else if phase != Some(DurableActionPhase::Delivered) {
                    bail!("durable {prefix} Completed 缺合法前驱 Delivered");
                }
                phase = Some(DurableActionPhase::Completed);
            }
            "Released" => {
                // kind-specific 前驱阶段表：wake 仅 Claimed | Launching；
                // collect 仅 Claimed | Executing。Completed / Delivered / Executed
                // 之后的 Released 一律非法。
                let legal_predecessor = match kind {
                    DurableActionKind::DispatchWake | DurableActionKind::ResumeWake => matches!(
                        phase,
                        Some(DurableActionPhase::Claimed) | Some(DurableActionPhase::Launching)
                    ),
                    DurableActionKind::ReportCollect => matches!(
                        phase,
                        Some(DurableActionPhase::Claimed) | Some(DurableActionPhase::Executing)
                    ),
                };
                if !legal_predecessor {
                    bail!("durable {prefix} Released 前驱阶段非法：{phase:?}");
                }
                // owner / generation 与锚点的等值已由上方统一 lineage 校验覆盖。
                phase = Some(DurableActionPhase::Released);
            }
            other => {
                bail!("durable {prefix} 未知阶段：{other:?}");
            }
        }
    }

    phase.ok_or_else(|| anyhow::anyhow!("durable {prefix} 无 Claimed 事件"))
}

// ─────────────────── REPORT frontmatter 解析面（B144 定点开孔，H15） ───────────────────
//
// 本段是 attempt.rs 历史冻结面的最小定点开孔：仅新增 REPORT 自报实现 HEAD 的
// 解析内核与 collect/await 读取入口，其余面零触碰。

/// REPORT 自报实现 HEAD 的机读结果（B144/H15）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportHeads {
    /// 写 REPORT 前的生产实现 HEAD。`implementationHead` 为规范字段；
    /// 历史 `headSha` 兼容映射为同义（存量 REPORT 零破坏）。
    pub implementation_head: String,
}

/// 纯解析内核（B144 种子契约）：从 REPORT frontmatter 文本提取自报实现 HEAD。
///
/// - `implementationHead` 规范字段；`headSha` 兼容映射；
/// - 双字段同值 → Ok；不等 → Err（矛盾非选择）；双缺 → Err。
///
/// 行级解析：按首个 `:` 切 key/value，空值视为缺失；与 frontmatter 其余键零耦合。
pub fn report_head_fields(frontmatter: &str) -> Result<ReportHeads, String> {
    let mut canonical: Option<String> = None;
    let mut legacy: Option<String> = None;
    for line in frontmatter.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "implementationHead" => canonical = Some(value.to_string()),
            "headSha" => legacy = Some(value.to_string()),
            _ => {}
        }
    }
    match (canonical, legacy) {
        (Some(c), Some(l)) if c == l => Ok(ReportHeads {
            implementation_head: c,
        }),
        (Some(c), Some(l)) => Err(format!(
            "implementationHead({c}) 与 headSha({l}) 矛盾（矛盾非选择）"
        )),
        (Some(c), None) => Ok(ReportHeads {
            implementation_head: c,
        }),
        (None, Some(l)) => Ok(ReportHeads {
            implementation_head: l,
        }),
        (None, None) => Err("REPORT frontmatter 缺 implementationHead/headSha".to_string()),
    }
}

/// collect/await 的 REPORT frontmatter 读取入口（B144 接线）：claim 路径内对
/// REPORT evidence 做 head 机检。从完整 REPORT 文本切出首个 frontmatter 块，
/// 经 `report_head_fields` 解析 REPORT 自报实现 HEAD：
///
/// - 解析失败（非 UTF-8 / 无 frontmatter 块 / 双缺 / 矛盾）→ Err，沿用
///   `report_head_fields` 的 Err 语义融入 claim 既有错误路径；
/// - 解析成功 → 与分支 `task/<task>` 实际提交做一致性比对（等值或祖先即一致，
///   collector pin 的最终 HEAD 含 REPORT commit 自身）；不一致或分支不可解析
///   仅返回提示行进 collect 输出，永不改变既有判定结果。
pub fn report_head_claim_check(
    root: &std::path::Path,
    task: &str,
    report_bytes: &[u8],
) -> Result<Option<String>> {
    let text = std::str::from_utf8(report_bytes)
        .map_err(|_| anyhow::anyhow!("REPORT head 机检失败(B144): evidence 不是 UTF-8"))?;
    let fm = report_frontmatter_block(text).unwrap_or("");
    let heads =
        report_head_fields(fm).map_err(|e| anyhow::anyhow!("REPORT head 机检失败(B144): {e}"))?;
    let branch = format!("task/{task}");
    let branch_head = match crate::gitx::rev_parse(root, &branch) {
        Ok(head) => head,
        Err(_) => {
            return Ok(Some(format!(
                "REPORT head 一致性提示: 分支 {branch} 不可解析，跳过自报实现 HEAD {} 对照（仅提示不阻断）",
                heads.implementation_head
            )));
        }
    };
    if heads.implementation_head == branch_head {
        return Ok(None);
    }
    let on_branch =
        crate::gitx::is_ancestor(root, &heads.implementation_head, &branch_head).unwrap_or(false);
    if on_branch {
        return Ok(None);
    }
    Ok(Some(format!(
        "REPORT head 一致性提示: 自报实现 HEAD {} 不在分支 {branch} 实际提交链上（HEAD {}）（H15 语义收口期存量兼容，仅提示）",
        heads.implementation_head, branch_head
    )))
}

/// REPORT evidence 判定：canonical 路径文件名以 `-REPORT.md` 结尾
/// （与 mech::task_evidence_path 的 canonical REPORT 形态一致）。
fn is_report_evidence_path(canonical_path: &str) -> bool {
    std::path::Path::new(canonical_path)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with("-REPORT.md"))
}

/// 切出 REPORT 文本首个 `---` … `---` 行级块（容忍 §0 段前置等前导内容）。
fn report_frontmatter_block(report_text: &str) -> Option<&str> {
    let mut offset = 0usize;
    let mut content_start: Option<usize> = None;
    for line in report_text.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\n', '\r']);
        if bare == "---" {
            match content_start {
                None => content_start = Some(offset + line.len()),
                Some(start) => return Some(&report_text[start..offset]),
            }
        }
        offset += line.len();
    }
    // 末行无换行的闭合 --- 兼容：split_inclusive 已覆盖（bare 比较不依赖换行）。
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::SystemTime;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    // ─────────────── B144 REPORT head 解析面：存量兼容回放 + 接线行为 ───────────────

    /// r48-r50 真实 REPORT frontmatter 样本（逐字摘自 coordination/rounds/*/reports/），
    /// 全部必须经 headSha 兼容映射可解析（历史 REPORT 零破坏）。
    #[test]
    fn b144_legacy_reports_r48_r50_all_parse() {
        let samples: [(&str, &str); 9] = [
            // r48 B131
            (
                "taskId: B131\nagent: executor-desktop\nbranch: task/B131\nheadSha: eeb3e2433c31530bc272dc9ae7f51e40416eb8f6\nwroteAt: 2026-07-26T20:11:47Z\n",
                "eeb3e2433c31530bc272dc9ae7f51e40416eb8f6",
            ),
            // r48 B132
            (
                "taskId: B132\nagent: executor-desktop\nbranch: task/B132\nheadSha: 010900b28118c828c274dc14ca0d5088188d086e\nwroteAt: 2026-07-27T03:17:16Z\n",
                "010900b28118c828c274dc14ca0d5088188d086e",
            ),
            // r48 B133
            (
                "taskId: B133\nagent: executor-desktop\nbranch: task/B133\nheadSha: 4474aa5ebfedc807cb59b89b4a714e1d699345db\nwroteAt: 2026-07-27T04:10:47Z\n",
                "4474aa5ebfedc807cb59b89b4a714e1d699345db",
            ),
            // r49 B134
            (
                "taskId: B134\nagent: executor-desktop\nbranch: task/B134\nheadSha: 69bc3c401395f1f0239f879611ff874c5970c0df\nwroteAt: 2026-07-27T05:41:05Z\n",
                "69bc3c401395f1f0239f879611ff874c5970c0df",
            ),
            // r49 B135
            (
                "taskId: B135\nagent: executor-claw\nbranch: task/B135\nheadSha: 8e67e236dbaa53b2bab0ebdaa5ab2d6350975f96\nwroteAt: 2026-07-27T05:53:09Z\n",
                "8e67e236dbaa53b2bab0ebdaa5ab2d6350975f96",
            ),
            // r49 B136（含 attemptId 键）
            (
                "taskId: B136\nattemptId: B136-A0002\nagent: executor-desktop\nbranch: task/B136\nheadSha: b5f19ac18fd909bf82ba310ffef826f0f877762a\nwroteAt: 2026-07-27T07:30:14Z\n",
                "b5f19ac18fd909bf82ba310ffef826f0f877762a",
            ),
            // r49 B137（含 revision 键）
            (
                "taskId: B137\nagent: executor-opencode\nbranch: task/B137\nheadSha: b2c865af93a9941b08e8d0e8b221809ac6e71ea4\nattemptId: B137-A0002\nrevision: 2\nwroteAt: 2026-07-27T19:05:00Z\n",
                "b2c865af93a9941b08e8d0e8b221809ac6e71ea4",
            ),
            // r50 B141
            (
                "taskId: B141\nagent: executor-opencode\nbranch: task/B141\nheadSha: bf55c89e6c87b10afb59ab5fb05eafb951afbdc0\nwroteAt: 2026-07-27T12:30:39Z\n",
                "bf55c89e6c87b10afb59ab5fb05eafb951afbdc0",
            ),
            // r50 B142
            (
                "taskId: B142\nattemptId: B142-A0002\nagent: executor-desktop\nbranch: task/B142\nheadSha: 97faa95b2a308f148a170f80d79279634e2a71a8\nwroteAt: 2026-07-27T12:31:51Z\n",
                "97faa95b2a308f148a170f80d79279634e2a71a8",
            ),
        ];
        for (fm, expect) in samples {
            let heads = report_head_fields(fm).expect("存量 REPORT 必须可解析");
            assert_eq!(heads.implementation_head, expect, "样本解析值漂移: {fm}");
        }
        // 更早的 r41 已用规范字段 implementationHead（短 SHA 原样保留）。
        let old = report_head_fields(
            "taskId: B85\nagent: executor-desktop\nbranch: task/B85\nimplementationHead: eef5482\nwroteAt: 2026-07-25T04:48:24+0800\n",
        )
        .expect("implementationHead 规范字段可解析");
        assert_eq!(old.implementation_head, "eef5482");
    }

    /// 接线入口行为：§0 段前置（r48/B130 真实形态）也能切出 frontmatter；
    /// 分支不可解析 → 跳过对照提示（不阻断）；自报不在提交链上 → 不一致提示。
    #[test]
    fn b144_claim_check_hint_paths() {
        let root = setup_go_repo("b144-claim-check");
        // §0 前置形态 + 分支不存在 → 跳过对照提示，不 Err。
        let report = "## §0 执行环境自报\nMODEL=gpt-5.6-sol\nDEPTH=high\nCAPTURE=Codex runtime model metadata\n\n---\ntaskId: B130\nagent: executor-desktop\nheadSha: aaaa1111\nwroteAt: 2026-07-26T00:00:00Z\n---\n\n正文\n";
        let note = report_head_claim_check(&root, "B130", report.as_bytes())
            .expect("分支不可解析不得 Err")
            .expect("分支不可解析必须有提示行");
        assert!(note.contains("跳过") && note.contains("不阻断"), "{note}");
        // 分支存在且自报 == 分支 HEAD → 一致，无提示。
        let head = git(&root, &["rev-parse", "main"]);
        git(&root, &["branch", "task/B999", &head]);
        let agreeing = format!("---\nimplementationHead: {head}\nheadSha: {head}\n---\n");
        assert!(
            report_head_claim_check(&root, "B999", agreeing.as_bytes())
                .unwrap()
                .is_none(),
            "一致时不得产生提示行"
        );
        // 自报不在分支提交链上（良构但仓库中不存在的 SHA）→ 不一致提示，不 Err。
        let off_branch = format!("---\nheadSha: {}\n---\n", "9".repeat(40));
        let note = report_head_claim_check(&root, "B999", off_branch.as_bytes())
            .expect("不一致不得 Err")
            .expect("不一致必须有提示行");
        assert!(
            note.contains("不在分支") && note.contains("仅提示"),
            "{note}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// 接线入口 Err 语义：双缺/矛盾/无 frontmatter 全部融入既有错误路径。
    #[test]
    fn b144_claim_check_parse_failures_are_errors() {
        let root = setup_go_repo("b144-claim-err");
        let conflict = report_head_claim_check(
            &root,
            "B1",
            b"---\nimplementationHead: aaaa1111\nheadSha: bbbb2222\n---\n",
        )
        .unwrap_err();
        assert!(format!("{conflict:#}").contains("矛盾"), "{conflict:#}");
        let missing = report_head_claim_check(&root, "B1", b"---\ntaskId: B1\n---\n").unwrap_err();
        assert!(
            format!("{missing:#}").contains("缺 implementationHead/headSha"),
            "{missing:#}"
        );
        let no_fm = report_head_claim_check(&root, "B1", b"# no frontmatter\n").unwrap_err();
        assert!(format!("{no_fm:#}").contains("B144"), "{no_fm:#}");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// B144 接线测试脚手架：带一条 modern DispatchIssued 的账本 + GO 文件，
    /// control marker 全部压到过去，保证 evidence（当前 mtime）被判 current。
    /// 返回 (root, report_path, 分支 task/B144T 的 HEAD)；REPORT 由调用方写。
    fn b144_claim_site(tag: &str) -> (std::path::PathBuf, std::path::PathBuf, String) {
        let root = setup_go_repo(tag);
        let base = git(&root, &["rev-parse", "main"]);
        git(&root, &["branch", "task/B144T", &base]);
        let go = make_go(&root, "rT", "executor-claw", "GO-B144T-A0001.md");
        // GO mtime 压到过去：marker = max(旧 ULID, GO mtime) 仍在过去。
        std::fs::File::options()
            .write(true)
            .open(&go)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000))
            .unwrap();
        let ledger_dir = root.join("coordination/rounds/rT");
        std::fs::create_dir_all(&ledger_dir).unwrap();
        let dispatch = format!(
            "{{\"eventId\":\"01ARZ3NDEKTSV4RRFFQ69G5FAV\",\"ts\":\"2020-01-01T00:00:00Z\",\"actor\":\"runtime:orch\",\"type\":\"DispatchIssued\",\"taskId\":\"B144T\",\"round\":\"rT\",\"payload\":{{\"agent\":\"executor-claw\",\"attemptId\":\"B144T-A0001\",\"attemptNo\":1,\"baseSha\":\"{base}\",\"goPath\":\"coordination/rounds/rT/dispatch/executor-claw/GO-B144T-A0001.md\"}}}}\n"
        );
        std::fs::write(ledger_dir.join("events.jsonl"), dispatch).unwrap();
        let report_path = ledger_dir.join("reports/B144T-REPORT.md");
        std::fs::create_dir_all(report_path.parent().unwrap()).unwrap();
        (root, report_path, base)
    }

    fn b144_report_claim_decision(
        _events: &[EventRecord],
        _ctx: &DispatchContext,
        _marker: Option<std::time::SystemTime>,
        _claimed: Option<&ClaimedEvidence>,
        _epoch: &str,
    ) -> Result<ClaimDecision> {
        Ok(ClaimDecision::Append(vec![crate::ledger::event(
            "ReportObserved",
            "runtime:orch",
            Some("B144T"),
            Some("rT"),
            serde_json::json!({"actionId": "report-observed"}),
        )]))
    }

    /// 接线在 claim 路径内生效：矛盾 REPORT 的 claim 必须 Err（摘除接线本测试即红）。
    #[test]
    fn b144_claim_rejects_report_with_conflicting_heads() {
        let (root, report_path, _head) = b144_claim_site("b144-claim-conflict");
        std::fs::write(
            &report_path,
            b"---\ntaskId: B144T\nimplementationHead: aaaa1111\nheadSha: bbbb2222\nwroteAt: 2020-01-01T00:00:00Z\n---\n",
        )
        .unwrap();
        let observed = observe_evidence(&report_path).unwrap().unwrap();
        let outcome = claim_attempt_action(
            &root,
            "rT",
            "B144T",
            &AttemptObservation {
                attempt_id: Some("B144T-A0001".into()),
                attempt_no: Some(1),
            },
            Some(&observed),
            &b144_report_claim_decision,
        );
        let error = outcome.expect_err("矛盾 REPORT 必须经 claim 错误路径失败");
        assert!(format!("{error:#}").contains("矛盾"), "{error:#}");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// 接线不改变既有判定：自报 == 分支 HEAD 的存量形态 REPORT 照常 claim 成功。
    #[test]
    fn b144_claim_accepts_report_with_consistent_heads() {
        let (root, report_path, head) = b144_claim_site("b144-claim-ok");
        std::fs::write(
            &report_path,
            format!("---\ntaskId: B144T\nheadSha: {head}\nwroteAt: 2020-01-01T00:00:00Z\n---\n"),
        )
        .unwrap();
        let observed = observe_evidence(&report_path).unwrap().unwrap();
        let outcome = claim_attempt_action(
            &root,
            "rT",
            "B144T",
            &AttemptObservation {
                attempt_id: Some("B144T-A0001".into()),
                attempt_no: Some(1),
            },
            Some(&observed),
            &b144_report_claim_decision,
        )
        .expect("存量形态 REPORT 必须照常 claim 成功");
        match outcome {
            ClaimOutcome::Appended { count, .. } => assert_eq!(count, 1),
            other => panic!("期望 Appended，实际 {other:?}"),
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    static TEST_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn unique_repo(tag: &str) -> std::path::PathBuf {
        let seq = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "orch-b97-internal-{tag}-{}-{seq}-{nanos}",
            std::process::id()
        ))
    }

    fn init_repo() -> std::path::PathBuf {
        let root = unique_repo("hook");
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(
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
        fs::create_dir_all(root.join(".worktrees")).unwrap();
        let wt = root.join(".worktrees/B97");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "task/B97",
                wt.to_str().unwrap(),
                "HEAD",
            ],
        );
        root
    }

    #[test]
    fn modern_dispatch_record_requires_complete_exact_identity_tuple() {
        let derived = AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        };
        let canonical = "coordination/rounds/r44/dispatch/executor-test/GO-B97-A0001.md";
        let make = |attempt_id: bool, attempt_no: bool, go_path: bool| {
            let mut payload = serde_json::json!({
                "agent": "executor-test",
                "baseSha": "base"
            });
            if attempt_id {
                payload["attemptId"] = serde_json::json!("B97-A0001");
            }
            if attempt_no {
                payload["attemptNo"] = serde_json::json!(1);
            }
            if go_path {
                payload["goPath"] = serde_json::json!(canonical);
            }
            EventRecord {
                event_id: format!("partial-{attempt_id}-{attempt_no}-{go_path}"),
                ts: "2026-07-25T00:00:00Z".into(),
                actor: "runtime:test".into(),
                kind: "DispatchIssued".into(),
                task_id: Some("B97".into()),
                round: Some("r44".into()),
                payload: Some(payload),
                extra: serde_json::Map::new(),
            }
        };
        for mask in 1u8..7 {
            let event = make(mask & 1 != 0, mask & 2 != 0, mask & 4 != 0);
            assert!(
                dispatch_record_for_attempt(&event, &derived).is_err(),
                "partial modern mask {mask:03b} must fail"
            );
        }
        let attempt_id_only = make(true, false, false);
        assert_eq!(
            current_attempt(&[attempt_id_only], "B97").unwrap().unwrap(),
            derived,
            "pure current_attempt inference keeps attemptId-only compatibility"
        );
        let complete = make(true, true, true);
        assert_eq!(
            dispatch_record_for_attempt(&complete, &derived)
                .unwrap()
                .go_path,
            canonical
        );
        let mut wrong_path = complete;
        wrong_path.payload.as_mut().unwrap()["goPath"] =
            serde_json::json!("coordination/rounds/r44/dispatch/executor-test/GO-B97.md");
        assert!(dispatch_record_for_attempt(&wrong_path, &derived).is_err());
    }

    #[test]
    fn snapshot_publishes_no_archive_ref_on_postcheck_mismatch() {
        // Deterministic real mismatch: the hook mutates the worktree's index
        // AFTER commit-tree but BEFORE postchecks. Postcheck must detect the
        // changed index, return Err, AND no archive ref may be published.
        let root = init_repo();
        let wt = root.join(".worktrees/B97");
        fs::write(wt.join("untracked.txt"), "untracked\n").unwrap();

        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        };
        let archive_ref = format!("archive/r44-{}-wip", attempt.attempt_id);

        // Hook: stage a new file into the REAL index (simulates a changed site)
        let result = snapshot_worktree_wip_with_hook(
            &root,
            "r44",
            &attempt,
            &mut |phase, _root, worktree| {
                if phase != "between-captures" {
                    return Ok(());
                }
                let out = Command::new("git")
                    .arg("-C")
                    .arg(worktree)
                    .args(["add", "untracked.txt"])
                    .output()?;
                if !out.status.success() {
                    anyhow::bail!(
                        "hook git add failed: {}",
                        String::from_utf8_lossy(&out.stderr)
                    );
                }
                Ok(())
            },
        );
        assert!(result.is_err(), "postcheck mismatch must return Err");

        // No archive ref must exist — use raw Command (not git helper which asserts success)
        let ref_check = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["rev-parse", "--verify", &format!("refs/{archive_ref}")])
            .output()
            .unwrap();
        assert!(
            !ref_check.status.success(),
            "no archive ref must be published on mismatch, but got: {}",
            String::from_utf8_lossy(&ref_check.stdout)
        );

        // Restore the changed test site
        let _ = Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["reset", "-q", "untracked.txt"])
            .output();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_publishes_archive_ref_when_postcheck_passes() {
        let root = init_repo();
        let wt = root.join(".worktrees/B97");
        fs::write(wt.join("untracked.txt"), "untracked\n").unwrap();
        let snapshot = snapshot_worktree_wip(
            &root,
            "r44",
            &AttemptRef {
                task_id: "B97".into(),
                ordinal: 1,
                attempt_id: "B97-A0001".into(),
            },
        )
        .unwrap()
        .unwrap();
        let ref_check = git(
            &root,
            &[
                "rev-parse",
                "--verify",
                &format!("refs/{}", snapshot.archive_ref),
            ],
        );
        assert!(!ref_check.is_empty(), "archive ref should exist");
        fs::remove_dir_all(root).unwrap();
    }

    // ── audit item 2: malformed identity tests ──

    fn event(id: &str, kind: &str, task: &str, payload: serde_json::Value) -> EventRecord {
        EventRecord {
            event_id: id.into(),
            ts: "2026-07-25T00:00:00Z".into(),
            actor: "runtime:test".into(),
            kind: kind.into(),
            task_id: Some(task.into()),
            round: Some("r44".into()),
            payload: Some(payload),
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn malformed_attempt_id_non_string_is_err() {
        let events = vec![event(
            "d1",
            "DispatchIssued",
            "B97",
            serde_json::json!({
                "agent": "x", "baseSha": "b0", "attemptId": 123
            }),
        )];
        assert!(current_attempt(&events, "B97").is_err());
    }

    #[test]
    fn malformed_attempt_id_null_is_err() {
        let events = vec![event(
            "d1",
            "DispatchIssued",
            "B97",
            serde_json::json!({
                "agent": "x", "baseSha": "b0", "attemptId": serde_json::Value::Null
            }),
        )];
        assert!(current_attempt(&events, "B97").is_err());
    }

    #[test]
    fn malformed_attempt_no_non_integer_is_err() {
        let events = vec![event(
            "d1",
            "DispatchIssued",
            "B97",
            serde_json::json!({
                "agent": "x", "baseSha": "b0", "attemptId": "B97-A0001", "attemptNo": "not-a-number"
            }),
        )];
        assert!(current_attempt(&events, "B97").is_err());
    }

    #[test]
    fn malformed_attempt_no_wrong_value_is_err() {
        // attemptNo=5 but ordinal=1
        let events = vec![event(
            "d1",
            "DispatchIssued",
            "B97",
            serde_json::json!({
                "agent": "x", "baseSha": "b0", "attemptId": "B97-A0001", "attemptNo": 5
            }),
        )];
        assert!(current_attempt(&events, "B97").is_err());
    }

    #[test]
    fn malformed_attempt_no_null_is_err() {
        let events = vec![event(
            "d1",
            "DispatchIssued",
            "B97",
            serde_json::json!({
                "agent": "x", "baseSha": "b0", "attemptId": "B97-A0001", "attemptNo": serde_json::Value::Null
            }),
        )];
        assert!(current_attempt(&events, "B97").is_err());
    }

    #[test]
    fn mixed_explicit_then_legacy_derives_correct_ordinal() {
        // A0001(explicit) → A0002(legacy, no attemptId field at all)
        let events = vec![
            event(
                "d1",
                "DispatchIssued",
                "B97",
                serde_json::json!({
                    "agent": "a", "baseSha": "b0", "attemptId": "B97-A0001", "attemptNo": 1,
                    "goPath": "coordination/rounds/r44/dispatch/a/GO-B97-A0001.md"
                }),
            ),
            event(
                "d2",
                "DispatchIssued",
                "B97",
                serde_json::json!({
                    "agent": "b", "baseSha": "b1"  // no attemptId/attemptNo = legacy
                }),
            ),
        ];
        let cur = current_attempt(&events, "B97").unwrap().unwrap();
        assert_eq!(cur.ordinal, 2);
        assert_eq!(cur.attempt_id, "B97-A0002");
    }

    #[test]
    fn legacy_dispatch_missing_all_identity_fields_is_ok() {
        let events = vec![event(
            "d1",
            "DispatchIssued",
            "B97",
            serde_json::json!({
                "agent": "a", "baseSha": "b0"
            }),
        )];
        let cur = current_attempt(&events, "B97").unwrap().unwrap();
        assert_eq!(cur.attempt_id, "B97-A0001");
    }

    // ── audit item 5: sub-second marker ──

    #[test]
    fn ulid_timestamp_decodes_valid_ulid() {
        // A real ULID generated by the project's own ulid crate
        let u = ulid::Ulid::new();
        let t = ulid_timestamp(&u.to_string());
        assert!(t.is_some(), "valid ULID should decode timestamp");
        // Should be close to now
        let now = SystemTime::now();
        let diff = now
            .duration_since(t.unwrap())
            .unwrap_or(std::time::Duration::ZERO);
        assert!(
            diff.as_secs() < 10,
            "decoded timestamp should be close to now"
        );
    }

    #[test]
    fn ulid_timestamp_rejects_invalid_ulid() {
        // Non-26-char / garbage / valid prefix garbage
        assert!(
            ulid_timestamp("short").is_none(),
            "short string should fail"
        );
        assert!(
            ulid_timestamp("0123456789ABCDEFGHIJ").is_none(),
            "20 chars should fail"
        );
        assert!(
            ulid_timestamp("0123456789ABCDEFGHIJKLMNOPQRSTUVWX!").is_none(),
            "non-crockford char should fail"
        );
        assert!(
            ulid_timestamp("not-a-ulid-at-all-xyz").is_none(),
            "garbage should fail"
        );
    }

    #[test]
    fn ulid_timestamp_fallback_to_rfc3339_on_invalid() {
        // Invalid ULID in event_id, but valid RFC3339 ts — should fallback
        let ev = EventRecord {
            event_id: "bad-ulid".into(),
            ts: "2026-07-25T10:30:00Z".into(),
            actor: "test".into(),
            kind: "DispatchIssued".into(),
            task_id: Some("B97".into()),
            round: Some("r44".into()),
            payload: Some(serde_json::json!({})),
            extra: serde_json::Map::new(),
        };
        let t = control_event_timestamp(&ev);
        assert!(t.is_some(), "should fallback to RFC3339 ts");
    }

    #[test]
    fn ulid_timestamp_prefers_ulid_over_rfc3339() {
        // Valid ULID in event_id, also has RFC3339 ts — should prefer ULID (higher precision)
        let u = ulid::Ulid::new();
        let ev = EventRecord {
            event_id: u.to_string(),
            ts: "2020-01-01T00:00:00Z".into(), // old ts, ULID is now
            actor: "test".into(),
            kind: "DispatchIssued".into(),
            task_id: Some("B97".into()),
            round: Some("r44".into()),
            payload: Some(serde_json::json!({})),
            extra: serde_json::Map::new(),
        };
        let t = control_event_timestamp(&ev).unwrap();
        // ULID timestamp should be close to now, not 2020
        let now = SystemTime::now();
        let diff = now.duration_since(t).unwrap_or(std::time::Duration::ZERO);
        assert!(
            diff.as_secs() < 10,
            "should prefer ULID timestamp, not old RFC3339"
        );
    }

    #[test]
    fn compute_evidence_marker_takes_max() {
        let sig = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(100);
        let go_mt = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(200);
        // max(100, 200) = 200
        assert_eq!(compute_evidence_marker(Some(sig), Some(go_mt)), Some(go_mt));
        // max(200, 100) = 200
        assert_eq!(compute_evidence_marker(Some(go_mt), Some(sig)), Some(go_mt));
        // only sig
        assert_eq!(compute_evidence_marker(Some(sig), None), Some(sig));
        // only go_mt
        assert_eq!(compute_evidence_marker(None, Some(go_mt)), Some(go_mt));
        // none
        assert_eq!(compute_evidence_marker(None, None), None);
    }

    #[test]
    fn stale_evidence_between_truncated_ts_and_go_mtime_is_rejected() {
        // 账本 ts 截整秒 = 100s。GO 文件 mtime = 100.5s（晚于截秒，纳秒精度）。
        // 旧 REPORT mtime = 100.3s（晚于截秒 event ts，但早于新 GO 文件 mtime）。
        // marker = max(100s, 100.5s) = 100.5s。
        // evidence 100.3s <= 100.5s ⇒ rejected。
        let sig = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(100);
        let go_mt = SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(100500);
        let marker = compute_evidence_marker(Some(sig), Some(go_mt));
        let stale_report = SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(100300);
        assert!(!evidence_is_current(Some(stale_report), marker));
        // fresh REPORT mtime = 100.7s > 100.5s ⇒ accepted
        let fresh_report = SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(100700);
        assert!(evidence_is_current(Some(fresh_report), marker));
    }

    // ── resolve_go_path_strict tests ──

    fn setup_go_repo(tag: &str) -> std::path::PathBuf {
        let root = unique_repo(tag);
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        std::fs::write(root.join("README.md"), "base\n").unwrap();
        git(&root, &["add", "README.md"]);
        git(
            &root,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "base",
            ],
        );
        root
    }

    fn make_go(root: &Path, round: &str, agent: &str, filename: &str) -> std::path::PathBuf {
        let dir = root.join(format!("coordination/rounds/{round}/dispatch/{agent}"));
        std::fs::create_dir_all(&dir).unwrap();
        let go = dir.join(filename);
        std::fs::write(&go, "# GO\n").unwrap();
        go
    }

    #[test]
    fn resolve_go_modern_path_accepted() {
        let root = setup_go_repo("go-modern");
        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        };
        let go = make_go(&root, "r44", "executor-opencode", "GO-B97-A0001.md");
        let go_str = go.to_string_lossy().to_string();
        let abs =
            resolve_go_path_strict(&root, &go_str, "r44", "executor-opencode", &attempt, false)
                .unwrap();
        assert_eq!(abs, go);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolve_go_modern_wrong_attempt_rejected() {
        let root = setup_go_repo("go-wrong-aid");
        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 2,
            attempt_id: "B97-A0002".into(),
        };
        let go = make_go(&root, "r44", "executor-opencode", "GO-B97-A0001.md");
        let go_str = go.to_string_lossy().to_string();
        let result =
            resolve_go_path_strict(&root, &go_str, "r44", "executor-opencode", &attempt, false);
        assert!(result.is_err(), "modern with wrong attemptId should fail");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolve_go_legacy_accepted_only_when_legacy_dispatch() {
        let root = setup_go_repo("go-legacy");
        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        };
        let go = make_go(&root, "r44", "executor-opencode", "GO-B97.md");
        let go_str = go.to_string_lossy().to_string();
        // legacy dispatch = true → accept GO-B97.md
        let abs =
            resolve_go_path_strict(&root, &go_str, "r44", "executor-opencode", &attempt, true)
                .unwrap();
        assert_eq!(abs, go);
        // modern dispatch = false → reject GO-B97.md
        let result =
            resolve_go_path_strict(&root, &go_str, "r44", "executor-opencode", &attempt, false);
        assert!(result.is_err(), "modern with legacy filename should fail");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolve_go_traversal_rejected() {
        let root = setup_go_repo("go-traversal");
        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        };
        // relative path with ..
        let go_str = "coordination/rounds/r44/dispatch/executor-opencode/../../GO-B97-A0001.md";
        make_go(&root, "r44", "executor-opencode", "GO-B97-A0001.md");
        let result =
            resolve_go_path_strict(&root, go_str, "r44", "executor-opencode", &attempt, false);
        assert!(result.is_err(), "traversal via ../ should be rejected");
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains("..") || err.contains("ParentDir"),
            "error should mention traversal: {err}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolve_go_curdir_component_rejected_before_normalization() {
        let root = setup_go_repo("go-curdir");
        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        };
        make_go(&root, "r44", "executor-opencode", "GO-B97-A0001.md");
        let go_str = "coordination/rounds/r44/dispatch/executor-opencode/./GO-B97-A0001.md";
        assert!(
            resolve_go_path_strict(&root, go_str, "r44", "executor-opencode", &attempt, false)
                .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolve_go_nested_rejected() {
        let root = setup_go_repo("go-nested");
        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        };
        // 7 components (extra nesting)
        let go_str = "coordination/rounds/r44/dispatch/executor-opencode/subdir/GO-B97-A0001.md";
        let result =
            resolve_go_path_strict(&root, go_str, "r44", "executor-opencode", &attempt, false);
        assert!(result.is_err(), "nested path should be rejected");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolve_go_missing_file_rejected() {
        let root = setup_go_repo("go-missing");
        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        };
        let go_str = "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0001.md";
        // don't create the file
        let result =
            resolve_go_path_strict(&root, go_str, "r44", "executor-opencode", &attempt, false);
        assert!(result.is_err(), "missing file should be rejected");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolve_go_symlink_rejected() {
        let root = setup_go_repo("go-symlink");
        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        };
        // create a real GO file at a different location
        let real_go_dir = root.join("coordination/rounds/r44/dispatch/executor-opencode/real");
        std::fs::create_dir_all(&real_go_dir).unwrap();
        let real_go = real_go_dir.join("GO-B97-A0001.md");
        std::fs::write(&real_go, "# GO\n").unwrap();
        // create symlink AT the exact expected filename pointing to the real file
        let symlink_go =
            root.join("coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0001.md");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_go, &symlink_go).unwrap();
        #[cfg(not(unix))]
        {
            std::fs::remove_dir_all(root).unwrap();
            return;
        }
        let go_str = symlink_go.to_string_lossy().to_string();
        let result =
            resolve_go_path_strict(&root, &go_str, "r44", "executor-opencode", &attempt, false);
        // Must fail because the expected filename is a symlink, not a regular file
        assert!(
            result.is_err(),
            "symlink at exact expected filename should be rejected"
        );
        let err = format!("{:#}", result.unwrap_err());
        assert!(err.contains("symlink"), "error must mention symlink: {err}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn ack_observation_requires_exact_regular_non_symlink_slot() {
        let root = setup_go_repo("ack-strict");
        let go = make_go(&root, "r44", "executor-opencode", "GO-B97-A0001.md");
        let ack = std::path::PathBuf::from(format!("{}.ack", go.display()));
        assert!(!ack_observed_strict(&go).unwrap());
        std::fs::write(&ack, "ack\n").unwrap();
        assert!(ack_observed_strict(&go).unwrap());
        std::fs::remove_file(&ack).unwrap();
        let target = ack.with_file_name("real-ack");
        std::fs::write(&target, "ack\n").unwrap();
        std::os::unix::fs::symlink(&target, &ack).unwrap();
        assert!(ack_observed_strict(&go).is_err());
        std::fs::remove_file(&ack).unwrap();
        std::os::unix::fs::symlink(ack.with_file_name("missing-ack"), &ack).unwrap();
        assert!(ack_observed_strict(&go).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolve_go_absolute_outside_root_rejected() {
        let root = setup_go_repo("go-outside");
        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        };
        // absolute path outside root
        // O28：固定名临时目录会被并发跑门的另一个进程 remove_dir_all 掉（r45 实测：
        // 收取链与执行者自跑门同时进行 → NotFound）。改用已有的 pid+seq+nanos 助手。
        let outside = unique_repo("go-outside-external");
        std::fs::create_dir_all(&outside).unwrap();
        let go_str = outside.join("GO-B97-A0001.md");
        std::fs::write(&go_str, "# GO\n").unwrap();
        let go_str_abs = go_str.to_string_lossy().to_string();
        let result = resolve_go_path_strict(
            &root,
            &go_str_abs,
            "r44",
            "executor-opencode",
            &attempt,
            false,
        );
        assert!(
            result.is_err(),
            "absolute path outside root should be rejected"
        );
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn m7_go_worktree_instruction_does_not_force_b_when_branch_exists() {
        // M7 回归：已有 branch/worktree 不应要求 -b 创建
        let neither = go_worktree_instruction("B97", "sha", false, false).unwrap();
        assert!(
            neither.contains("-b task/"),
            "neither exists should use -b task/"
        );

        let branch_only = go_worktree_instruction("B97", "sha", true, false).unwrap();
        assert!(
            !branch_only.contains("-b task/"),
            "branch exists should NOT use -b task/: {}",
            branch_only
        );

        let both = go_worktree_instruction("B97", "sha", true, true).unwrap();
        assert!(
            !both.contains("-b task/"),
            "both exist should NOT use -b task/: {}",
            both
        );

        assert!(go_worktree_instruction("B97", "sha", false, true).is_err());
    }

    #[test]
    fn fold_filters_by_action_and_task_identity() {
        // 行为回归：fold 精确筛选 expectation 身份（action_id/task_id），
        // 账本中并存多个 action/task 的交错事件不互相污染。
        fn durable_event(
            kind: &str,
            task: &str,
            action_id: &str,
            owner: &str,
            gen: &str,
        ) -> orch_core::EventRecord {
            orch_core::EventRecord {
                event_id: format!("e-{kind}-{action_id}"),
                ts: "2026-07-25T00:00:00Z".into(),
                actor: "runtime:test".into(),
                kind: kind.into(),
                task_id: Some(task.into()),
                round: Some("r44".into()),
                payload: Some(serde_json::json!({
                    "attemptId": "B97-A0004",
                    "attemptNo": 4,
                    "agent": "executor-opencode",
                    "baseSha": "base",
                    "goPath": "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0004.md",
                    "actionId": action_id,
                    "owner": owner,
                    "leaseGeneration": gen,
                })),
                extra: serde_json::Map::new(),
            }
        }
        // 交错两个 action 的事件
        let events = vec![
            durable_event("DispatchWakeClaimed", "B97", "action-a", "owner-1", "gen-1"),
            durable_event("DispatchWakeClaimed", "B97", "action-b", "owner-2", "gen-2"),
            durable_event(
                "DispatchWakeLaunching",
                "B97",
                "action-a",
                "owner-1",
                "gen-1",
            ),
            durable_event(
                "DispatchWakeReleased",
                "B97",
                "action-b",
                "owner-2",
                "gen-2",
            ),
        ];
        let expectation = DurableActionExpectation {
            round: "r44".into(),
            task_id: "B97".into(),
            attempt_id: "B97-A0004".into(),
            attempt_no: 4,
            agent: "executor-opencode".into(),
            base_sha: "base".into(),
            go_path: "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0004.md".into(),
            action_id: "action-a".into(),
            evidence_sha256: None,
            evidence_len: None,
            control_epoch: None,
            branch_sha: None,
        };
        // fold 只折叠 action-a 事件，action-b 事件不污染
        let phase =
            fold_durable_action(&events, DurableActionKind::DispatchWake, &expectation).unwrap();
        assert_eq!(phase, DurableActionPhase::Launching);
    }

    #[test]
    fn fold_released_without_claimed_predecessor_fails_closed() {
        // 行为回归：Released 无合法前驱 Claimed 时 fail-closed
        let ev = orch_core::EventRecord {
            event_id: "e-rel".into(),
            ts: "2026-07-25T00:00:00Z".into(),
            actor: "runtime:test".into(),
            kind: "DispatchWakeReleased".into(),
            task_id: Some("B97".into()),
            round: Some("r44".into()),
            payload: Some(serde_json::json!({
                "attemptId": "B97-A0004",
                "attemptNo": 4,
                "agent": "executor-opencode",
                "baseSha": "base",
                "goPath": "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0004.md",
                "actionId": "action-4",
                "owner": "owner-1",
                "leaseGeneration": "gen-1",
            })),
            extra: serde_json::Map::new(),
        };
        let expectation = DurableActionExpectation {
            round: "r44".into(),
            task_id: "B97".into(),
            attempt_id: "B97-A0004".into(),
            attempt_no: 4,
            agent: "executor-opencode".into(),
            base_sha: "base".into(),
            go_path: "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0004.md".into(),
            action_id: "action-4".into(),
            evidence_sha256: None,
            evidence_len: None,
            control_epoch: None,
            branch_sha: None,
        };
        assert!(fold_durable_action(&[ev], DurableActionKind::DispatchWake, &expectation).is_err());
    }

    // ── revision 8：fold 严格性 / 多代 lease / ack 对象身份 / legacy logical fold ──

    fn durable_state_action(
        kind: &str,
        owner: &str,
        gen: &str,
        action_id: &str,
    ) -> orch_core::EventRecord {
        orch_core::EventRecord {
            event_id: format!("e-{kind}-{owner}-{gen}-{action_id}"),
            ts: "2026-07-25T00:00:00Z".into(),
            actor: "runtime:test".into(),
            kind: kind.into(),
            task_id: Some("B97".into()),
            round: Some("r44".into()),
            payload: Some(serde_json::json!({
                "attemptId": "B97-A0004",
                "attemptNo": 4,
                "agent": "executor-opencode",
                "baseSha": "base",
                "goPath": "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0004.md",
                "actionId": action_id,
                "owner": owner,
                "leaseGeneration": gen,
            })),
            extra: serde_json::Map::new(),
        }
    }

    fn durable_state(kind: &str, owner: &str, gen: &str) -> orch_core::EventRecord {
        durable_state_action(kind, owner, gen, "action-4")
    }

    fn durable_expectation() -> DurableActionExpectation {
        DurableActionExpectation {
            round: "r44".into(),
            task_id: "B97".into(),
            attempt_id: "B97-A0004".into(),
            attempt_no: 4,
            agent: "executor-opencode".into(),
            base_sha: "base".into(),
            go_path: "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0004.md".into(),
            action_id: "action-4".into(),
            evidence_sha256: None,
            evidence_len: None,
            control_epoch: None,
            branch_sha: None,
        }
    }

    #[test]
    fn fold_supports_multi_generation_reclaim_and_rejects_stale_generation() {
        let expectation = durable_expectation();
        // Claim(g1) → Release(g1) → Claim(g2) → Launching(g2)：合法多代 reclaim。
        let legal = vec![
            durable_state("DispatchWakeClaimed", "owner-1", "gen-1"),
            durable_state("DispatchWakeReleased", "owner-1", "gen-1"),
            durable_state("DispatchWakeClaimed", "owner-2", "gen-2"),
            durable_state("DispatchWakeLaunching", "owner-2", "gen-2"),
        ];
        assert_eq!(
            fold_durable_action(&legal, DurableActionKind::DispatchWake, &expectation).unwrap(),
            DurableActionPhase::Launching
        );
        // 旧 generation 的 Released 在新 claim 之后 → lineage 违反 fail-closed。
        let mut stale = legal.clone();
        stale.push(durable_state("DispatchWakeReleased", "owner-1", "gen-1"));
        assert!(
            fold_durable_action(&stale, DurableActionKind::DispatchWake, &expectation).is_err()
        );
        // 未 Released 的重复 Claim → Err。
        let double_claim = vec![
            durable_state("DispatchWakeClaimed", "owner-1", "gen-1"),
            durable_state("DispatchWakeClaimed", "owner-1", "gen-1"),
        ];
        assert!(
            fold_durable_action(&double_claim, DurableActionKind::DispatchWake, &expectation)
                .is_err()
        );
        // Released 之后再次 Released（phase 不在 kind 前驱表内）→ Err。
        let double_release = vec![
            durable_state("DispatchWakeClaimed", "owner-1", "gen-1"),
            durable_state("DispatchWakeReleased", "owner-1", "gen-1"),
            durable_state("DispatchWakeReleased", "owner-1", "gen-1"),
        ];
        assert!(fold_durable_action(
            &double_release,
            DurableActionKind::DispatchWake,
            &expectation
        )
        .is_err());
        // Completed 之后 Released → 非法前驱 fail-closed。
        let release_after_completed = vec![
            durable_state("DispatchWakeClaimed", "owner-1", "gen-1"),
            durable_state("DispatchWakeLaunching", "owner-1", "gen-1"),
            durable_state("DispatchWakeDelivered", "owner-1", "gen-1"),
            durable_state("DispatchWakeCompleted", "owner-1", "gen-1"),
            durable_state("DispatchWakeReleased", "owner-1", "gen-1"),
        ];
        assert!(fold_durable_action(
            &release_after_completed,
            DurableActionKind::DispatchWake,
            &expectation
        )
        .is_err());
    }

    #[test]
    fn fold_rejects_missing_wrong_type_and_wrong_value_identity_fields() {
        let base_payload = || {
            serde_json::json!({
                "attemptId": "B97-A0004",
                "attemptNo": 4,
                "agent": "executor-opencode",
                "baseSha": "base",
                "goPath": "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0004.md",
                "actionId": "action-4",
                "owner": "owner-1",
                "leaseGeneration": "gen-1",
            })
        };
        let claim_with = |payload: serde_json::Value| {
            vec![orch_core::EventRecord {
                event_id: "e-claim".into(),
                ts: "2026-07-25T00:00:00Z".into(),
                actor: "runtime:test".into(),
                kind: "DispatchWakeClaimed".into(),
                task_id: Some("B97".into()),
                round: Some("r44".into()),
                payload: Some(payload),
                extra: serde_json::Map::new(),
            }]
        };
        for field in [
            "attemptId",
            "attemptNo",
            "agent",
            "baseSha",
            "goPath",
            "actionId",
            "owner",
            "leaseGeneration",
        ] {
            let mut missing = base_payload();
            missing.as_object_mut().unwrap().remove(field);
            assert!(
                fold_durable_action(
                    &claim_with(missing),
                    DurableActionKind::DispatchWake,
                    &durable_expectation()
                )
                .is_err(),
                "{field} 缺失必须 Err"
            );
            let mut wrong_type = base_payload();
            wrong_type[field] = serde_json::json!(123);
            assert!(
                fold_durable_action(
                    &claim_with(wrong_type),
                    DurableActionKind::DispatchWake,
                    &durable_expectation()
                )
                .is_err(),
                "{field} 类型错误必须 Err"
            );
        }
        // 值错误（非 owner/generation 的固定字段）→ Err；actionId 值错误视为
        // foreign → scope 内无事件 → Err。
        for field in [
            "attemptId",
            "attemptNo",
            "agent",
            "baseSha",
            "goPath",
            "actionId",
        ] {
            let mut wrong_value = base_payload();
            wrong_value[field] = serde_json::json!("wrong-value");
            assert!(
                fold_durable_action(
                    &claim_with(wrong_value),
                    DurableActionKind::DispatchWake,
                    &durable_expectation()
                )
                .is_err(),
                "{field} 值错误必须 Err"
            );
        }
        // 空串身份字段 → Err。
        for field in [
            "attemptId",
            "agent",
            "baseSha",
            "goPath",
            "owner",
            "leaseGeneration",
        ] {
            let mut empty = base_payload();
            empty[field] = serde_json::json!("");
            assert!(
                fold_durable_action(
                    &claim_with(empty),
                    DurableActionKind::DispatchWake,
                    &durable_expectation()
                )
                .is_err(),
                "{field} 空串必须 Err"
            );
        }
        // top-level round 错误 / 缺失 → Err。
        let mut wrong_round = claim_with(base_payload()).pop().unwrap();
        wrong_round.round = Some("r45".into());
        assert!(fold_durable_action(
            &[wrong_round],
            DurableActionKind::DispatchWake,
            &durable_expectation()
        )
        .is_err());
        let mut missing_round = claim_with(base_payload()).pop().unwrap();
        missing_round.round = None;
        assert!(fold_durable_action(
            &[missing_round],
            DurableActionKind::DispatchWake,
            &durable_expectation()
        )
        .is_err());
        // lineage：非 Claimed 事件的 owner / generation 必须等于锚点。
        let wrong_owner_lineage = vec![
            durable_state("DispatchWakeClaimed", "owner-1", "gen-1"),
            durable_state("DispatchWakeLaunching", "owner-2", "gen-1"),
        ];
        assert!(fold_durable_action(
            &wrong_owner_lineage,
            DurableActionKind::DispatchWake,
            &durable_expectation()
        )
        .is_err());
        let wrong_gen_lineage = vec![
            durable_state("DispatchWakeClaimed", "owner-1", "gen-1"),
            durable_state("DispatchWakeLaunching", "owner-1", "gen-2"),
        ];
        assert!(fold_durable_action(
            &wrong_gen_lineage,
            DurableActionKind::DispatchWake,
            &durable_expectation()
        )
        .is_err());
    }

    #[test]
    fn scope_has_events_distinguishes_first_call_from_history() {
        let expectation = durable_expectation();
        assert!(!durable_action_scope_has_events(
            &[],
            DurableActionKind::DispatchWake,
            &expectation
        ));
        // foreign action（不同非空 actionId）不算历史。
        let foreign = vec![durable_state_action(
            "DispatchWakeClaimed",
            "owner-9",
            "gen-9",
            "action-9",
        )];
        assert!(!durable_action_scope_has_events(
            &foreign,
            DurableActionKind::DispatchWake,
            &expectation
        ));
        // 本 action 的任意事件 → 有历史。
        let own = vec![durable_state("DispatchWakeClaimed", "owner-1", "gen-1")];
        assert!(durable_action_scope_has_events(
            &own,
            DurableActionKind::DispatchWake,
            &expectation
        ));
        // 不同 kind prefix 不算。
        assert!(!durable_action_scope_has_events(
            &own,
            DurableActionKind::ReportCollect,
            &expectation
        ));

        // Same action but wrong round, or malformed/missing actionId, is not
        // a pristine first call. Production must enter the common fold and
        // fail closed on the immutable identity violation.
        let mut wrong_round = own[0].clone();
        wrong_round.round = Some("r45".into());
        assert!(durable_action_scope_has_events(
            &[wrong_round],
            DurableActionKind::DispatchWake,
            &expectation
        ));
        let mut missing_action = own[0].clone();
        missing_action
            .payload
            .as_mut()
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("actionId");
        assert!(durable_action_scope_has_events(
            &[missing_action],
            DurableActionKind::DispatchWake,
            &expectation
        ));
        let mut wrong_type_action = own[0].clone();
        wrong_type_action.payload.as_mut().unwrap()["actionId"] = serde_json::json!(7);
        assert!(durable_action_scope_has_events(
            &[wrong_type_action],
            DurableActionKind::DispatchWake,
            &expectation
        ));
    }

    #[test]
    fn ack_same_inode_partial_recovery_completes_when_object_unchanged() {
        // positive-success：partial recovery 捕获的 ack 对象未被替换时，
        // GO mutation 放行并完成归档。
        let root = unique_repo("ack-positive");
        let go_rel =
            "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0001.md".to_string();
        let go_src = root.join(&go_rel);
        let ack_src = std::path::PathBuf::from(format!("{}.ack", go_src.display()));
        let go_dst = root.join(
            "coordination/rounds/r44/dispatch/executor-opencode/superseded/B97-A0001/GO-B97-A0001.md",
        );
        let ack_dst = std::path::PathBuf::from(format!("{}.ack", go_dst.display()));
        fs::create_dir_all(go_src.parent().unwrap()).unwrap();
        fs::create_dir_all(ack_dst.parent().unwrap()).unwrap();
        fs::write(&go_src, b"go-bytes").unwrap();
        fs::write(&ack_src, b"authorized-ack").unwrap();
        fs::hard_link(&ack_src, &ack_dst).unwrap();

        let mut saw_final_seam = false;
        archive_superseded_dispatch_with_hook(
            &root,
            &go_rel,
            "B97-A0001",
            "executor-opencode",
            &mut |phase| {
                if phase == "before-hard-link" {
                    saw_final_seam = true;
                }
                Ok(())
            },
        )
        .unwrap();
        assert!(saw_final_seam, "positive path must cross exact final seam");
        assert!(!go_src.exists(), "GO source 必须已归档");
        assert!(!ack_src.exists(), "ack source 必须已归档");
        assert_eq!(fs::read(&go_dst).unwrap(), b"go-bytes");
        assert_eq!(fs::read(&ack_dst).unwrap(), b"authorized-ack");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ack_authorized_object_removed_before_go_mutation_fails_closed() {
        // replacement-failure 的退化形：授权 ack 在 GO mutation 前被删除 → fail-closed。
        let root = unique_repo("ack-removed");
        let go_rel =
            "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0001.md".to_string();
        let go_src = root.join(&go_rel);
        let ack_src = std::path::PathBuf::from(format!("{}.ack", go_src.display()));
        let go_dst = root.join(
            "coordination/rounds/r44/dispatch/executor-opencode/superseded/B97-A0001/GO-B97-A0001.md",
        );
        let ack_dst = std::path::PathBuf::from(format!("{}.ack", go_dst.display()));
        fs::create_dir_all(go_src.parent().unwrap()).unwrap();
        fs::create_dir_all(ack_dst.parent().unwrap()).unwrap();
        fs::write(&go_src, b"go-bytes").unwrap();
        fs::write(&ack_src, b"authorized-ack").unwrap();
        fs::hard_link(&ack_src, &ack_dst).unwrap();

        let result = archive_superseded_dispatch_with_hook(
            &root,
            &go_rel,
            "B97-A0001",
            "executor-opencode",
            &mut |phase| {
                if phase == "before-move" {
                    fs::remove_file(&ack_dst)?;
                }
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(go_src.exists(), "GO 必须保持 live");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ack_replaced_at_before_hard_link_seam_keeps_go_live() {
        let root = unique_repo("ack-final-seam-replaced");
        let go_rel =
            "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0001.md".to_string();
        let go_src = root.join(&go_rel);
        let ack_src = std::path::PathBuf::from(format!("{}.ack", go_src.display()));
        let go_dst = root.join(
            "coordination/rounds/r44/dispatch/executor-opencode/superseded/B97-A0001/GO-B97-A0001.md",
        );
        let ack_dst = std::path::PathBuf::from(format!("{}.ack", go_dst.display()));
        fs::create_dir_all(go_src.parent().unwrap()).unwrap();
        fs::create_dir_all(ack_dst.parent().unwrap()).unwrap();
        fs::write(&go_src, b"go-live").unwrap();
        fs::write(&ack_src, b"authorized-ack").unwrap();
        fs::hard_link(&ack_src, &ack_dst).unwrap();

        let mut replaced = false;
        let result = archive_superseded_dispatch_with_hook(
            &root,
            &go_rel,
            "B97-A0001",
            "executor-opencode",
            &mut |phase| {
                if phase == "before-hard-link" && !replaced {
                    fs::remove_file(&ack_dst)?;
                    fs::write(&ack_dst, b"replacement-ack")?;
                    replaced = true;
                }
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(replaced);
        assert_eq!(fs::read(&go_src).unwrap(), b"go-live");
        assert!(!go_dst.exists(), "GO destination must not be published");
        fs::remove_dir_all(root).unwrap();
    }

    fn legacy_record_for(task: &str, attempt_id: &str) -> DispatchRecord {
        DispatchRecord {
            attempt: AttemptRef {
                task_id: task.into(),
                ordinal: 1,
                attempt_id: attempt_id.into(),
            },
            agent: "executor-opencode".into(),
            base_sha: Some("legacy-base".into()),
            go_path: format!("coordination/rounds/r44/dispatch/executor-opencode/GO-{task}.md"),
            is_legacy: true,
            previous_attempt_id: None,
            previous_agent: None,
            reassignment: false,
            has_companion_reassignment: false,
            wake_pending: false,
            wake_completed: false,
        }
    }

    #[test]
    fn legacy_resolver_folds_live_and_destination_per_schema_into_one_candidate() {
        // scoped-destination-only → 必须返回 scoped basename（不是 legacy basename）。
        let root = unique_repo("legacy-scoped-dst");
        let scoped_dst = root.join(
            "coordination/rounds/r44/dispatch/executor-opencode/superseded/B97-A0001/GO-B97-A0001.md",
        );
        fs::create_dir_all(scoped_dst.parent().unwrap()).unwrap();
        fs::write(&scoped_dst, b"archived-scoped").unwrap();
        let rel =
            resolve_superseded_go(&root, "r44", &legacy_record_for("B97", "B97-A0001")).unwrap();
        assert_eq!(
            rel,
            "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0001.md"
        );
        fs::remove_dir_all(root).unwrap();

        // cross-schema：legacy live + scoped destination 共存 → fail-closed。
        let root = unique_repo("legacy-cross");
        let legacy_live = root.join("coordination/rounds/r44/dispatch/executor-opencode/GO-B97.md");
        fs::create_dir_all(legacy_live.parent().unwrap()).unwrap();
        fs::write(&legacy_live, b"live-legacy").unwrap();
        let scoped_dst = root.join(
            "coordination/rounds/r44/dispatch/executor-opencode/superseded/B97-A0001/GO-B97-A0001.md",
        );
        fs::create_dir_all(scoped_dst.parent().unwrap()).unwrap();
        fs::write(&scoped_dst, b"archived-scoped").unwrap();
        assert!(
            resolve_superseded_go(&root, "r44", &legacy_record_for("B97", "B97-A0001")).is_err()
        );
        fs::remove_dir_all(root).unwrap();

        // zero candidates：live 与 destination 均缺 → fail-closed。
        let root = unique_repo("legacy-zero");
        fs::create_dir_all(&root).unwrap();
        assert!(
            resolve_superseded_go(&root, "r44", &legacy_record_for("B97", "B97-A0001")).is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_final_capture_catches_mutation_before_publish() {
        // final capture 回归：postchecks 之后、CAS publish 之前的同状态内容变更
        // （porcelain/HEAD/index 形状不变）必须被 tree3 捕获，不发布 archive ref。
        let root = init_repo();
        let wt = root.join(".worktrees/B97");
        fs::write(wt.join("untracked.txt"), "untracked\n").unwrap();
        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        };
        let result = snapshot_worktree_wip_with_hook(
            &root,
            "r44",
            &attempt,
            &mut |phase, _root, worktree| {
                if phase == "after-final-capture" {
                    fs::write(worktree.join("untracked.txt"), "mutated\n")?;
                }
                Ok(())
            },
        );
        assert!(result.is_err(), "final capture 必须捕获发布前变更");
        let ref_check = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["rev-parse", "--verify", "refs/archive/r44-B97-A0001-wip"])
            .output()
            .unwrap();
        assert!(!ref_check.status.success(), "变更后不得发布 archive ref");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_ref_publish_fence_catches_mutation_and_leaves_no_ref() {
        let root = init_repo();
        let wt = root.join(".worktrees/B97");
        fs::write(wt.join("untracked.txt"), "before\n").unwrap();
        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 6,
            attempt_id: "B97-A0006".into(),
        };
        let result = snapshot_worktree_wip_with_hook(
            &root,
            "r44",
            &attempt,
            &mut |phase, _root, worktree| {
                if phase == "before-ref-publish" {
                    fs::write(worktree.join("untracked.txt"), "mutated-at-publish\n")?;
                }
                Ok(())
            },
        );
        assert!(result.is_err());
        let archive = "refs/archive/r44-B97-A0006-wip";
        let output = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["show-ref", "--verify", "--quiet", archive])
            .status()
            .unwrap();
        assert!(!output.success(), "publish-fence failure must leave no ref");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_refuses_when_cooperative_lock_is_held() {
        let root = init_repo();
        let wt = root.join(".worktrees/B97");
        fs::write(wt.join("untracked.txt"), "untracked\n").unwrap();
        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        };
        let lock = root.join(".git/orch-snapshot-B97.lock");
        fs::write(&lock, b"held").unwrap();
        assert!(
            snapshot_worktree_wip(&root, "r44", &attempt).is_err(),
            "lock 被持有时必须 fail-closed"
        );
        fs::remove_file(&lock).unwrap();
        assert!(snapshot_worktree_wip(&root, "r44", &attempt)
            .unwrap()
            .is_some());
        assert!(!lock.exists(), "成功路径必须释放 lock");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn temp_index_paths_are_unique_per_call() {
        let root = Path::new("/tmp/nonexistent-orch-b97-temp-index");
        let first = gitx::temp_index_path(root, "B97-A0001-tree1");
        let second = gitx::temp_index_path(root, "B97-A0001-tree1");
        assert_ne!(first, second, "同 tag 连续调用必须产生唯一临时路径");
    }

    fn b204_dispatch(task: &str, ordinal: usize, previous_attempt: Option<&str>) -> EventRecord {
        let attempt = format_attempt_id(task, ordinal);
        let mut payload = serde_json::json!({
            "agent": "executor-desktop",
            "attemptId": attempt,
            "attemptNo": ordinal,
            "baseSha": "a".repeat(40),
            "goPath": format!(
                "coordination/rounds/r62/dispatch/executor-desktop/GO-{attempt}.md"
            ),
            "wakePending": false,
        });
        if let Some(previous_attempt) = previous_attempt {
            payload["previousAttemptId"] = serde_json::json!(previous_attempt);
            payload["previousAgent"] = serde_json::json!("executor-desktop");
            payload["reassignment"] = serde_json::json!(false);
        }
        crate::ledger::event(
            "DispatchIssued",
            "runtime:orch",
            Some(task),
            Some("r62"),
            payload,
        )
    }

    fn b204_root_pass(task: &str, ordinal: usize) -> EventRecord {
        let attempt = format_attempt_id(task, ordinal);
        crate::ledger::event(
            "VerdictIssued",
            "verifier:root",
            Some(task),
            Some("r62"),
            serde_json::to_value(crate::verify::RootVerdictPayload {
                verdict: "PASS".into(),
                reason: None,
                ir_revision: 1,
                validation_digest: "b".repeat(64),
                attempt_id: attempt,
                attempt_no: ordinal,
                implementer_agent: "executor-desktop".into(),
                head_sha: "c".repeat(40),
                main_head_sha: "d".repeat(40),
                collect_completed_event_id: format!("collect-{ordinal}"),
                bootstrap_pre_signoff_attempt: None,
                reviews: Vec::new(),
                evidence: Vec::new(),
                gates: Vec::new(),
            })
            .unwrap(),
        )
    }

    fn b204_approved_terminal(
        task: &str,
        ordinal: usize,
        verdict_event_id: &str,
        reason: &str,
    ) -> EventRecord {
        crate::ledger::event(
            "AttemptBlocked",
            "runtime:orch",
            Some(task),
            Some("r62"),
            serde_json::json!({
                "attemptId": format_attempt_id(task, ordinal),
                "attemptNo": ordinal,
                "agent": "executor-desktop",
                "stage": "approved-reattempt",
                "verdictEventId": verdict_event_id,
                "reason": reason,
            }),
        )
    }

    #[test]
    fn approved_reattempt_supports_replay_and_multiple_generations() {
        let task = "B204T";
        let first_dispatch = b204_dispatch(task, 1, None);
        let first_verdict = b204_root_pass(task, 1);
        let mut events = vec![first_dispatch, first_verdict.clone()];
        match approved_reattempt_decision(&events, "r62", task, "retry one").unwrap() {
            ApprovedReattemptDecision::Append {
                previous,
                next_attempt_id,
                ..
            } => {
                assert_eq!(previous.attempt_id, "B204T-A0001");
                assert_eq!(next_attempt_id, "B204T-A0002");
            }
            _ => panic!("first approved generation must append its terminal"),
        }
        events.push(b204_approved_terminal(
            task,
            1,
            &first_verdict.event_id,
            "retry one",
        ));
        match approved_reattempt_decision(&events, "r62", task, "retry one").unwrap() {
            ApprovedReattemptDecision::AlreadyPrepared {
                previous_attempt_id,
                next_attempt_id,
            } => {
                assert_eq!(previous_attempt_id, "B204T-A0001");
                assert_eq!(next_attempt_id, "B204T-A0002");
            }
            _ => panic!("crash before successor dispatch must replay terminal"),
        }
        events.push(b204_dispatch(task, 2, Some("B204T-A0001")));
        match approved_reattempt_decision(&events, "r62", task, "retry one").unwrap() {
            ApprovedReattemptDecision::AlreadyPrepared {
                previous_attempt_id,
                next_attempt_id,
            } => {
                assert_eq!(previous_attempt_id, "B204T-A0001");
                assert_eq!(next_attempt_id, "B204T-A0002");
            }
            _ => panic!("crash after successor dispatch must replay exact lineage"),
        }
        let second_verdict = b204_root_pass(task, 2);
        events.push(second_verdict);
        match approved_reattempt_decision(&events, "r62", task, "retry one").unwrap() {
            ApprovedReattemptDecision::AlreadyPrepared {
                previous_attempt_id,
                next_attempt_id,
            } => {
                assert_eq!(previous_attempt_id, "B204T-A0001");
                assert_eq!(next_attempt_id, "B204T-A0002");
            }
            _ => panic!("delayed replay must not reinterpret A1->A2 as A2->A3"),
        }
        match approved_reattempt_decision(&events, "r62", task, "retry two").unwrap() {
            ApprovedReattemptDecision::Append {
                previous,
                next_attempt_id,
                ..
            } => {
                assert_eq!(previous.attempt_id, "B204T-A0002");
                assert_eq!(next_attempt_id, "B204T-A0003");
            }
            _ => panic!("a later approved attempt may mint the next generation"),
        }
    }

    #[test]
    fn approved_reattempt_rejects_reason_drift_and_forged_durable_completion() {
        let task = "B204U";
        let verdict = b204_root_pass(task, 1);
        let mut events = vec![b204_dispatch(task, 1, None), verdict.clone()];
        events.push(b204_approved_terminal(
            task,
            1,
            &verdict.event_id,
            "original reason",
        ));
        let error = approved_reattempt_decision(&events, "r62", task, "changed reason")
            .err()
            .expect("changed replay reason must fail")
            .to_string();
        assert!(error.contains("conflicts"), "{error}");

        let task = "B204V";
        let mut forged = vec![b204_dispatch(task, 1, None), b204_root_pass(task, 1)];
        forged.push(crate::ledger::event(
            "DispatchWakeCompleted",
            "runtime:orch",
            Some(task),
            Some("r62"),
            serde_json::json!({
                "actionId": "forged-completion",
                "attemptId": "B204V-A0001",
                "attemptNo": 1,
                "agent": "executor-desktop",
                "baseSha": "a".repeat(40),
                "goPath": "coordination/rounds/r62/dispatch/executor-desktop/GO-B204V-A0001.md",
                "owner": "owner",
                "leaseGeneration": "generation",
            }),
        ));
        let error = approved_reattempt_decision(&forged, "r62", task, "retry")
            .err()
            .expect("terminal without durable predecessors must fail")
            .to_string();
        assert!(
            error.contains("Claimed") || error.contains("前驱"),
            "{error}"
        );
    }

    #[test]
    fn approved_reattempt_crash_terminal_requires_exact_explicit_context() {
        let task = "B204W";
        let reason = "operator-approved retry";
        let verdict = b204_root_pass(task, 1);
        let events = vec![
            b204_dispatch(task, 1, None),
            verdict.clone(),
            b204_approved_terminal(task, 1, &verdict.event_id, reason),
        ];

        let plain = plan_dispatch_locked(&events, task, "executor-desktop", &"d".repeat(40), "r62")
            .err()
            .expect("plain dispatch must reject the approved crash terminal")
            .to_string();
        assert!(plain.contains("explicit --new-attempt"), "{plain}");

        {
            let _wrong = ApprovedReattemptExplicitScope::enter(task, "different reason").unwrap();
            let wrong =
                plan_dispatch_locked(&events, task, "executor-desktop", &"d".repeat(40), "r62")
                    .err()
                    .expect("reason drift must reject the approved crash terminal")
                    .to_string();
            assert!(wrong.contains("exact reason"), "{wrong}");
        }

        let _explicit = ApprovedReattemptExplicitScope::enter(task, reason).unwrap();
        let plan = plan_dispatch_locked(&events, task, "executor-desktop", &"d".repeat(40), "r62")
            .unwrap();
        match plan {
            DispatchPlan::New { next, .. } => {
                assert_eq!(next.attempt.attempt_id, "B204W-A0002");
                assert_eq!(next.base_sha, "a".repeat(40));
            }
            DispatchPlan::Existing { .. } => panic!("explicit recovery must mint A0002"),
        }
    }

    #[test]
    fn approved_reattempt_refuses_live_managed_review_and_attach_writers() {
        let task = "B204X";
        let attempt = "B204X-A0001";
        let wake_id = "019fc204-1111-4222-8333-444455556666";
        let mut events = vec![
            crate::ledger::event(
                "ReviewRequested",
                "runtime:orch",
                Some(task),
                Some("r62"),
                serde_json::json!({"attemptId": attempt, "wakeId": wake_id}),
            ),
            crate::ledger::event(
                "WakeIssued",
                "runtime:orch",
                Some(task),
                Some("r62"),
                serde_json::json!({
                    "attemptId": attempt,
                    "wakeId": wake_id,
                    "controlWakeId": wake_id,
                    "agent": "executor-opencode",
                }),
            ),
        ];
        let live = ensure_no_inflight_managed_review(&events, "r62", task, attempt)
            .unwrap_err()
            .to_string();
        assert!(live.contains("expected one terminal"), "{live}");

        events.push(crate::ledger::event(
            "ManagedWakeTerminated",
            "runtime:orch",
            Some(task),
            Some("r62"),
            serde_json::json!({
                "wakeId": wake_id,
                "agent": "executor-opencode",
                "managedScopeTerminated": true,
            }),
        ));
        ensure_no_inflight_managed_review(&events, "r62", task, attempt).unwrap();

        events.push(crate::ledger::event(
            "ManagedWakeAttachState",
            "runtime:orch",
            Some(task),
            Some("r62"),
            serde_json::json!({
                "attemptId": attempt,
                "actionId": "attach-action",
                "phase": "claimed",
            }),
        ));
        let attach = ensure_no_inflight_managed_review(&events, "r62", task, attempt)
            .unwrap_err()
            .to_string();
        assert!(
            attach.contains("in-flight managed review attach"),
            "{attach}"
        );
    }
}
