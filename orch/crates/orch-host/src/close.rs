//! `orch merge`：收口——账本核 Approved → no-ff 合并 → 即时落 MergeExecuted → 合后逐项门
//! → 全绿后落 TaskRecorded → 人读尾务/清理（best-effort，不推翻已完成事实）。
//! 纪律：先验证账本状态才动 git（E8 反向：动作前核状态、动作后写账目）；
//! merge 不可逆——main 推进后任何后续失败只升级、不 reset/回滚。

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use fd_lock::RwLock;
use orch_core::{fold, read_ledger, EventRecord, TaskState};

use crate::{binding, buildcache, card, gate, gitx, ledger, wake};

fn note_gate_orphan_evidence(log_dir: &Path, tag: &str, gate_name: &str, observation: &str) {
    if let Err(error) = gate::append_gate_orphan_evidence(log_dir, tag, gate_name, observation) {
        eprintln!(
            "gate orphan evidence degraded to stderr for {tag}/{gate_name}: {error:#}; {observation}"
        );
    }
}

fn close_orphan_baseline(log_dir: &Path, tag: &str, gate_name: &str) -> Option<BTreeSet<u32>> {
    match gate::orphan_baseline() {
        Ok(baseline) => Some(baseline),
        Err(error) => {
            note_gate_orphan_evidence(
                log_dir,
                tag,
                gate_name,
                &format!("phase=baseline snapshot=degraded error={error:#}"),
            );
            None
        }
    }
}

fn finish_close_orphan_watch(
    root: &Path,
    round: &str,
    task_id: &str,
    log_dir: &Path,
    tag: &str,
    gate_name: &str,
    baseline: Option<&BTreeSet<u32>>,
) -> Result<()> {
    let Some(baseline) = baseline else {
        return Ok(());
    };
    let first = match gate::orphans_since(baseline) {
        Ok(rows) => rows,
        Err(error) => {
            note_gate_orphan_evidence(
                log_dir,
                tag,
                gate_name,
                &format!("phase=post-gate sample=first snapshot=degraded error={error:#}"),
            );
            return Ok(());
        }
    };
    std::thread::sleep(Duration::from_millis(50));
    let second = match gate::orphans_since(baseline) {
        Ok(rows) => rows,
        Err(error) => {
            note_gate_orphan_evidence(
                log_dir,
                tag,
                gate_name,
                &format!("phase=post-gate sample=second snapshot=degraded error={error:#}"),
            );
            return Ok(());
        }
    };
    let persistent = gate::persistent_gate_orphans(&first, &second);
    let (reportable, evidence_only) = gate::partition_gate_orphans(&persistent);
    note_gate_orphan_evidence(
        log_dir,
        tag,
        gate_name,
        &format!(
            "phase=post-gate first={first:?} second={second:?} persistent={persistent:?} reportable={reportable:?} evidence_only={evidence_only:?}"
        ),
    );
    if !reportable.is_empty() {
        ledger::append(
            root,
            round,
            &[gate::orphan_failure_event(task_id, round, &reportable)],
        )?;
    }
    Ok(())
}

fn with_close_orphan_watch<T>(
    root: &Path,
    round: &str,
    task_id: &str,
    log_dir: &Path,
    tag: &str,
    gate_name: &str,
    run: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let baseline = close_orphan_baseline(log_dir, tag, gate_name);
    let result = run();
    let observation = finish_close_orphan_watch(
        root,
        round,
        task_id,
        log_dir,
        tag,
        gate_name,
        baseline.as_ref(),
    );
    match (result, observation) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(observation_error)) => {
            eprintln!(
                "gate failed and orphan accounting also failed for {tag}/{gate_name}: {observation_error:#}"
            );
            Err(error)
        }
    }
}

/// Round-close production policy for trial build generations.  Keeping one generation per slot
/// preserves the next warm-start opportunity while bounding all older evidence generations.
pub fn sweep_trial_cache_before_round_close(
    root: &Path,
) -> Result<buildcache::TrialCacheSweepReport> {
    buildcache::sweep_trial_cache(root, 1)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProtocolLeaseState {
    None,
    Shared {
        root: PathBuf,
        depth: u32,
    },
    Exclusive {
        root: PathBuf,
        depth: u32,
        merge_lifecycle: bool,
    },
}

thread_local! {
    /// Process-local nesting token.  `flock` is intentionally process-visible,
    /// so ledger append must not reopen/relock `merge.lock` while this thread
    /// already owns the shared or exclusive protocol lease.
    static PROTOCOL_LEASE_STATE: RefCell<ProtocolLeaseState> =
        const { RefCell::new(ProtocolLeaseState::None) };
}

struct ProtocolLeaseScope {
    previous: ProtocolLeaseState,
    entered: ProtocolLeaseState,
}

impl ProtocolLeaseScope {
    fn enter(next: ProtocolLeaseState) -> Self {
        let previous = PROTOCOL_LEASE_STATE.with(|state| state.replace(next.clone()));
        Self {
            previous,
            entered: next,
        }
    }
}

impl Drop for ProtocolLeaseScope {
    fn drop(&mut self) {
        PROTOCOL_LEASE_STATE.with(|state| {
            let current = state.borrow().clone();
            assert_eq!(
                current, self.entered,
                "unbalanced protocol lease nesting: entered={:?} current={:?}",
                self.entered, current
            );
            *state.borrow_mut() = self.previous.clone();
        });
    }
}

fn protocol_lease_state() -> ProtocolLeaseState {
    PROTOCOL_LEASE_STATE.with(|state| state.borrow().clone())
}

fn protocol_root(root: &Path) -> Result<PathBuf> {
    fs::canonicalize(root)
        .with_context(|| format!("解析 protocol lease root 失败: {}", root.display()))
}

fn protocol_lock(root: &Path) -> Result<(PathBuf, RwLock<std::fs::File>)> {
    let identity = protocol_root(root)?;
    let lock_dir = root.join("coordination/runtime/locks");
    fs::create_dir_all(&lock_dir).context("创建 protocol-transition lock 目录失败")?;
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_dir.join("merge.lock"))
        .context("打开 protocol-transition lease 失败")?;
    Ok((identity, RwLock::new(lock_file)))
}

/// Serialize protocol transitions that can change (or consume) the active
/// round, IR, or fixed-HEAD authorization.  The lease is deliberately
/// fail-fast: a synchronous git hook may invoke another transition while
/// `run_merge` owns the lease, and blocking there would self-deadlock.
///
/// Every caller must acquire this lease before its first semantic side effect
/// and keep it outside any ledger lease (lock order: transition -> ledger).
pub fn with_protocol_transition<T>(
    root: &Path,
    operation: &str,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    with_protocol_transition_mode(root, operation, false, None, action)
}

/// 带被拒 identity 的 transition 入口（B153-A0002）：barrier preflight 拒收
/// 消息同时点名被拒 task 与屏障归属 task。`refused_task=None` 与
/// [`with_protocol_transition`] 完全等价（旧调用点零改动）。
pub fn with_protocol_transition_named<T>(
    root: &Path,
    operation: &str,
    refused_task: Option<&str>,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    with_protocol_transition_mode(root, operation, false, refused_task, action)
}

fn with_merge_lifecycle_transition<T>(
    root: &Path,
    operation: &str,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    with_protocol_transition_mode(root, operation, true, None, action)
}

fn with_protocol_transition_mode<T>(
    root: &Path,
    operation: &str,
    merge_lifecycle: bool,
    refused_task: Option<&str>,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let identity = protocol_root(root)?;
    match protocol_lease_state() {
        ProtocolLeaseState::Shared { root: held, .. } => {
            bail!(
                "shared→exclusive protocol lease upgrade rejected：{operation} root={} heldRoot={}",
                identity.display(),
                held.display()
            )
        }
        ProtocolLeaseState::Exclusive {
            root: held,
            depth,
            merge_lifecycle: held_capability,
        } => {
            if held != identity {
                bail!(
                    "cross-root nested exclusive protocol lease rejected：{operation} root={} heldRoot={}",
                    identity.display(),
                    held.display()
                );
            }
            if merge_lifecycle && !held_capability {
                bail!("nested protocol lease cannot escalate to merge-lifecycle capability");
            }
            let next = depth
                .checked_add(1)
                .context("protocol-transition nesting depth overflow")?;
            let _scope = ProtocolLeaseScope::enter(ProtocolLeaseState::Exclusive {
                root: identity,
                depth: next,
                merge_lifecycle: held_capability,
            });
            if !merge_lifecycle {
                ledger::ensure_no_unresolved_merge_barrier_named(root, operation, refused_task)?;
            }
            return action();
        }
        ProtocolLeaseState::None => {}
    }

    let (locked_identity, mut transition_lock) = protocol_lock(root)?;
    debug_assert_eq!(locked_identity, identity);
    let _transition_guard = match transition_lock.try_write() {
        Ok(guard) => guard,
        Err(error) if error.kind() == ErrorKind::WouldBlock => {
            bail!(
                "protocol-transition lease busy：{operation} fail-fast 拒绝等待；请在当前 transition 完成后重试"
            )
        }
        Err(error) => return Err(error).context("获取 protocol-transition lease 失败"),
    };
    let _scope = ProtocolLeaseScope::enter(ProtocolLeaseState::Exclusive {
        root: identity,
        depth: 1,
        merge_lifecycle,
    });
    if !merge_lifecycle {
        ledger::ensure_no_unresolved_merge_barrier_named(root, operation, refused_task)?;
    }
    action()
}

/// Hold the shared side of the protocol R/W lease across a reversible effect
/// (GO/file creation, provider spawn/wake, or an ordinary ledger append).
///
/// Shared nesting is explicit and thread-local: public Tier-F wrappers may
/// call lower-level wake/ledger helpers without trying to relock the same file.
/// An exclusive owner may likewise append its canonical terminal facts.  A
/// fresh thread/process has no token and must acquire the OS lease, so hooks
/// and concurrent CLIs still fail fast while merge owns exclusive access.
pub fn with_protocol_effect<T>(
    root: &Path,
    operation: &str,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    with_protocol_shared(root, operation, true, None, action)
}

/// 带被拒 identity 的 effect 入口（B153-A0002）：barrier preflight 拒收消息
/// 同时点名被拒 task 与屏障归属 task。`refused_task=None` 与
/// [`with_protocol_effect`] 完全等价（旧调用点零改动）。
pub fn with_protocol_effect_named<T>(
    root: &Path,
    operation: &str,
    refused_task: Option<&str>,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    with_protocol_shared(root, operation, true, refused_task, action)
}

pub(crate) fn with_protocol_ledger_effect<T>(
    root: &Path,
    operation: &str,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    with_protocol_shared(root, operation, false, None, action)
}

fn with_protocol_shared<T>(
    root: &Path,
    operation: &str,
    ensure_open: bool,
    refused_task: Option<&str>,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let identity = protocol_root(root)?;
    match protocol_lease_state() {
        ProtocolLeaseState::Exclusive {
            root: held,
            depth,
            merge_lifecycle,
        } => {
            if held != identity {
                bail!(
                    "cross-root nested protocol effect rejected：{operation} root={} heldRoot={}",
                    identity.display(),
                    held.display()
                );
            }
            // A merge capability is deliberately lexical, not ambient.  An
            // ordinary effect invoked anywhere below run_merge/run_record must
            // not inherit authority to mint lifecycle facts.  Only the
            // restricted append_checked_merge_lifecycle callsites execute
            // while the capability bit remains set.
            let _scope = ProtocolLeaseScope::enter(ProtocolLeaseState::Exclusive {
                root: identity,
                depth,
                merge_lifecycle: false,
            });
            if ensure_open && !merge_lifecycle {
                ledger::ensure_no_unresolved_merge_barrier_named(root, operation, refused_task)?;
            }
            action()
        }
        ProtocolLeaseState::Shared { root: held, depth } => {
            if held != identity {
                bail!(
                    "cross-root nested protocol effect rejected：{operation} root={} heldRoot={}",
                    identity.display(),
                    held.display()
                );
            }
            let next = depth
                .checked_add(1)
                .context("protocol-effect shared nesting depth overflow")?;
            let _scope = ProtocolLeaseScope::enter(ProtocolLeaseState::Shared {
                root: identity,
                depth: next,
            });
            action()
        }
        ProtocolLeaseState::None => {
            let (locked_identity, effect_lock) = protocol_lock(root)?;
            debug_assert_eq!(locked_identity, identity);
            let _effect_guard = match effect_lock.try_read() {
                Ok(guard) => guard,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    bail!(
                        "protocol-effect lease busy：{operation} fail-fast 拒绝等待；请在当前 transition 完成后重试"
                    )
                }
                Err(error) => return Err(error).context("获取 protocol-effect shared lease 失败"),
            };
            let _scope = ProtocolLeaseScope::enter(ProtocolLeaseState::Shared {
                root: identity,
                depth: 1,
            });
            if ensure_open {
                ledger::ensure_no_unresolved_merge_barrier_named(root, operation, refused_task)?;
            }
            action()
        }
    }
}

pub(crate) fn has_merge_lifecycle_capability(root: &Path) -> Result<bool> {
    let identity = protocol_root(root)?;
    Ok(matches!(
        protocol_lease_state(),
        ProtocolLeaseState::Exclusive {
            root,
            merge_lifecycle: true,
            ..
        } if root == identity
    ))
}

pub struct MergeOutcome {
    pub merge_sha_short: String,
    pub gates: Vec<gate::GateResult>,
}

/// Result of the one-command root verdict/merge/record lifecycle.
pub struct SealOutcome {
    pub merge_sha_short: String,
    pub gates: Vec<gate::GateResult>,
    /// True when the complete durable chain already existed on entry.
    pub replayed_complete: bool,
}

impl std::fmt::Debug for SealOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SealOutcome")
            .field("merge_sha_short", &self.merge_sha_short)
            .field("gates", &self.gates.len())
            .field("replayed_complete", &self.replayed_complete)
            .finish()
    }
}

/// git merge 命令失败的结构化现场：stdout（CONFLICT 行所在）与 stderr 都保留，
/// 供冲突文件清单解析与边界违例归因各自取用。
struct MergeCommandFailure {
    status: i32,
    stdout: String,
    stderr: String,
}

const MAIN_GUARD_BLOCK_MARKER: &str = "orch main guard: BLOCK:";

/// A reference-transaction rejection is not a content conflict.  In
/// particular, accounting it as `merge-conflict` would permanently terminate
/// an otherwise valid approved attempt merely because the guard could not
/// prove its authorization.  Keep the already-durable `MergeStarted` barrier
/// open so the operator can repair the guard/ledger and retry or use the
/// existing recovery command.
fn merge_was_blocked_by_main_guard(failure: &MergeCommandFailure) -> bool {
    failure.stdout.contains(MAIN_GUARD_BLOCK_MARKER)
        || failure.stderr.contains(MAIN_GUARD_BLOCK_MARKER)
}

impl std::fmt::Display for MergeCommandFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "git merge --no-verify 失败 (exit={}): {}",
            self.status,
            self.stderr.trim()
        )
    }
}

fn merge_no_ff_without_hooks(
    root: &Path,
    head: &str,
    message: &str,
) -> std::result::Result<(), MergeCommandFailure> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["merge", "--no-ff", "--no-verify", head, "-m", message])
        // `--no-verify` does not skip reference-transaction hooks.  This
        // narrowly-scoped marker is set only on orch's authorized no-ff
        // command; ORCH_MAIN_GUARD_BYPASS remains an operator-only emergency
        // escape hatch and is never injected by ordinary runtime plumbing.
        .env("ORCH_MAIN_GUARD_CONTEXT", "authorized-no-ff-merge")
        .output();
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            return Err(MergeCommandFailure {
                status: -1,
                stdout: String::new(),
                stderr: format!("启动 git merge --no-verify 失败: {error}"),
            })
        }
    };
    if !output.status.success() {
        return Err(MergeCommandFailure {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(())
}

/// 从 git merge 输出解析冲突文件清单。覆盖两种行形：
/// `CONFLICT (content): Merge conflict in <path>`（含 add/add 等同形）与
/// `CONFLICT (modify/delete): <path> deleted in ...`。保序去重。
fn parse_conflict_files(output: &str) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    let mut push = |path: &str| {
        let path = path.trim();
        if !path.is_empty() && !files.iter().any(|seen| seen == path) {
            files.push(path.to_string());
        }
    };
    for line in output.lines() {
        let line = line.trim();
        if !line.starts_with("CONFLICT") {
            continue;
        }
        if let Some((_, path)) = line.rsplit_once("Merge conflict in ") {
            push(path);
        } else if let Some(rest) = line.strip_prefix("CONFLICT (modify/delete): ") {
            if let Some((path, _)) = rest.split_once(" deleted in ") {
                push(path);
            }
        }
    }
    files
}

/// 冲突文件清单：先解析 merge 输出；解析不到时回退 `git diff --name-only
/// --diff-filter=U`（merge 中途现场的未合并路径）；再拿不到给空数组——
/// 清单是证据附件，缺失绝不阻断落账（B153）。
fn conflict_files_from_merge(root: &Path, failure: &MergeCommandFailure) -> Vec<String> {
    let mut files = parse_conflict_files(&failure.stdout);
    for path in parse_conflict_files(&failure.stderr) {
        if !files.iter().any(|seen| seen == &path) {
            files.push(path);
        }
    }
    if files.is_empty() {
        if let Ok(output) = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["diff", "--name-only", "--diff-filter=U"])
            .output()
        {
            if output.status.success() {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    let path = line.trim();
                    if !path.is_empty() && !files.iter().any(|seen| seen == path) {
                        files.push(path.to_string());
                    }
                }
            }
        }
    }
    files
}

/// P1（r52 主审+复审）：merge-conflict 落账失败后的诚实诊断。闭合判定必须
/// 绑定「当前」这次 merge 的屏障——用与拒收/恢复同一组 canonical 谓词与
/// 状态机（`ledger::merge_barrier_closure`）判定本 barrier：起点
/// （canonical MergeStarted）在场且已被终态事实闭合才算「已落账」；绝不
/// 在全历史里 `.any` 一个不带 attemptId 的 merge-conflict——旧 attempt 的
/// 历史冲突事件闭合的是旧 barrier，会让新悬空屏障被误判为「已闭合」（复审
/// 点名的 A0001 闭合→A0002 新 MergeStarted→A0002 落账失败序列）。
/// 本批（merge-conflict escalation + AttemptBlocked）单次 write_all 原子
/// 落账：当前 barrier 闭合 ⟺ 闭合它的 escalation 在场 ⟺ 整批已落
/// （WAL 失败时事件实际已落账，回读可见闭合事实——该方向复审确认保持）。
/// 回读失败或账本含坏行则报「闭合状态未知」；三个分支都携带 accounting
/// error，未证实前绝不宣称已闭合。
fn conflict_closure_diagnosis(
    ledger_path: &Path,
    task_id: &str,
    round: &str,
    account_error: &anyhow::Error,
    failure: &MergeCommandFailure,
) -> String {
    let landed = match read_ledger(ledger_path) {
        Ok(lr) if lr.bad_lines.is_empty() => {
            Some(
                match ledger::merge_barrier_closure(&lr.events, task_id, round) {
                    // 本 barrier 的 MergeStarted 在场且状态机确认其已闭合：闭合它
                    // 的正是本批 merge-conflict escalation（同批原子 write_all，
                    // escalation 在 ⟺ 含 AttemptBlocked 的整批在）。
                    ledger::MergeBarrierClosure::Closed => true,
                    // Active：闭合事实未落账，当前 barrier 仍悬空——哪怕历史里有
                    // 旧 attempt 的 merge-conflict。Absent：连本 barrier 的
                    // MergeStarted 都不可见，闭合证据无从谈起。两者都按未落账。
                    _ => false,
                },
            )
        }
        // 读取失败或账本含坏行：无法判定，按未知处理（fail-closed 文案）。
        _ => None,
    };
    match landed {
        Some(true) => format!(
            "git merge 冲突失败；merge-conflict 落账返回错误（{account_error:#}），但账本回读确认该批事件已实际落账——\
             当前屏障已闭合（错误发生在 ledger 落盘后的 WAL/收尾阶段），可重派冲突修复: {failure}"
        ),
        Some(false) => format!(
            "git merge 冲突失败；merge-conflict 落账失败（{account_error:#}）且账本回读确认事件未落账——\
             屏障仍悬空，需 `orch merge --recover {task_id}` 恢复: {failure}"
        ),
        None => format!(
            "git merge 冲突失败；merge-conflict 落账失败（{account_error:#}）且账本回读失败——\
             屏障闭合状态未知，需人工核查账本后走 `orch merge --recover {task_id}` 恢复: {failure}"
        ),
    }
}

/// 冲突失败后主工作区可能停在 MERGING 态。refs 未动已由调用方证明，故
/// `git merge --abort` 只是把主工作区还原到 merge 前状态（不动任何 ref），
/// 失败只警告——现场保留供人工检查，绝不阻断已完成的落账。
fn abort_inflight_merge_best_effort(root: &Path) {
    let in_flight = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "-q", "MERGE_HEAD"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if !in_flight {
        return;
    }
    match Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["merge", "--abort"])
        .output()
    {
        Ok(output) if output.status.success() => {}
        Ok(output) => eprintln!(
            "[orch] 警告：git merge --abort 失败: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(error) => eprintln!("[orch] 警告：git merge --abort 启动失败: {error}"),
    }
}

// gate::GateResult 未派生 Debug（gate.rs 不在本棒 writeSet），手写 impl 只暴露安全字段。
impl std::fmt::Debug for MergeOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MergeOutcome")
            .field("merge_sha_short", &self.merge_sha_short)
            .field("gates", &self.gates.len())
            .finish()
    }
}

/// git merge 成功后立即落账的真值事件（actor 固定 reviewer:orch-runtime）。
pub fn merge_executed_event(task: &str, round: &str, sha: &str) -> EventRecord {
    ledger::event(
        "MergeExecuted",
        "reviewer:orch-runtime",
        Some(task),
        Some(round),
        serde_json::json!({"mergeSha": sha, "policy": "no-ff"}),
    )
}

fn append_merge_lifecycle_events(root: &Path, round: &str, events: Vec<EventRecord>) -> Result<()> {
    let expected = events.len();
    let appended = ledger::append_checked_merge_lifecycle(root, round, move |_| Ok(events))?;
    if appended != expected {
        bail!("merge lifecycle append count mismatch: expected {expected}, appended {appended}");
    }
    Ok(())
}

/// 合后门任一红即落的升级事实；绝不伪装成 TaskRecorded。
pub fn post_merge_gate_failed_event(
    task: &str,
    round: &str,
    sha: &str,
    gate: &str,
    exit: i32,
) -> EventRecord {
    ledger::event(
        "EscalationRaised",
        "reviewer:orch-runtime",
        Some(task),
        Some(round),
        serde_json::json!({
            "stage": "post-merge-gate",
            "gate": gate,
            "exit": exit,
            "mergeSha": sha,
            "reason": format!("合并后门 {gate} 红（exit {exit}）"),
            "hint": "main 已含合并，禁止 reset/回滚；需人工裁决修复后再验",
        }),
    )
}

