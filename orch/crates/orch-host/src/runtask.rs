//! run-task：运行时驱动一整棒（Tier S）。
//! 流程 = 参考协议规划者主循环的机器化：lease worktree → 派发(spawn) → 收 REPORT →
//! 机检（E4 merge-base 域检 / 种子 SHA / 提交形状）→ 跑门 → 落账 → ready_for_verification。
//! 每步小步原子、失败即停并落 EscalationRaised（E7/E8）。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::{adapter, binding, card, failure, gate, gitx, ledger};

fn with_run_task_dispatch_effect<T>(root: &Path, action: impl FnOnce() -> Result<T>) -> Result<T> {
    crate::close::with_protocol_effect(root, "run-task dispatch", action)
}

fn cleanup_run_task_orphan(root: &Path, wt: &Path, branch: &str) -> Result<()> {
    let remove_error = wt
        .exists()
        .then(|| gitx::worktree_remove(root, wt))
        .and_then(Result::err);
    let branch_error = gitx::branch_exists(root, branch)
        .then(|| gitx::branch_delete(root, branch))
        .and_then(Result::err);
    if remove_error.is_some() || branch_error.is_some() {
        anyhow::bail!(
            "orphan cleanup failed: worktree={:?}, branch={:?}",
            remove_error,
            branch_error
        );
    }
    Ok(())
}

pub struct RunOutcome {
    pub task_id: String,
    pub branch: String,
    pub base_sha_short: String,
    pub report_rel: String,
    pub gates: Vec<gate::GateResult>,
    pub usage: Option<serde_json::Value>,
    pub mech_notes: Vec<String>,
}

fn current_round(root: &Path) -> Result<String> {
    Ok(
        fs::read_to_string(root.join("coordination/runtime/CURRENT-ROUND"))
            .context("runtime/CURRENT-ROUND 缺失（先开轮）")?
            .trim()
            .to_string(),
    )
}

fn command_text(b: &binding::Binding, command_ref: &str) -> Result<String> {
    let spec = b
        .commands
        .get(command_ref)
        .with_context(|| format!("绑定缺命令: {command_ref}"))?;
    Ok(spec.argv.join(" "))
}

fn gate_command_lines(c: &card::Card, b: &binding::Binding) -> Result<String> {
    let available = b.commands.keys().cloned().collect::<Vec<_>>();
    let gate_refs =
        binding::resolve_gates(&c.meta.gates.fast, &available).map_err(anyhow::Error::msg)?;
    if gate_refs.is_empty() {
        return Ok("  - （卡内 gates.fast 为空：合法跳过快速门段）".into());
    }
    let mut lines = Vec::new();
    for command_ref in gate_refs {
        lines.push(format!(
            "  - `{command_ref}`: `{}`",
            command_text(b, &command_ref)?
        ));
    }
    Ok(lines.join("\n"))
}

