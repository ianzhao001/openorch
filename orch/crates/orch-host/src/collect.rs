//! 收取阶段（run-task 与 await-report 共用）：REPORT 真值检查 → 机检 → 先红复跑 → 门 → 试合并门 → 落账。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::{binding, buildcache, card, gate, gitx, ledger, mech, oracle};

pub struct CollectOutcome {
    pub mech_notes: Vec<String>,
    pub gates: Vec<gate::GateResult>,
}

fn note_gate_orphan_evidence(log_dir: &Path, tag: &str, gate_name: &str, observation: &str) {
    if let Err(error) = gate::append_gate_orphan_evidence(log_dir, tag, gate_name, observation) {
        eprintln!(
            "gate orphan evidence degraded to stderr for {tag}/{gate_name}: {error:#}; {observation}"
        );
    }
}

fn collect_orphan_baseline(log_dir: &Path, tag: &str, gate_name: &str) -> Option<BTreeSet<u32>> {
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

fn finish_collect_orphan_watch(
    root: &Path,
    context: Option<(&str, &str)>,
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
        if let Some((round, task_id)) = context {
            ledger::append(
                root,
                round,
                &[gate::orphan_failure_event(task_id, round, &reportable)],
            )?;
        }
    }
    Ok(())
}

fn with_collect_orphan_watch<T>(
    root: &Path,
    context: Option<(&str, &str)>,
    log_dir: &Path,
    tag: &str,
    gate_name: &str,
    run: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let baseline = collect_orphan_baseline(log_dir, tag, gate_name);
    let result = run();
    let observation =
        finish_collect_orphan_watch(root, context, log_dir, tag, gate_name, baseline.as_ref());
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

/// 收取期试合并的不可变计划。两个输入都是 concrete SHA，scratch 是仓内
/// detached worktree；计划本身没有任何可移动 ref。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrialMergePlan {
    pub scratch_worktree: String,
    pub attempt_head: String,
    pub main_head: String,
}

/// 试合并门的可审计结论。只有 [`Green`](Self::Green) 可继续 collect；
/// 红门必须携带门名、退出码、同轮提交和原始失败详情，冲突必须携带文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrialMergeVerdict {
    Green,
    Red {
        gate: String,
        exit_code: i32,
        interacting_commits: Vec<String>,
        detail: String,
    },
    Conflict {
        files: Vec<String>,
    },
}

impl TrialMergeVerdict {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Green)
    }

    pub fn render(&self) -> String {
        match self {
            Self::Green => "收取期试合并门通过".to_string(),
            Self::Red {
                gate,
                exit_code,
                interacting_commits,
                detail,
            } => format!(
                "收取期试合并门拒绝：门 {gate} 红（exit {exit_code}）；\
                 同轮交互提交：{}；失败详情：\n{detail}",
                render_items(interacting_commits)
            ),
            Self::Conflict { files } => format!(
                "收取期试合并冲突，拒绝 collect；冲突文件：{}",
                render_items(files)
            ),
        }
    }
}

fn render_items(items: &[String]) -> String {
    if items.is_empty() {
        "<无法解析>".to_string()
    } else {
        items.join(", ")
    }
}

/// 精确触发条件：当前 main concrete SHA 与本 attempt 的派发基线不同。
/// 生产路径还会证明 base 是 current main 的祖先，拒绝把 ref 倒退/改写误称为
/// “同轮推进”。
pub fn trial_merge_needed(base_sha: &str, current_main_sha: &str) -> bool {
    base_sha != current_main_sha
}

/// 构造仓内、一次性、detached 的 scratch 计划。task id 先做 component
/// 校验，防止路径穿越；两个 SHA 只作为 git 对象名使用，不成为分支名。
pub fn trial_merge_plan(
    root: &str,
    task_id: &str,
    attempt_head: &str,
    main_head: &str,
) -> Result<TrialMergePlan> {
    card::validate_task_id(task_id)?;
    let root = Path::new(root);
    if !root.is_absolute() {
        bail!("试合并仓根必须是绝对路径: {}", root.display());
    }
    if attempt_head.is_empty() || main_head.is_empty() {
        bail!("试合并必须绑定非空 attempt/main concrete SHA");
    }
    let scratch =
        root.join(".cowork-temp")
            .join(format!("trial-{}-{}", task_id, ulid::Ulid::new()));
    Ok(TrialMergePlan {
        scratch_worktree: scratch.to_string_lossy().into_owned(),
        attempt_head: attempt_head.to_string(),
        main_head: main_head.to_string(),
    })
}