/// 全部合后门绿之后才允许落的单条记账事件。
pub fn task_recorded_event(task: &str, round: &str) -> EventRecord {
    ledger::event(
        "TaskRecorded",
        "runtime:orch",
        Some(task),
        Some(round),
        serde_json::json!({"postMergeGates": "all-green"}),
    )
}

fn task_recorded_batch(events: &[EventRecord], task: &str, round: &str) -> Vec<EventRecord> {
    let recorded = task_recorded_event(task, round);
    let mut batch = vec![recorded.clone()];
    batch.extend(crate::sites::retire_task_sites(
        events,
        task,
        &recorded.event_id,
    ));
    batch
}

/// Prove the exact successful lifecycle suffix for one attempt.
///
/// Historical failed attempts are deliberately ignored: the chain is
/// anchored at the one PASS verdict whose payload names `attempt_id`, and
/// every lifecycle count is taken only after that anchor.  This function is a
/// pure ledger predicate; the live seal path additionally revalidates refs,
/// bound artifacts, and the ledger/WAL byte mirror before returning success.
pub fn validate_seal_postcondition(
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
) -> Result<()> {
    let scoped = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.task_id.as_deref() == Some(task_id) && event.round.as_deref() == Some(round)
        })
        .collect::<Vec<_>>();

    let verdicts = scoped
        .iter()
        .filter(|(_, event)| {
            event.kind == "VerdictIssued"
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("attemptId"))
                    .and_then(serde_json::Value::as_str)
                    == Some(attempt_id)
        })
        .collect::<Vec<_>>();
    if verdicts.len() != 1 {
        bail!(
            "seal postcondition requires exactly one VerdictIssued for {attempt_id}, found {}",
            verdicts.len()
        );
    }
    let (verdict_position, verdict) = *verdicts[0];
    let verdict_value = verdict
        .payload
        .as_ref()
        .context("seal postcondition VerdictIssued missing payload")?;
    // The frozen seed exercises a deliberately minimal pure-kernel shape.
    // Production events are distinguishable by irRevision and take the full
    // exact branch; the minimal branch is exact to its two-key fixture schema
    // and is never emitted by run_seal.
    let production_verdict = if verdict_value.get("irRevision").is_some() {
        let payload: crate::verify::RootVerdictPayload =
            serde_json::from_value(verdict_value.clone())
                .context("seal postcondition VerdictIssued payload is not exact")?;
        if verdict.actor != "verifier:root"
            || payload.verdict != "PASS"
            || payload.reason.is_some()
            || payload.attempt_id != attempt_id
        {
            bail!("seal postcondition VerdictIssued is not an exact root PASS");
        }
        Some(payload)
    } else {
        if verdict.actor != "runtime:orch"
            || verdict_value != &serde_json::json!({"attemptId": attempt_id, "verdict": "PASS"})
        {
            bail!("seal postcondition minimal VerdictIssued fixture is not exact");
        }
        None
    };

    let after_verdict = scoped
        .iter()
        .copied()
        .filter(|(position, _)| *position > verdict_position)
        .collect::<Vec<_>>();
    let starts = after_verdict
        .iter()
        .filter(|(_, event)| {
            event.kind == "MergeStarted"
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("attemptId"))
                    .and_then(serde_json::Value::as_str)
                    == Some(attempt_id)
        })
        .collect::<Vec<_>>();
    if starts.len() != 1 {
        bail!(
            "seal postcondition requires exactly one MergeStarted for {attempt_id}, found {}",
            starts.len()
        );
    }
    let (started_position, started_event) = *starts[0];
    let started_exact = if let Some(verdict_payload) = production_verdict.as_ref() {
        started_event.actor == "runtime:orch"
            && started_event.payload.as_ref()
                == Some(&serde_json::json!({
                    "attemptId": verdict_payload.attempt_id,
                    "attemptNo": verdict_payload.attempt_no,
                    "headSha": verdict_payload.head_sha,
                    "mainHeadSha": verdict_payload.main_head_sha,
                    "collectCompletedEventId": verdict_payload.collect_completed_event_id,
                    "verdictEventId": verdict.event_id,
                }))
    } else {
        started_event.actor == "runtime:orch"
            && started_event.payload.as_ref()
                == Some(&serde_json::json!({
                    "attemptId": attempt_id,
                    "verdictEventId": "verdict",
                }))
    };
    if !started_exact {
        bail!("seal postcondition MergeStarted does not exactly bind root PASS");
    }

    let merges = after_verdict
        .iter()
        .filter(|(_, event)| event.kind == "MergeExecuted")
        .collect::<Vec<_>>();
    if merges.len() != 1 {
        bail!(
            "seal postcondition requires exactly one MergeExecuted, found {}",
            merges.len()
        );
    }
    let (merge_position, merge) = *merges[0];
    let merge_payload = merge
        .payload
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .context("seal postcondition MergeExecuted payload is not an object")?;
    let merge_sha = merge_payload
        .get("mergeSha")
        .and_then(serde_json::Value::as_str)
        .context("seal postcondition MergeExecuted missing mergeSha")?;
    let merge_envelope_exact = if production_verdict.is_some() {
        merge.actor == "reviewer:orch-runtime"
            && merge_payload.len() == 2
            && merge_payload.contains_key("mergeSha")
            && merge_payload
                .get("policy")
                .and_then(serde_json::Value::as_str)
                == Some("no-ff")
    } else {
        merge.actor == "runtime:orch"
            && merge_payload.len() == 1
            && merge_payload.contains_key("mergeSha")
    };
    if !merge_envelope_exact
        || merge_sha.len() != 40
        || !merge_sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("seal postcondition MergeExecuted mergeSha is not a full lowercase SHA");
    }

    let recorded = after_verdict
        .iter()
        .filter(|(_, event)| event.kind == "TaskRecorded")
        .collect::<Vec<_>>();
    if recorded.len() != 1 {
        bail!(
            "seal postcondition requires exactly one TaskRecorded, found {}",
            recorded.len()
        );
    }
    let (recorded_position, recorded_event) = *recorded[0];
    if recorded_event.actor != "runtime:orch"
        || recorded_event.payload.as_ref()
            != Some(&serde_json::json!({"postMergeGates": "all-green"}))
    {
        bail!("seal postcondition TaskRecorded is not all-green");
    }
    if !(verdict_position < started_position
        && started_position < merge_position
        && merge_position < recorded_position)
    {
        bail!(
            "seal postcondition lifecycle order must be VerdictIssued < MergeStarted < MergeExecuted < TaskRecorded"
        );
    }
    Ok(())
}

fn exact_root_pass_payload<'a>(
    events: &'a [EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
) -> Result<(&'a EventRecord, crate::verify::RootVerdictPayload)> {
    let mut matches = Vec::new();
    for event in events.iter().filter(|event| {
        event.kind == "VerdictIssued"
            && event.actor == "verifier:root"
            && event.task_id.as_deref() == Some(task_id)
            && event.round.as_deref() == Some(round)
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("attemptId"))
                .and_then(serde_json::Value::as_str)
                == Some(attempt_id)
    }) {
        let payload: crate::verify::RootVerdictPayload = serde_json::from_value(
            event
                .payload
                .clone()
                .context("seal root VerdictIssued missing payload")?,
        )
        .context("seal root VerdictIssued payload is not canonical")?;
        matches.push((event, payload));
    }
    if matches.len() != 1 {
        bail!(
            "seal requires exactly one canonical root VerdictIssued for {attempt_id}, found {}",
            matches.len()
        );
    }
    let (event, payload) = matches.remove(0);
    if payload.verdict != "PASS" {
        bail!("seal requires root PASS for {attempt_id}");
    }
    Ok((event, payload))
}

fn seal_lifecycle_counts_after_root(
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    root_event_id: &str,
) -> Result<(usize, usize, usize)> {
    let root_position = events
        .iter()
        .position(|event| event.event_id == root_event_id)
        .context("seal root verdict event disappeared")?;
    let suffix = events.iter().skip(root_position + 1).filter(|event| {
        event.task_id.as_deref() == Some(task_id) && event.round.as_deref() == Some(round)
    });
    let mut started = 0;
    let mut merged = 0;
    let mut recorded = 0;
    for event in suffix {
        match event.kind.as_str() {
            "MergeStarted" => started += 1,
            "MergeExecuted" => merged += 1,
            "TaskRecorded" => recorded += 1,
            _ => {}
        }
    }
    if started > 1 || merged > 1 || recorded > 1 {
        bail!(
            "seal lifecycle contains duplicate canonical events: MergeStarted={started} MergeExecuted={merged} TaskRecorded={recorded}"
        );
    }
    Ok((started, merged, recorded))
}

fn validate_seal_merge_started(
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    root_event: &EventRecord,
    payload: &crate::verify::RootVerdictPayload,
) -> Result<()> {
    let root_position = events
        .iter()
        .position(|event| event.event_id == root_event.event_id)
        .context("seal MergeStarted validation lost root verdict")?;
    let starts = events
        .iter()
        .enumerate()
        .filter(|(position, event)| {
            *position > root_position
                && event.kind == "MergeStarted"
                && event.task_id.as_deref() == Some(task_id)
                && event.round.as_deref() == Some(round)
        })
        .collect::<Vec<_>>();
    if starts.len() != 1 {
        bail!(
            "seal recovery requires exactly one MergeStarted, found {}",
            starts.len()
        );
    }
    let (position, event) = starts[0];
    let expected = serde_json::json!({
        "attemptId": payload.attempt_id,
        "attemptNo": payload.attempt_no,
        "headSha": payload.head_sha,
        "mainHeadSha": payload.main_head_sha,
        "collectCompletedEventId": payload.collect_completed_event_id,
        "verdictEventId": root_event.event_id,
    });
    if position <= root_position
        || event.actor != "runtime:orch"
        || event.payload.as_ref() != Some(&expected)
    {
        bail!("seal recovery MergeStarted does not exactly bind the root PASS tuple/order");
    }
    Ok(())
}

/// Recover the narrow crash window where git created the authorized merge
/// commit but the process died before `MergeExecuted` reached the ledger.
fn recover_seal_merge_fact_if_needed(
    root: &Path,
    round: &str,
    task_id: &str,
    attempt_id: &str,
    events: &[EventRecord],
) -> Result<bool> {
    let (root_event, payload) = exact_root_pass_payload(events, round, task_id, attempt_id)?;
    let (started, merged, recorded) =
        seal_lifecycle_counts_after_root(events, round, task_id, &root_event.event_id)?;
    if merged != 0 || recorded != 0 || started != 1 {
        return Ok(false);
    }
    validate_seal_merge_started(events, round, task_id, root_event, &payload)?;
    let actual_main = gitx::rev_parse(root, "main")?;
    if !gitx::is_ancestor(root, &payload.head_sha, &actual_main)? {
        return Ok(false);
    }
    let bound_artifacts = payload
        .reviews
        .iter()
        .map(|binding| binding.path.clone())
        .chain(payload.evidence.iter().map(|binding| binding.path.clone()))
        .collect::<Vec<_>>();
    crate::verify::validate_merge_commit_shape(
        root,
        &actual_main,
        &payload.main_head_sha,
        &payload.head_sha,
        &bound_artifacts,
    )
    .context("seal crash recovery found task head in main but merge shape is unauthorized")?;
    if gitx::current_branch(root)?.as_deref() != Some("main")
        || gitx::rev_parse(root, "HEAD")? != actual_main
    {
        bail!("seal crash recovery requires primary worktree HEAD == main");
    }
    let merge_sha = actual_main.clone();
    let appended = ledger::append_checked_merge_lifecycle(root, round, |fresh| {
        let (fresh_root, fresh_payload) =
            exact_root_pass_payload(fresh, round, task_id, attempt_id)?;
        let counts = seal_lifecycle_counts_after_root(fresh, round, task_id, &fresh_root.event_id)?;
        if counts != (1, 0, 0) || fresh_payload != payload {
            bail!("seal crash recovery authorization drifted before MergeExecuted append");
        }
        validate_seal_merge_started(fresh, round, task_id, fresh_root, &fresh_payload)?;
        if gitx::rev_parse(root, "main")? != merge_sha {
            bail!("seal crash recovery main moved before MergeExecuted append");
        }
        crate::verify::validate_merge_commit_shape(
            root,
            &merge_sha,
            &fresh_payload.main_head_sha,
            &fresh_payload.head_sha,
            &bound_artifacts,
        )?;
        Ok(vec![merge_executed_event(task_id, round, &merge_sha)])
    })?;
    if appended != 1 {
        bail!("seal crash recovery expected one MergeExecuted, appended {appended}");
    }
    Ok(true)
}

fn validate_seal_storage_mirror(root: &Path, round: &str) -> Result<()> {
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let wal_path = root.join(format!("coordination/runtime/ledger-wal/{round}.jsonl"));
    let ledger_bytes = fs::read(&ledger_path)
        .with_context(|| format!("seal readback failed: {}", ledger_path.display()))?;
    let wal_bytes = fs::read(&wal_path)
        .with_context(|| format!("seal WAL readback failed: {}", wal_path.display()))?;
    if ledger_bytes != wal_bytes {
        bail!("seal postcondition failed: tracked ledger and WAL bytes differ");
    }
    Ok(())
}

fn emit_site_gc_line(lines: &mut Vec<String>, line: String) {
    eprintln!("{line}");
    lines.push(line);
}

fn trigger_site_gc(root: &Path, trigger: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let round = match crate::current_round(root) {
        Ok(round) => round,
        Err(error) => {
            emit_site_gc_line(
                &mut lines,
                format!("[orch] {trigger} 触发 site GC 时无法解析当前轮: {error:#}"),
            );
            return lines;
        }
    };
    if let Err(error) = crate::wake::reconcile_pending_backend_receipts(root, &round) {
        emit_site_gc_line(
            &mut lines,
            format!(
                "[orch] {trigger} 的 backend receipt 对账失败；已降级为诊断并继续 site GC: {error:#}"
            ),
        );
    }
    match crate::sites::reap_released_sites_reported(root, &round) {
        Ok(report) => {
            emit_site_gc_line(
                &mut lines,
                format!(
                    "[orch] {trigger} site GC: removed={} refused={} freedBytes={}",
                    report.reaped.len(),
                    report.refused.len(),
                    report.freed_bytes
                ),
            );
            for (site_id, reason) in report.refused {
                emit_site_gc_line(
                    &mut lines,
                    format!("[orch] {trigger} site GC REFUSED {site_id}: {reason}"),
                );
            }
        }
        Err(error) => {
            emit_site_gc_line(
                &mut lines,
                format!(
                    "[orch] {trigger} 触发 site GC 后保守留场（TaskRecorded/merge 事实不回滚）: {error:#}"
                ),
            );
        }
    }
    lines
}

/// One normal command for PASS verdict -> no-ff merge -> post-merge gates ->
/// TaskRecorded.  The exclusive lifecycle lease spans the complete sequence;
/// every irreversible crash window is resumed from fresh durable facts.
pub fn run_seal(
    root: &Path,
    task_id: &str,
    attempt_id: &str,
    expected_head: &str,
) -> Result<SealOutcome> {
    let outcome = with_merge_lifecycle_transition(root, "orch seal", || {
        let round = crate::current_round(root)?;
        let ledger_rel = format!("coordination/rounds/{round}/events.jsonl");
        let ledger_path = root.join(&ledger_rel);
        let initial = read_ledger(&ledger_path)?;
        if !initial.bad_lines.is_empty() {
            bail!("seal refuses a ledger with bad lines before root verdict");
        }
        validate_premerge_root_status(root, &ledger_rel, &round, &initial.events)?;
        let existing_roots = initial
            .events
            .iter()
            .filter(|event| {
                event.kind == "VerdictIssued"
                    && event.actor == "verifier:root"
                    && event.task_id.as_deref() == Some(task_id)
                    && event.round.as_deref() == Some(round.as_str())
                    && event
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("attemptId"))
                        .and_then(serde_json::Value::as_str)
                        == Some(attempt_id)
            })
            .count();
        match existing_roots {
            0 => {
                // Captured inside the lifecycle lease and immediately before
                // verdict; the operator cannot supply or stale this value.
                let expected_main = gitx::rev_parse(root, "main")?;
                crate::verify::run_root_verdict(
                    root,
                    task_id,
                    attempt_id,
                    expected_head,
                    &expected_main,
                    crate::verify::RootVerdict::Pass,
                    None,
                    false,
                )?;
            }
            1 => {
                let (_, payload) =
                    exact_root_pass_payload(&initial.events, &round, task_id, attempt_id)?;
                if payload.head_sha != expected_head {
                    bail!(
                        "seal replay expected-head drifted: verdict={} requested={expected_head}",
                        payload.head_sha
                    );
                }
            }
            count => bail!("seal found {count} root verdicts for {attempt_id}"),
        }

        let before = read_ledger(&ledger_path)?;
        if !before.bad_lines.is_empty() {
            bail!("seal refuses a ledger with bad lines after root verdict");
        }
        let (root_event, _) = exact_root_pass_payload(&before.events, &round, task_id, attempt_id)?;
        let counts = seal_lifecycle_counts_after_root(
            &before.events,
            &round,
            task_id,
            &root_event.event_id,
        )?;

        let (gates, replayed_complete) = match counts {
            (1, 1, 1) => (Vec::new(), true),
            (1, 1, 0) => {
                let outcome = run_record(root, task_id)?;
                (outcome.gates, false)
            }
            (1, 0, 0) => {
                if recover_seal_merge_fact_if_needed(
                    root,
                    &round,
                    task_id,
                    attempt_id,
                    &before.events,
                )? {
                    let outcome = run_record(root, task_id)?;
                    (outcome.gates, false)
                } else {
                    let outcome = run_merge_locked(root, task_id)?;
                    (outcome.gates, false)
                }
            }
            (0, 0, 0) => {
                let outcome = run_merge_locked(root, task_id)?;
                (outcome.gates, false)
            }
            other => bail!(
                "seal refuses partial/noncanonical lifecycle counts: MergeStarted={} MergeExecuted={} TaskRecorded={}",
                other.0,
                other.1,
                other.2
            ),
        };

        let final_ledger = read_ledger(&ledger_path)?;
        if !final_ledger.bad_lines.is_empty() {
            bail!("seal postcondition failed: ledger contains bad lines");
        }
        validate_seal_postcondition(&final_ledger.events, &round, task_id, attempt_id)?;
        let authorization = crate::verify::validate_root_record_authorization(
            root,
            &round,
            task_id,
            &final_ledger.events,
        )?;
        if authorization.attempt_id != attempt_id
            || authorization.head_sha != expected_head
            || !authorization.already_recorded
        {
            bail!("seal postcondition did not bind current attempt/TaskRecorded");
        }
        validate_seal_storage_mirror(root, &round)?;
        Ok(SealOutcome {
            merge_sha_short: gitx::short(&authorization.merge_sha).to_string(),
            gates,
            replayed_complete,
        })
    })?;
    trigger_site_gc(root, "TaskRecorded");
    Ok(outcome)
}

// ─────────────────────── B153 · 悬空屏障的恢复出口与拒收归属 ───────────────────────

/// git merge 失败后的处置纯判据（seed 契约）。
/// r51/B147 事故：`run_merge` 落 MergeStarted 后冲突失败、refs 未动，而
/// boundary-violation 记账以「refs 已移动」为触发条件，冲突路径永远到不了
/// ⇒ 屏障悬空冻结全轮。本判据把「refs 未动的失败」显式命名为新路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeFailure {
    /// 合法 main merge——根本不是失败处置。
    None,
    /// refs 未动且失败 ⇒ 冲突终态，落 merge-conflict escalation 闭合屏障。
    ConflictEscalation,
    /// refs 已动 ⇒ 既有 boundary-violation 路径（语义零改写）。
    BoundaryViolation,
}

pub fn merge_failure_disposition(refs_moved: bool, valid_main_merge: bool) -> MergeFailure {
    if valid_main_merge {
        MergeFailure::None
    } else if refs_moved {
        MergeFailure::BoundaryViolation
    } else {
        MergeFailure::ConflictEscalation
    }
}

/// 屏障恢复计划的纯判据结果（seed 契约）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarrierRecovery {
    /// 清屏障并允许重派冲突修复 attempt。
    ClearAndAllowRedispatch,
}

/// 「屏障活跃 ∧ verdict 仍有效 ∧（main 未推进 ∨ 合并确证不存在）」允许清屏障：
/// - 无屏障 ⇒ 无可恢复，拒；
/// - verdict 失效 ⇒ 不复活死链，拒（**该条不受任何证明豁免**）；
/// - main 已推进 ⇒ 默认拒（merge 可能真发生了，清屏障会掩盖真实合并）。
///
/// H97/H98（r62/B204 实证）：「main 未推进」只是「合并未发生」的**代理**判据。
/// planner 在屏障期为别的原因推进 main（应急直修产品代码）会让它误判，
/// 而屏障期账本只接受 `MergeExecuted` / merge escalation / `TaskRecorded`，
/// 于是 `await-report`、`RoundClosed` 全被拒——**全轮锁死且无任何 CLI 出口**。
///
/// `merge_provably_absent` 是**更强的直接证明**：task 分支既不是 main 的祖先，
/// main 的历史里也不存在以该分支尖端为父的 merge commit ⇒ 合并确证没有发生，
/// 清屏障不可能掩盖任何真实合并。该证明由调用方从 git 拓扑机械计算；
/// 任一步查不到就必须传 `false`，退回原保守判据。
pub fn barrier_recovery_plan(
    barrier_active: bool,
    main_moved: bool,
    verdict_still_valid: bool,
    merge_provably_absent: bool,
) -> Result<BarrierRecovery, String> {
    if !barrier_active {
        return Err("无活跃 MergeStarted 屏障，无可恢复".to_string());
    }
    if main_moved && !merge_provably_absent {
        return Err("main 已推进：merge 可能真实发生，清屏障会掩盖真实合并，拒绝恢复".to_string());
    }
    if !verdict_still_valid {
        return Err("verdict 已失效：不复活死链，拒绝恢复".to_string());
    }
    Ok(BarrierRecovery::ClearAndAllowRedispatch)
}

/// 冲突终态事实（actor 固定 reviewer:orch-runtime）：`mergeSha` 恒 null
/// （声明 main 未有效推进），`conflictFiles` 从 git 输出解析，拿不到给空数组。
/// 该形状经 `ledger` 的 canonical 谓词与 pre-executed 闸门后闭合屏障。
pub fn merge_conflict_event(task: &str, round: &str, conflict_files: &[String]) -> EventRecord {
    ledger::event(
        "EscalationRaised",
        "reviewer:orch-runtime",
        Some(task),
        Some(round),
        serde_json::json!({
            "stage": "merge-conflict",
            "mergeSha": serde_json::Value::Null,
            "conflictFiles": conflict_files,
        }),
    )
}

/// H29 释放留痕事实：合并**已落**但合后门红时的唯一出口。落它只闭合屏障
/// （解冻全轮），**绝不落 `TaskRecorded`**——任务停在 `merged`，合并事实原样
/// 留账，二次 `orch merge` 仍被 `MergeExecuted` 拒。修复走后续正常流程。
pub fn barrier_released_event(task: &str, round: &str, merge_sha: &str, gate: &str) -> EventRecord {
    ledger::event(
        "EscalationRaised",
        "reviewer:orch-runtime",
        Some(task),
        Some(round),
        serde_json::json!({
            "stage": "post-merge-gate-released",
            "mergeSha": merge_sha,
            "gate": gate,
            "reason": format!("合后门 {gate} 红且修复必然在合并之后——补记门钉死 merge_sha，永不可能转绿"),
            "hint": "屏障已释放解冻全轮；任务停在 merged 未 Recorded，须在收轮时显式披露",
        }),
    )
}

