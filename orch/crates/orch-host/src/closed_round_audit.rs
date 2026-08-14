//! 已收轮产物脏改检测（r57/B171，H34）：已 `RoundClosed` 轮次的
//! `rounds/<r>/{reviews,reports,evidence}/**` 若工作区 ≠ HEAD，逐文件报出。
//! 背景：r56 开轮清理时撞见一个 r53 时代的会话仍在运行、并改写它自己那份 r53 审查文件
//! （+157/-77）；审查产物的字节是 root verdict 的绑定对象，一次 `git add -A` 就会把篡改
//! 静默提交进历史。**只诊断、绝不自动写回**——修复提示用定点还原（`git show HEAD:<p> > <p>`），
//! 不得建议本仓禁用的 `git checkout --` / `reset --hard`。
//! （planner 预置占位：lib.rs 声明先行入库，B171 在本文件内实现，勿动 lib.rs——frozenPaths。）

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use orch_core::{fold, read_ledger};

/// Inputs to the pure drift classifier. Paths are repository-relative and use
/// git's `/` separator regardless of the host platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftInput {
    pub closed_rounds: Vec<String>,
    pub dirty_paths: Vec<String>,
}

/// One binding artifact that differs from the repository's HEAD tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftFinding {
    pub round: String,
    pub path: String,
    pub remediation: String,
}

/// Classify working-tree drift without reading or changing the filesystem.
///
/// Only verdict-binding artifact areas of rounds whose ledgers contain
/// `RoundClosed` are covered. Agent summaries are deliberately excluded: the
/// close protocol creates those after `RoundClosed` has already been written.
pub fn closed_round_drift(input: &DriftInput) -> Vec<DriftFinding> {
    let closed = input
        .closed_rounds
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut findings = input
        .dirty_paths
        .iter()
        .filter_map(|path| {
            let relative = path.strip_prefix("coordination/rounds/")?;
            let mut parts = relative.split('/');
            let round = parts.next()?;
            let area = parts.next()?;
            let remainder = parts.collect::<Vec<_>>();

            if !closed.contains(round) || remainder.is_empty() {
                return None;
            }
            if !matches!(area, "reviews" | "reports" | "evidence") {
                return None;
            }
            if area == "reports" && remainder.len() == 1 && remainder[0].ends_with("-SUMMARY.md") {
                return None;
            }

            Some(DriftFinding {
                round: round.to_string(),
                path: path.clone(),
                remediation: format!("git show HEAD:{path} > {path}"),
            })
        })
        .collect::<Vec<_>>();
    findings.sort_by(|a, b| a.path.cmp(&b.path));
    findings.dedup_by(|a, b| a.path == b.path);
    findings
}

/// Inspect a repository using only read-only git commands.
pub fn inspect_repository(root: &Path) -> Result<Vec<DriftFinding>> {
    let rounds_dir = root.join("coordination/rounds");
    let mut closed_rounds = Vec::new();
    match std::fs::read_dir(&rounds_dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry
                    .with_context(|| format!("读取轮次目录项失败: {}", rounds_dir.display()))?;
                if !entry
                    .file_type()
                    .with_context(|| format!("读取轮次目录项类型失败: {}", entry.path().display()))?
                    .is_dir()
                {
                    continue;
                }
                let ledger_path = entry.path().join("events.jsonl");
                if !ledger_path.is_file() {
                    continue;
                }
                let ledger = read_ledger(&ledger_path)
                    .with_context(|| format!("读取历史轮账本失败: {}", ledger_path.display()))?;
                if fold(&ledger.events).round_closed {
                    closed_rounds.push(entry.file_name().to_string_lossy().into_owned());
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("读取轮次目录失败: {}", rounds_dir.display()));
        }
    }

    let head_paths = head_paths(root)?;
    let mut findings = closed_round_drift(&DriftInput {
        closed_rounds,
        dirty_paths: dirty_paths_relative_to_head(root)?,
    });
    for finding in &mut findings {
        if !head_paths.contains(&finding.path) {
            finding.remediation = format!(
                "HEAD 无此 blob；doctor 不自动处理。请人工核实并优先将单文件 {} 隔离到 gitignored 的 coordination/runtime/closed-round-quarantine/，或核实后定点删除该文件",
                finding.path
            );
        }
    }
    Ok(findings)
}

fn dirty_paths_relative_to_head(root: &Path) -> Result<Vec<String>> {
    let mut paths = BTreeSet::new();
    collect_git_paths(
        root,
        &["diff", "--no-renames", "--name-only", "-z", "HEAD", "--"],
        &mut paths,
    )?;
    collect_git_paths(
        root,
        &["ls-files", "--others", "--exclude-standard", "-z", "--"],
        &mut paths,
    )?;
    Ok(paths.into_iter().collect())
}

fn head_paths(root: &Path) -> Result<BTreeSet<String>> {
    let mut paths = BTreeSet::new();
    collect_git_paths(
        root,
        &[
            "ls-tree",
            "-r",
            "--name-only",
            "-z",
            "HEAD",
            "--",
            "coordination/rounds",
        ],
        &mut paths,
    )?;
    Ok(paths)
}

fn collect_git_paths(root: &Path, args: &[&str], paths: &mut BTreeSet<String>) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .with_context(|| format!("启动 git {args:?} 失败"))?;
    if !output.status.success() {
        bail!(
            "git {:?} 失败({}): {}",
            args,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    for raw in output.stdout.split(|byte| *byte == 0) {
        if !raw.is_empty() {
            paths.insert(String::from_utf8_lossy(raw).into_owned());
        }
    }
    Ok(())
}