struct TrialWorktree<'a> {
    root: &'a Path,
    path: PathBuf,
    active: bool,
}

impl TrialWorktree<'_> {
    fn cleanup(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        gitx::worktree_remove(self.root, &self.path)
            .with_context(|| format!("清理试合并 worktree 失败: {}", self.path.display()))?;
        self.active = false;
        if fs::symlink_metadata(&self.path).is_ok() {
            bail!(
                "试合并 worktree remove 成功后路径仍存在: {}",
                self.path.display()
            );
        }
        Ok(())
    }
}

impl Drop for TrialWorktree<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = gitx::worktree_remove(self.root, &self.path);
        }
    }
}

fn conflict_files(scratch: &Path, stdout: &str, stderr: &str) -> Vec<String> {
    fn push_unique(files: &mut Vec<String>, path: &str) {
        let path = path.trim();
        if !path.is_empty() && !files.iter().any(|seen| seen == path) {
            files.push(path.to_string());
        }
    }

    let mut files = Vec::new();
    for line in stdout.lines().chain(stderr.lines()) {
        let line = line.trim();
        if !line.starts_with("CONFLICT") {
            continue;
        }
        if let Some((_, path)) = line.rsplit_once("Merge conflict in ") {
            push_unique(&mut files, path);
        } else if let Some(rest) = line.strip_prefix("CONFLICT (modify/delete): ") {
            if let Some((path, _)) = rest.split_once(" deleted in ") {
                push_unique(&mut files, path);
            }
        }
    }
    if files.is_empty() {
        if let Ok(output) = Command::new("git")
            .arg("-C")
            .arg(scratch)
            .args(["diff", "--name-only", "--diff-filter=U"])
            .output()
        {
            if output.status.success() {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    push_unique(&mut files, line);
                }
            }
        }
    }
    files
}

/// 小日志完整保留；大日志保留关键失败行和尾部，既让错误能点名具体
/// 测试/编译错误，也避免把一次完整 Cargo 日志塞进拒绝文案。按 bytes
/// 截取后用 lossy UTF-8，永不因非 UTF-8 丢掉红证据。
fn gate_failure_detail(log_path: &str) -> String {
    const LIMIT: usize = 16 * 1024;
    match fs::read(log_path) {
        Ok(bytes) if bytes.len() <= LIMIT => String::from_utf8_lossy(&bytes).into_owned(),
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            let signals = text
                .lines()
                .filter(|line| {
                    line.contains("FAILED")
                        || line.contains("failures:")
                        || line.contains("panicked at")
                        || line.contains("error[")
                        || line.contains("error:")
                        || line.contains("test result:")
                })
                .take(64)
                .collect::<Vec<_>>()
                .join("\n");
            let start = bytes.len() - LIMIT;
            format!(
                "关键失败行：\n{}\n日志尾部（最后 {LIMIT} bytes）：\n{}",
                if signals.is_empty() {
                    "<未匹配标准失败行>"
                } else {
                    &signals
                },
                String::from_utf8_lossy(&bytes[start..])
            )
        }
        Err(error) => format!("门日志读取失败（{log_path}）：{error}"),
    }
}

fn interacting_commits(root: &Path, base_sha: &str, main_sha: &str) -> Result<Vec<String>> {
    gitx::commits_after(root, base_sha, main_sha)?
        .into_iter()
        .map(|sha| {
            let subject = gitx::commit_subject(root, &sha)?;
            Ok(format!("{} {}", gitx::short(&sha), subject))
        })
        .collect()
}