/// H29 受控补记放宽的审计事实。该事件故意排在 canonical `TaskRecorded`
/// **之后**同批落账：活跃 merge barrier 仍只接受既有 canonical 终态，
/// `TaskRecorded` 先闭合屏障，随后本事件完整记录显式 `--at-tip` 逃生舱
/// 放宽的 sha 区间、理由与文件清单。
pub fn record_gate_relaxed_event(
    task: &str,
    round: &str,
    merge_sha: &str,
    tip_sha: &str,
    reason: &str,
    files: &[String],
) -> EventRecord {
    ledger::event(
        // ledger.rs is frozen for B159. Reuse the established auditable
        // EscalationRaised envelope and name the durable fact by stage; this
        // keeps existing post-merge suffix validators compatible.
        "EscalationRaised",
        "reviewer:orch-runtime",
        Some(task),
        Some(round),
        serde_json::json!({
            "stage": "RecordGateRelaxed",
            "mergeSha": merge_sha,
            "tipSha": tip_sha,
            "reason": reason,
            "files": files,
        }),
    )
}

/// 恢复留痕事实：`run_merge_recovery` 过 `barrier_recovery_plan` 三条件后落，
/// 闭合屏障并允许重派冲突修复。
pub fn barrier_recovered_event(task: &str, round: &str) -> EventRecord {
    ledger::event(
        "EscalationRaised",
        "reviewer:orch-runtime",
        Some(task),
        Some(round),
        serde_json::json!({"stage": "barrier-recovered"}),
    )
}

/// 冲突终态的 attempt 终局事实（B153-A0002 整改）。merge 冲突且 refs 未动时，
/// 同一 head 的 merge 重放必然再冲突——该 attempt 永远抵达不了 TaskRecorded。
/// 只落 merge-conflict escalation 而不落本事实的话，最近决定性事实仍是 root
/// PASS verdict（非接管终态，attempt.rs `latest_attempt_is_terminal`），
/// `plan_dispatch_locked` 只会 `Existing` 幂等空转：不产生新 DispatchIssued，
/// wake 已完成不重发，nudge 拒 Approved——「可重派」沦为语义空成功（r52 主审
/// P0-2）。本事实把该 attempt 显式判为终局阻塞：
/// - `latest_attempt_is_terminal` ⇒ true，`run_dispatch` 规划接管 attempt；
/// - 投影态 Approved ⇒ Blocked（`orch_core::fold`），nudge 身份解析不再拒；
/// - 终局 identity（attemptId/attemptNo/agent）来自 fresh 的 root merge
///   authorization，绝不凭空调造。
/// 必须与闭合屏障的 escalation 同批原子落账——两事件之间崩溃会重现上述空洞。
fn merge_conflict_attempt_blocked_event(
    task: &str,
    round: &str,
    authorization: &crate::verify::RootMergeAuthorization,
) -> EventRecord {
    ledger::event(
        "AttemptBlocked",
        "runtime:orch",
        Some(task),
        Some(round),
        serde_json::json!({
            "attemptId": authorization.attempt_id,
            "attemptNo": authorization.attempt_no,
            "agent": authorization.implementer_agent,
            "stage": "merge-conflict",
            "reason": "git merge 冲突且 refs 未动——同一 head 重放 merge 必然再冲突，本 attempt 无法抵达 TaskRecorded；终局阻塞以放行冲突修复接管 attempt",
        }),
    )
}

/// 现场清理处置：Tier F（注册表内驻留 shell）延后；其余 best-effort。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupDisposition {
    DeferredTierF,
    AttemptBestEffort,
}

pub fn cleanup_disposition(agent_is_registered: bool) -> CleanupDisposition {
    if agent_is_registered {
        CleanupDisposition::DeferredTierF
    } else {
        CleanupDisposition::AttemptBestEffort
    }
}

fn validate_premerge_root_status(
    root: &Path,
    ledger_rel: &str,
    round: &str,
    events: &[EventRecord],
) -> Result<()> {
    let late = wake::validate_late_review_deliveries(root, round, events)?;
    let status = gitx::porcelain_v2(root)?;
    for line in status.lines().filter(|line| !line.is_empty()) {
        if let Some(rest) = line.strip_prefix("1 ") {
            let mut fields = rest.splitn(8, ' ');
            let xy = fields.next().context("porcelain-v2 type-1 缺 XY")?;
            for _ in 0..6 {
                fields.next().context("porcelain-v2 type-1 字段不足")?;
            }
            let path = fields.next().context("porcelain-v2 type-1 缺 path")?;
            if xy == ".M" && (path == ledger_rel || path == "coordination/BOARD.md") {
                continue;
            }
            bail!(
                "merge 前主工作区 dirty 非法（仅允许 unstaged {ledger_rel} 与 coordination/BOARD.md）: {line}"
            );
        }
        if let Some(path) = line.strip_prefix("? ") {
            if late.paths.contains(path) {
                continue;
            }
        }
        // Type-2 renames, unmerged entries, untracked files and every unknown
        // record are all forbidden.  In particular no staged bit may cross the
        // no-ff boundary and become an unrelated merge-tree blob.
        bail!("merge 前主工作区含 staged/untracked/unmerged 变更: {line}");
    }
    Ok(())
}

pub(crate) fn commit_parents(root: &Path, sha: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-list", "--parents", "-n", "1", sha])
        .output()
        .context("读取 merge commit parents 失败")?;
    if !output.status.success() {
        bail!(
            "读取 merge commit parents 失败: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let fields = String::from_utf8(output.stdout)
        .context("merge commit parents 输出非 UTF-8")?
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    if fields.first().map(String::as_str) != Some(sha) {
        bail!("merge commit parents 输出未绑定 actual SHA");
    }
    Ok(fields.into_iter().skip(1).collect())
}

fn account_merge_boundary_violation(
    root: &Path,
    round: &str,
    task_id: &str,
    actual_main: &str,
    actual_head: &str,
    record_valid_main_merge: bool,
    reason: &str,
) -> Result<()> {
    let mut events = Vec::new();
    if record_valid_main_merge {
        events.push(merge_executed_event(task_id, round, actual_main));
    }
    events.push(
        ledger::event(
            "EscalationRaised",
            "reviewer:orch-runtime",
            Some(task_id),
            Some(round),
            serde_json::json!({
                "stage": "merge-boundary-shape",
                "mergeSha": if record_valid_main_merge { Some(actual_main) } else { None },
                "actualMain": actual_main,
                "actualHead": actual_head,
                "reason": reason,
                "hint": "git merge 已越过命令边界；仅经精确 parent 校验的 main 推进可投影 Merged，禁止 reset/TaskRecorded",
            }),
        ),
    );
    append_merge_lifecycle_events(root, round, events)
        .context("记录 merge boundary 真实结果/升级失败")
}

pub fn run_merge(root: &Path, task_id: &str) -> Result<MergeOutcome> {
    let outcome =
        with_merge_lifecycle_transition(root, "orch merge", || run_merge_locked(root, task_id))?;
    trigger_site_gc(root, "TaskRecorded");
    Ok(outcome)
}

fn run_merge_locked(root: &Path, task_id: &str) -> Result<MergeOutcome> {
    let round = fs::read_to_string(root.join("coordination/runtime/CURRENT-ROUND"))
        .context("CURRENT-ROUND 缺失")?
        .trim()
        .to_string();
    // ── 步骤 1：transition lease 内重读全部可变事实；直接 orch merge 也只能走此边界。──
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let ledger_rel = format!("coordination/rounds/{round}/events.jsonl");
    let lr = read_ledger(&ledger_path)?;
    if !lr.bad_lines.is_empty() {
        bail!("拒绝合并：账本含坏行");
    }
    validate_premerge_root_status(root, &ledger_rel, &round, &lr.events)?;
    let branch = format!("task/{task_id}");
    let authorization =
        crate::verify::validate_root_merge_authorization(root, &round, task_id, &lr.events)?;
    let started_payload = serde_json::json!({
        "attemptId": authorization.attempt_id,
        "attemptNo": authorization.attempt_no,
        "headSha": authorization.head_sha,
        "mainHeadSha": authorization.main_head_sha,
        "collectCompletedEventId": authorization.collect_completed_event_id,
        "verdictEventId": authorization.verdict_event_id,
    });
    ledger::append_checked_merge_lifecycle(root, &round, |events| {
        let fresh =
            crate::verify::validate_root_merge_authorization(root, &round, task_id, events)?;
        let expected = serde_json::json!({
            "attemptId": fresh.attempt_id,
            "attemptNo": fresh.attempt_no,
            "headSha": fresh.head_sha,
            "mainHeadSha": fresh.main_head_sha,
            "collectCompletedEventId": fresh.collect_completed_event_id,
            "verdictEventId": fresh.verdict_event_id,
        });
        if expected != started_payload {
            bail!("MergeStarted append 前 root authorization 漂移");
        }
        // H97 (planner emergency direct fix, r62/B204): a merge that was
        // aborted before touching refs still leaves its `MergeStarted` behind.
        // The runtime closes that barrier with `AttemptBlocked`, but the event
        // stays in the round ledger, and an unfiltered scan here then rejects
        // every later attempt of the same task for the rest of the round —
        // with no CLI arm able to clear it (`merge --recover` keys off an
        // *active* barrier, so it reports "无活跃 MergeStarted 屏障").
        //
        // A `MergeStarted` is stale exactly when its attempt was terminally
        // blocked and no `MergeExecuted` ever followed: the merge provably did
        // not happen, so it cannot describe main's current state and must not
        // authorize or veto anything. Barriers that are still live — or that
        // did reach `MergeExecuted` — keep their full veto, so the "one merge
        // per task" guarantee is untouched for every non-aborted lifecycle.
        let existing = events
            .iter()
            .filter(|event| {
                event.kind == "MergeStarted" && event.task_id.as_deref() == Some(task_id)
            })
            .filter(|event| !merge_started_is_stale(events, task_id, event))
            .collect::<Vec<_>>();
        if existing.len() > 1 {
            bail!("已有多条 MergeStarted，fail closed");
        }
        if let Some(event) = existing.first() {
            if event.actor != "runtime:orch"
                || event.round.as_deref() != Some(round.as_str())
                || event.payload.as_ref() != Some(&expected)
            {
                bail!("既有 MergeStarted 与 current authorization 冲突");
            }
            return Ok(Vec::new());
        }
        Ok(vec![ledger::event(
            "MergeStarted",
            "runtime:orch",
            Some(task_id),
            Some(&round),
            expected,
        )])
    })?;

    // MergeStarted 落账后、越过 git merge 前在同一 OS lock 内再 fresh-read 一次。
    let fresh_ledger = read_ledger(&ledger_path)?;
    if !fresh_ledger.bad_lines.is_empty() {
        bail!("MergeStarted 后账本含坏行，拒绝越过 git merge");
    }
    validate_premerge_root_status(root, &ledger_rel, &round, &fresh_ledger.events)?;
    let authorization = crate::verify::validate_root_merge_authorization(
        root,
        &round,
        task_id,
        &fresh_ledger.events,
    )?;
    validate_merge_started_barrier(&fresh_ledger.events, &round, task_id, &authorization)?;
    // H48: the signed main may have advanced through coordination-only commits.
    // Bind the merge to the actual main ref observed immediately before the
    // irreversible command; the post-command proof must name this exact SHA as
    // its first parent.
    let premerge_main_sha = gitx::rev_parse(root, "main")?;

    // ── 步骤 2：no-ff 合并，成功后下一项有状态操作必须是落 MergeExecuted 真值 ──
    let merge_result = merge_no_ff_without_hooks(
        root,
        &authorization.head_sha,
        &format!("merge({task_id}): root fixed-HEAD PASS — merged by orch runtime"),
    );
    let actual_head = gitx::rev_parse(root, "HEAD")?;
    let actual_main = gitx::rev_parse(root, "main")?;
    let refs_moved = actual_head != premerge_main_sha || actual_main != premerge_main_sha;
    let merge_proof = crate::verify::validate_merge_commit_shape(
        root,
        &actual_main,
        &authorization.main_head_sha,
        &authorization.head_sha,
        &authorization.bound_artifacts,
    );
    let valid_main_merge = gitx::current_branch(root)?.as_deref() == Some("main")
        && actual_head == actual_main
        && actual_main != premerge_main_sha
        && merge_proof
            .as_ref()
            .is_ok_and(|proof| proof.first_parent_sha == premerge_main_sha);
    if let Err(failure) = merge_result {
        if !refs_moved && merge_was_blocked_by_main_guard(&failure) {
            // The hook rejected the prepared ref transaction, so this is not a
            // merge conflict and must not mint the conflict terminal pair.
            // Git may nevertheless have left an in-progress merge/index; refs
            // are proven unchanged above, making abort safe while preserving
            // the durable MergeStarted fact for retry/recovery.
            abort_inflight_merge_best_effort(root);
            bail!(
                "git merge 被 main guard 拒绝（refs 未动）；未记 merge-conflict，MergeStarted 保持悬空供修复后重试或 `orch merge --recover {task_id}`：stdout={} stderr={}",
                failure.stdout.trim(),
                failure.stderr.trim()
            );
        }
        match merge_failure_disposition(refs_moved, valid_main_merge) {
            MergeFailure::BoundaryViolation => {
                let reason = format!("git merge 返回错误但 ref 已移动: {failure}");
                account_merge_boundary_violation(
                    root,
                    &round,
                    task_id,
                    &actual_main,
                    &actual_head,
                    valid_main_merge,
                    &reason,
                )?;
                bail!("{reason}");
            }
            MergeFailure::ConflictEscalation => {
                // B153：r51/B147 的悬空屏障原样重放处。refs 未动的失败必须落
                // merge-conflict 终态事实——它是屏障的唯一闭合出口；同批原子落
                // AttemptBlocked 判该 attempt 终局（否则 root PASS 仍是非接管
                // 终态，真实 dispatch 幂等空转，「可重派」是语义空成功）。
                let conflict_files = conflict_files_from_merge(root, &failure);
                let account = append_merge_lifecycle_events(
                    root,
                    &round,
                    vec![
                        merge_conflict_event(task_id, &round, &conflict_files),
                        merge_conflict_attempt_blocked_event(task_id, &round, &authorization),
                    ],
                );
                abort_inflight_merge_best_effort(root);
                match account {
                    Ok(()) => bail!(
                        "git merge 冲突失败（refs 未动，已落 merge-conflict 升级并闭合屏障，可重派冲突修复）: {failure}"
                    ),
                    // P1（r52 主审）：落账失败时绝不宣称已闭合——回读账本判定
                    // 真实状态（ledger 已写 WAL 未写时事件实际已落），回读失败
                    // 则报「闭合状态未知」，任何分支都携带 accounting error。
                    Err(account_error) => bail!(
                        "{}",
                        conflict_closure_diagnosis(
                            &ledger_path,
                            task_id,
                            &round,
                            &account_error,
                            &failure,
                        )
                    ),
                }
            }
            MergeFailure::None => {
                // 防御臂：merge 报错但 main 已含 exact 合法合并——矛盾态，不记账。
                bail!("git merge 返回错误但 main 已含合法合并（矛盾态，未记账）: {failure}");
            }
        }
    }
    let shape_error = (!valid_main_merge).then(|| {
        let proof_error = merge_proof
            .as_ref()
            .err()
            .map(|error| format!(" proof={error:#}"))
            .unwrap_or_default();
        format!(
            "git merge 成功但 main/HEAD/parents 非 authorized tuple: main={actual_main} HEAD={actual_head} premergeMain={premerge_main_sha}{proof_error}"
        )
    });
    if let Some(reason) = shape_error {
        account_merge_boundary_violation(
            root,
            &round,
            task_id,
            &actual_main,
            &actual_head,
            false,
            &reason,
        )?;
        bail!("{reason}");
    }
    let merge_sha = actual_main;
    let short = gitx::short(&merge_sha).to_string();
    // 即时 append 失败：main 已推进，禁止 reset/回滚/继续门，直接返回错误。
    append_merge_lifecycle_events(
        root,
        &round,
        vec![merge_executed_event(task_id, &round, &merge_sha)],
    )
    .context("落 MergeExecuted 失败——main 已推进，停止后续门/记账，需人工介入")?;

    // ── 步骤 3：在 exact merge SHA 的 clean detached worktree 复跑快门。
    // 主工作区可能含不相关的本地修改；它们绝不能弱化或污染 post-merge gate。
    let log_dir = root.join("coordination/runtime/logs");
    let gate_wt =
        root.join(".cowork-temp")
            .join(format!("postmerge-{}-{}", task_id, ulid::Ulid::new()));
    fs::create_dir_all(gate_wt.parent().context("postmerge worktree 缺 parent")?)?;
    gitx::worktree_add_detached(root, &gate_wt, &merge_sha)?;
    let gate_run = (|| -> Result<(Vec<gate::GateResult>, Option<(String, i32)>)> {
        if gitx::rev_parse(&gate_wt, "HEAD")? != merge_sha
            || !gitx::porcelain_v2(&gate_wt)?.trim().is_empty()
        {
            bail!("postmerge detached worktree 初始 HEAD/clean 不匹配");
        }
        // Parse the gate contract only inside the immutable merge commit
        // worktree.  Never reopen mutable root card/binding bytes after the
        // fixed-HEAD authorization check (ABA-safe).
        let c = card::load(&gate_wt, &round, task_id)?;
        let b = binding::load(&gate_wt)?;
        let mut gates = Vec::new();
        for gref in &c.meta.gates.fast {
            let spec = b
                .commands
                .get(gref)
                .with_context(|| format!("绑定缺命令: {gref}"))?;
            let tag = format!("{task_id}-postmerge");
            let g = with_close_orphan_watch(root, &round, task_id, &log_dir, &tag, gref, || {
                gate::run_gate_with_audit_identity(
                    root,
                    &round,
                    ledger::GateAuditIdentity::Attempt {
                        task_id,
                        attempt_id: &authorization.attempt_id,
                    },
                    gref,
                    spec,
                    &gate_wt,
                    &log_dir,
                    &tag,
                )
            })?;
            if gitx::rev_parse(&gate_wt, "HEAD")? != merge_sha
                || !gitx::porcelain_v2(&gate_wt)?.trim().is_empty()
            {
                bail!("postmerge gate {gref} 改变 detached HEAD/worktree");
            }
            let exit = g.exit_code;
            gates.push(g);
            if exit != 0 {
                return Ok((gates, Some((gref.clone(), exit))));
            }
        }
        Ok((gates, None))
    })();
    gitx::worktree_remove(root, &gate_wt).context(
        "清理 postmerge detached worktree 失败——main 已推进，停止记账并保留现场供人工检查",
    )?;
    let (gates, red_gate) = gate_run?;
    if let Some((gref, exit)) = red_gate {
        let _ = append_merge_lifecycle_events(
            root,
            &round,
            vec![post_merge_gate_failed_event(
                task_id, &round, &short, &gref, exit,
            )],
        );
        bail!("合并后门 {gref} 红（exit {exit}）——main 已含合并 {short}，需人工裁决回退");
    }
    println!("{}", gate::render_gate_summary_line(&gates));

    // ── 步骤 4：全部门绿后 fresh 重算完整 post-merge authorization，再原子记账。──
    let before_record = read_ledger(&ledger_path)?;
    if !before_record.bad_lines.is_empty() {
        bail!("合后门绿但账本出现坏行，拒绝 TaskRecorded");
    }
    let record_authorization = crate::verify::validate_root_record_authorization(
        root,
        &round,
        task_id,
        &before_record.events,
    )?;
    if record_authorization.merge_sha != merge_sha || record_authorization.already_recorded {
        bail!("合后门绿时 post-merge authorization 与本次 merge 不匹配");
    }
    ledger::append_checked_merge_lifecycle(root, &round, |events| {
        let fresh =
            crate::verify::validate_root_record_authorization(root, &round, task_id, events)?;
        if fresh != record_authorization {
            bail!("TaskRecorded append 前 post-merge authorization 漂移");
        }
        Ok(task_recorded_batch(events, task_id, &round))
    })?;

    // ── 步骤 5：人读尾务——BOARD 追加失败只警告，不推翻已完成的 merge+gate+记账 ──
    let board_line =
        format!("- [orch] **{task_id} merged**（`{short}`，no-ff，合后门全绿）· verifier:root fixed-HEAD PASS · 运行时自动收口。");
    let board_result = fs::OpenOptions::new()
        .append(true)
        .open(root.join("coordination/BOARD.md"))
        .and_then(|mut f| writeln!(f, "{board_line}"));
    if let Err(e) = board_result {
        eprintln!("[orch] 警告：BOARD.md 追加失败（{e}）——merge 与记账已完成，仅人读账本缺一行");
    }

    // ── 步骤 6：现场清理——Tier F 延后（O1）；其余 best-effort，互不阻断、不推翻事实 ──
    let disposition = match wake::load_registry(root) {
        Ok(registry) => {
            let registered = registry.contains_key(&authorization.implementer_agent);
            cleanup_disposition(registered)
        }
        Err(e) => {
            eprintln!("[orch] 警告：AgentRegistry 读取失败（{e}）——保守按 Tier F 延后清理");
            CleanupDisposition::DeferredTierF
        }
    };
    match disposition {
        CleanupDisposition::DeferredTierF => {
            println!(
                "[orch] Tier F 现场延迟清理：保留 worktree .worktrees/{task_id} 与分支 {branch}，待确认会话退出后由 Tier F 流程回收"
            );
        }
        CleanupDisposition::AttemptBestEffort => {
            let wt = root.join(".worktrees").join(task_id);
            if let Err(e) = gitx::worktree_remove(root, &wt) {
                eprintln!("[orch] 警告：worktree remove 失败（{e}）——best-effort，继续尝试删分支");
            }
            if let Err(e) = gitx::branch_delete(root, &branch) {
                eprintln!(
                    "[orch] 警告：branch delete 失败（{e}）——best-effort，merge 事实不受影响"
                );
            }
        }
    }

    Ok(MergeOutcome {
        merge_sha_short: short,
        gates,
    })
}

/// H97: a `MergeStarted` whose attempt was terminally blocked before any
/// `MergeExecuted` describes a merge that provably never happened. It must not
/// be counted as a canonical barrier for a later attempt of the same task —
/// otherwise one aborted merge permanently freezes the task for the rest of the
/// round, with no CLI arm able to clear it.
///
/// The `MergeExecuted` veto is *positional*, not task-wide (r62/B204): a later
/// attempt that does reach `MergeExecuted` must not resurrect the earlier
/// aborted barrier. The window examined is exactly this barrier's own lifetime
/// — from the `MergeStarted` itself to the `AttemptBlocked` that ends its
/// attempt. `ledger.rs`'s barrier state machine is what makes that window
/// meaningful: while a barrier is `Active` the ledger admits only canonical
/// `MergeExecuted` / merge escalation / `TaskRecorded`, so an `AttemptBlocked`
/// can only land once the barrier is `Open` again, and any `MergeExecuted`
/// after it belongs to a successor attempt's barrier. (Note the escalation
/// paired with an `executor-blocked` terminal does *not* close a barrier —
/// `tierf.rs` emits it under `runtime:orch`, which `canonical_merge_escalation`
/// rejects. The state-machine argument above does not depend on it.)
///
/// Two fail-closed guards, both load-bearing:
/// - a `MergeExecuted` *before* the block still vetoes staleness (that merge
///   may describe main's current state);
/// - a `MergeStarted` that lands *after* its own attempt was already terminated
///   is never stale. It is a live barrier that should not have been armed at
///   all, and calling it stale would make it invisible to
///   `validate_merge_started_barrier` while the ledger still holds it Active —
///   freezing the round with no CLI arm able to clear it. `verify.rs` refuses
///   the authorization before it can be armed; this is the second line.
pub(crate) fn merge_started_is_stale(
    events: &[EventRecord],
    task_id: &str,
    event: &EventRecord,
) -> bool {
    let Some(attempt) = event
        .payload
        .as_ref()
        .and_then(|payload| payload.get("attemptId"))
        .and_then(serde_json::Value::as_str)
    else {
        return false;
    };
    // `event` is always an element of `events` at every call site (all three
    // reach this predicate from an `events.iter()` filter), so identity search
    // is exact; a caller that passes an outside record gets a conservative
    // "not stale" instead of a wrong window.
    let Some(self_position) = events
        .iter()
        .position(|candidate| std::ptr::eq(candidate, event))
    else {
        return false;
    };
    let Some(blocked_position) = events.iter().position(|candidate| {
        candidate.kind == "AttemptBlocked"
            && candidate.task_id.as_deref() == Some(task_id)
            && candidate.round.as_deref() == event.round.as_deref()
            && candidate
                .payload
                .as_ref()
                .and_then(|payload| payload.get("attemptId"))
                .and_then(serde_json::Value::as_str)
                == Some(attempt)
    }) else {
        return false;
    };
    if self_position >= blocked_position {
        return false;
    }
    !events[self_position..blocked_position]
        .iter()
        .any(|candidate| {
            candidate.kind == "MergeExecuted"
                && candidate.task_id.as_deref() == Some(task_id)
                && candidate.actor == "reviewer:orch-runtime"
                && candidate.round.as_deref() == event.round.as_deref()
        })
}

fn validate_merge_started_barrier(
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    authorization: &crate::verify::RootMergeAuthorization,
) -> Result<()> {
    let starts = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "MergeStarted"
                && event.task_id.as_deref() == Some(task_id)
                && !merge_started_is_stale(events, task_id, event)
        })
        .collect::<Vec<_>>();
    if starts.len() != 1 {
        bail!("越过 git merge 前要求恰好一条 canonical MergeStarted");
    }
    let (started_position, started) = starts[0];
    let expected = serde_json::json!({
        "attemptId": authorization.attempt_id,
        "attemptNo": authorization.attempt_no,
        "headSha": authorization.head_sha,
        "mainHeadSha": authorization.main_head_sha,
        "collectCompletedEventId": authorization.collect_completed_event_id,
        "verdictEventId": authorization.verdict_event_id,
    });
    if started.actor != "runtime:orch"
        || started.round.as_deref() != Some(round)
        || started.payload.as_ref() != Some(&expected)
    {
        bail!("越过 git merge 前 MergeStarted envelope/payload 非 canonical");
    }
    let verdict_positions = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.event_id == authorization.verdict_event_id)
        .map(|(position, _)| position)
        .collect::<Vec<_>>();
    if verdict_positions.len() != 1 || verdict_positions[0] >= started_position {
        bail!("越过 git merge 前 MergeStarted 未晚于 exact root verdict");
    }
    Ok(())
}