/// 渲染 Tier S 执行者提示词（bootstrap 模板的 direct-spawn 变体：无需等待、无需自开 worktree）
fn render_prompt(
    root: &Path,
    round: &str,
    c: &card::Card,
    base_short: &str,
    report_rel: &str,
    b: &binding::Binding,
) -> Result<String> {
    let seeds = c
        .meta
        .seeds
        .iter()
        .map(|s| {
            format!(
                "  - 逐字节复制 {} → {}（作为单独 commit：`seed({}): relocate contract test`）",
                s.src, s.target, c.meta.task_id
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let gate_commands = gate_command_lines(c, b)?;
    let node_modules_hint = b
        .has_ecosystem("node")
        .then_some("；node_modules 已由运行时软链就位")
        .unwrap_or_default();
    Ok(format!(
        "你是多 AI 协作轮 {round} 的执行者（Tier S 单次调用，由运行时直接调起）。\n\
         你当前的工作目录**就是**任务 worktree（分支 task/{id}，基线 {base}）——不要自己再开 worktree、不要离开此目录树。\n\
         主仓协议与任务卡（相对本目录可读）：coordination/PROTOCOL.md 与 {card_rel}。先完整读这两个文件。\n\n\
         执行序列（红→绿）：\n\
         1. 种子搬运（第一个 commit 只含种子文件）：\n{seeds}\n\
         2. 若有红种子，使用下列卡 gates.fast 命令中的适用测试门取红（记录 failed 原文）。\n\
         卡驱动快门命令：\n{gate_commands}\n\
         3. 只改 writeSet 内的产品文件使种子转绿（禁止改任何测试断言消红）。\n\
         4. 快门全绿：逐项执行上述非空命令清单并全部成功；空列表按任务卡意图合法跳过。\n\
         5. 按种子头注释的负向变异清单逐条：注入→确认对应用例红→撤销→复绿。\n\
         6. **最后**写 {report} 并 commit（`report({id}): record verification evidence`）——REPORT 是最后一个动作。\n\n\
         铁律：绝不 push、绝不合并 main、绝不改 writeSet 外文件、绝不动 coordination/（除新建你的 REPORT）。\n\
         完成后直接结束，不需要等待。\n\
         （项目根提示：{root_disp}{node_modules_hint}。）",
        round = round,
        id = c.meta.task_id,
        base = base_short,
        card_rel = c.rel_path,
        seeds = seeds,
        report = report_rel,
        root_disp = root.display(),
        gate_commands = gate_commands,
        node_modules_hint = node_modules_hint,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::time::Instant;

    static TEST_REPO_SEQ: AtomicU64 = AtomicU64::new(0);

    fn run_git(root: &Path, args: &[&str]) {
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
    }

    fn b134_test_repo(tag: &str) -> PathBuf {
        let seq = TEST_REPO_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "orch-b134-runtask-{tag}-{}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("coordination/runtime")).unwrap();
        fs::create_dir_all(root.join("coordination/rounds/r1")).unwrap();
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r1\n").unwrap();
        fs::write(root.join("coordination/rounds/r1/events.jsonl"), "").unwrap();
        run_git(&root, &["init", "-q"]);
        fs::write(root.join("README.md"), "base\n").unwrap();
        run_git(&root, &["add", "README.md"]);
        run_git(
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
        run_git(&root, &["branch", "-M", "main"]);
        root
    }

    #[test]
    fn prompt_uses_binding_commands_without_node_hint_for_rust() {
        let meta: card::CardMeta =
            serde_yaml::from_str(
                "taskId: T1\nseeds:\n  - {src: seed.rs, target: tests/seed.rs}\ngates: {fast: [unitRust, lintRust]}\n",
            )
            .unwrap();
        let card = card::Card {
            meta,
            body: String::new(),
            rel_path: "coordination/rounds/r9/tasks/T1.md".into(),
        };
        let binding: binding::Binding = serde_yaml::from_str(
            "project:\n  ecosystems: [rust]\ncommands:\n  unitRust:\n    argv: [/tool/cargo, test]\n  lintRust:\n    argv: [/tool/cargo, check]\n",
        )
        .unwrap();
        let prompt = render_prompt(
            Path::new("/repo"),
            "r9",
            &card,
            "abc1234",
            "coordination/rounds/r9/reports/T1-REPORT.md",
            &binding,
        )
        .unwrap();
        assert!(prompt.contains("/tool/cargo test"));
        assert!(prompt.contains("/tool/cargo check"));
        assert!(prompt.contains("unitRust"));
        assert!(prompt.contains("lintRust"));
        assert!(!prompt.contains("testFast"));
        assert!(!prompt.contains("npx vitest"));
        assert!(!prompt.contains("node_modules"));
    }

    #[test]
    fn prompt_allows_empty_card_gate_list() {
        let meta: card::CardMeta = serde_yaml::from_str("taskId: T2\ngates: {fast: []}\n").unwrap();
        let card = card::Card {
            meta,
            body: String::new(),
            rel_path: "coordination/rounds/r17/tasks/T2.md".into(),
        };
        let binding: binding::Binding =
            serde_yaml::from_str("project:\n  ecosystems: [rust]\ncommands: {}\n").unwrap();
        let prompt = render_prompt(
            Path::new("/repo"),
            "r17",
            &card,
            "abc1234",
            "coordination/rounds/r17/reports/T2-REPORT.md",
            &binding,
        )
        .unwrap();
        assert!(prompt.contains("gates.fast 为空"));
        assert!(prompt.contains("快门全绿"));
    }

    #[test]
    fn run_task_dispatch_shared_lease_rejects_exclusive_transition_fast() {
        let root = b134_test_repo("exclusive");
        let worker_root = root.clone();
        let (held_tx, held_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            with_run_task_dispatch_effect(&worker_root, || {
                held_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                Ok(())
            })
        });
        held_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        let error =
            crate::close::with_protocol_transition(&root, "test exclusive", || Ok(())).unwrap_err();
        assert!(started.elapsed() < Duration::from_millis(200));
        assert!(format!("{error:#}").contains("fail-fast"));
        release_tx.send(()).unwrap();
        worker.join().unwrap().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn run_task_orphan_cleanup_removes_worktree_and_branch() {
        let root = b134_test_repo("cleanup");
        let wt = root.join(".worktrees/T1");
        let base = gitx::rev_parse(&root, "main").unwrap();
        gitx::worktree_add(&root, &wt, "task/T1", &base).unwrap();
        assert!(wt.is_dir());
        assert!(gitx::branch_exists(&root, "task/T1"));
        cleanup_run_task_orphan(&root, &wt, "task/T1").unwrap();
        assert!(!wt.exists());
        assert!(!gitx::branch_exists(&root, "task/T1"));
        fs::remove_dir_all(root).unwrap();
    }
}

pub fn run_task(
    root: &Path,
    task_id: &str,
    agent_override: Option<String>,
    timeout_secs: u64,
) -> Result<RunOutcome> {
    let round = current_round(root)?;
    match run_task_inner(root, task_id, agent_override, timeout_secs, &round) {
        Ok(outcome) => Ok(outcome),
        Err(error) if failure::is_action_rejection(&error) => Err(error),
        Err(error) => failure::reject_attempt_action_from_ledger_disposition(
            root,
            &round,
            task_id,
            "run-task",
            &format!("run-task:{round}:{task_id}"),
            &format!("{error:#}"),
            failure::CliDisposition::Rejected,
        ),
    }
}

fn run_task_inner(
    root: &Path,
    task_id: &str,
    agent_override: Option<String>,
    timeout_secs: u64,
    round: &str,
) -> Result<RunOutcome> {
    let c = card::load(root, &round, task_id)?;
    let agent = agent_override
        .or_else(|| c.meta.agent.clone())
        .context("任务卡无 agent 且未指定 --agent")?;
    // 卡内 agent 名 → adapter 名：集中映射（r44/B96），不再硬编码二分
    let adapter_name = adapter::adapter_name_for_agent(&agent);
    let b = binding::load(root)?;
    let report_rel = format!("coordination/rounds/{round}/reports/{task_id}-REPORT.md");
    let branch = format!("task/{task_id}");
    let log_dir = root.join("coordination/runtime/logs");

    let base_sha = gitx::rev_parse(root, "main")?;
    let base_short = gitx::short(&base_sha).to_string();
    let wt: PathBuf = root.join(&b.workspace.worktree_root).join(task_id);
    // Reservation → worktree → optional dependency link → DispatchIssued is
    // one short shared protocol effect.  The long-running provider stays
    // outside this lease so exclusive transitions remain fail-fast instead of
    // waiting for an executor process.
    with_run_task_dispatch_effect(root, || {
        let mut wake_permit = crate::budget::check_before_model_wake(root, &round)?;
        if gitx::branch_exists(root, &branch) {
            let reason = format!(
                "分支 {branch} 已存在——请先处理（新 Attempt 应换 attempt 分支或清理旧现场）"
            );
            let rejection = failure::run_task_pre_attempt_rejection(&round, task_id, &reason)?;
            failure::append_rejection(root, &round, Some(task_id), &rejection)?;
            return Err(rejection.into_error());
        }
        if wt.exists() {
            let reason = format!("worktree 目录已存在: {}", wt.display());
            let rejection = failure::run_task_pre_attempt_rejection(&round, task_id, &reason)?;
            failure::append_rejection(root, &round, Some(task_id), &rejection)?;
            return Err(rejection.into_error());
        }

        if let Err(error) = gitx::worktree_add(root, &wt, &branch, &base_sha) {
            cleanup_run_task_orphan(root, &wt, &branch)
                .with_context(|| format!("run-task worktree add failed ({error:#})"))?;
            return Err(error);
        }
        let dispatch_result = (|| -> Result<()> {
            // 依赖软链（wiki/05 sharedCacheRefs 思想；O2：软链会绕过
            // gitignore 目录模式，属预期）。
            let nm_src = root.join("node_modules");
            if b.has_ecosystem("node") && nm_src.exists() && !wt.join("node_modules").exists() {
                #[cfg(unix)]
                std::os::unix::fs::symlink(&nm_src, wt.join("node_modules"))
                    .context("创建 run-task node_modules 软链失败")?;
            }
            ledger::append(
                root,
                &round,
                &[ledger::event(
                    "DispatchIssued",
                    "runtime:orch",
                    Some(task_id),
                    Some(&round),
                    serde_json::json!({"agent": agent, "adapter": adapter_name, "method": "direct-spawn(TierS)", "baseSha": base_short}),
                )],
            )?;
            wake_permit.commit();
            Ok(())
        })();
        if let Err(error) = dispatch_result {
            // No dispatch fact became durable, so the branch/worktree are
            // orphan resources rather than recoverable task state.
            cleanup_run_task_orphan(root, &wt, &branch)
                .with_context(|| format!("run-task dispatch failed ({error:#})"))?;
            return Err(error);
        }
        Ok(())
    })?;
    println!(
        "① worktree 就绪: {}（{branch} @ {base_short}）",
        wt.display()
    );

    // ── 步骤 3：spawn 执行者，等待退出（Tier S：退出即完成信号）──
    let prompt = render_prompt(root, &round, &c, &base_short, &report_rel, &b)?;
    println!("② spawn {adapter_name}（超时 {timeout_secs}s）……");
    let run = adapter::run(
        adapter_name,
        &prompt,
        &wt,
        &log_dir,
        &format!("{task_id}-{adapter_name}"),
        Duration::from_secs(timeout_secs),
        Some(&root.join(".git")), // E11：worktree 任务加白主仓 .git
        Some(root),               // AdapterSpec 外置化查找根
    )?;
    println!(
        "   退出码 {}，耗时 {}s，日志 {}",
        run.exit_code, run.duration_secs, run.log_path
    );

    // ── 步骤 4：REPORT 是唯一完成真值（design/03 §4.3）──
    let report_abs = wt.join(&report_rel);
    if !report_abs.is_file() {
        let reason = format!(
            "REPORT 缺失: {}（executor 退出码 {}）",
            report_abs.display(),
            run.exit_code
        );
        let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
        let ledger_read = orch_core::read_ledger(&ledger_path)
            .with_context(|| format!("REPORT 缺失后重读账本失败: {}", ledger_path.display()))?;
        crate::attempt::reject_bad_lines(&ledger_read)?;
        let events = ledger_read.events;
        let rejection = failure::rejection_with_current_attempt_disposition(
            &events,
            task_id,
            "run-task",
            &format!("run-task:{}:{}", round, task_id),
            &reason,
            failure::CliDisposition::Rejected,
        )?;
        failure::append_rejection(root, &round, Some(task_id), &rejection)?;
        ledger::append(
            root,
            &round,
            &[ledger::event(
                "EscalationRaised",
                "runtime:orch",
                Some(task_id),
                Some(&round),
                serde_json::json!({"stage":"collect","reason":"进程退出但 REPORT 缺失","exit":run.exit_code}),
            )],
        )?;
        return Err(rejection.into_error());
    }
    ledger::append(
        root,
        &round,
        &[ledger::event(
            "ReportObserved",
            "runtime:orch",
            Some(task_id),
            Some(&round),
            serde_json::json!({"reportPath": report_rel, "executorExit": run.exit_code}),
        )],
    )?;
    println!("③ REPORT 已收: {report_rel}");

    // ── 步骤 5+6：机检 + 门（共享 collect 阶段，与 await-report 同路径）──
    let co = crate::collect::check_and_gate(
        root,
        &round,
        &c,
        &branch,
        &report_rel,
        Some(&base_sha),
        &wt,
    )?;
    let mech_notes = co.mech_notes;
    let gates = co.gates;

    // ── 步骤 7：成本采样 + 完成 ──
    ledger::append(
        root,
        &round,
        &[ledger::event(
            "CostSampled",
            "runtime:orch",
            Some(task_id),
            Some(&round),
            serde_json::json!({"agent": agent, "durationSecs": run.duration_secs, "usage": run.usage}),
        )],
    )?;

    Ok(RunOutcome {
        task_id: task_id.to_string(),
        branch,
        base_sha_short: base_short,
        report_rel,
        gates,
        usage: run.usage,
        mech_notes,
    })
}