fn execute_trial_merge(
    root: &Path,
    round: &str,
    task_id: &str,
    audit_identity: ledger::GateAuditIdentity<'_>,
    storage_permit: &crate::storage::StoragePermit,
    orphan_context: Option<(&str, &str)>,
    plan: &TrialMergePlan,
    commits: Vec<String>,
    gate_refs: &[String],
    commands: &BTreeMap<String, binding::CommandSpec>,
    log_dir: &Path,
) -> Result<TrialMergeVerdict> {
    if buildcache::trial_cargo_program(gate_refs, commands).is_some() {
        return buildcache::with_trial_slot(root, task_id, |slot| {
            let mut fixed_plan = plan.clone();
            fixed_plan.scratch_worktree = slot.source_root().to_string_lossy().into_owned();
            execute_trial_merge_at(
                root,
                round,
                task_id,
                audit_identity,
                storage_permit,
                orphan_context,
                &fixed_plan,
                &commits,
                gate_refs,
                commands,
                log_dir,
                Some(slot),
            )
        });
    }
    execute_trial_merge_at(
        root,
        round,
        task_id,
        audit_identity,
        storage_permit,
        orphan_context,
        plan,
        &commits,
        gate_refs,
        commands,
        log_dir,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn execute_trial_merge_at(
    root: &Path,
    round: &str,
    task_id: &str,
    audit_identity: ledger::GateAuditIdentity<'_>,
    storage_permit: &crate::storage::StoragePermit,
    orphan_context: Option<(&str, &str)>,
    plan: &TrialMergePlan,
    commits: &[String],
    gate_refs: &[String],
    commands: &BTreeMap<String, binding::CommandSpec>,
    log_dir: &Path,
    slot: Option<&buildcache::TrialSlot>,
) -> Result<TrialMergeVerdict> {
    let scratch = PathBuf::from(&plan.scratch_worktree);
    if fs::symlink_metadata(&scratch).is_ok() {
        bail!("试合并 scratch 已存在，拒绝复用: {}", scratch.display());
    }
    fs::create_dir_all(scratch.parent().context("试合并 scratch 缺 parent")?)?;
    gitx::worktree_add_detached(root, &scratch, &plan.attempt_head)?;
    let mut guard = TrialWorktree {
        root,
        path: scratch.clone(),
        active: true,
    };

    let run = (|| -> Result<TrialMergeVerdict> {
        if gitx::rev_parse(&scratch, "HEAD")? != plan.attempt_head {
            bail!("试合并 detached worktree HEAD 与 attempt concrete SHA 不符");
        }
        let output = Command::new("git")
            .arg("-C")
            .arg(&scratch)
            .args([
                "merge",
                "--no-commit",
                "--no-ff",
                "--no-verify",
                &plan.main_head,
            ])
            .output()
            .context("启动试合并 git merge 失败")?;
        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let files = conflict_files(&scratch, &stdout, &stderr);
            if files.is_empty() {
                bail!(
                    "试合并 git merge 失败但未解析出冲突文件（exit={}）：{}{}",
                    output.status.code().unwrap_or(-1),
                    stdout.trim(),
                    stderr.trim()
                );
            }
            return Ok(TrialMergeVerdict::Conflict { files });
        }

        let mut cache_target = if let Some(slot) = slot {
            let cargo_program = buildcache::trial_cargo_program(gate_refs, commands)
                .context("trial slot selected without a uniform Cargo command")?;
            let config_digest = buildcache::build_config_digest(gate_refs, commands)?;
            let identity =
                buildcache::inspect_build_identity(&scratch, &cargo_program, config_digest)?;
            let hot_target = root.join("orch/target");
            let target = buildcache::prepare_trial_target(slot, &identity, Some(&hot_target))?;
            println!(
                "trial-cache slot={} generation={} target={} · {}",
                slot.index(),
                target.generation(),
                target.target_dir().display(),
                target.warm_preparation().render()
            );
            Some((identity, target))
        } else {
            None
        };

        let tag = format!("{}-trial-{}", task_id, gitx::short(&plan.main_head));
        for (gate_index, gate_ref) in gate_refs.iter().enumerate() {
            let spec = commands
                .get(gate_ref)
                .with_context(|| format!("试合并绑定缺命令: {gate_ref}"))?;
            // The outer permit covers trial-worktree creation; the typed gate wrappers re-probe
            // immediately before every warm, uncached, and fallback child spawn.
            let mut result =
                with_collect_orphan_watch(root, orphan_context, log_dir, &tag, gate_ref, || {
                    match cache_target.as_ref() {
                        Some((_, target)) => {
                            // `run_trial_gate` reaches the common `gate_log_sinks` production path; the
                            // warm assessment below therefore rereads one non-overwriting stdout/stderr
                            // log rather than the benchmark helper's separately concatenated buffers.
                            crate::storage::refresh_gate_permit(
                                root,
                                round,
                                audit_identity,
                                storage_permit,
                                &[
                                    scratch.clone(),
                                    log_dir.to_path_buf(),
                                    root.join("orch/target"),
                                    target.target_dir().to_path_buf(),
                                ],
                            )?;
                            gate::run_trial_gate_with_permit_and_identity(
                                storage_permit,
                                audit_identity,
                                gate_ref,
                                spec,
                                &scratch,
                                log_dir,
                                &tag,
                                target,
                            )
                        }
                        None => {
                            crate::storage::refresh_gate_permit(
                                root,
                                round,
                                audit_identity,
                                storage_permit,
                                &[
                                    scratch.clone(),
                                    log_dir.to_path_buf(),
                                    root.join("orch/target"),
                                ],
                            )?;
                            gate::run_gate_with_permit_and_identity(
                                storage_permit,
                                audit_identity,
                                gate_ref,
                                spec,
                                &scratch,
                                log_dir,
                                &tag,
                            )
                        }
                    }
                })?;

            // The first warm run is evidence, not a blind optimization.  If its log cannot prove
            // orch-core/orch-host rebuilt while registry dependencies stayed warm, abandon that
            // generation and re-run the gate against a fresh cold generation.  The warm target is
            // preserved for diagnosis; it is never cleaned or rebound in place.
            if gate_index == 0 {
                let assessment = cache_target
                    .as_ref()
                    .map(|(_, target)| {
                        target.assess_gate_log(result.exit_code, Path::new(&result.log_path))
                    })
                    .transpose()?;
                if let Some(assessment) = assessment {
                    if !matches!(assessment, buildcache::WarmStartAssessment::NotApplicable) {
                        println!("{}", assessment.render());
                    }
                    if assessment.requires_fallback() {
                        let slot = slot.context("warm fallback requires a held trial slot")?;
                        let identity = &cache_target
                            .as_ref()
                            .context("warm fallback missing build identity")?
                            .0;
                        let cold = buildcache::prepare_cold_fallback_target(
                            slot,
                            identity,
                            assessment.render(),
                        )?;
                        println!(
                            "trial-cache slot={} generation={} target={} · {}",
                            slot.index(),
                            cold.generation(),
                            cold.target_dir().display(),
                            cold.warm_preparation().render()
                        );
                        let fallback_tag = format!("{tag}-fallback");
                        result = with_collect_orphan_watch(
                            root,
                            orphan_context,
                            log_dir,
                            &fallback_tag,
                            gate_ref,
                            || {
                                crate::storage::refresh_gate_permit(
                                    root,
                                    round,
                                    audit_identity,
                                    storage_permit,
                                    &[
                                        scratch.clone(),
                                        log_dir.to_path_buf(),
                                        root.join("orch/target"),
                                        cold.target_dir().to_path_buf(),
                                    ],
                                )?;
                                gate::run_trial_gate_with_permit_and_identity(
                                    storage_permit,
                                    audit_identity,
                                    gate_ref,
                                    spec,
                                    &scratch,
                                    log_dir,
                                    &fallback_tag,
                                    &cold,
                                )
                            },
                        )?;
                        let identity = identity.clone();
                        cache_target = Some((identity, cold));
                    }
                }
            }
            if result.exit_code != 0 {
                return Ok(TrialMergeVerdict::Red {
                    gate: gate_ref.clone(),
                    exit_code: result.exit_code,
                    interacting_commits: commits.to_vec(),
                    detail: gate_failure_detail(&result.log_path),
                });
            }
        }
        Ok(TrialMergeVerdict::Green)
    })();

    let cleanup = guard.cleanup();
    match (run, cleanup) {
        (Ok(verdict), Ok(())) => Ok(verdict),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
        (Err(error), Err(cleanup_error)) => Err(error).context(format!(
            "试合并执行失败且 scratch 清理也失败: {cleanup_error:#}"
        )),
    }
}

fn run_trial_merge_if_needed(
    root: &Path,
    round: &str,
    task_id: &str,
    audit_identity: ledger::GateAuditIdentity<'_>,
    storage_permit: &crate::storage::StoragePermit,
    orphan_context: Option<(&str, &str)>,
    attempt_head: &str,
    base_sha: &str,
    current_main_sha: &str,
    gate_refs: &[String],
    commands: &BTreeMap<String, binding::CommandSpec>,
    log_dir: &Path,
) -> Result<Option<TrialMergeVerdict>> {
    if !trial_merge_needed(base_sha, current_main_sha) {
        return Ok(None);
    }
    if !gitx::is_ancestor(root, base_sha, current_main_sha)? {
        bail!(
            "current main {} 不是 attempt base {} 的后代，拒绝把 ref 倒退/改写当成同轮推进",
            gitx::short(current_main_sha),
            gitx::short(base_sha)
        );
    }
    let plan = trial_merge_plan(
        &root.to_string_lossy(),
        task_id,
        attempt_head,
        current_main_sha,
    )?;
    let commits = interacting_commits(root, base_sha, current_main_sha)?;
    execute_trial_merge(
        root,
        round,
        task_id,
        audit_identity,
        storage_permit,
        orphan_context,
        &plan,
        commits,
        gate_refs,
        commands,
        log_dir,
    )
    .map(Some)
}

/// 前提：REPORT 文件已确认存在（调用方负责 ReportObserved 事件）。
pub fn check_and_gate(
    root: &Path,
    round: &str,
    c: &card::Card,
    branch: &str,
    report_rel: &str,
    expected_base: Option<&str>,
    worktree: &Path,
) -> Result<CollectOutcome> {
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let ledger_read = orch_core::read_ledger(&ledger_path)
        .with_context(|| format!("读取 collect attempt 账本失败: {}", ledger_path.display()))?;
    crate::attempt::reject_bad_lines(&ledger_read)?;
    let dispatch = crate::attempt::resolve_current_dispatch(
        &ledger_read.events,
        &c.meta.task_id,
        round,
    )?;
    let attempt_id = dispatch
        .attempt_id
        .context("collect gate 缺 durable current attemptId")?;
    let audit_identity = ledger::GateAuditIdentity::Attempt {
        task_id: &c.meta.task_id,
        attempt_id: &attempt_id,
    };
    audit_identity.validate()?;

    // One outer permit precedes seed replay, trial-worktree creation, log creation and every gate
    // child spawned by collect. Holding it across the sequence prevents self-races between gates.
    let log_dir = root.join("coordination/runtime/logs");
    let _storage_permit = crate::storage::guard_gate_operation(
        root,
        round,
        audit_identity,
        &[
            worktree.to_path_buf(),
            log_dir.clone(),
            root.join("orch/target"),
            root.join(".cowork-temp"),
            root.join(".cowork-temp/trial-cache/slots"),
        ],
    )?;
    let mut m = match mech::check(root, c, branch, report_rel, expected_base) {
        Ok(m) => m,
        Err(e) => {
            let msg = e.to_string();
            let stage = infer_mech_stage(&msg);
            record_failure(root, round, &c.meta.task_id, stage, &msg)?;
            return Err(e);
        }
    };
    let tree_paths = match mech::tree_paths_at_head(root, branch) {
        Ok(paths) => paths,
        Err(e) => {
            let e = anyhow::anyhow!("REPORT 未 commit(E15): 无法读取 {branch} HEAD 树: {e}");
            let msg = e.to_string();
            record_failure(root, round, &c.meta.task_id, "report-committed", &msg)?;
            return Err(e);
        }
    };
    if !mech::report_committed(&tree_paths, report_rel) {
        let e = anyhow::anyhow!("REPORT 未 commit(E15): {report_rel} 不在 {branch} HEAD");
        let msg = e.to_string();
        record_failure(root, round, &c.meta.task_id, "report-committed", &msg)?;
        return Err(e);
    }
    m.notes
        .push("REPORT 已 commit ✅ 分支 HEAD 树含精确路径".into());
    for (target, digest) in &m.seed_digests {
        ledger::append(
            root,
            round,
            &[ledger::event(
                "SeedRelocated",
                "runtime:orch",
                Some(&c.meta.task_id),
                Some(round),
                serde_json::json!({"target": target, "sha256": digest, "cmp": "identical"}),
            )],
        )?;
    }
    println!("④ 机检通过: {}", m.notes.join("；"));

    // ④½ 机检级先红复跑（M2/E12 零模型兜底）：seed commit 处复跑测试门核对红计数
    // 复跑命令经 red_replay_gate 卡驱动解析（B32）
    let replay_gate =
        oracle::red_replay_gate(&c.meta.gates.fast).map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("④½ 先红复跑: 复跑门={replay_gate}（卡 gates.fast[0]）");
    let replay_tag = format!("{}-red-replay", c.meta.task_id);
    if let Err(e) = with_collect_orphan_watch(
        root,
        Some((round, &c.meta.task_id)),
        &log_dir,
        &replay_tag,
        &replay_gate,
        || {
            oracle::replay_seed_red(
                root,
                round,
                c,
                branch,
                worktree,
                &attempt_id,
                &_storage_permit,
            )
        },
    ) {
        let msg = e.to_string();
        let stage = if msg.contains("报数造假") || msg.contains("claim") {
            "red-replay-claim"
        } else {
            "red-replay"
        };
        record_failure(root, round, &c.meta.task_id, stage, &msg)?;
        return Err(e);
    }

    let b = binding::load(root)?;
    let available = b.commands.keys().cloned().collect::<Vec<_>>();
    let gate_refs =
        binding::resolve_gates(&c.meta.gates.fast, &available).map_err(anyhow::Error::msg)?;
    let mut gates = Vec::new();
    for gref in &gate_refs {
        let spec = b
            .commands
            .get(gref)
            .ok_or_else(|| anyhow::anyhow!("绑定缺命令: {gref}"))?;
        let g = with_collect_orphan_watch(
            root,
            Some((round, &c.meta.task_id)),
            &log_dir,
            &c.meta.task_id,
            gref,
            || {
                crate::storage::refresh_gate_permit(
                    root,
                    round,
                    audit_identity,
                    &_storage_permit,
                    &[
                        worktree.to_path_buf(),
                        log_dir.to_path_buf(),
                        root.join("orch/target"),
                    ],
                )?;
                gate::run_gate_with_permit_and_identity(
                    &_storage_permit,
                    audit_identity,
                    gref,
                    spec,
                    worktree,
                    &log_dir,
                    &c.meta.task_id,
                )
            },
        )?;
        ledger::append(
            root,
            round,
            &[ledger::event(
                "GateExecuted",
                "runtime:orch",
                Some(&c.meta.task_id),
                Some(round),
                serde_json::json!({"commandRef": gref, "exitCode": g.exit_code, "durationMs": g.duration_ms as u64}),
            )],
        )?;
        println!("⑤ 门 {gref}: exit={} ({}ms)", g.exit_code, g.duration_ms);
        if g.exit_code != 0 {
            bail!("门 {gref} 红（exit {}），日志 {}", g.exit_code, g.log_path);
        }
        gates.push(g);
    }

    // ⑤½ 收取期试合并门（B161）：只有 main 相对本 attempt 基线推进过才付费。
    // attempt/main 均钉 concrete SHA，合并只发生在仓内 detached scratch；
    // 不 checkout main、不移动 main/task ref，所有返回路径都显式 remove scratch。
    if let Some(base_sha) = expected_base {
        let current_main = gitx::rev_parse(root, "main")?;
        let attempt_head = gitx::rev_parse(root, branch)?;
        if let Some(verdict) = run_trial_merge_if_needed(
            root,
            round,
            &c.meta.task_id,
            audit_identity,
            &_storage_permit,
            Some((round, &c.meta.task_id)),
            &attempt_head,
            base_sha,
            &current_main,
            &gate_refs,
            &b.commands,
            &log_dir,
        )? {
            println!("⑤½ {}", verdict.render());
            if !verdict.is_ok() {
                bail!("{}", verdict.render());
            }
        }
    }
    Ok(CollectOutcome {
        mech_notes: m.notes,
        gates,
    })
}

/// 按 mech::check 错误信息推断机检环节（stage）。失败也落账（E8 补充），
/// stage 取对应环节名 domain/frozen/shape/seed-bytes。
fn infer_mech_stage(msg: &str) -> &'static str {
    if msg.contains("冻结") || msg.contains("frozen") {
        "frozen"
    } else if msg.contains("字节") || msg.contains("SHA") || msg.contains("种子") {
        "seed-bytes"
    } else if msg.contains("首 commit")
        || msg.contains("无提交")
        || msg.contains("形状")
        || msg.contains("upstream")
    {
        "shape"
    } else {
        "domain"
    }
}