// ─────────────────────── R45S · merged → recorded 恢复路径 ───────────────────────
// r44 死角：合并已落 + 合后门红 → 任务永远停在 merged，此前只有 --force 能收轮。
// run_record 是新增旁路（不动 run_merge 既有行为），语义严格 fail-closed。

/// 补记决策（纯判据）：只消费账本事件切片与 main 祖先复核结果，不触 IO。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordDecision {
    /// merged + MergeExecuted 在账 + main 已含该 merge → 允许补记
    Proceed,
    /// 已有 TaskRecorded——幂等，绝不产第二条
    AlreadyRecorded,
    /// 其余一律拒绝（理由随枚举带出，说明该走哪条路）
    Reject(String),
}

/// 纯判据：`merge_in_main` 由调用方用 gitx 复核后传入——`None` 表示未复核，一律拒。
pub fn record_decision(
    events: &[EventRecord],
    task_id: &str,
    merge_in_main: Option<bool>,
) -> RecordDecision {
    let proj = fold(events);
    // fold 里 Merged 状态只由 MergeExecuted 事件产生（见 orch-core fold），故「投影为
    // Merged」与「MergeExecuted 在账」等价：无 MergeExecuted 时投影必非 Merged，
    // 在状态闸被拒绝（合并事实必须在账由此保证，无冗余事件扫描）。
    match proj.tasks.get(task_id).and_then(|t| t.state) {
        Some(TaskState::Recorded) => return RecordDecision::AlreadyRecorded,
        Some(TaskState::Merged) => {}
        other => {
            return RecordDecision::Reject(format!(
                "账本投影为 {other:?}（需 Merged——Approved 应走 run_merge；无 MergeExecuted 在账时投影不可能为 Merged，合并事实不在账同此拒绝）"
            ))
        }
    }
    match merge_in_main {
        Some(true) => RecordDecision::Proceed,
        Some(false) => {
            RecordDecision::Reject("main 未含该 merge——合并事实复核不通过，拒绝补记".to_string())
        }
        None => RecordDecision::Reject("未复核 main 祖先——不得只信账本自述，拒绝补记".to_string()),
    }
}

/// 纯落账：空门集/任一门红 → Err；全绿 → 恰好一条 TaskRecorded（actor=runtime:orch）。
pub fn record_events(
    task_id: &str,
    round: &str,
    gates: &[gate::GateResult],
) -> Result<Vec<EventRecord>> {
    if gates.is_empty() {
        bail!("拒绝记账：门集为空——没跑门绝不落 TaskRecorded");
    }
    if let Some(red) = gates.iter().find(|g| g.exit_code != 0) {
        bail!(
            "拒绝记账：门 {} 红（exit {}）——门红绝不落 TaskRecorded",
            red.name,
            red.exit_code
        );
    }
    Ok(vec![task_recorded_event(task_id, round)])
}

/// 显式 `orch record --at-tip` 的纯判据结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordRelaxation {
    /// main 尖端门已实跑全绿；完整 diff 文件清单进入审计事件。
    Allowed { files: Vec<String> },
    /// 为后续策略扩展保留的可审计拒绝形状；当前 fail-closed 拒绝直接走 Err。
    Refused { reason: String },
}

/// H29 补记门时点放宽纯判据。显式逃生舱仍要求：
/// - `merge_sha..tip_sha` 非空（空范围应走默认固定 merge_sha 路径）；
/// - main 尖端的真实 gate run 全绿；
/// - 文件清单逐项非空，并原样进入 `RecordGateRelaxed` 留证。
pub fn record_relaxation_plan(
    files: &[String],
    tip_gate_green: bool,
) -> Result<RecordRelaxation, String> {
    if files.is_empty() {
        return Err(
            "record --at-tip 拒绝：merge_sha..tipSha 为空——没有放宽时点，使用默认 record"
                .to_string(),
        );
    }
    if files.iter().any(|file| file.trim().is_empty()) {
        return Err("record --at-tip 拒绝：放宽文件清单含空路径，无法审计".to_string());
    }
    if !tip_gate_green {
        return Err("record --at-tip 拒绝：main 尖端门仍红——门红绝不落 TaskRecorded".to_string());
    }
    Ok(RecordRelaxation::Allowed {
        files: files.to_vec(),
    })
}

const RECORD_AT_TIP_REASON: &str =
    "explicit --at-tip escape hatch: reran every fast gate at the current main tip";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordRelaxationProof {
    pub merge_sha: String,
    pub tip_sha: String,
    pub reason: String,
    pub files: Vec<String>,
}

pub struct RecordOutcome {
    pub gates: Vec<gate::GateResult>,
    pub already_recorded: bool,
    pub relaxation: Option<RecordRelaxationProof>,
}

// 与 MergeOutcome 同理：GateResult 未派生 Debug，手写 impl 只暴露安全字段。
impl std::fmt::Debug for RecordOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordOutcome")
            .field("already_recorded", &self.already_recorded)
            .field("gates", &self.gates.len())
            .field("relaxation", &self.relaxation)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordGatePoint {
    MergeCommit,
    MainTip,
}

/// `merged → recorded` 补记入口：账本核 Merged → gitx 复核 mergeSha 是 main 祖先
/// → 逐项复跑 gates.fast（日志 tag `<task>-record`，不覆盖 `-postmerge`）→ 全绿落单条
/// TaskRecorded。任一门红 → 与 run_merge 同形的升级事实并返回错误，绝不落账。
pub fn run_record(root: &Path, task_id: &str) -> Result<RecordOutcome> {
    let outcome = with_merge_lifecycle_transition(root, "orch record", || {
        run_record_locked(root, task_id, RecordGatePoint::MergeCommit)
    })?;
    trigger_site_gc(root, "TaskRecorded");
    Ok(outcome)
}

/// H29 显式逃生舱：保留全部 root authorization 校验，只把 gate 的时间绑定
/// 从 exact MergeExecuted SHA 放宽到调用时的 current main tip。成功必须同批落
/// canonical `TaskRecorded` + 完整 `RecordGateRelaxed` 审计事实。
pub fn run_record_at_tip(root: &Path, task_id: &str) -> Result<RecordOutcome> {
    let outcome = with_merge_lifecycle_transition(root, "orch record --at-tip", || {
        run_record_locked(root, task_id, RecordGatePoint::MainTip)
    })?;
    trigger_site_gc(root, "TaskRecorded");
    Ok(outcome)
}

fn run_record_locked(
    root: &Path,
    task_id: &str,
    gate_point: RecordGatePoint,
) -> Result<RecordOutcome> {
    // Keep the retirement producer visibly bound to the record path; the
    // checked append below invokes this exact pure function with fresh events.
    let retire_task_sites = crate::sites::retire_task_sites;
    let round = fs::read_to_string(root.join("coordination/runtime/CURRENT-ROUND"))
        .context("CURRENT-ROUND 缺失")?
        .trim()
        .to_string();
    // Recovery crosses the same terminal state boundary as run_merge, so it
    // shares the exact transition lease.  Every mutable fact is read only
    // after the lease is held.
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let lr = read_ledger(&ledger_path)?;
    if !lr.bad_lines.is_empty() {
        bail!("record 拒绝坏账本");
    }
    let authorization =
        crate::verify::validate_root_record_authorization(root, &round, task_id, &lr.events)?;
    if authorization.already_recorded {
        println!("[orch] {task_id} 已记账（canonical TaskRecorded 在账）——幂等返回");
        return Ok(RecordOutcome {
            gates: Vec::new(),
            already_recorded: true,
            relaxation: None,
        });
    }
    let short = gitx::short(&authorization.merge_sha).to_string();
    let (gate_sha, widened_files) = match gate_point {
        RecordGatePoint::MergeCommit => (authorization.merge_sha.clone(), None),
        RecordGatePoint::MainTip => {
            let tip_sha = gitx::rev_parse(root, "main")?;
            let files = gitx::diff_names(root, &authorization.merge_sha, &tip_sha)?;
            if files.is_empty() {
                bail!(
                    "{}",
                    record_relaxation_plan(&files, true).expect_err("空 diff 必须 fail-closed")
                );
            }
            (tip_sha, Some(files))
        }
    };

    // Run only inside a clean detached worktree at the selected immutable
    // commit.  Default remains exact MergeExecuted; --at-tip captures exact
    // current main before creating the worktree.
    let log_dir = root.join("coordination/runtime/logs");
    let gate_wt =
        root.join(".cowork-temp")
            .join(format!("record-{}-{}", task_id, ulid::Ulid::new()));
    fs::create_dir_all(gate_wt.parent().context("record worktree 缺 parent")?)?;
    gitx::worktree_add_detached(root, &gate_wt, &gate_sha)?;
    let gate_run = (|| -> Result<Vec<gate::GateResult>> {
        if gitx::rev_parse(&gate_wt, "HEAD")? != gate_sha
            || !gitx::porcelain_v2(&gate_wt)?.trim().is_empty()
        {
            bail!("record detached worktree 初始 HEAD/clean 不匹配");
        }
        let c = card::load(&gate_wt, &round, task_id)?;
        let b = binding::load(&gate_wt)?;
        let mut gates = Vec::new();
        for gref in &c.meta.gates.fast {
            let spec = b
                .commands
                .get(gref)
                .with_context(|| format!("绑定缺命令: {gref}"))?;
            let tag = match gate_point {
                RecordGatePoint::MergeCommit => format!("{task_id}-record"),
                RecordGatePoint::MainTip => format!("{task_id}-record-at-tip"),
            };
            let result =
                with_close_orphan_watch(root, &round, task_id, &log_dir, &tag, gref, || {
                    gate::run_gate_with_audit_identity(
                        root,
                        &round,
                        ledger::GateAuditIdentity::Attempt {
                            task_id,
                            attempt_id: &authorization.attempt_id,
                        },
                        gref,
                        spec,
                        &gate_wt,
                        &log_dir,
                        &tag,
                    )
                })?;
            if gitx::rev_parse(&gate_wt, "HEAD")? != gate_sha
                || !gitx::porcelain_v2(&gate_wt)?.trim().is_empty()
            {
                bail!("record gate {gref} 改变 detached HEAD/worktree");
            }
            gates.push(result);
        }
        Ok(gates)
    })();
    gitx::worktree_remove(root, &gate_wt).context("清理 record detached worktree 失败")?;
    let gates = gate_run?;
    if let Some(red) = gates.iter().find(|gate| gate.exit_code != 0) {
        let _ = append_merge_lifecycle_events(
            root,
            &round,
            vec![post_merge_gate_failed_event(
                task_id,
                &round,
                &authorization.merge_sha,
                &red.name,
                red.exit_code,
            )],
        );
        let point = match gate_point {
            RecordGatePoint::MergeCommit => format!("merge {}", authorization.merge_sha),
            RecordGatePoint::MainTip => format!("main tip {gate_sha}"),
        };
        let relaxation_error = widened_files
            .as_ref()
            .and_then(|files| record_relaxation_plan(files, false).err());
        bail!(
            "补记门 {} 在 {} 红（exit {}）——main 已含合并 {short}，绝不落 TaskRecorded{}",
            red.name,
            point,
            red.exit_code,
            relaxation_error
                .map(|reason| format!("；{reason}"))
                .unwrap_or_default()
        );
    }
    println!("{}", gate::render_gate_summary_line(&gates));

    let relaxation = match widened_files {
        Some(files) => match record_relaxation_plan(&files, true)
            .map_err(anyhow::Error::msg)
            .context("record --at-tip 放宽判据拒绝")?
        {
            RecordRelaxation::Allowed { files } => Some(RecordRelaxationProof {
                merge_sha: authorization.merge_sha.clone(),
                tip_sha: gate_sha.clone(),
                reason: RECORD_AT_TIP_REASON.to_string(),
                files,
            }),
            RecordRelaxation::Refused { reason } => {
                bail!("record --at-tip 放宽判据拒绝：{reason}")
            }
        },
        None => None,
    };

    // The gate result authorizes no stale state: append_checked reacquires the
    // ledger lease and recomputes the complete chain/refs/source bindings.
    let mut recorded = record_events(task_id, &round, &gates)?;
    if let Some(proof) = &relaxation {
        // TaskRecorded first closes the existing merge barrier; the audit fact
        // follows in the same checked append and cannot exist on a red path.
        recorded.push(record_gate_relaxed_event(
            task_id,
            &round,
            &proof.merge_sha,
            &proof.tip_sha,
            &proof.reason,
            &proof.files,
        ));
    }
    let expected_relaxation = relaxation.clone();
    let appended = ledger::append_checked_merge_lifecycle(root, &round, |events| {
        let fresh =
            crate::verify::validate_root_record_authorization(root, &round, task_id, events)?;
        let mut expected = authorization.clone();
        expected.already_recorded = fresh.already_recorded;
        if fresh != expected {
            bail!("TaskRecorded append 前 post-merge authorization 漂移");
        }
        if fresh.already_recorded {
            return Ok(Vec::new());
        }
        if let Some(proof) = &expected_relaxation {
            let fresh_tip = gitx::rev_parse(root, "main")?;
            if fresh_tip != proof.tip_sha {
                bail!(
                    "TaskRecorded append 前 main tip 漂移：gate={} fresh={fresh_tip}",
                    proof.tip_sha
                );
            }
            let fresh_files = gitx::diff_names(root, &proof.merge_sha, &proof.tip_sha)?;
            if fresh_files != proof.files {
                bail!("TaskRecorded append 前 merge_sha..tipSha 文件清单漂移");
            }
        }
        let mut batch = recorded.clone();
        let retirements = retire_task_sites(events, task_id, &batch[0].event_id);
        batch.splice(1..1, retirements);
        Ok(batch)
    })?;

    Ok(RecordOutcome {
        gates,
        already_recorded: appended == 0,
        relaxation,
    })
}

// ─────────────────────── B153 · 悬空屏障恢复入口 ───────────────────────
// r51/B147 事故形态：MergeStarted 已落、git merge 冲突失败、refs 未动，账本
// 没有终态事实 ⇒ 屏障悬空，全轮 wake/nudge/verdict/append 全被拒。本入口是
// 该形态（含冲突落账前的崩溃窗口）的机械恢复出口：lease 内核
// `barrier_recovery_plan` 三条件 ⇒ 原子落 barrier-recovered（闭合屏障）+
// AttemptBlocked（冲突终局，放行接管）⇒ 真实 `run_dispatch` 可规划并唤醒
// 冲突修复 attempt。CLI 接线待后续卡（orch-cli 冻结），本卡以
// host 侧 pub fn 交付全部语义。

pub struct RecoveryOutcome {
    /// 是否实际落了 barrier-recovered 并闭合屏障（本实现恒 true——任何拒绝
    /// 都走 Err，绝不出「假成功」）。
    pub cleared: bool,
}

impl std::fmt::Debug for RecoveryOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecoveryOutcome")
            .field("cleared", &self.cleared)
            .finish()
    }
}

pub fn run_merge_recovery(root: &Path, task_id: &str) -> Result<RecoveryOutcome> {
    let outcome = with_merge_lifecycle_transition(root, "orch merge --recover", || {
        run_merge_recovery_locked(root, task_id)
    })?;
    trigger_site_gc(root, "merge recovery");
    Ok(outcome)
}

/// H29 释放臂的三条件纯判据。顺序即诊断优先级：先证明门确实红过，再证明
/// merge 仍在 main，最后证明 main 尖端门已真实复跑全绿。
pub fn release_preconditions(
    recorded_gate_failure: bool,
    merge_in_main: bool,
    tip_gate_green: bool,
) -> Result<(), String> {
    if !recorded_gate_failure {
        return Err("缺少 post-merge-gate 红记录——释放臂不得绕过从未失败过的门".to_string());
    }
    if !merge_in_main {
        return Err("main 已不含该 merge——释放屏障会掩盖真实合并状态".to_string());
    }
    if !tip_gate_green {
        return Err("main 尖端门仍红（red）——先修好再释放屏障".to_string());
    }
    Ok(())
}

