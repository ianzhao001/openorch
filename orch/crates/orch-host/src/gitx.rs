//! git 操作：刻意 shell-out `git` 二进制（design/05 §4——libgit2 worktree 支持不全，
//! 且与参考协议实证行为一致、可调试）。全部命令带 `-C <root>` 绝对路径（E6）。

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{bail, Context, Result};
use fd_lock::RwLock;

fn run(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    run_with_envs(root, args, &[])
}

fn run_with_envs(root: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<Vec<u8>> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root).args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd
        .output()
        .with_context(|| format!("git {args:?} 启动失败"))?;
    if !out.status.success() {
        bail!(
            "git {:?} 失败({}): {}",
            args,
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

fn run_str(root: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8_lossy(&run(root, args)?)
        .trim()
        .to_string())
}

fn run_str_with_envs(root: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<String> {
    Ok(String::from_utf8_lossy(&run_with_envs(root, args, envs)?)
        .trim()
        .to_string())
}

pub fn rev_parse(root: &Path, rev: &str) -> Result<String> {
    run_str(root, &["rev-parse", rev])
}

pub fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

pub fn branch_exists(root: &Path, branch: &str) -> bool {
    run(
        root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok()
}

type WorktreeInitLocks = Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>;

fn worktree_init_locks() -> &'static WorktreeInitLocks {
    static LOCKS: OnceLock<WorktreeInitLocks> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with_worktree_init_lock<T>(root: &Path, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let common_dir = canonical_worktree_common_dir(root)?;
    let process_lock = {
        let mut locks = worktree_init_locks()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(
            locks
                .entry(common_dir.clone())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    };
    let _process_guard = process_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let lock_path = common_dir.join("orch-worktree-init.lock");
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| {
            format!(
                "open worktree initialization lock failed: {}",
                lock_path.display()
            )
        })?;
    let mut cross_process_lock = RwLock::new(lock_file);
    let _cross_process_guard = cross_process_lock.write().with_context(|| {
        format!(
            "wait for worktree initialization lock failed: {}",
            lock_path.display()
        )
    })?;

    operation()
}

pub fn worktree_add(root: &Path, path: &Path, branch: &str, base_sha: &str) -> Result<()> {
    // E5：直接给 SHA，不用 main ref
    with_worktree_init_lock(root, || {
        run(
            root,
            &[
                "worktree",
                "add",
                &path.to_string_lossy(),
                "-b",
                branch,
                base_sha,
            ],
        )?;
        Ok(())
    })
}

/// 无分支 detached worktree（oracle 预验等一次性隔离现场）
pub fn worktree_add_detached(root: &Path, path: &Path, base_sha: &str) -> Result<()> {
    with_worktree_init_lock(root, || {
        run(
            root,
            &[
                "worktree",
                "add",
                "--detach",
                &path.to_string_lossy(),
                base_sha,
            ],
        )?;
        Ok(())
    })
}

/// 在指定目录（主仓或 worktree）checkout 到 rev（SHA=detached / 分支名=回附着）
pub fn checkout(dir: &Path, rev: &str) -> Result<()> {
    run(dir, &["checkout", "--quiet", rev])?;
    Ok(())
}

pub fn merge_base(root: &Path, a: &str, b: &str) -> Result<String> {
    run_str(root, &["merge-base", a, b])
}

/// Materialize Git's recursive merge result as an unreferenced tree object.
///
/// A conflict or malformed object id is a hard error; callers must not run a
/// final gate against a partially merged index or guess at the first parent.
#[cfg(feature = "selfhost")]
pub(crate) fn write_merge_tree(root: &Path, main_sha: &str, candidate_sha: &str) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["merge-tree", "--write-tree", main_sha, candidate_sha])
        .output()
        .context("git merge-tree --write-tree 启动失败")?;
    if !output.status.success() {
        bail!(
            "synthetic merge-tree 冲突或失败({}): stdout={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8(output.stdout).context("merge-tree stdout 非 UTF-8")?;
    let tree = stdout
        .lines()
        .next()
        .map(str::trim)
        .filter(|value| {
            value.len() == 40
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .context("merge-tree 未返回 canonical full tree SHA")?
        .to_string();
    if rev_parse(root, &format!("{tree}^{{tree}}"))? != tree {
        bail!("merge-tree 返回对象不是 exact tree");
    }
    Ok(tree)
}

/// Wrap an already computed tree in an unreferenced two-parent commit so a
/// clean detached worktree can execute gates without moving any branch ref.
#[cfg(feature = "selfhost")]
pub(crate) fn commit_synthetic_merge_tree(
    root: &Path,
    tree_sha: &str,
    main_sha: &str,
    candidate_sha: &str,
    message: &str,
) -> Result<String> {
    let commit = run_str(
        root,
        &[
            "commit-tree",
            tree_sha,
            "-p",
            main_sha,
            "-p",
            candidate_sha,
            "-m",
            message,
        ],
    )?;
    if rev_parse(root, &format!("{commit}^{{tree}}"))? != tree_sha {
        bail!("synthetic merge commit tree 漂移");
    }
    Ok(commit)
}

/// Return whether `ancestor` is an ancestor of (or equal to) `descendant`.
///
/// Unlike [`run`], exit status 1 is a normal negative answer for
/// `merge-base --is-ancestor`; other failures remain errors.
pub fn is_ancestor(root: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .output()
        .with_context(|| "git merge-base --is-ancestor 启动失败")?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "git merge-base --is-ancestor 失败({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// `tip` 的历史里是否存在一个 merge commit 以 `parent_sha` 为其任一父。
///
/// H97/H98（r62/B204 实证）：屏障恢复此前只用「main 未推进」当作「合并未发生」的
/// 代理判据，而 planner 在屏障期为别的原因推进 main 会让它误判并锁死全轮。
/// 本函数与 [`is_ancestor`] 一起给出**直接证明**：分支既不是 `tip` 的祖先、
/// 也不是 `tip` 历史里任何 merge 的父 ⇒ 该分支确证从未被合入。
/// 失败一律向上传播，**绝不把「查不到」当作「不存在」**。
pub fn merge_parents_contain(root: &Path, tip: &str, parent_sha: &str) -> Result<bool> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["log", "--merges", "--format=%P", tip])
        .output()
        .with_context(|| "git log --merges 启动失败")?;
    if !out.status.success() {
        bail!(
            "git log --merges 失败({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .any(|parent| parent == parent_sha))
}

/// 分支侧改动文件清单（E4：以 merge-base 为基）
pub fn diff_names(root: &Path, from: &str, to: &str) -> Result<Vec<String>> {
    Ok(
        run_str(root, &["diff", "--name-only", &format!("{from}..{to}")])?
            .lines()
            .map(String::from)
            .filter(|l| !l.is_empty())
            .collect(),
    )
}

/// merge-base 之后按时间序的提交列表
pub fn commits_after(root: &Path, mb: &str, branch: &str) -> Result<Vec<String>> {
    Ok(
        run_str(root, &["rev-list", "--reverse", &format!("{mb}..{branch}")])?
            .lines()
            .map(String::from)
            .filter(|l| !l.is_empty())
            .collect(),
    )
}

/// 单个提交触碰的文件
pub fn commit_files(root: &Path, sha: &str) -> Result<Vec<String>> {
    Ok(run_str(root, &["show", "--name-only", "--format=", sha])?
        .lines()
        .map(String::from)
        .filter(|l| !l.is_empty())
        .collect())
}

pub fn commit_subject(root: &Path, sha: &str) -> Result<String> {
    run_str(root, &["show", "-s", "--format=%s", sha])
}

/// 某 rev 最后一次 commit 的 Unix 秒（liveness 活动信号之一）
pub fn commit_unix_time(root: &Path, rev: &str) -> Result<i64> {
    run_str(root, &["log", "-1", "--format=%ct", rev])?
        .parse()
        .context("解析 commit 时间失败")
}

/// 读分支上某文件的字节内容
pub fn show_bytes(root: &Path, branch: &str, path: &str) -> Result<Vec<u8>> {
    run(root, &["show", &format!("{branch}:{path}")])
}

/// Prove whether an exact literal path exists in a commit/tree.
///
/// `git show <rev>:<path>` reports both an absent path and unrelated object
/// failures through the same error channel.  `ls-tree` instead lets a
/// successful, empty object-layer lookup prove absence while still
/// propagating an invalid/corrupt tree as an error.  `-z` keeps the returned
/// path byte-exact and `:(literal)` prevents pathspec interpretation.
#[cfg(feature = "selfhost")]
pub(crate) fn tree_path_exists(root: &Path, treeish: &str, path: &str) -> Result<bool> {
    let literal_pathspec = format!(":(literal){path}");
    let output = run(
        root,
        &[
            "ls-tree",
            "--full-tree",
            "--name-only",
            "-z",
            treeish,
            "--",
            &literal_pathspec,
        ],
    )?;
    let mut entries = output
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty());
    let Some(entry) = entries.next() else {
        return Ok(false);
    };
    if entry != path.as_bytes() || entries.next().is_some() {
        bail!("git ls-tree exact-path probe returned unexpected entries for {path:?}");
    }
    Ok(true)
}

pub fn merge_no_ff(root: &Path, branch: &str, msg: &str) -> Result<()> {
    run(root, &["merge", "--no-ff", branch, "-m", msg])?;
    Ok(())
}

pub fn worktree_remove(root: &Path, path: &Path) -> Result<()> {
    run(
        root,
        &["worktree", "remove", "--force", &path.to_string_lossy()],
    )?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRegistryEntry {
    pub path: PathBuf,
    pub prunable: bool,
}

/// Parse Git's own linked-worktree registry.  Generated orch paths are safe
/// UTF-8 components, so the line-oriented porcelain form is sufficient and
/// keeps `prunable` attached to its exact entry.
pub fn worktree_registry(root: &Path) -> Result<Vec<WorktreeRegistryEntry>> {
    let text = run_str(root, &["worktree", "list", "--porcelain"])?;
    let mut entries = Vec::new();
    let mut path = None::<PathBuf>;
    let mut prunable = false;
    for line in text.lines().chain(std::iter::once("")) {
        if let Some(value) = line.strip_prefix("worktree ") {
            if path.is_some() {
                bail!("git worktree porcelain entry 缺空行分隔");
            }
            path = Some(PathBuf::from(value));
        } else if line == "prunable" || line.starts_with("prunable ") {
            prunable = true;
        } else if line.is_empty() {
            if let Some(path) = path.take() {
                entries.push(WorktreeRegistryEntry { path, prunable });
                prunable = false;
            }
        }
    }
    if path.is_some() {
        bail!("git worktree porcelain 尾记录未闭合");
    }
    Ok(entries)
}

fn canonical_git_path(dir: &Path, argument: &str) -> Result<PathBuf> {
    let raw = run_str(dir, &["rev-parse", argument])?;
    if raw.is_empty() || raw.lines().count() != 1 {
        bail!("git rev-parse {argument} 返回空/多行路径");
    }
    let path = PathBuf::from(raw);
    let absolute = if path.is_absolute() {
        path
    } else {
        dir.join(path)
    };
    std::fs::canonicalize(&absolute)
        .with_context(|| format!("canonicalize git path 失败: {}", absolute.display()))
}

#[doc(hidden)]
pub fn canonical_worktree_common_dir(root: &Path) -> Result<PathBuf> {
    canonical_git_path(root, "--git-common-dir")
}

pub fn same_common_dir(root: &Path, worktree: &Path) -> Result<bool> {
    Ok(canonical_worktree_common_dir(root)? == canonical_worktree_common_dir(worktree)?)
}

pub fn worktree_is_detached(worktree: &Path) -> Result<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .output()
        .context("git symbolic-ref detached probe 启动失败")?;
    match output.status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => bail!(
            "git symbolic-ref detached probe 失败({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

/// Tracked/staged modifications only; untracked residue is inventoried by a
/// separate NUL-delimited command before teardown.
pub fn tracked_porcelain(worktree: &Path) -> Result<Vec<u8>> {
    run(
        worktree,
        &["status", "--porcelain=v1", "-z", "--untracked-files=no"],
    )
}

pub fn untracked_paths(worktree: &Path) -> Result<Vec<PathBuf>> {
    let bytes = run(
        worktree,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    let mut paths = Vec::new();
    for raw in bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let value = std::str::from_utf8(raw).context("git ls-files untracked path 非 UTF-8")?;
        let path = PathBuf::from(value);
        if path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            bail!("git ls-files 返回不安全 untracked path: {value:?}");
        }
        paths.push(path);
    }
    Ok(paths)
}

/// Prune only when the requested missing worktree is Git's sole prunable
/// entry.  `git worktree prune` is otherwise global, so refusing unrelated
/// candidates is what makes this operation exact rather than a pattern GC.
pub fn worktree_prune_exact(root: &Path, path: &Path) -> Result<()> {
    let expected = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let entries = worktree_registry(root)?;
    let registered = entries.iter().any(|entry| entry.path == expected);
    if !registered {
        return Ok(());
    }
    let prunable = entries
        .iter()
        .filter(|entry| entry.prunable)
        .map(|entry| entry.path.clone())
        .collect::<Vec<_>>();
    if prunable != [expected.clone()] {
        bail!(
            "精确 prune 拒绝：requested={}，全部 prunable={:?}",
            expected.display(),
            prunable
        );
    }
    run(root, &["worktree", "prune", "--expire", "now"])?;
    if worktree_registry(root)?
        .iter()
        .any(|entry| entry.path == expected)
    {
        bail!("精确 prune 后注册项仍存在: {}", expected.display());
    }
    Ok(())
}

pub fn branch_delete(root: &Path, branch: &str) -> Result<()> {
    run(root, &["branch", "-d", branch])?;
    Ok(())
}

// ─────────────────────── B97: 非破坏 WIP snapshot 的 plumbing ───────────────────────
// 全部用 `-C <root>` 绝对路径 + 环境变量 `GIT_INDEX_FILE=<temp>` 指向临时索引，
// 永不触碰真实 index、真实 HEAD 或 worktree 字节（design/05 §3 + errata E6/E8）。

/// 在临时索引上构建一棵「HEAD 树 + 当前 worktree 的 staged+unstaged+untracked 全部内容」
/// 的 tree，返回 tree SHA。过程不改变真实 index、HEAD 与 worktree 任何字节。
///
/// 实现：用 `GIT_INDEX_FILE=<temp>` + `-C <worktree>` + `--work-tree=<worktree>`，把 HEAD
/// 读进临时索引，再用 `add -A` 把 worktree 当前内容（含未跟踪）整体 stage 进临时索引，
/// 最后 `write-tree`。环境变量隔离临时索引，绝不触碰真实 index。
pub fn snapshot_tree(_root: &Path, worktree: &Path, temp_index: &Path) -> Result<String> {
    let binding = temp_index.to_string_lossy().into_owned();
    let envs = [("GIT_INDEX_FILE", binding.as_str())];
    // 临时索引从 HEAD 的 tree 初始化（read-tree 作用于 temp_index，-C worktree）
    run_with_envs(worktree, &["read-tree", "HEAD"], &envs)?;
    // 把 worktree 当前所有改动（staged/unstaged/untracked 全量）stage 到临时索引
    run_with_envs(worktree, &["add", "-A"], &envs)?;
    run_str_with_envs(worktree, &["write-tree"], &envs)
}

/// `git rev-parse --git-path <name>` 原文（绝对路径）。
pub fn rev_parse_path(dir: &Path, name: &str) -> Result<String> {
    run_str(dir, &["rev-parse", "--git-path", name])
}

/// 用 commit-tree 把 tree 封进一个游离 commit（不更新任何 ref），返回 commit SHA。
/// parent=HEAD 保证可追溯，但绝不移动 HEAD（commit-tree 只造对象）。
pub fn commit_tree(root: &Path, tree: &str, parent: &str, msg: &str) -> Result<String> {
    run_str(root, &["commit-tree", tree, "-p", parent, "-m", msg])
}

/// 把游离 commit SHA 落为本地 ref（archive ref，不动 HEAD）。
pub fn update_ref(root: &Path, ref_name: &str, sha: &str) -> Result<()> {
    run(root, &["update-ref", ref_name, sha])?;
    Ok(())
}

/// CAS (compare-and-swap) update-ref：仅当 ref 不存在 (old=zero) 或已存在等价 commit
/// 时才成功。等价比较：tree SHA + parent SHA 相同（commit SHA 本身因 commit-tree 时间
/// 不同会变，不能只比较 commit SHA）。
/// - ref 不存在 ⇒ create（成功后 ref=sha）
/// - ref 已存在且 tree+parent 等价 ⇒ 幂等成功（复用旧 ref，不覆盖）
/// - ref 已存在且 tree+parent 不同 ⇒ Err（collision，不覆盖）
#[doc(hidden)]
pub fn validate_exact_single_parent_commit(
    root: &Path,
    candidate: &str,
    expected_tree: &str,
    expected_parent: &str,
) -> Result<()> {
    let object_type = run_str(root, &["cat-file", "-t", candidate])
        .with_context(|| format!("读取 candidate object type 失败: {candidate}"))?;
    if object_type != "commit" {
        bail!("archive candidate {candidate} object type={object_type:?}, expected commit");
    }
    let actual_tree = run_str(root, &["show", "-s", "--format=%T", candidate])?;
    if actual_tree != expected_tree {
        bail!("archive candidate {candidate} tree mismatch: {actual_tree} != {expected_tree}");
    }
    let parent_line = run_str(root, &["show", "-s", "--format=%P", candidate])?;
    let parents = parent_line.split_whitespace().collect::<Vec<_>>();
    if parents.len() != 1 {
        bail!(
            "archive candidate {candidate} 必须 exactly one parent，实际 {}: {:?}",
            parents.len(),
            parents
        );
    }
    if parents[0] != expected_parent {
        bail!(
            "archive candidate {candidate} parent mismatch: {} != {expected_parent}",
            parents[0]
        );
    }
    Ok(())
}

fn resolve_ref_optional(root: &Path, ref_name: &str) -> Result<Option<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "--quiet", ref_name])
        .output()
        .with_context(|| format!("git rev-parse ref 启动失败: {ref_name}"))?;
    if out.status.success() {
        return Ok(Some(
            String::from_utf8(out.stdout)
                .context("git rev-parse ref 输出不是 UTF-8")?
                .trim()
                .to_string(),
        ));
    }
    if out.status.code() == Some(1) {
        return Ok(None);
    }
    bail!(
        "git rev-parse ref 失败({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    )
}

pub fn update_ref_cas_create(
    root: &Path,
    ref_name: &str,
    sha: &str,
    tree: &str,
    parent: &str,
) -> Result<String> {
    update_ref_cas_create_with_hook(root, ref_name, sha, tree, parent, &mut || Ok(()))
}

/// Deterministic production CAS seam: the hook runs after the absent-ref read
/// and immediately before `git update-ref <ref> <sha> <zero>`.
#[doc(hidden)]
pub fn update_ref_cas_create_with_hook(
    root: &Path,
    ref_name: &str,
    sha: &str,
    tree: &str,
    parent: &str,
    hook: &mut dyn FnMut() -> Result<()>,
) -> Result<String> {
    validate_exact_single_parent_commit(root, sha, tree, parent)?;
    let existing = resolve_ref_optional(root, ref_name)?;
    match existing {
        Some(existing_sha) => {
            validate_exact_single_parent_commit(root, &existing_sha, tree, parent)
                .with_context(|| format!("archive ref {ref_name} collision"))?;
            Ok(existing_sha)
        }
        None => {
            // ref 不存在——create（用 update-ref 带 old=zero 保证 CAS）
            hook()?;
            let mut cmd = Command::new("git");
            cmd.arg("-C").arg(root).args([
                "update-ref",
                ref_name,
                sha,
                "0000000000000000000000000000000000000000",
            ]);
            let out = cmd
                .output()
                .with_context(|| format!("git update-ref CAS 启动失败"))?;
            if !out.status.success() {
                // CAS 失败：只有一个经过完整验证的并发等价 ref 才可复用。
                let now_existing = resolve_ref_optional(root, ref_name)?;
                if let Some(now_sha) = now_existing {
                    validate_exact_single_parent_commit(root, &now_sha, tree, parent)
                        .with_context(|| format!("archive ref {ref_name} CAS race collision"))?;
                    return Ok(now_sha);
                }
                bail!(
                    "archive ref {} CAS 创建失败（并发冲突？）: {}",
                    ref_name,
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            let actual = resolve_ref_optional(root, ref_name)?
                .context("archive ref CAS reported success but ref is missing")?;
            validate_exact_single_parent_commit(root, &actual, tree, parent)?;
            Ok(actual)
        }
    }
}

/// `git status --porcelain=v2` 原文（前后一致性比对用）。
pub fn porcelain_v2(dir: &Path) -> Result<String> {
    run_str(dir, &["status", "--porcelain=v2"])
}

/// worktree 当前所在分支名（detached 返 None）。
pub fn current_branch(dir: &Path) -> Result<Option<String>> {
    let out = run_str(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    Ok(if out.is_empty() { None } else { Some(out) })
}

/// 临时索引文件路径（OS 临时目录，随用随删；与真实 index 完全隔离）。
/// 使用 process-local AtomicU64 + pid + nanos 三重保证并行不冲突。
pub fn temp_index_path(root: &Path, tag: &str) -> std::path::PathBuf {
    let _ = root;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "orch-b97-tmp-index-{tag}-{}-{seq}-{nanos}",
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_test_path(tag: &str) -> std::path::PathBuf {
        let sequence = TEST_PATH_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "orch-b97-gitx-{tag}-{}-{sequence}",
            std::process::id()
        ))
    }

    #[test]
    fn default_parallel_temp_paths_are_unique() {
        let root = unique_test_path("root");
        let first = temp_index_path(&root, "same-tag");
        let second = temp_index_path(&root, "same-tag");
        assert_ne!(first, second);
        assert_ne!(unique_test_path("repo"), unique_test_path("repo"));
    }

    #[test]
    fn worktree_init_lock_releases_after_git_failure_without_deleting_lockfile() {
        let root = crate::util::test_scratch_dir("b249-worktree-init-failure");
        let bad_site = root.join("bad-site");
        let good_site = root.join("good-site");
        std::fs::create_dir_all(&root).unwrap();
        run(&root, &["init"]).unwrap();
        run(
            &root,
            &[
                "-c",
                "user.name=orch-test",
                "-c",
                "user.email=orch-test@example.invalid",
                "-c",
                "commit.gpgSign=false",
                "commit",
                "--allow-empty",
                "-m",
                "base",
            ],
        )
        .unwrap();

        let common_dir = canonical_worktree_common_dir(&root).unwrap();
        let lock_path = common_dir.join("orch-worktree-init.lock");
        assert!(
            worktree_add_detached(&root, &bad_site, "definitely-not-a-revision").is_err(),
            "invalid revision unexpectedly created a worktree"
        );
        assert!(lock_path.is_file(), "failure path deleted the lock file");

        let head = rev_parse(&root, "HEAD").unwrap();
        let (result_sender, result_receiver) = std::sync::mpsc::channel();
        let worker_root = root.clone();
        let worker_site = good_site.clone();
        let worker = std::thread::spawn(move || {
            let result = worktree_add_detached(&worker_root, &worker_site, &head);
            let _ = result_sender.send(result);
        });
        result_receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("failure path left a worktree initialization lock held")
            .expect("valid worktree add after failure");
        worker.join().expect("join valid worktree add worker");
        assert!(good_site.join(".git").is_file());
        worktree_remove(&root, &good_site).unwrap();
        assert!(
            lock_path.is_file(),
            "unlock deleted the persistent lock file"
        );

        std::fs::remove_dir_all(root).unwrap();
    }
}