/// 机检失败落账：先 append MechCheckFailed 事件再由调用方 bail。
fn record_failure(
    root: &Path,
    round: &str,
    task_id: &str,
    stage: &str,
    reason: &str,
) -> Result<()> {
    ledger::append(
        root,
        round,
        &[mech::failure_event(task_id, round, stage, reason)],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn commit_all(root: &Path, message: &str) -> String {
        git(root, &["add", "-A"]);
        git(
            root,
            &[
                "-c",
                "user.name=orch-test",
                "-c",
                "user.email=orch-test@example.invalid",
                "commit",
                "-m",
                message,
            ],
        );
        git(root, &["rev-parse", "HEAD"])
    }

    fn init_repo(tag: &str) -> (PathBuf, String) {
        let root = crate::util::test_scratch_dir(tag);
        git(&root, &["init", "-b", "main"]);
        fs::write(
            root.join(".gitignore"),
            ".cowork-temp/\ncoordination/runtime/\n",
        )
        .unwrap();
        fs::write(root.join("README.md"), "base\n").unwrap();
        fs::write(root.join("collision.txt"), "base\n").unwrap();
        let base = commit_all(&root, "base");
        (root, base)
    }

    fn gate_commands(command: &str) -> BTreeMap<String, binding::CommandSpec> {
        BTreeMap::from([(
            "testFast".to_string(),
            binding::CommandSpec {
                argv: vec!["sh".into(), "-c".into(), command.into()],
                timeout_seconds: 30,
                trial_timeout_seconds: None,
                approval: None,
            },
        )])
    }

    fn fixture_storage_permit(root: &Path) -> crate::storage::StoragePermit {
        crate::storage::fixture_gate_permit(root, &[
            root.to_path_buf(),
            root.join("coordination/runtime/logs"),
            root.join("orch/target"),
            root.join(".cowork-temp"),
        ])
    }

    #[test]
    fn long_gate_log_keeps_an_early_failed_test_name() {
        let root = crate::util::test_scratch_dir("collect-trial-log-detail");
        let log = root.join("red.log");
        let mut bytes = b"test semantic_interaction ... FAILED\n".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', 20 * 1024));
        fs::write(&log, bytes).unwrap();
        let detail = gate_failure_detail(&log.to_string_lossy());
        assert!(detail.contains("semantic_interaction"));
        assert!(detail.contains("日志尾部"));
    }

    #[test]
    fn r53_semantic_interaction_is_red_but_reverting_the_other_card_is_green() {
        let (root, base) = init_repo("collect-trial-semantic");
        fs::create_dir_all(root.join("coordination/rounds/r-test")).unwrap();
        fs::create_dir_all(root.join("coordination/runtime/ledger-wal")).unwrap();
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r-test\n").unwrap();
        fs::write(root.join("coordination/rounds/r-test/events.jsonl"), "").unwrap();
        fs::write(root.join("coordination/runtime/ledger-wal/r-test.jsonl"), "").unwrap();
        let commands = gate_commands(
            "if [ -f require-entrypoints ] && [ -f fixture-card.md ] && \
             ! grep -q '^entryPoints:' fixture-card.md; then \
             echo 'task B900: entryPoints 不得为空' >&2; exit 101; fi",
        );
        let gates = vec!["testFast".to_string()];
        let log_dir = root.join("coordination/runtime/logs");
        git(&root, &["switch", "-c", "task/B-fixture", &base]);
        fs::write(root.join("fixture-card.md"), "taskId: B900\nwriteSet: []\n").unwrap();
        let attempt = commit_all(&root, "B card adds a legacy fixture");
        let b_alone = gate::run_gate(
            "testFast",
            commands.get("testFast").unwrap(),
            &root,
            &log_dir,
            "B-alone",
        )
        .unwrap();
        assert_eq!(
            b_alone.exit_code, 0,
            "B branch's legacy fixture is valid under the old schema"
        );
        git(&root, &["switch", "main"]);
        fs::write(root.join("require-entrypoints"), "required\n").unwrap();
        let moved_main = commit_all(&root, "A card makes entryPoints mandatory");
        let a_alone = gate::run_gate(
            "testFast",
            commands.get("testFast").unwrap(),
            &root,
            &log_dir,
            "A-alone",
        )
        .unwrap();
        assert_eq!(
            a_alone.exit_code, 0,
            "A branch has no legacy fixture and remains green on its own"
        );
        let before_main = git(&root, &["rev-parse", "main"]);
        let before_task = git(&root, &["rev-parse", "task/B-fixture"]);
        let storage_permit = fixture_storage_permit(&root);

        let red = run_trial_merge_if_needed(
            &root,
            "r-test",
            "B900",
            ledger::GateAuditIdentity::Attempt {
                task_id: "B900",
                attempt_id: "B900-A0001",
            },
            &storage_permit,
            None,
            &attempt,
            &base,
            &moved_main,
            &gates,
            &commands,
            &log_dir,
        )
        .unwrap()
        .expect("main moved, trial must run");
        let message = red.render();
        assert!(!red.is_ok());
        assert!(message.contains("testFast"));
        assert!(message.contains(gitx::short(&moved_main)));
        assert!(message.contains("entryPoints"));
        assert_eq!(git(&root, &["rev-parse", "main"]), before_main);
        assert_eq!(git(&root, &["rev-parse", "task/B-fixture"]), before_task);
        assert!(
            fs::read_dir(root.join(".cowork-temp"))
                .unwrap()
                .next()
                .is_none(),
            "red path must clean its scratch worktree"
        );

        fs::remove_file(root.join("require-entrypoints")).unwrap();
        let reverted_main = commit_all(&root, "remove A card's mandatory-field change");
        let green = run_trial_merge_if_needed(
            &root,
            "r-test",
            "B900",
            ledger::GateAuditIdentity::Attempt {
                task_id: "B900",
                attempt_id: "B900-A0001",
            },
            &storage_permit,
            None,
            &attempt,
            &base,
            &reverted_main,
            &gates,
            &commands,
            &log_dir,
        )
        .unwrap()
        .expect("main still moved, trial must run");
        assert_eq!(green, TrialMergeVerdict::Green);
        assert_eq!(git(&root, &["rev-parse", "task/B-fixture"]), before_task);
        assert!(
            fs::read_dir(root.join(".cowork-temp"))
                .unwrap()
                .next()
                .is_none(),
            "green path must clean its scratch worktree"
        );
    }

    #[test]
    fn unchanged_main_skips_worktree_and_gate_entirely() {
        let (root, base) = init_repo("collect-trial-skip");
        let sentinel = root.join("gate-ran");
        let commands = gate_commands(&format!("touch '{}'", sentinel.display()));
        let storage_permit = fixture_storage_permit(&root);
        let verdict = run_trial_merge_if_needed(
            &root,
            "r-test",
            "B900",
            ledger::GateAuditIdentity::Attempt {
                task_id: "B900",
                attempt_id: "B900-A0001",
            },
            &storage_permit,
            None,
            &base,
            &base,
            &base,
            &["testFast".to_string()],
            &commands,
            &root.join("coordination/runtime/logs"),
        )
        .unwrap();
        assert_eq!(verdict, None);
        assert!(
            !sentinel.exists(),
            "unchanged main must not run a second gate"
        );
        assert!(
            !root.join(".cowork-temp").exists(),
            "unchanged main must not create a worktree parent"
        );
    }

    #[test]
    fn conflict_refuses_with_file_and_cleans_without_moving_refs() {
        let (root, base) = init_repo("collect-trial-conflict");
        git(&root, &["switch", "-c", "task/B-conflict", &base]);
        fs::write(root.join("collision.txt"), "task\n").unwrap();
        let attempt = commit_all(&root, "task changes collision");
        git(&root, &["switch", "main"]);
        fs::write(root.join("collision.txt"), "main\n").unwrap();
        let moved_main = commit_all(&root, "main changes collision");
        let before_main = git(&root, &["rev-parse", "main"]);
        let before_task = git(&root, &["rev-parse", "task/B-conflict"]);
        let storage_permit = fixture_storage_permit(&root);

        let verdict = run_trial_merge_if_needed(
            &root,
            "r-test",
            "B900",
            ledger::GateAuditIdentity::Attempt {
                task_id: "B900",
                attempt_id: "B900-A0001",
            },
            &storage_permit,
            None,
            &attempt,
            &base,
            &moved_main,
            &["testFast".to_string()],
            &gate_commands("exit 0"),
            &root.join("coordination/runtime/logs"),
        )
        .unwrap()
        .expect("main moved, trial must run");
        assert_eq!(
            verdict,
            TrialMergeVerdict::Conflict {
                files: vec!["collision.txt".into()]
            }
        );
        assert_eq!(git(&root, &["rev-parse", "main"]), before_main);
        assert_eq!(git(&root, &["rev-parse", "task/B-conflict"]), before_task);
        assert!(
            fs::read_dir(root.join(".cowork-temp"))
                .unwrap()
                .next()
                .is_none(),
            "conflict path must clean its MERGING scratch worktree"
        );
    }
}