/// H29 合后门红支：把「屏障永不可闭合 ⇒ 全轮冻结」变成一条留证的释放出口。
/// 三条件 fail-closed，任一不满足即拒：
/// 1. 账本确有本 (task, round) 的 `post-merge-gate` 升级——证明门确实红过，
///    不是拿释放臂绕过一次从未跑过的门；
/// 2. main 仍**确含**该 merge——合并事实未被回滚，释放屏障不会掩盖任何东西；
/// 3. 合后门在 **main 尖端**复跑全绿——证明红已被修复，release 不是掩盖问题。
/// 通过后只落 `post-merge-gate-released`（闭合屏障），**不落 TaskRecorded**：
/// 任务停在 `merged`，收轮必须显式披露。
fn release_post_merge_barrier(
    root: &Path,
    round: &str,
    task_id: &str,
    events: &[EventRecord],
    _main_head_sha: &str,
    authorization: &crate::verify::RootRecordAuthorization,
) -> Result<RecoveryOutcome> {
    if authorization.already_recorded {
        bail!("merge --recover {task_id} 释放臂拒绝已 TaskRecorded authorization");
    }
    let gate_red = events.iter().rev().find(|event| {
        event.kind == "EscalationRaised"
            && event.task_id.as_deref() == Some(task_id)
            && event.round.as_deref() == Some(round)
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("stage"))
                .and_then(serde_json::Value::as_str)
                == Some("post-merge-gate")
    });
    let Some(gate_red) = gate_red else {
        let reason = release_preconditions(false, true, true)
            .expect_err("缺 post-merge-gate 必须 fail-closed");
        bail!("merge --recover {task_id} 拒绝：{reason}；正常出口是 orch record 补记");
    };
    let payload = gate_red
        .payload
        .as_ref()
        .context("post-merge-gate 升级缺 payload")?;
    // post-merge-gate 允许短 sha（7..=40）；释放留痕一律存**全量 40 位**——
    // 它是「合并确实发生过」的证据，不该比合并本身更含糊。
    let recorded_sha = payload
        .get("mergeSha")
        .and_then(serde_json::Value::as_str)
        .context("post-merge-gate 升级缺 mergeSha")?;
    let merge_sha = gitx::rev_parse(root, recorded_sha)
        .with_context(|| format!("无法把 post-merge-gate 记录的 {recorded_sha} 解析为全量 sha"))?;
    let executed_sha = events
        .iter()
        .find(|event| {
            event.kind == "MergeExecuted"
                && event.task_id.as_deref() == Some(task_id)
                && event.round.as_deref() == Some(round)
        })
        .and_then(|event| event.payload.as_ref())
        .and_then(|payload| payload.get("mergeSha"))
        .and_then(serde_json::Value::as_str)
        .context("释放臂缺本 task/round 的 MergeExecuted.mergeSha")?;
    let executed_sha = gitx::rev_parse(root, executed_sha)
        .context("无法把 MergeExecuted.mergeSha 解析为全量 sha")?;
    if merge_sha != executed_sha {
        bail!(
            "merge --recover {task_id} 拒绝：post-merge-gate mergeSha {merge_sha} 未绑定 MergeExecuted {executed_sha}"
        );
    }
    let gate_name = payload
        .get("gate")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("<unknown>")
        .to_string();
    // 条件 2：main 仍确含该 merge（合并事实未被回滚）。
    let merge_in_main = gitx::is_ancestor(root, &merge_sha, "main")?;
    if let Err(reason) = release_preconditions(true, merge_in_main, true) {
        bail!("merge --recover {task_id} 拒绝：{reason}");
    }

    // 条件 3：在 clean detached **main tip** 逐门实跑。主工作区允许存在
    // 未提交 ledger/runtime 字节，它们绝不能污染或弱化释放证明。
    let tip_sha = gitx::rev_parse(root, "main")?;
    let log_dir = root.join("coordination/runtime/logs");
    let gate_wt =
        root.join(".cowork-temp")
            .join(format!("release-{}-{}", task_id, ulid::Ulid::new()));
    fs::create_dir_all(gate_wt.parent().context("release worktree 缺 parent")?)?;
    gitx::worktree_add_detached(root, &gate_wt, &tip_sha)?;
    let gate_run = (|| -> Result<Vec<gate::GateResult>> {
        if gitx::rev_parse(&gate_wt, "HEAD")? != tip_sha
            || !gitx::porcelain_v2(&gate_wt)?.trim().is_empty()
        {
            bail!("release detached worktree 初始 HEAD/clean 不匹配 main tip");
        }
        let card = card::load(&gate_wt, round, task_id)?;
        let binding = binding::load(&gate_wt)?;
        let mut gates = Vec::new();
        for gate_ref in &card.meta.gates.fast {
            let spec = binding
                .commands
                .get(gate_ref)
                .with_context(|| format!("绑定缺命令: {gate_ref}"))?;
            let tag = format!("{task_id}-release");
            let result =
                with_close_orphan_watch(root, round, task_id, &log_dir, &tag, gate_ref, || {
                    gate::run_gate_with_audit_identity(
                        root,
                        round,
                        ledger::GateAuditIdentity::Attempt {
                            task_id,
                            attempt_id: &authorization.attempt_id,
                        },
                        gate_ref,
                        spec,
                        &gate_wt,
                        &log_dir,
                        &tag,
                    )
                })?;
            if gitx::rev_parse(&gate_wt, "HEAD")? != tip_sha
                || !gitx::porcelain_v2(&gate_wt)?.trim().is_empty()
            {
                bail!("release gate {gate_ref} 改变 detached HEAD/worktree");
            }
            gates.push(result);
        }
        Ok(gates)
    })();
    gitx::worktree_remove(root, &gate_wt).context("清理 release detached worktree 失败")?;
    let gates = gate_run?;
    let tip_green = !gates.is_empty() && gates.iter().all(|gate| gate.exit_code == 0);
    if !tip_green {
        let reason =
            release_preconditions(true, true, false).expect_err("红/空 gate 必须 fail-closed");
        let detail = gates
            .iter()
            .find(|gate| gate.exit_code != 0)
            .map(|gate| format!("；门 {} exit={}", gate.name, gate.exit_code))
            .unwrap_or_else(|| "；门集为空".to_string());
        bail!("merge --recover {task_id} 拒绝：{reason}{detail}");
    }
    release_preconditions(true, true, true).map_err(anyhow::Error::msg)?;

    let release = barrier_released_event(task_id, round, &merge_sha, &gate_name);
    let expected_tip = tip_sha.clone();
    let expected_merge = merge_sha.clone();
    let appended = ledger::append_checked_merge_lifecycle(root, round, move |fresh_events| {
        let active =
            ledger::active_merge_barrier(fresh_events).context("释放留痕前活跃屏障已消失")?;
        if active.task_id != task_id || active.round != round || !active.merge_executed {
            bail!("释放留痕前活跃屏障 identity/phase 漂移");
        }
        let fresh_tip = gitx::rev_parse(root, "main")?;
        let fresh_contains = gitx::is_ancestor(root, &expected_merge, "main")?;
        if fresh_tip != expected_tip {
            bail!("释放留痕前 main tip 漂移：gate={expected_tip} fresh={fresh_tip}");
        }
        release_preconditions(
            fresh_events.iter().any(|event| {
                event.kind == "EscalationRaised"
                    && event.task_id.as_deref() == Some(task_id)
                    && event.round.as_deref() == Some(round)
                    && event
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("stage"))
                        .and_then(serde_json::Value::as_str)
                        == Some("post-merge-gate")
            }),
            fresh_contains,
            true,
        )
        .map_err(anyhow::Error::msg)?;
        Ok(vec![release.clone()])
    })?;
    if appended != 1 {
        bail!("post-merge-gate-released append count mismatch: expected 1, appended {appended}");
    }
    println!(
        "[orch] {task_id} 屏障已释放（post-merge-gate-released 留痕）——合后门红已在 main 尖端修复；\n\
         该任务停在 merged 未 Recorded，收轮时必须显式披露"
    );
    Ok(RecoveryOutcome { cleared: true })
}

const H48_BOUNDARY_RECOVERY_REASON: &str =
    "H48 merge-boundary recovery: verified the real no-ff merge and reran every fast gate at the captured current main tip";

/// Recover the H48 crash window: git produced a valid no-ff merge, but the old
/// exact-parent check emitted `merge-boundary-shape` before `MergeExecuted`.
/// The verifier supplies a provisional authorization derived from that unique
/// boundary fact; this function only chooses the immutable gate point and
/// atomically records the honest green/red result.
fn recover_boundary_merge(
    root: &Path,
    round: &str,
    task_id: &str,
    events: &[EventRecord],
) -> Result<RecoveryOutcome> {
    let authorization =
        crate::verify::validate_root_boundary_recovery_authorization(root, round, task_id, events)?;
    let tip_sha = gitx::rev_parse(root, "main")?;
    let widened_files = gitx::diff_names(root, &authorization.merge_sha, &tip_sha)?;

    let log_dir = root.join("coordination/runtime/logs");
    let gate_wt = root.join(".cowork-temp").join(format!(
        "boundary-recovery-{}-{}",
        task_id,
        ulid::Ulid::new()
    ));
    fs::create_dir_all(
        gate_wt
            .parent()
            .context("boundary recovery worktree 缺 parent")?,
    )?;
    gitx::worktree_add_detached(root, &gate_wt, &tip_sha)?;
    let gate_run = (|| -> Result<Vec<gate::GateResult>> {
        if gitx::rev_parse(&gate_wt, "HEAD")? != tip_sha
            || !gitx::porcelain_v2(&gate_wt)?.trim().is_empty()
        {
            bail!("boundary recovery detached worktree 初始 HEAD/clean 不匹配 main tip");
        }
        let c = card::load(&gate_wt, round, task_id)?;
        let b = binding::load(&gate_wt)?;
        if c.meta.gates.fast.is_empty() {
            bail!("boundary recovery fast gate 集为空，拒绝补记");
        }
        let mut gates = Vec::new();
        for gref in &c.meta.gates.fast {
            let spec = b
                .commands
                .get(gref)
                .with_context(|| format!("绑定缺命令: {gref}"))?;
            let tag = format!("{task_id}-boundary-recovery");
            let result =
                with_close_orphan_watch(root, round, task_id, &log_dir, &tag, gref, || {
                    gate::run_gate_with_audit_identity(
                        root,
                        round,
                        ledger::GateAuditIdentity::Attempt {
                            task_id,
                            attempt_id: &authorization.attempt_id,
                        },
                        gref,
                        spec,
                        &gate_wt,
                        &log_dir,
                        &tag,
                    )
                })?;
            if gitx::rev_parse(&gate_wt, "HEAD")? != tip_sha
                || !gitx::porcelain_v2(&gate_wt)?.trim().is_empty()
            {
                bail!("boundary recovery gate {gref} 改变 detached HEAD/worktree");
            }
            gates.push(result);
        }
        Ok(gates)
    })();
    gitx::worktree_remove(root, &gate_wt)
        .context("清理 boundary recovery detached worktree 失败")?;
    let gates = gate_run?;
    let red_gate = gates.iter().find(|gate| gate.exit_code != 0);

    let expected_authorization = authorization.clone();
    let expected_tip = tip_sha.clone();
    let expected_files = widened_files.clone();
    let proposed = if let Some(red) = red_gate {
        vec![
            merge_executed_event(task_id, round, &authorization.merge_sha),
            post_merge_gate_failed_event(
                task_id,
                round,
                &authorization.merge_sha,
                &red.name,
                red.exit_code,
            ),
        ]
    } else {
        vec![
            merge_executed_event(task_id, round, &authorization.merge_sha),
            task_recorded_event(task_id, round),
            record_gate_relaxed_event(
                task_id,
                round,
                &authorization.merge_sha,
                &tip_sha,
                H48_BOUNDARY_RECOVERY_REASON,
                &widened_files,
            ),
        ]
    };
    let proposed_count = proposed.len();
    let green_recovery = red_gate.is_none();
    let mut expected_appended = proposed_count;
    let appended = ledger::append_checked_merge_lifecycle(root, round, |fresh_events| {
        let fresh = crate::verify::validate_root_boundary_recovery_authorization(
            root,
            round,
            task_id,
            fresh_events,
        )?;
        if fresh != expected_authorization {
            bail!("H48 recovery append 前 provisional authorization 漂移");
        }
        let fresh_tip = gitx::rev_parse(root, "main")?;
        if fresh_tip != expected_tip {
            bail!("H48 recovery append 前 main tip 漂移：gate={expected_tip} fresh={fresh_tip}");
        }
        let fresh_files = gitx::diff_names(root, &fresh.merge_sha, &fresh_tip)?;
        if fresh_files != expected_files {
            bail!("H48 recovery append 前 mergeSha..tipSha 文件清单漂移");
        }
        let mut batch = proposed.clone();
        if green_recovery {
            let recorded_at = batch
                .iter()
                .position(|event| event.kind == "TaskRecorded")
                .context("H48 green recovery missing TaskRecorded candidate")?;
            let retirements = crate::sites::retire_task_sites(
                fresh_events,
                task_id,
                &batch[recorded_at].event_id,
            );
            batch.splice(recorded_at + 1..recorded_at + 1, retirements);
        }
        expected_appended = batch.len();
        Ok(batch)
    })?;
    if appended != expected_appended {
        bail!(
            "H48 recovery append count mismatch: expected {}, appended {appended}",
            expected_appended
        );
    }
    if let Some(red) = red_gate {
        bail!(
            "H48 recovery 门 {} 在 main tip {} 红（exit {}）——已原子补记 MergeExecuted + post-merge-gate；修绿后走 orch record --at-tip",
            red.name,
            tip_sha,
            red.exit_code
        );
    }
    println!("{}", gate::render_gate_summary_line(&gates));
    println!(
        "[orch] {task_id} H48 boundary 已恢复：MergeExecuted + TaskRecorded + RecordGateRelaxed 原子落账"
    );
    Ok(RecoveryOutcome { cleared: true })
}

fn run_merge_recovery_locked(root: &Path, task_id: &str) -> Result<RecoveryOutcome> {
    let round = fs::read_to_string(root.join("coordination/runtime/CURRENT-ROUND"))
        .context("CURRENT-ROUND 缺失")?
        .trim()
        .to_string();
    // Recovery crosses the same terminal state boundary as run_merge/run_record,
    // so it shares the exact transition lease.  Every mutable fact is read only
    // after the lease is held.
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let lr = read_ledger(&ledger_path)?;
    if !lr.bad_lines.is_empty() {
        bail!("merge --recover 拒绝坏账本");
    }
    let has_boundary_shape = lr.events.iter().any(|event| {
        event.kind == "EscalationRaised"
            && event.task_id.as_deref() == Some(task_id)
            && event.round.as_deref() == Some(round.as_str())
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("stage"))
                .and_then(serde_json::Value::as_str)
                == Some("merge-boundary-shape")
    });
    if has_boundary_shape {
        // Idempotent replay after a successful H48 recovery.  Recompute the
        // full ordinary record chain; never infer success from TaskRecorded
        // alone, and do not change `--recover` semantics for ordinary merges.
        if lr.events.iter().any(|event| {
            event.kind == "TaskRecorded"
                && event.task_id.as_deref() == Some(task_id)
                && event.round.as_deref() == Some(round.as_str())
        }) {
            let recorded = crate::verify::validate_root_record_authorization(
                root, &round, task_id, &lr.events,
            )?;
            if !recorded.already_recorded {
                bail!("TaskRecorded 在账但完整 record authorization 未投影 already_recorded");
            }
            println!("[orch] {task_id} H48 boundary 已恢复并记账——幂等返回");
            return Ok(RecoveryOutcome { cleared: true });
        }
        return recover_boundary_merge(root, &round, task_id, &lr.events);
    }
    let barrier = ledger::active_merge_barrier(&lr.events);
    if let Some(active) = &barrier {
        if active.task_id != task_id {
            bail!(
                "merge --recover {task_id} 拒绝：活跃屏障归属 task={}（round={}），张冠李戴的清屏障会掩盖他人的真实合并",
                active.task_id,
                active.round
            );
        }
        if active.round != round {
            bail!(
                "merge --recover {task_id} 拒绝：活跃屏障归属 round={} 与 CURRENT-ROUND {round} 不一致",
                active.round
            );
        }
        if active.merge_executed {
            // H29：正常出口仍是 orch record；只有当补记门**不可能转绿**时
            // （合后门已红且修复必然在合并之后）才走释放臂。三条件全部
            // fail-closed，任一不满足即拒。
            // Check the immutable merge fact before authorization inspects the
            // primary worktree branch.  A detached diagnostic checkout must
            // still report the real condition: main no longer contains the
            // merge, rather than leaking a symbolic-ref implementation error.
            let recorded_merge_sha = lr
                .events
                .iter()
                .rev()
                .find(|event| {
                    event.kind == "MergeExecuted"
                        && event.task_id.as_deref() == Some(task_id)
                        && event.round.as_deref() == Some(round.as_str())
                })
                .and_then(|event| event.payload.as_ref())
                .and_then(|payload| payload.get("mergeSha"))
                .and_then(serde_json::Value::as_str)
                .context("merge --recover 释放臂缺 MergeExecuted.mergeSha")?;
            if !gitx::is_ancestor(root, recorded_merge_sha, "main")? {
                bail!(
                    "merge --recover {task_id} 拒绝：main 已不含该 merge——释放屏障会掩盖真实合并状态"
                );
            }
            let authorization = crate::verify::validate_root_record_authorization(
                root,
                &round,
                task_id,
                &lr.events,
            )?;
            let released = release_post_merge_barrier(
                root,
                &round,
                task_id,
                &lr.events,
                &active.main_head_sha,
                &authorization,
            )?;
            return Ok(released);
        }
    }
    let barrier_active = barrier.is_some();
    // main 是否推进只信 git refs（barrier 快照里的 mainHeadSha 是 MergeStarted
    // 固定的 main），绝不信账本自述。
    let main_moved = match &barrier {
        Some(active) => gitx::rev_parse(root, "main")? != active.main_head_sha,
        None => false,
    };
    // verdict 有效性走完整 root merge authorization 重算（attempt/collect/
    // 账本次序/refs/review/evidence 字节全量复核），任一不过都按失效计——
    // 宁可拒恢复，绝不复活死链。authorization 本身也留住：恢复放行后它是
    // 冲突终局 attempt 事实的唯一合法 identity 来源。
    let authorization = if barrier_active {
        crate::verify::validate_root_merge_authorization(root, &round, task_id, &lr.events).ok()
    } else {
        None
    };
    let verdict_still_valid = authorization.is_some();
    // 直接证明「该 task 分支从未合入 main」：既不是 main 的祖先，也不是 main 历史里
    // 任何 merge commit 的父。两条都成立才算证明；任一步查询失败按 false 处理
    // ——绝不因为查不到就放行（fail-closed 方向）。
    let merge_provably_absent = if barrier_active {
        let branch = format!("task/{task_id}");
        match gitx::rev_parse(root, &branch) {
            Ok(branch_sha) => {
                let is_ancestor = gitx::is_ancestor(root, &branch_sha, "main").unwrap_or(true);
                let parented =
                    gitx::merge_parents_contain(root, "main", &branch_sha).unwrap_or(true);
                !is_ancestor && !parented
            }
            Err(_) => false,
        }
    } else {
        false
    };
    match barrier_recovery_plan(
        barrier_active,
        main_moved,
        verdict_still_valid,
        merge_provably_absent,
    ) {
        Ok(BarrierRecovery::ClearAndAllowRedispatch) => {}
        Err(reason) => bail!("merge --recover {task_id} 拒绝：{reason}"),
    }
    let authorization =
        authorization.context("恢复三条件已证 verdict 有效，authorization 必在场")?;
    // 原子同批：barrier-recovered 闭合屏障 + AttemptBlocked 判终局。二者缺一
    // 都留空洞——只清屏障时最近决定性事实仍是 root PASS（非接管终态），真实
    // run_dispatch 会 Existing 空转、不重派不唤醒（r52 主审 P0-2）。
    append_merge_lifecycle_events(
        root,
        &round,
        vec![
            barrier_recovered_event(task_id, &round),
            merge_conflict_attempt_blocked_event(task_id, &round, &authorization),
        ],
    )
    .context("落 barrier-recovered 留痕失败")?;
    // 崩溃窗口可能留下 MERGING 态主工作区；refs 未动已经内核证明，安全还原。
    abort_inflight_merge_best_effort(root);
    println!("[orch] {task_id} 屏障已清除（barrier-recovered 留痕）——可重派冲突修复 attempt");
    Ok(RecoveryOutcome { cleared: true })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── 纯判据内核（seed 契约表）───

    #[test]
    fn merge_failure_disposition_table() {
        assert_eq!(
            merge_failure_disposition(false, false),
            MergeFailure::ConflictEscalation
        );
        assert_eq!(
            merge_failure_disposition(true, false),
            MergeFailure::BoundaryViolation
        );
        assert_eq!(merge_failure_disposition(true, true), MergeFailure::None);
        assert_eq!(merge_failure_disposition(false, true), MergeFailure::None);
    }

    #[test]
    fn barrier_recovery_plan_truth_table() {
        assert_eq!(
            barrier_recovery_plan(true, false, true, false).unwrap(),
            BarrierRecovery::ClearAndAllowRedispatch
        );
        assert_eq!(
            barrier_recovery_plan(true, true, true, true).unwrap(),
            BarrierRecovery::ClearAndAllowRedispatch,
            "main 推进但合并确证不存在时必须允许恢复（H97/H98）"
        );
        assert!(
            barrier_recovery_plan(true, true, false, true).is_err(),
            "verdict 失效时即使合并确证不存在也不得复活死链"
        );
        assert!(barrier_recovery_plan(true, true, true, false)
            .unwrap_err()
            .contains("main 已推进"));
        assert!(barrier_recovery_plan(false, false, true, false)
            .unwrap_err()
            .contains("无活跃"));
        assert!(barrier_recovery_plan(true, false, false, false)
            .unwrap_err()
            .contains("verdict 已失效"));
        assert!(barrier_recovery_plan(false, true, false, false).is_err());
    }

    /// r62/B204: 一个 task 在一轮内可以有多条 `MergeStarted`——中止的在前，
    /// 成功的在后。原判据用 task 级「本轮有过 MergeExecuted」一票否决 staleness，
    /// 于是成功合并一落账，早先那条中止记录就"复活"成第二条 canonical 屏障，
    /// 收轮的「恰好一条」检查（round.rs / verify.rs）永久拒绝该 task。
    /// 位置化后：MergeExecuted 只否决**早于**本 attempt 终结的那些。
    #[test]
    fn stale_merge_started_survives_a_later_attempts_merge_executed() {
        let started = |attempt: &str| {
            ledger::event(
                "MergeStarted",
                "runtime:orch",
                Some("BT"),
                Some("rT"),
                serde_json::json!({ "attemptId": attempt }),
            )
        };
        let blocked = |attempt: &str| {
            ledger::event(
                "AttemptBlocked",
                "runtime:orch",
                Some("BT"),
                Some("rT"),
                serde_json::json!({ "attemptId": attempt }),
            )
        };
        let executed = || {
            ledger::event(
                "MergeExecuted",
                "reviewer:orch-runtime",
                Some("BT"),
                Some("rT"),
                serde_json::json!({ "mergeSha": "f".repeat(40), "policy": "no-ff" }),
            )
        };

        // A0001 中止 → A0002 合成。收轮必须只数到 A0002 那一条。
        let events = vec![
            started("BT-A0001"),
            blocked("BT-A0001"),
            started("BT-A0002"),
            executed(),
        ];
        assert!(
            merge_started_is_stale(&events, "BT", &events[0]),
            "中止在前、成功在后：早先那条必须仍判陈旧，否则收轮数到两条"
        );
        assert!(
            !merge_started_is_stale(&events, "BT", &events[2]),
            "承载 MergeExecuted 的那条永远不是陈旧记录"
        );

        // 反向：先有 MergeExecuted，之后才出现同 attempt 的 AttemptBlocked。
        // 该合并可能正描述 main 当前状态，保持 fail-closed 不判陈旧。
        let inverted = vec![started("BT-A0001"), executed(), blocked("BT-A0001")];
        assert!(
            !merge_started_is_stale(&inverted, "BT", &inverted[0]),
            "MergeExecuted 早于终结时必须保留否决（fail-closed 方向）"
        );

        // 屏障落在自己 attempt 终结之后：它是刚 arm 的活屏障，不是陈旧记录。
        // 判它陈旧会让 validate_merge_started_barrier 查无此条，而账本里屏障
        // 已 Active —— 全轮无出口。verify.rs 的终态守卫在上游拒掉这次授权，
        // 这里是第二道。
        let late = vec![blocked("BT-A0001"), started("BT-A0001")];
        assert!(
            !merge_started_is_stale(&late, "BT", &late[1]),
            "终结之后才落的屏障是活屏障，判陈旧会让它对屏障校验隐身"
        );
    }

    #[test]
    fn conflict_event_shapes_are_canonical() {
        let conflict = merge_conflict_event("B147", "r51", &["plan.rs".to_string()]);
        assert_eq!(conflict.kind, "EscalationRaised");
        assert_eq!(conflict.actor, "reviewer:orch-runtime");
        let payload = conflict.payload.as_ref().unwrap().as_object().unwrap();
        assert_eq!(payload.len(), 3, "merge-conflict payload 恰好三键");
        assert_eq!(
            payload.get("stage").and_then(|v| v.as_str()),
            Some("merge-conflict")
        );
        assert!(payload.get("mergeSha").unwrap().is_null());
        assert_eq!(
            payload
                .get("conflictFiles")
                .and_then(|v| v.as_array())
                .map(|files| files.len()),
            Some(1)
        );
        let recovered = barrier_recovered_event("B147", "r51");
        assert_eq!(recovered.kind, "EscalationRaised");
        let payload = recovered.payload.as_ref().unwrap().as_object().unwrap();
        assert_eq!(payload.len(), 1, "barrier-recovered payload 仅 stage 一键");
        assert_eq!(
            payload.get("stage").and_then(|v| v.as_str()),
            Some("barrier-recovered")
        );
    }

    #[test]
    fn stale_receipt_diagnostic_does_not_stop_reap_and_refusals_are_named() {
        let root = crate::util::test_scratch_dir("close-site-gc-live-report");
        git(&root, &["init", "-q"]);
        fs::write(root.join("tracked.txt"), "baseline\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=orch-test",
                "-c",
                "user.email=orch-test@example.invalid",
                "commit",
                "-q",
                "-m",
                "baseline",
            ],
        );
        fs::create_dir_all(root.join("coordination/rounds/rT")).unwrap();
        fs::create_dir_all(root.join("coordination/runtime/ledger-wal")).unwrap();
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "rT\n").unwrap();
        fs::write(root.join("coordination/rounds/rT/events.jsonl"), "").unwrap();
        fs::write(root.join("coordination/runtime/ledger-wal/rT.jsonl"), "").unwrap();
        fs::create_dir_all(root.join(".worktrees")).unwrap();
        fs::create_dir_all(root.join("orch/target")).unwrap();
        let head = gitx::rev_parse(&root, "HEAD").unwrap();

        let provision = |task: &str, attempt: &str, agent: &str, wake: &str| {
            crate::sites::lease_review_site_with(
                &root,
                "rT",
                task,
                attempt,
                crate::sites::SiteRole::Primary,
                agent,
                &head,
                wake,
                |site| {
                    gitx::worktree_add_detached(&root, &root.join(&site.worktree), &head)?;
                    fs::create_dir_all(root.join(&site.target))?;
                    Ok(())
                },
            )
            .unwrap()
        };
        let clean = provision("BT1", "BT1-A0001", "executor-clean", "wake-clean");
        let dirty = provision("BT2", "BT2-A0001", "executor-dirty", "wake-dirty");
        for site in [&clean, &dirty] {
            ledger::append(
                &root,
                "rT",
                &[ledger::event(
                    "WorkspaceReleased",
                    "runtime:orch",
                    Some(&site.task_id),
                    Some("rT"),
                    serde_json::json!({
                        "siteId": site.site_id,
                        "generation": site.generation,
                        "attemptId": site.attempt_id,
                        "role": site.role.as_str(),
                        "agent": site.agent,
                        "wakeId": site.wake_id,
                        "completionReceipt": crate::sites::MANAGED_COMPLETION_RECEIPT,
                    }),
                )],
            )
            .unwrap();
        }
        fs::write(root.join(&clean.target).join("reclaim-me"), b"actual bytes").unwrap();
        fs::write(root.join(&dirty.worktree).join("tracked.txt"), "dirty\n").unwrap();

        let stale_wake = "019fc246-1111-4222-8333-444455556666";
        let continuation = "review:rT:BST:BST-A0001:primary:executor-stale";
        let digest = "1111111111111111111111111111111111111111111111111111111111111111";
        let rendered = "2222222222222222222222222222222222222222222222222222222222222222";
        let log_path = root.join("stale-backend.jsonl").display().to_string();
        ledger::append(
            &root,
            "rT",
            &[
                ledger::event(
                    "WakeIssued",
                    "runtime:orch",
                    Some("BST"),
                    Some("rT"),
                    serde_json::json!({
                        "wakeId": stale_wake,
                        "continuationId": continuation,
                        "attemptId": "BST-A0001",
                        "agent": "executor-stale",
                        "providerKind": "opencode",
                        "requestMessageSha256": digest,
                        "renderedMessageSha256": rendered,
                        "requestSessionId": null,
                        "backendState": "pending",
                        "probeOffset": 0,
                        "logPath": log_path,
                    }),
                ),
                ledger::event(
                    "ReviewRequested",
                    "runtime:orch",
                    Some("BST"),
                    Some("rT"),
                    serde_json::json!({
                        "attemptId": "BST-A0001",
                        "role": "primary",
                        "agent": "executor-stale-other",
                        "deadlineSecs": 900,
                        "wakeId": stale_wake,
                        "continuationId": continuation,
                        "requestMessageSha256": digest,
                        "renderedMessageSha256": rendered,
                        "providerKind": "opencode",
                        "requestSessionId": null,
                        "logPath": log_path,
                        "reviewedHead": head,
                    }),
                ),
            ],
        )
        .unwrap();

        let lines = trigger_site_gc(&root, "TaskRecorded");
        assert!(lines.iter().any(|line| {
            line.contains("对账失败；已降级为诊断并继续 site GC")
                && line.contains("conflicting ReviewRequested")
        }));
        assert!(lines
            .iter()
            .any(|line| line.contains("removed=1 refused=1 freedBytes=")));
        assert!(lines.iter().any(|line| {
            line.contains(&format!("REFUSED {}", dirty.site_id))
                && line.contains("tracked/staged")
        }));
        assert!(!root.join(&clean.worktree).exists());
        assert!(!root.join(&clean.target).exists());
        assert!(root.join(&dirty.worktree).exists());
        assert!(root.join(&dirty.target).exists());

        fs::write(root.join(&dirty.worktree).join("tracked.txt"), "baseline\n").unwrap();
        gitx::worktree_remove(&root, &root.join(&dirty.worktree)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parse_conflict_files_from_git_output() {
        let stdout = "Auto-merging plan.rs\nCONFLICT (content): Merge conflict in plan.rs\n\
                      Auto-merging a/b.txt\nCONFLICT (add/add): Merge conflict in a/b.txt\n\
                      Automatic merge failed; fix conflicts and then commit the result.\n";
        assert_eq!(
            parse_conflict_files(stdout),
            vec!["plan.rs".to_string(), "a/b.txt".to_string()]
        );
        let modify_delete =
            "CONFLICT (modify/delete): old.rs deleted in HEAD and modified in task/B1.\n";
        assert_eq!(
            parse_conflict_files(modify_delete),
            vec!["old.rs".to_string()]
        );
        assert!(parse_conflict_files("nothing here\n").is_empty());
        // 去重保序
        let dup = "CONFLICT (content): Merge conflict in x.rs\nCONFLICT (content): Merge conflict in x.rs\n";
        assert_eq!(parse_conflict_files(dup), vec!["x.rs".to_string()]);
    }

    #[test]
    fn main_guard_block_marker_is_detected_on_either_output_stream_only() {
        for (stdout, stderr, expected) in [
            ("orch main guard: BLOCK: stdout rejection", "", true),
            ("", "fatal: orch main guard: BLOCK: stderr rejection", true),
            ("CONFLICT (content): Merge conflict in x.rs", "", false),
        ] {
            let failure = MergeCommandFailure {
                status: 1,
                stdout: stdout.to_string(),
                stderr: stderr.to_string(),
            };
            assert_eq!(merge_was_blocked_by_main_guard(&failure), expected);
        }
    }

    // ─── 生产路径实弹（r51/B147 事故原样重放 + 恢复出口）───

    const ROUND: &str = "rT";
    const TASK: &str = "BT";

    struct MergeFixture {
        root: PathBuf,
        task_head: String,
        main_head: String,
    }

    impl Drop for MergeFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn git_output(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn payload<'a>(event: &'a EventRecord, key: &str) -> Option<&'a serde_json::Value> {
        event.payload.as_ref()?.get(key)
    }

    fn is_record_gate_relaxed(event: &EventRecord) -> bool {
        event.kind == "EscalationRaised"
            && event.actor == "reviewer:orch-runtime"
            && payload(event, "stage").and_then(serde_json::Value::as_str)
                == Some("RecordGateRelaxed")
    }

    /// 与 merge_irreversible.rs 的 TestRoot 同形的全链 fixture（plan→sign_off→
    /// dispatch/collect→review/evidence→root verdict），差异仅在 `conflict`：
    /// true 时 main 与 task 分支改同一文件，run_merge 必冲突（r51/B147 现场）。
    fn merge_fixture(tag: &str, conflict: bool) -> MergeFixture {
        merge_fixture_with_gate(tag, conflict, "exit 0")
    }

    fn merge_fixture_with_gate(tag: &str, conflict: bool, gate_command: &str) -> MergeFixture {
        let root = crate::util::test_scratch_dir(tag);
        git(&root, &["init", "-q"]);
        git(
            &root,
            &["config", "user.email", "orch-test@example.invalid"],
        );
        git(&root, &["config", "user.name", "orch test"]);
        fs::write(root.join("README.md"), "base\n").unwrap();
        fs::write(
            root.join(".gitignore"),
            ".worktrees/\n.cowork-temp/\ncoordination/runtime/\n",
        )
        .unwrap();
        fs::write(root.join("conflict.txt"), "base\n").unwrap();
        git(&root, &["add", "README.md", ".gitignore", "conflict.txt"]);
        git(&root, &["commit", "-q", "-m", "base"]);
        git(&root, &["branch", "-M", "main"]);
        git(&root, &["checkout", "-q", "-b", "task/BT"]);
        fs::write(root.join("feature.txt"), "merged\n").unwrap();
        if conflict {
            fs::write(root.join("conflict.txt"), "task\n").unwrap();
        }
        git(&root, &["add", "feature.txt", "conflict.txt"]);
        git(&root, &["commit", "-q", "-m", "feature"]);
        git(&root, &["checkout", "-q", "main"]);
        if conflict {
            fs::write(root.join("conflict.txt"), "main\n").unwrap();
            git(&root, &["add", "conflict.txt"]);
            git(&root, &["commit", "-q", "-m", "main advances conflict.txt"]);
        }
        fs::create_dir_all(root.join(".worktrees")).unwrap();
        git(
            &root,
            &["worktree", "add", "-q", ".worktrees/BT", "task/BT"],
        );

        fs::create_dir_all(root.join("coordination/runtime")).unwrap();
        fs::create_dir_all(root.join("coordination/rounds/rT/tasks")).unwrap();
        fs::create_dir_all(root.join("coordination/rounds/rT/reviews")).unwrap();
        fs::create_dir_all(root.join("coordination/rounds/rT/evidence")).unwrap();
        fs::create_dir_all(root.join("coordination/modes")).unwrap();
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "rT\n").unwrap();
        fs::write(
            root.join("coordination/rounds/rT/tasks/BT.md"),
            "---\ntaskId: BT\nround: rT\nagent: executor-claw\nseedProtocol: pure-spec\nwriteSet: [feature.txt]\nfrozenPaths: [coordination/**]\ngates: {fast: [postGate]}\nbudgets: {wallMinutes: 30}\nrequiredReviews:\n  - {role: primary, agent: executor-desktop}\nrequiredEvidence: [merge-boundary]\n---\n# test\n",
        )
        .unwrap();
        fs::write(
            root.join("coordination/modes/test.yaml"),
            r#"agents:
  executor: {adapter: test, tier: none}
  verifier: {adapter: root-manual, tier: none}
hitl: {mergeGate: auto}
verification: {mode: root-manual-fixed-head}
liveness: {monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}
scheduling:
  allowedAgents: [executor-desktop, executor-claw, executor-opencode]
  capacities:
    executor-desktop: {agent: 2, quota: 2, roles: [implement, primary-review]}
    executor-claw: {agent: 1, quota: 1, roles: [implement, primary-review]}
    executor-opencode: {agent: 3, quota: 3, roles: [secondary-review]}
budgets:
  round: {maxUsd: 1, wallMinutes: 60, maxModelWakes: 2}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
"#,
        )
        .unwrap();
        let binding = "project: {ecosystems: [test]}\nworkspace: {worktreeRoot: .worktrees}\nscope: {protectedPaths: [\"coordination/**\"]}\ngit: {pushPolicy: forbidden}\ncommands:\n  postGate:\n    argv: [\"sh\", \"-c\", \"__GATE_COMMAND__\"]\n    timeoutSeconds: 30\n"
            .replace("__GATE_COMMAND__", gate_command);
        fs::write(root.join("coordination/PROJECT-BINDING.yaml"), binding).unwrap();
        // 真实可注入的 wake adapter（与 tierf fixture 同形）：spawn 往 marker
        // 文件追加一行，恢复后重派的「实际可达 wake」据此端到端断言。
        let wake_marker = root.join("wake-marker.txt");
        fs::write(
            root.join("coordination/agents.yaml"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "agents": {
                    "executor-claw": {
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
        fs::write(root.join("coordination/BOARD.md"), "# board\n").unwrap();

        let planned = crate::plan::run_plan(&root).unwrap();
        assert!(planned.event_appended);
        crate::round::run_sign_off(&root, Some("fixture root signoff")).unwrap();
        let task_head = git_output(&root, &["rev-parse", "task/BT"]);
        let main_head = git_output(&root, &["rev-parse", "main"]);
        let dispatch = ledger::event(
            "DispatchIssued",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "taskId": TASK,
                "agent": "executor-claw",
                "attemptId": "BT-A0001",
                "attemptNo": 1,
                "goPath": "coordination/rounds/rT/dispatch/executor-claw/GO-BT-A0001.md",
                "baseSha": main_head,
            }),
        );
        let receipt = ledger::event(
            "CollectGateSuccessReceipt",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "actionId": "collect-BT-A0001",
                "attemptId": "BT-A0001",
                "attemptNo": 1,
                "agent": "executor-claw",
                "baseSha": main_head,
                "goPath": "coordination/rounds/rT/dispatch/executor-claw/GO-BT-A0001.md",
                "branchSha": task_head,
            }),
        );
        let collect = ledger::event(
            "ReportCollectCompleted",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "actionId": "collect-BT-A0001",
                "attemptId": "BT-A0001",
                "attemptNo": 1,
                "agent": "executor-claw",
                "baseSha": main_head,
                "goPath": "coordination/rounds/rT/dispatch/executor-claw/GO-BT-A0001.md",
                "branchSha": task_head,
                "gateReceipt": receipt.event_id,
            }),
        );
        ledger::append(&root, ROUND, &[dispatch, receipt, collect]).unwrap();
        fs::write(
            root.join("coordination/rounds/rT/reviews/BT-A0001-primary-executor-desktop.md"),
            format!(
                "---\ntaskId: BT\nround: rT\nattemptId: BT-A0001\nrole: primary\nreviewer: executor-desktop\nverdict: PASS\nreviewedHead: {task_head}\n---\nfixture review\n"
            ),
        )
        .unwrap();
        fs::write(
            root.join("coordination/rounds/rT/evidence/BT-merge-boundary.json"),
            "{\"fixture\":true}\n",
        )
        .unwrap();
        git(&root, &["add", "coordination"]);
        git(&root, &["commit", "-q", "-m", "fixture signed contract"]);
        let main_head = git_output(&root, &["rev-parse", "main"]);
        crate::verify::run_root_verdict(
            &root,
            TASK,
            "BT-A0001",
            &task_head,
            &main_head,
            crate::verify::RootVerdict::Pass,
            None,
            false,
        )
        .unwrap();
        MergeFixture {
            root,
            task_head,
            main_head,
        }
    }

    impl MergeFixture {
        fn events(&self) -> Vec<EventRecord> {
            orch_core::read_ledger(
                &self
                    .root
                    .join(format!("coordination/rounds/{ROUND}/events.jsonl")),
            )
            .unwrap()
            .events
        }

        /// 复刻 r51/B147 的悬空屏障：不走 run_merge，直接把 canonical
        /// MergeStarted 塞进账本（模拟落 MergeStarted 后、任何终态事实前的
        /// 崩溃窗口）。
        fn arm_dangling_barrier(&self) {
            let authorization = crate::verify::validate_root_merge_authorization(
                &self.root,
                ROUND,
                TASK,
                &self.events(),
            )
            .unwrap();
            with_merge_lifecycle_transition(&self.root, "fixture arm barrier", || {
                ledger::append_checked_merge_lifecycle(&self.root, ROUND, move |_| {
                    Ok(vec![ledger::event(
                        "MergeStarted",
                        "runtime:orch",
                        Some(TASK),
                        Some(ROUND),
                        serde_json::json!({
                            "attemptId": authorization.attempt_id,
                            "attemptNo": authorization.attempt_no,
                            "headSha": authorization.head_sha,
                            "mainHeadSha": authorization.main_head_sha,
                            "collectCompletedEventId": authorization.collect_completed_event_id,
                            "verdictEventId": authorization.verdict_event_id,
                        }),
                    )])
                })?;
                Ok(())
            })
            .unwrap();
        }

        fn merge_task_without_accounting(&self) -> String {
            git(
                &self.root,
                &[
                    "merge",
                    "--no-ff",
                    "--no-verify",
                    "task/BT",
                    "-m",
                    "fixture: simulate crash after ref transaction",
                ],
            );
            git_output(&self.root, &["rev-parse", "main"])
        }

        fn append_merge_executed_only(&self, merge_sha: &str) {
            with_merge_lifecycle_transition(&self.root, "fixture append MergeExecuted", || {
                let appended = ledger::append_checked_merge_lifecycle(&self.root, ROUND, |_| {
                    Ok(vec![merge_executed_event(TASK, ROUND, merge_sha)])
                })?;
                if appended != 1 {
                    bail!("fixture expected one MergeExecuted, appended {appended}");
                }
                Ok(())
            })
            .unwrap();
        }

        fn storage_event(&self, task_id: &str, recovered: bool) -> EventRecord {
            let attempt_id = format!("{task_id}-A0001");
            let mut payload = serde_json::json!({
                "stage": "storage",
                "availableBytes": if recovered { 50 * 1024_u64.pow(3) } else { 45 * 1024_u64.pow(3) },
                "thresholdBytes": 46 * 1024_u64.pow(3),
                "entry": "gate",
                "probe": if recovered { "ok" } else { "low" },
                "probeReason": null,
                "attemptId": attempt_id,
            });
            if recovered {
                payload.as_object_mut().unwrap().insert(
                    "state".to_string(),
                    serde_json::Value::String("recovered".to_string()),
                );
            }
            ledger::event(
                "EscalationRaised",
                "runtime:orch",
                Some(task_id),
                Some(ROUND),
                payload,
            )
        }
    }

    #[test]
    fn active_merge_barrier_accepts_only_capability_bound_storage_audit() {
        let fixture = merge_fixture("storage-audit-authority", false);
        fixture.arm_dangling_barrier();
        let before = ledger::active_merge_barrier(&fixture.events()).unwrap();

        let unpaired_recovery = fixture.storage_event(TASK, true);
        let unpaired_id = unpaired_recovery.event_id.clone();
        with_merge_lifecycle_transition(
            &fixture.root,
            "fixture unpaired storage recovery",
            || ledger::append_storage_audit(&fixture.root, ROUND, unpaired_recovery),
        )
        .unwrap();
        assert!(fixture
            .events()
            .iter()
            .all(|event| event.event_id != unpaired_id));

        let ordinary = fixture.storage_event(TASK, false);
        assert!(ledger::append(&fixture.root, ROUND, &[ordinary.clone()]).is_err());
        assert!(ledger::append_storage_audit(&fixture.root, ROUND, ordinary.clone()).is_err());
        assert!(fixture.events().iter().all(|event| event.event_id != ordinary.event_id));

        let wrong_task = fixture.storage_event("BOTHER", false);
        let wrong = with_merge_lifecycle_transition(&fixture.root, "fixture wrong storage task", || {
            ledger::append_storage_audit(&fixture.root, ROUND, wrong_task)
        });
        assert!(wrong.is_err());

        let mut wrong_round = fixture.storage_event(TASK, false);
        wrong_round.round = Some("r-other".to_string());
        let wrong = with_merge_lifecycle_transition(&fixture.root, "fixture wrong storage round", || {
            ledger::append_storage_audit(&fixture.root, ROUND, wrong_round)
        });
        assert!(wrong.is_err());

        let mut wrong_actor = fixture.storage_event(TASK, false);
        wrong_actor.actor = "planner".to_string();
        let wrong = with_merge_lifecycle_transition(&fixture.root, "fixture wrong storage actor", || {
            ledger::append_storage_audit(&fixture.root, ROUND, wrong_actor)
        });
        assert!(wrong.is_err());

        let mut smuggled = fixture.storage_event(TASK, false);
        smuggled.payload.as_mut().unwrap()["mergeSha"] = serde_json::json!(fixture.main_head);
        let wrong = with_merge_lifecycle_transition(&fixture.root, "fixture smuggled storage key", || {
            ledger::append_storage_audit(&fixture.root, ROUND, smuggled)
        });
        assert!(wrong.is_err());

        with_merge_lifecycle_transition(&fixture.root, "fixture storage refusal", || {
            ledger::append_storage_audit(&fixture.root, ROUND, ordinary.clone())
        })
        .unwrap();
        // Same fresh locked lane is idempotent rather than duplicating the refusal.
        let duplicate_refusal = fixture.storage_event(TASK, false);
        with_merge_lifecycle_transition(&fixture.root, "fixture duplicate storage refusal", || {
            ledger::append_storage_audit(&fixture.root, ROUND, duplicate_refusal)
        })
        .unwrap();
        let events = fixture.events();
        assert_eq!(
            events.iter().filter(|event| {
                ledger::canonical_gate_storage_audit_event(event).is_some_and(|audit| {
                    audit.identity
                        == ledger::GateAuditIdentity::Attempt {
                            task_id: TASK,
                            attempt_id: "BT-A0001",
                        }
                        && !audit.recovered
                })
            }).count(),
            1
        );
        let after_refusal = ledger::active_merge_barrier(&events).unwrap();
        assert_eq!(before.task_id, after_refusal.task_id);
        assert_eq!(before.round, after_refusal.round);
        assert_eq!(before.merge_executed, after_refusal.merge_executed);
        assert_eq!(before.main_head_sha, after_refusal.main_head_sha);

        let recovered = fixture.storage_event(TASK, true);
        with_merge_lifecycle_transition(&fixture.root, "fixture storage recovery", || {
            ledger::append_storage_audit(&fixture.root, ROUND, recovered.clone())
        })
        .unwrap();
        let duplicate_recovery = fixture.storage_event(TASK, true);
        with_merge_lifecycle_transition(&fixture.root, "fixture duplicate storage recovery", || {
            ledger::append_storage_audit(&fixture.root, ROUND, duplicate_recovery)
        })
        .unwrap();
        let events = fixture.events();
        assert_eq!(
            events.iter().filter(|event| {
                ledger::canonical_gate_storage_audit_event(event).is_some_and(|audit| {
                    audit.identity
                        == ledger::GateAuditIdentity::Attempt {
                            task_id: TASK,
                            attempt_id: "BT-A0001",
                        }
                        && audit.recovered
                })
            }).count(),
            1
        );
        let after_recovery = ledger::active_merge_barrier(&events).unwrap();
        assert_eq!(before.task_id, after_recovery.task_id);
        assert_eq!(before.round, after_recovery.round);
        assert_eq!(before.merge_executed, after_recovery.merge_executed);
        assert_eq!(before.main_head_sha, after_recovery.main_head_sha);

        let merge_sha = fixture.merge_task_without_accounting();
        fixture.append_merge_executed_only(&merge_sha);
        let before_post_merge = ledger::active_merge_barrier(&fixture.events()).unwrap();
        assert!(before_post_merge.merge_executed);
        let post_merge_refusal = fixture.storage_event(TASK, false);
        with_merge_lifecycle_transition(&fixture.root, "fixture post-merge storage refusal", || {
            ledger::append_storage_audit(&fixture.root, ROUND, post_merge_refusal)
        })
        .unwrap();
        let post_merge_recovery = fixture.storage_event(TASK, true);
        with_merge_lifecycle_transition(&fixture.root, "fixture post-merge storage recovery", || {
            ledger::append_storage_audit(&fixture.root, ROUND, post_merge_recovery)
        })
        .unwrap();
        let after_post_merge = ledger::active_merge_barrier(&fixture.events()).unwrap();
        assert_eq!(before_post_merge.task_id, after_post_merge.task_id);
        assert_eq!(before_post_merge.round, after_post_merge.round);
        assert_eq!(before_post_merge.merge_executed, after_post_merge.merge_executed);
        assert_eq!(before_post_merge.main_head_sha, after_post_merge.main_head_sha);
    }

    #[test]
    fn record_storage_refusal_retry_recovers_and_reaches_the_real_gate_spawn() {
        let fixture = merge_fixture("record-storage-retry", false);
        fixture.arm_dangling_barrier();
        let merge_sha = fixture.merge_task_without_accounting();
        fixture.append_merge_executed_only(&merge_sha);
        let log_path = fixture
            .root
            .join("coordination/runtime/logs/BT-record-gate-postGate.log");
        fs::create_dir_all(fixture.root.join(".orch")).unwrap();
        fs::write(
            fixture.root.join(".orch/machine.yaml"),
            format!("storage:\n  floorBytes: {}\n", u64::MAX),
        )
        .unwrap();

        assert!(run_record(&fixture.root, TASK).is_err());
        assert!(!log_path.exists(), "record refusal must precede gate log/spawn");
        let events = fixture.events();
        assert!(ledger::active_merge_barrier(&events).unwrap().merge_executed);
        assert!(!events.iter().any(|event| {
            event.kind == "TaskRecorded" && event.task_id.as_deref() == Some(TASK)
        }));
        assert!(events.iter().any(|event| {
            ledger::canonical_gate_storage_audit_event(event).is_some_and(|audit| {
                audit.identity
                    == ledger::GateAuditIdentity::Attempt {
                        task_id: TASK,
                        attempt_id: "BT-A0001",
                    }
                    && !audit.recovered
            })
        }));

        fs::remove_file(fixture.root.join(".orch/machine.yaml")).unwrap();
        let retried = run_record(&fixture.root, TASK).unwrap();
        assert!(retried.gates.iter().any(|gate| gate.name == "postGate"));
        assert!(log_path.is_file(), "record retry must reach the real gate spawn marker");
        let events = fixture.events();
        assert!(ledger::active_merge_barrier(&events).is_none());
        assert!(events.iter().any(|event| {
            ledger::canonical_gate_storage_audit_event(event).is_some_and(|audit| {
                audit.identity
                    == ledger::GateAuditIdentity::Attempt {
                        task_id: TASK,
                        attempt_id: "BT-A0001",
                    }
                    && audit.recovered
            })
        }));
        assert!(events.iter().any(|event| {
            event.kind == "TaskRecorded" && event.task_id.as_deref() == Some(TASK)
        }));
    }

    fn assert_complete_seal_chain(fixture: &MergeFixture) {
        let events = fixture.events();
        validate_seal_postcondition(&events, ROUND, TASK, "BT-A0001").unwrap();
        for kind in [
            "VerdictIssued",
            "MergeStarted",
            "MergeExecuted",
            "TaskRecorded",
        ] {
            let count = events
                .iter()
                .filter(|event| event.kind == kind && event.task_id.as_deref() == Some(TASK))
                .count();
            assert_eq!(count, 1, "{kind} must be durable exactly once");
        }
        let authorization =
            crate::verify::validate_root_record_authorization(&fixture.root, ROUND, TASK, &events)
                .unwrap();
        assert!(authorization.already_recorded);
        assert_eq!(authorization.head_sha, fixture.task_head);
        assert_eq!(
            fs::read(fixture.root.join("coordination/rounds/rT/events.jsonl")).unwrap(),
            fs::read(
                fixture
                    .root
                    .join("coordination/runtime/ledger-wal/rT.jsonl")
            )
            .unwrap(),
            "successful seal must leave ledger/WAL byte-identical"
        );
    }

    #[test]
    fn seal_from_root_pass_is_complete_and_durable_replay_is_idempotent() {
        let fixture = merge_fixture("b204-seal-complete-replay", false);
        let first = run_seal(&fixture.root, TASK, "BT-A0001", &fixture.task_head).unwrap();
        assert!(!first.replayed_complete);
        assert_complete_seal_chain(&fixture);

        let ledger_before =
            fs::read(fixture.root.join("coordination/rounds/rT/events.jsonl")).unwrap();
        let replay = run_seal(&fixture.root, TASK, "BT-A0001", &fixture.task_head).unwrap();
        assert!(replay.replayed_complete);
        assert!(replay.gates.is_empty());
        assert_eq!(
            fs::read(fixture.root.join("coordination/rounds/rT/events.jsonl")).unwrap(),
            ledger_before,
            "complete replay must append no lifecycle facts"
        );
        assert_complete_seal_chain(&fixture);
    }

    #[test]
    fn seal_postcondition_rejects_noncanonical_lifecycle_envelopes_and_payloads() {
        let fixture = merge_fixture("b204-seal-postcondition-exact", false);
        run_seal(&fixture.root, TASK, "BT-A0001", &fixture.task_head).unwrap();
        let events = fixture.events();

        let mut wrong_verdict_actor = events.clone();
        wrong_verdict_actor
            .iter_mut()
            .find(|event| event.kind == "VerdictIssued")
            .unwrap()
            .actor = "runtime:orch".to_string();
        assert!(
            validate_seal_postcondition(&wrong_verdict_actor, ROUND, TASK, "BT-A0001").is_err()
        );

        let mut wrong_started_tuple = events.clone();
        wrong_started_tuple
            .iter_mut()
            .find(|event| event.kind == "MergeStarted")
            .unwrap()
            .payload
            .as_mut()
            .unwrap()["attemptNo"] = serde_json::json!(2);
        assert!(
            validate_seal_postcondition(&wrong_started_tuple, ROUND, TASK, "BT-A0001").is_err()
        );

        let mut wrong_policy = events.clone();
        wrong_policy
            .iter_mut()
            .find(|event| event.kind == "MergeExecuted")
            .unwrap()
            .payload
            .as_mut()
            .unwrap()["policy"] = serde_json::json!("ff");
        assert!(validate_seal_postcondition(&wrong_policy, ROUND, TASK, "BT-A0001").is_err());

        let mut extra_recorded_payload = events;
        extra_recorded_payload
            .iter_mut()
            .find(|event| event.kind == "TaskRecorded")
            .unwrap()
            .payload
            .as_mut()
            .unwrap()["extra"] = serde_json::json!(true);
        assert!(
            validate_seal_postcondition(&extra_recorded_payload, ROUND, TASK, "BT-A0001").is_err()
        );
    }

    #[test]
    fn seal_recovers_crash_after_merge_started_before_ref_move() {
        let fixture = merge_fixture("b204-seal-recover-started", false);
        fixture.arm_dangling_barrier();
        assert_eq!(
            git_output(&fixture.root, &["rev-parse", "main"]),
            fixture.main_head
        );
        assert!(!fixture
            .events()
            .iter()
            .any(|event| event.kind == "MergeExecuted"));

        let outcome = run_seal(&fixture.root, TASK, "BT-A0001", &fixture.task_head).unwrap();
        assert!(!outcome.replayed_complete);
        assert_complete_seal_chain(&fixture);
    }

    #[test]
    fn seal_recovers_crash_after_merge_ref_before_merge_executed() {
        let fixture = merge_fixture("b204-seal-recover-ref", false);
        fixture.arm_dangling_barrier();
        let merge_sha = fixture.merge_task_without_accounting();
        let before = fixture.events();
        assert_eq!(
            before
                .iter()
                .filter(|event| event.kind == "MergeStarted")
                .count(),
            1
        );
        assert!(!before.iter().any(|event| event.kind == "MergeExecuted"));

        let outcome = run_seal(&fixture.root, TASK, "BT-A0001", &fixture.task_head).unwrap();
        assert!(!outcome.replayed_complete);
        let events = fixture.events();
        let recorded_merge = events
            .iter()
            .find(|event| event.kind == "MergeExecuted")
            .and_then(|event| payload(event, "mergeSha"))
            .and_then(serde_json::Value::as_str);
        assert_eq!(recorded_merge, Some(merge_sha.as_str()));
        assert_complete_seal_chain(&fixture);
    }

    #[test]
    fn seal_recovers_crash_after_merge_executed_before_record() {
        let fixture = merge_fixture("b204-seal-recover-record", false);
        fixture.arm_dangling_barrier();
        let merge_sha = fixture.merge_task_without_accounting();
        fixture.append_merge_executed_only(&merge_sha);
        let before = fixture.events();
        assert_eq!(
            before
                .iter()
                .filter(|event| event.kind == "MergeExecuted")
                .count(),
            1
        );
        assert!(!before.iter().any(|event| event.kind == "TaskRecorded"));

        let outcome = run_seal(&fixture.root, TASK, "BT-A0001", &fixture.task_head).unwrap();
        assert!(!outcome.replayed_complete);
        assert_complete_seal_chain(&fixture);
    }

    #[cfg(unix)]
    #[test]
    fn main_guard_rejection_preserves_started_barrier_without_forging_conflict() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = merge_fixture("b204-main-guard-block", false);
        let root = &fixture.root;
        // Keep the injected hook below the git dir so the pre-merge clean-tree
        // check sees only the intended ledger mutation, not test scaffolding.
        let hook_dir = root.join(".git/reject-hooks");
        fs::create_dir_all(&hook_dir).unwrap();
        let hook = hook_dir.join("reference-transaction");
        fs::write(
            &hook,
            "#!/bin/sh\n\
             if [ \"${1-}\" = prepared ]; then\n\
               while read -r old new refname; do\n\
                 if [ \"$refname\" = refs/heads/main ] && [ \"$old\" != \"$new\" ]; then\n\
                   echo 'orch main guard: BLOCK: injected test rejection' >&2\n\
                   exit 1\n\
                 fi\n\
               done\n\
             fi\n\
             exit 0\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();
        git(
            root,
            &[
                "config",
                "core.hooksPath",
                hook_dir.to_str().expect("hook path UTF-8"),
            ],
        );

        let error = run_merge(root, TASK).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(MAIN_GUARD_BLOCK_MARKER), "{message}");
        assert!(message.contains("未记 merge-conflict"), "{message}");
        assert_eq!(git_output(root, &["rev-parse", "main"]), fixture.main_head);

        let events = fixture.events();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == "MergeStarted")
                .count(),
            1
        );
        assert!(!events.iter().any(|event| {
            event.kind == "EscalationRaised"
                && payload(event, "stage").and_then(serde_json::Value::as_str)
                    == Some("merge-conflict")
        }));
        assert!(!events.iter().any(|event| event.kind == "AttemptBlocked"));
        assert!(ledger::active_merge_barrier(&events).is_some());

        let merge_head = Command::new("git")
            .args(["rev-parse", "--verify", "-q", "MERGE_HEAD"])
            .current_dir(root)
            .status()
            .unwrap();
        assert!(
            !merge_head.success(),
            "guard rejection must be safely aborted"
        );

        let recovered = run_merge_recovery(root, TASK)
            .expect("unmoved main + valid verdict must retain the ordinary recovery exit");
        assert!(recovered.cleared);
    }

    fn postmerge_red_fixture(tag: &str) -> (MergeFixture, String) {
        // Root verdict runs in task/BT, whose historical tree predates the
        // coordination contract and therefore stays green. The exact merge
        // commit contains coordination/** but not repair.ok, so only the
        // post-merge gate turns red; a later tip repair makes it green again.
        let fixture = merge_fixture_with_gate(
            tag,
            false,
            "test ! -f coordination/PROJECT-BINDING.yaml || test -f repair.ok",
        );
        let error = run_merge(&fixture.root, TASK).unwrap_err().to_string();
        assert!(
            (error.contains("合并后门") || error.contains("合后门")) && error.contains("红"),
            "{error}"
        );
        let merge_sha = git_output(&fixture.root, &["rev-parse", "main"]);
        let events = fixture.events();
        assert!(events.iter().any(|event| {
            event.kind == "MergeExecuted"
                && payload(event, "mergeSha").and_then(|value| value.as_str())
                    == Some(merge_sha.as_str())
        }));
        assert!(events.iter().any(|event| {
            event.kind == "EscalationRaised"
                && payload(event, "stage").and_then(|value| value.as_str())
                    == Some("post-merge-gate")
        }));
        let barrier = ledger::active_merge_barrier(&events).expect("合后门红后屏障必须仍 active");
        assert!(barrier.merge_executed);
        (fixture, merge_sha)
    }

    fn commit_tip_file(root: &Path, path: &str, contents: &str, message: &str) -> String {
        fs::write(root.join(path), contents).unwrap();
        git(root, &["add", path]);
        git(root, &["commit", "-q", "-m", message]);
        git_output(root, &["rev-parse", "main"])
    }

    fn ordinary_event(kind: &str, task: Option<&str>) -> EventRecord {
        ledger::event(
            kind,
            "runtime:orch",
            task,
            Some(ROUND),
            serde_json::json!({}),
        )
    }

    #[test]
    fn release_stage_is_rejected_before_merge_executed() {
        let fixture = merge_fixture("b159-release-before-executed", false);
        fixture.arm_dangling_barrier();
        let release = barrier_released_event(
            TASK,
            ROUND,
            &git_output(&fixture.root, &["rev-parse", "main"]),
            "postGate",
        );
        let error =
            with_merge_lifecycle_transition(&fixture.root, "fixture premature release", || {
                ledger::append_checked_merge_lifecycle(&fixture.root, ROUND, |_| {
                    Ok(vec![release.clone()])
                })?;
                Ok(())
            })
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("只在 MergeExecuted 之后有效"),
            "premature release must be rejected: {error}"
        );
        let barrier = ledger::active_merge_barrier(&fixture.events())
            .expect("rejected release must leave barrier active");
        assert!(!barrier.merge_executed);
    }

    #[test]
    fn release_refuses_executed_barrier_without_gate_failure_record() {
        let fixture = merge_fixture("b159-release-no-red-record", false);
        fixture.arm_dangling_barrier();
        merge_no_ff_without_hooks(
            &fixture.root,
            &fixture.task_head,
            "fixture exact merge without post-gate",
        )
        .unwrap_or_else(|failure| panic!("{failure}"));
        let merge_sha = git_output(&fixture.root, &["rev-parse", "main"]);
        with_merge_lifecycle_transition(&fixture.root, "fixture account merge", || {
            append_merge_lifecycle_events(
                &fixture.root,
                ROUND,
                vec![merge_executed_event(TASK, ROUND, &merge_sha)],
            )
        })
        .unwrap();

        let error = run_merge_recovery(&fixture.root, TASK)
            .unwrap_err()
            .to_string();
        assert!(error.contains("post-merge-gate"), "{error}");
        assert!(ledger::active_merge_barrier(&fixture.events())
            .is_some_and(|barrier| barrier.merge_executed));
    }

    #[test]
    fn release_refuses_when_main_no_longer_contains_recorded_merge() {
        let (fixture, merge_sha) = postmerge_red_fixture("b159-release-main-lost");
        git(&fixture.root, &["checkout", "-q", "--detach", &merge_sha]);
        git(&fixture.root, &["branch", "-f", "main", &fixture.main_head]);

        let error = run_merge_recovery(&fixture.root, TASK)
            .unwrap_err()
            .to_string();
        assert!(error.contains("main 已不含"), "{error}");
        assert!(ledger::active_merge_barrier(&fixture.events())
            .is_some_and(|barrier| barrier.merge_executed));
    }

    #[test]
    fn release_refuses_while_gate_is_still_red_at_main_tip() {
        let (fixture, _) = postmerge_red_fixture("b159-release-tip-red");
        let error = run_merge_recovery(&fixture.root, TASK)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("仍红") && error.contains("postGate"),
            "{error}"
        );
        let events = fixture.events();
        assert!(!events.iter().any(|event| {
            payload(event, "stage").and_then(|value| value.as_str())
                == Some("post-merge-gate-released")
        }));
        assert!(!events.iter().any(|event| event.kind == "TaskRecorded"));
    }

    #[test]
    fn release_closes_only_the_barrier_and_second_merge_stays_rejected() {
        let (fixture, merge_sha) = postmerge_red_fixture("b159-release-success");
        commit_tip_file(
            &fixture.root,
            "repair.ok",
            "fixed\n",
            "repair gate at main tip",
        );

        let outcome =
            run_merge_recovery(&fixture.root, TASK).expect("three proofs release barrier");
        assert!(outcome.cleared);
        let events = fixture.events();
        let release = events
            .iter()
            .find(|event| {
                payload(event, "stage").and_then(|value| value.as_str())
                    == Some("post-merge-gate-released")
            })
            .expect("release audit event");
        assert_eq!(
            payload(release, "mergeSha").and_then(|value| value.as_str()),
            Some(merge_sha.as_str())
        );
        assert_eq!(
            payload(release, "mergeSha")
                .and_then(|value| value.as_str())
                .map(str::len),
            Some(40)
        );
        assert!(
            !events.iter().any(|event| event.kind == "TaskRecorded"),
            "release must never synthesize TaskRecorded"
        );
        assert!(
            ledger::active_merge_barrier(&events).is_none(),
            "release must close the post-merge barrier"
        );

        let second = run_merge(&fixture.root, TASK).unwrap_err().to_string();
        assert!(
            second.contains("Merged")
                || second.contains("MergeExecuted")
                || second.contains("Approved"),
            "second merge must remain rejected by existing merge truth: {second}"
        );
        assert_eq!(
            fixture
                .events()
                .iter()
                .filter(|event| event.kind == "MergeExecuted")
                .count(),
            1,
            "release cannot permit a double merge"
        );
    }

    #[test]
    fn at_tip_record_rejects_a_red_tip_without_audit_or_task_recorded() {
        let (fixture, _) = postmerge_red_fixture("b159-record-tip-red");
        commit_tip_file(
            &fixture.root,
            "later.txt",
            "tip moved but gate remains red\n",
            "advance red tip",
        );
        let error = run_record_at_tip(&fixture.root, TASK)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("main tip") && error.contains("红"),
            "{error}"
        );
        let events = fixture.events();
        assert!(!events.iter().any(|event| event.kind == "TaskRecorded"));
        assert!(!events.iter().any(is_record_gate_relaxed));
    }

    #[test]
    fn at_tip_record_is_real_gate_path_and_leaves_complete_audit() {
        let (fixture, merge_sha) = postmerge_red_fixture("b159-record-at-tip");
        let tip_sha = commit_tip_file(
            &fixture.root,
            "repair.ok",
            "fixed\n",
            "repair gate after merge",
        );

        // Default behavior remains pinned to merge_sha, so it still sees the
        // historical red tree even though current main tip is repaired.
        let default_error = run_record(&fixture.root, TASK).unwrap_err().to_string();
        assert!(
            default_error.contains(&merge_sha)
                && default_error.contains("红")
                && !default_error.contains("RecordGateRelaxed"),
            "{default_error}"
        );
        assert!(!fixture
            .events()
            .iter()
            .any(|event| event.kind == "TaskRecorded"));

        let expected_files = gitx::diff_names(&fixture.root, &merge_sha, &tip_sha).unwrap();
        assert_eq!(expected_files, vec!["repair.ok".to_string()]);
        let outcome =
            run_record_at_tip(&fixture.root, TASK).expect("green tip must allow audited record");
        assert!(!outcome.already_recorded);
        let proof = outcome.relaxation.as_ref().expect("relaxation proof");
        assert_eq!(proof.merge_sha, merge_sha);
        assert_eq!(proof.tip_sha, tip_sha);
        assert_eq!(proof.files, expected_files);
        assert!(!proof.reason.is_empty());
        assert!(outcome.gates.iter().all(|gate| gate.exit_code == 0));

        let events = fixture.events();
        let task_recorded_position = events
            .iter()
            .position(|event| event.kind == "TaskRecorded")
            .expect("TaskRecorded");
        let relaxed_position = events
            .iter()
            .position(is_record_gate_relaxed)
            .expect("RecordGateRelaxed");
        assert!(
            task_recorded_position < relaxed_position,
            "same checked batch first closes canonical barrier, then leaves audit"
        );
        let relaxed = &events[relaxed_position];
        assert_eq!(relaxed.actor, "reviewer:orch-runtime");
        assert_eq!(
            payload(relaxed, "mergeSha").and_then(|value| value.as_str()),
            Some(proof.merge_sha.as_str())
        );
        assert_eq!(
            payload(relaxed, "tipSha").and_then(|value| value.as_str()),
            Some(proof.tip_sha.as_str())
        );
        assert_eq!(
            payload(relaxed, "reason").and_then(|value| value.as_str()),
            Some(proof.reason.as_str())
        );
        assert_eq!(
            payload(relaxed, "files")
                .and_then(|value| value.as_array())
                .map(|files| {
                    files
                        .iter()
                        .filter_map(|file| file.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                }),
            Some(expected_files)
        );

        let replay =
            run_record_at_tip(&fixture.root, TASK).expect("record replay must be idempotent");
        assert!(replay.already_recorded);
        assert!(replay.relaxation.is_none());
        assert_eq!(
            fixture
                .events()
                .iter()
                .filter(|event| is_record_gate_relaxed(event))
                .count(),
            1
        );
    }

    /// 本卡灵魂：r51/B147 事故原样重放——真实 run_merge 走冲突路径，
    /// ①落 merge-conflict escalation ②屏障闭合 ③wake/append 可达（全轮冻结的反面）。
    #[test]
    fn conflict_failure_closes_barrier_and_keeps_round_alive() {
        let fixture = merge_fixture("b153-conflict-live", true);
        let root = &fixture.root;
        let main_before = git_output(root, &["rev-parse", "main"]);
        assert_eq!(main_before, fixture.main_head, "verdict 绑定的 main 未漂移");

        let error = run_merge(root, TASK).unwrap_err().to_string();
        assert!(error.contains("冲突失败"), "{error}");
        // refs 零移动。
        assert_eq!(git_output(root, &["rev-parse", "main"]), main_before);
        assert_eq!(
            git_output(root, &["rev-parse", "task/BT"]),
            fixture.task_head
        );

        let events = fixture.events();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == "MergeStarted")
                .count(),
            1
        );
        let conflict = events
            .iter()
            .find(|event| {
                event.kind == "EscalationRaised"
                    && payload(event, "stage").and_then(|v| v.as_str()) == Some("merge-conflict")
            })
            .expect("冲突路径必须落 merge-conflict escalation");
        assert!(payload(conflict, "mergeSha").unwrap().is_null());
        let files = payload(conflict, "conflictFiles")
            .and_then(|v| v.as_array())
            .unwrap();
        assert!(
            files.iter().any(|f| f.as_str() == Some("conflict.txt")),
            "conflictFiles 必须含 conflict.txt: {files:?}"
        );
        assert!(!events.iter().any(|event| event.kind == "MergeExecuted"));
        assert!(!events.iter().any(|event| event.kind == "TaskRecorded"));

        // P0-2：冲突终态必须同批落 AttemptBlocked 判该 attempt 终局——否则
        // root PASS 仍是非接管终态，真实 dispatch 只会 Existing 空转。
        let blocked = events
            .iter()
            .find(|event| event.kind == "AttemptBlocked" && event.task_id.as_deref() == Some(TASK))
            .expect("冲突路径必须落 AttemptBlocked 终局事实");
        assert_eq!(
            payload(blocked, "attemptId").and_then(|v| v.as_str()),
            Some("BT-A0001")
        );
        assert_eq!(
            payload(blocked, "agent").and_then(|v| v.as_str()),
            Some("executor-claw")
        );
        assert_eq!(
            payload(blocked, "stage").and_then(|v| v.as_str()),
            Some("merge-conflict")
        );
        // 投影态从 Approved 回到 Blocked（nudge/dispatch 身份解析不再拒）。
        let projection = orch_core::fold(&events);
        assert_eq!(
            projection.tasks.get(TASK).and_then(|task| task.state),
            Some(orch_core::TaskState::Blocked)
        );

        // ②③：屏障已闭合——普通 append / wake 类 transition / effect 全部可达
        // （r51 全轮冻结的反面）。
        ledger::append(root, ROUND, &[ordinary_event("TaskValidated", None)]).unwrap();
        with_protocol_transition(root, "planner nudge", || Ok(())).unwrap();
        with_protocol_effect(root, "planner wake", || Ok(())).unwrap();

        // 主工作区不停在 MERGING 态（best-effort abort），冲突文件还原。
        let merge_head = Command::new("git")
            .args(["rev-parse", "-q", "--verify", "MERGE_HEAD"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(!merge_head.status.success(), "MERGE_HEAD 必须已被清理");
        assert_eq!(
            fs::read_to_string(root.join("conflict.txt")).unwrap(),
            "main\n"
        );
        let porcelain = gitx::porcelain_v2(root).unwrap();
        assert!(
            !porcelain.lines().any(|line| line.starts_with("u ")),
            "不得残留 unmerged 条目: {porcelain}"
        );

        // 屏障已闭合 ⇒ 恢复入口拒（无屏障可恢复）。
        let error = run_merge_recovery(root, TASK).unwrap_err().to_string();
        assert!(error.contains("无活跃"), "{error}");
    }

    #[test]
    fn h48_boundary_recovery_records_real_merge_and_is_idempotent() {
        let fixture = merge_fixture("h48-boundary-recovery", false);
        let root = &fixture.root;
        fixture.arm_dangling_barrier();

        fs::OpenOptions::new()
            .append(true)
            .open(root.join("coordination/BOARD.md"))
            .and_then(|mut file| writeln!(file, "planner coordination-only advance"))
            .unwrap();
        git(root, &["add", "coordination/BOARD.md"]);
        git(root, &["commit", "-q", "-m", "planner board advance"]);
        let first_parent = git_output(root, &["rev-parse", "main"]);
        git(
            root,
            &[
                "merge",
                "--no-ff",
                "--no-edit",
                "task/BT",
                "-m",
                "fixture h48 merge",
            ],
        );
        let merge_sha = git_output(root, &["rev-parse", "main"]);
        let proof = crate::verify::validate_merge_commit_shape(
            root,
            &merge_sha,
            &fixture.main_head,
            &fixture.task_head,
            &[],
        )
        .unwrap();
        assert_eq!(proof.first_parent_sha, first_parent);
        assert_eq!(
            proof.changed_paths,
            vec!["coordination/BOARD.md".to_string()]
        );

        with_merge_lifecycle_transition(root, "fixture H48 boundary", || {
            account_merge_boundary_violation(
                root,
                ROUND,
                TASK,
                &merge_sha,
                &merge_sha,
                false,
                "old exact-parent check rejected coordination-only first-parent advance",
            )
        })
        .unwrap();

        let outcome = run_merge_recovery(root, TASK).expect("H48 boundary recovery must succeed");
        assert!(outcome.cleared);
        let events = fixture.events();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == "MergeExecuted")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == "TaskRecorded")
                .count(),
            1
        );
        let relaxed = events
            .iter()
            .find(|event| is_record_gate_relaxed(event))
            .expect("H48 green recovery must leave RecordGateRelaxed");
        assert_eq!(
            payload(relaxed, "mergeSha").and_then(serde_json::Value::as_str),
            Some(merge_sha.as_str())
        );
        assert_eq!(
            payload(relaxed, "tipSha").and_then(serde_json::Value::as_str),
            Some(merge_sha.as_str())
        );
        assert_eq!(
            payload(relaxed, "files").and_then(serde_json::Value::as_array),
            Some(&Vec::new())
        );

        run_merge_recovery(root, TASK).expect("successful H48 recovery replay must be idempotent");
        let replayed = fixture.events();
        assert_eq!(
            replayed
                .iter()
                .filter(|event| event.kind == "MergeExecuted")
                .count(),
            1
        );
        assert_eq!(
            replayed
                .iter()
                .filter(|event| event.kind == "TaskRecorded")
                .count(),
            1
        );
        assert_eq!(
            replayed
                .iter()
                .filter(|event| is_record_gate_relaxed(event))
                .count(),
            1
        );
    }

    /// P0-1 回归（真实入口）：B147 屏障（本 fixture 的 BT）挡住真实
    /// `run_root_verdict(BOTHER, ...)` 时，拒收文案必须同时点名被拒 task
    /// 与屏障归属 task——r51 误诊的根因就是真实路径只点名屏障归属。
    #[test]
    fn real_verdict_rejection_names_refused_task_and_barrier_owner() {
        let fixture = merge_fixture("b153-reject-identity", false);
        let root = &fixture.root;
        fixture.arm_dangling_barrier();

        let error = crate::verify::run_root_verdict(
            root,
            "BOTHER",
            "BOTHER-A0001",
            &"0".repeat(40),
            &"1".repeat(40),
            crate::verify::RootVerdict::Pass,
            None,
            false,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        for needle in ["orch verdict", "BOTHER", TASK, ROUND] {
            assert!(message.contains(needle), "拒收消息缺 {needle}: {message}");
        }
        assert!(
            !message.contains("<no-event>"),
            "真实 verdict 入口不得再报 <no-event>: {message}"
        );
    }

    /// 悬空屏障（r51 现场/崩溃窗口）→ --recover 清屏障 + 判旧 attempt 终局
    /// → 真实 `tierf::run_dispatch` 重派冲突修复 attempt 并实际唤醒。
    #[test]
    fn recovery_clears_dangling_barrier_and_allows_redispatch() {
        let fixture = merge_fixture("b153-recovery-live", false);
        let root = &fixture.root;
        fixture.arm_dangling_barrier();

        // 屏障活跃期：事件级拒收（真实 ledger::append 状态机）同时点名被拒
        // 事件与屏障归属（四要素）。
        let refused = ledger::event(
            "VerdictIssued",
            "verifier:root",
            Some("BOTHER"),
            Some(ROUND),
            serde_json::json!({}),
        );
        let error = ledger::append(root, ROUND, &[refused]).unwrap_err();
        let message = format!("{error:#}");
        for needle in ["VerdictIssued", "BOTHER", TASK, ROUND] {
            assert!(message.contains(needle), "拒收消息缺 {needle}: {message}");
        }
        // 操作级拒收（wake 类 effect）显式携带被拒 identity——不再把被拒
        // task 人工塞进 operation 字符串（r52 主审指出的测试伪造手法）。
        let error = with_protocol_effect_named(root, "planner wake", Some("BOTHER"), || Ok(()))
            .unwrap_err();
        let message = format!("{error:#}");
        for needle in ["planner wake", "BOTHER", TASK, ROUND] {
            assert!(message.contains(needle), "操作级拒收缺 {needle}: {message}");
        }
        assert!(
            !message.contains("<no-event>"),
            "带 identity 的 effect 不得再报 <no-event>: {message}"
        );

        // --recover ⇒ 清屏障（留痕 barrier-recovered）+ 判旧 attempt 终局
        // （AttemptBlocked）⇒ 真实 dispatch 可接管。
        let outcome = run_merge_recovery(root, TASK).expect("三条件全过必须放行");
        assert!(outcome.cleared);
        let events = fixture.events();
        assert!(events.iter().any(|event| {
            event.kind == "EscalationRaised"
                && payload(event, "stage").and_then(|v| v.as_str()) == Some("barrier-recovered")
        }));
        let blocked = events
            .iter()
            .find(|event| event.kind == "AttemptBlocked" && event.task_id.as_deref() == Some(TASK))
            .expect("恢复必须同批落 AttemptBlocked 终局事实");
        assert_eq!(
            payload(blocked, "attemptId").and_then(|v| v.as_str()),
            Some("BT-A0001")
        );
        assert_eq!(
            payload(blocked, "stage").and_then(|v| v.as_str()),
            Some("merge-conflict")
        );
        // 投影态 Approved ⇒ Blocked：Approved 空洞（dispatch Existing 空转 /
        // wake 不重发 / nudge 拒收）已由 canonical 事实闭合。
        let projection = orch_core::fold(&events);
        assert_eq!(
            projection.tasks.get(TASK).and_then(|task| task.state),
            Some(orch_core::TaskState::Blocked)
        );

        // 真实 `tierf::run_dispatch`：旧 attempt 已终局 ⇒ 接管规划新 attempt。
        // 旧 GO 必须在盘上（takeover archive 的合法来源），伪造 dispatch 的
        // fixture 原本只落账不落盘，此处补齐现场。
        let old_go = root.join("coordination/rounds/rT/dispatch/executor-claw/GO-BT-A0001.md");
        fs::create_dir_all(old_go.parent().unwrap()).unwrap();
        fs::write(&old_go, "# GO BT-A0001\n").unwrap();
        let (agent, _base_short) =
            crate::tierf::run_dispatch(root, TASK, false).expect("恢复后真实 dispatch 必须可达");
        assert_eq!(agent, "executor-claw");

        let events = fixture.events();
        let dispatches: Vec<&EventRecord> = events
            .iter()
            .filter(|event| {
                event.kind == "DispatchIssued" && event.task_id.as_deref() == Some(TASK)
            })
            .collect();
        assert_eq!(dispatches.len(), 2, "恢复后必须产生新的 DispatchIssued");
        let redispatch = dispatches[1];
        assert_eq!(
            payload(redispatch, "attemptId").and_then(|v| v.as_str()),
            Some("BT-A0002")
        );
        assert_eq!(
            payload(redispatch, "attemptNo").and_then(|v| v.as_u64()),
            Some(2)
        );
        assert_eq!(
            payload(redispatch, "agent").and_then(|v| v.as_str()),
            Some("executor-claw")
        );
        // 旧 GO 归档、新 GO 落盘（真实 takeover 现场）。
        assert!(!old_go.exists(), "旧 GO 必须已被接管归档");
        assert!(root
            .join(
                "coordination/rounds/rT/dispatch/executor-claw/superseded/BT-A0001/GO-BT-A0001.md"
            )
            .is_file());
        assert!(root
            .join("coordination/rounds/rT/dispatch/executor-claw/GO-BT-A0002.md")
            .is_file());
        // 实际可达的 wake：真实 spawn 走完 durable 管线（Claimed→Launching→
        // Delivered→Completed），新 attempt 的 DispatchWakeCompleted 在账。
        let wake_completed = events
            .iter()
            .find(|event| {
                event.kind == "DispatchWakeCompleted"
                    && event.task_id.as_deref() == Some(TASK)
                    && payload(event, "attemptId").and_then(|v| v.as_str()) == Some("BT-A0002")
            })
            .expect("新 attempt 的 wake 必须实际完成");
        assert_eq!(
            payload(wake_completed, "agent").and_then(|v| v.as_str()),
            Some("executor-claw")
        );
        wait_for_marker_lines(&root.join("wake-marker.txt"), 1);

        // 恢复是幂等边界：屏障已开，再走一次必拒。
        let error = run_merge_recovery(root, TASK).unwrap_err().to_string();
        assert!(error.contains("无活跃"), "{error}");
    }

    /// spawn 后台异步落 marker，轮询等待（与 tierf fixture 的 wait_for_lines
    /// 同形）。
    fn wait_for_marker_lines(path: &Path, expected: usize) {
        for _ in 0..200 {
            let count = fs::read_to_string(path).unwrap_or_default().lines().count();
            if count >= expected {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("marker {} did not reach {expected} lines", path.display());
    }

    /// P1：落账失败后的诊断三态契约——未证实前绝不宣称已闭合，且任何分支
    /// 都携带 accounting error（r52 主审：旧实现同时说「悬空」与「已闭合」）。
    #[test]
    fn conflict_closure_diagnosis_never_claims_closed_without_evidence() {
        let root = crate::util::test_scratch_dir("b153-conflict-diagnosis");
        fs::create_dir_all(root.join("coordination/rounds/rT")).unwrap();
        let ledger_path = root.join("coordination/rounds/rT/events.jsonl");
        let failure = MergeCommandFailure {
            status: 1,
            stdout: String::new(),
            stderr: "CONFLICT (content): Merge conflict in conflict.txt".to_string(),
        };
        let account_error = anyhow::anyhow!("wal mirror boom");

        // 账本不可读（缺失）⇒ 状态未知，不得宣称闭合。
        let message =
            conflict_closure_diagnosis(&ledger_path, TASK, ROUND, &account_error, &failure);
        assert!(message.contains("状态未知"), "{message}");
        assert!(message.contains("wal mirror boom"), "{message}");
        assert!(!message.contains("已闭合屏障"), "{message}");

        // 事件确实未落账 ⇒ 必须说「仍悬空」并指向恢复入口，不得宣称闭合。
        fs::write(&ledger_path, "").unwrap();
        let message =
            conflict_closure_diagnosis(&ledger_path, TASK, ROUND, &account_error, &failure);
        assert!(message.contains("仍悬空"), "{message}");
        assert!(message.contains("orch merge --recover"), "{message}");
        assert!(message.contains("wal mirror boom"), "{message}");
        assert!(!message.contains("已闭合"), "{message}");

        // ledger 已写 WAL 未写（事件实际已落）⇒ 可以确认闭合，但必须携带
        // accounting error，不得伪装成无事故成功。夹具取生产形状：当前
        // barrier 的 MergeStarted 早在前一次 append 落账，本批终态事实随后。
        write_ledger_events(
            &ledger_path,
            &[
                merge_started_event("BT-A0001", 1),
                merge_conflict_event(TASK, ROUND, &["conflict.txt".to_string()]),
                merge_blocked_event("BT-A0001", 1),
            ],
        );
        let message =
            conflict_closure_diagnosis(&ledger_path, TASK, ROUND, &account_error, &failure);
        assert!(message.contains("已实际落账"), "{message}");
        assert!(message.contains("wal mirror boom"), "{message}");

        let _ = fs::remove_dir_all(&root);
    }

    /// 诊断测试夹具用的 canonical MergeStarted（形状对齐
    /// `ledger::canonical_merge_started`：actor/exact payload 键/全 hex sha）。
    fn merge_started_event(attempt_id: &str, attempt_no: u64) -> EventRecord {
        ledger::event(
            "MergeStarted",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "attemptId": attempt_id,
                "attemptNo": attempt_no,
                "headSha": "1".repeat(40),
                "mainHeadSha": "2".repeat(40),
                "collectCompletedEventId": "collect-evt",
                "verdictEventId": "verdict-evt",
            }),
        )
    }

    /// 诊断测试夹具用的 AttemptBlocked 终局事实（形状对齐生产
    /// `merge_conflict_attempt_blocked_event`；屏障状态机不消费它，只为
    /// 原样复刻生产事件序列）。
    fn merge_blocked_event(attempt_id: &str, attempt_no: u64) -> EventRecord {
        ledger::event(
            "AttemptBlocked",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "attemptId": attempt_id,
                "attemptNo": attempt_no,
                "agent": "executor-claw",
                "stage": "merge-conflict",
            }),
        )
    }

    fn write_ledger_events(ledger_path: &Path, events: &[EventRecord]) {
        let mut bytes = Vec::new();
        for event in events {
            serde_json::to_writer(&mut bytes, event).unwrap();
            bytes.push(b'\n');
        }
        fs::write(ledger_path, bytes).unwrap();
    }

    /// P1（r52 复审）：诊断必须绑定「当前」屏障——复审点名的生产可达序列：
    /// A0001 `MergeStarted → merge-conflict → AttemptBlocked`（屏障已闭合）→
    /// 真实 dispatch 产生 A0002，root PASS 后落新 `MergeStarted`（屏障再次
    /// active）→ A0002 冲突终态 batch 写 ledger 前失败。此时账本里仍有
    /// A0001 的 merge-conflict，全历史 `.any` 会误报「已闭合」；状态机回读
    /// 则见当前 barrier 仍 active，必须报「仍悬空」。
    #[test]
    fn conflict_closure_diagnosis_binds_current_barrier_not_history() {
        let root = crate::util::test_scratch_dir("b153-diagnosis-current-barrier");
        fs::create_dir_all(root.join("coordination/rounds/rT")).unwrap();
        let ledger_path = root.join("coordination/rounds/rT/events.jsonl");
        let failure = MergeCommandFailure {
            status: 1,
            stdout: String::new(),
            stderr: "CONFLICT (content): Merge conflict in conflict.txt".to_string(),
        };
        let account_error = anyhow::anyhow!("wal mirror boom");

        // A0001 完整闭合链 + A0002 的新 MergeStarted（终态 batch 未落）。
        let mut events = vec![
            merge_started_event("BT-A0001", 1),
            merge_conflict_event(TASK, ROUND, &["conflict.txt".to_string()]),
            merge_blocked_event("BT-A0001", 1),
            merge_started_event("BT-A0002", 2),
        ];
        write_ledger_events(&ledger_path, &events);

        // 夹具自检：历史 conflict 确实在场（旧 .any 判定会中招），且同一
        // 状态机确认当前 active 屏障就是本 task/round 的——否则测试空转。
        let read = read_ledger(&ledger_path).unwrap();
        assert!(read.events.iter().any(|event| {
            event.kind == "EscalationRaised"
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("stage"))
                    .and_then(serde_json::Value::as_str)
                    == Some("merge-conflict")
        }));
        let barrier = ledger::active_merge_barrier(&read.events)
            .expect("旧 conflict 已闭合旧屏障 + 新 MergeStarted 后，当前屏障必须仍 active");
        assert_eq!(barrier.task_id, TASK);
        assert_eq!(barrier.round, ROUND);
        assert_eq!(
            ledger::merge_barrier_closure(&read.events, TASK, ROUND),
            ledger::MergeBarrierClosure::Active
        );

        // 历史 conflict 不得让诊断宣称当前屏障已闭合。
        let message =
            conflict_closure_diagnosis(&ledger_path, TASK, ROUND, &account_error, &failure);
        assert!(message.contains("仍悬空"), "{message}");
        assert!(message.contains("orch merge --recover"), "{message}");
        assert!(message.contains("wal mirror boom"), "{message}");
        assert!(!message.contains("已闭合"), "{message}");
        assert!(!message.contains("已实际落账"), "{message}");

        // 对照组：A0002 的终态 batch 实际已落（ledger 已写 WAL 未写）时，
        // 状态机见当前 barrier 被本批 escalation 闭合 ⇒ 确认已落账（方向
        // 与复审确认的半成功态判定一致）。
        events.push(merge_conflict_event(
            TASK,
            ROUND,
            &["conflict.txt".to_string()],
        ));
        events.push(merge_blocked_event("BT-A0002", 2));
        write_ledger_events(&ledger_path, &events);
        let read = read_ledger(&ledger_path).unwrap();
        assert!(
            ledger::active_merge_barrier(&read.events).is_none(),
            "本批闭合事实落账后状态机不得再有活跃屏障"
        );
        assert_eq!(
            ledger::merge_barrier_closure(&read.events, TASK, ROUND),
            ledger::MergeBarrierClosure::Closed
        );
        let message =
            conflict_closure_diagnosis(&ledger_path, TASK, ROUND, &account_error, &failure);
        assert!(message.contains("已实际落账"), "{message}");
        assert!(message.contains("wal mirror boom"), "{message}");

        let _ = fs::remove_dir_all(&root);
    }

    /// main 已推进 ⇒ merge 可能真发生了，恢复必拒（防掩盖真实合并）。
    #[test]
    fn recovery_allows_main_advance_when_merge_is_provably_absent() {
        // H97/H98（r62/B204 实证）：屏障悬空 + main 因无关原因推进，此前被
        // 「main 未推进」这个**代理**判据一刀切拒绝，导致全轮锁死且无 CLI 出口。
        // 现在只要能**直接证明**该 task 分支从未合入（既非 main 祖先、也非 main
        // 历史里任何 merge 的父），就允许清屏障——清的是一个可证不存在的合并。
        let fixture = merge_fixture("b153-recovery-main-moved", false);
        let root = &fixture.root;
        fixture.arm_dangling_barrier();
        // coordination-only 推进：这是屏障期**唯一被允许**的 main 改动（记账本、
        // 写 BOARD），verdict 因此仍然有效。旧判据却把它一并当成「merge 可能发生」
        // 而拒绝恢复——正是它把 r62 锁死的那一类场景。
        fs::write(
            root.join("coordination/BOARD.md"),
            "barrier-window bookkeeping\n",
        )
        .unwrap();
        git(root, &["add", "-f", "coordination/BOARD.md"]);
        git(
            root,
            &["commit", "-q", "-m", "coordination-only main advance"],
        );

        run_merge_recovery(root, TASK).expect("合并确证不存在时必须允许恢复");
        // 屏障已闭合：普通 append 恢复可用（这正是死锁的解除）。
        ledger::append(root, ROUND, &[ordinary_event("TaskValidated", None)])
            .expect("屏障闭合后普通 append 必须可用");
    }

    /// 证明拿不到时必须退回保守判据：分支查不到 ⇒ 不得清屏障（fail-closed）。
    #[test]
    fn recovery_refuses_when_merge_absence_cannot_be_proven() {
        let fixture = merge_fixture("b153-recovery-unprovable", false);
        let root = &fixture.root;
        fixture.arm_dangling_barrier();
        fs::write(root.join("later.txt"), "unrelated main advance\n").unwrap();
        git(root, &["add", "later.txt"]);
        git(root, &["commit", "-q", "-m", "later unrelated main commit"]);
        // 移除 task 分支 ⇒ rev_parse 失败 ⇒ merge_provably_absent = false。
        git(root, &["worktree", "remove", "--force", ".worktrees/BT"]);
        git(root, &["branch", "-D", &format!("task/{TASK}")]);

        let error = run_merge_recovery(root, TASK).unwrap_err().to_string();
        assert!(error.contains("main 已推进"), "{error}");
        // 屏障保持活跃（fail-closed），普通 append 仍被拒。
        assert!(ledger::append(root, ROUND, &[ordinary_event("TaskValidated", None)]).is_err());
    }

    /// verdict 失效（task 分支漂移）⇒ 不复活死链，恢复必拒。
    #[test]
    fn recovery_refuses_when_verdict_stale() {
        let fixture = merge_fixture("b153-recovery-stale", false);
        let root = &fixture.root;
        fixture.arm_dangling_barrier();
        let wt = root.join(".worktrees/BT");
        fs::write(wt.join("drift.txt"), "task branch moved\n").unwrap();
        git(&wt, &["add", "drift.txt"]);
        git(&wt, &["commit", "-q", "-m", "drift task branch"]);

        let error = run_merge_recovery(root, TASK).unwrap_err().to_string();
        assert!(error.contains("verdict 已失效"), "{error}");
    }

    /// 无屏障 ⇒ 恢复必拒。
    #[test]
    fn recovery_refuses_without_barrier() {
        let fixture = merge_fixture("b153-recovery-none", false);
        let error = run_merge_recovery(&fixture.root, TASK)
            .unwrap_err()
            .to_string();
        assert!(error.contains("无活跃"), "{error}");
    }

    /// 屏障归属他任务 ⇒ 张冠李戴的清屏障必拒。
    #[test]
    fn recovery_refuses_to_clear_another_tasks_barrier() {
        let fixture = merge_fixture("b153-recovery-wrong-task", false);
        let root = &fixture.root;
        fixture.arm_dangling_barrier();
        let error = run_merge_recovery(root, "BOTHER").unwrap_err().to_string();
        assert!(error.contains("张冠李戴"), "{error}");
    }
}
