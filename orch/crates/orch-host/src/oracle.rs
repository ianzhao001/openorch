//! oracle 预验命令化 + 机检级先红复跑（M2 首切片，design/09 M2 + errata E12/O5）。
//! - 预验（`orch round seed-verified`）：隔离 detached worktree 落位种子真跑测试门，
//!   O5 机器化判据 = 「种子文件内 passed ≤ 阈值(默认 0，无空洞位) ∧ 总红数==种子文件红数(基线不受扰)」。
//! - 先红复跑（收取机检阶段）：seed commit 处复跑测试门，与账本预验实测比对——
//!   伪造先红在**零模型成本**的机检层拦截（E12 兜底，不再单靠 verifier）。

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use orch_core::read_ledger;

use crate::{binding, card, gate, gitx, ledger};

/// vitest 汇总行计数（`Tests  5 failed | 33 passed (38)` / `Tests  43 passed (43)`）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SuiteCounts {
    pub failed: usize,
    pub passed: usize,
    pub total: usize,
}

/// 单文件行计数（`❯ test/x.test.ts (5 tests | 5 failed)` / `✓ test/x.test.ts (5 tests)`）
#[derive(Debug, Clone, Copy)]
pub struct FileCounts {
    pub tests: usize,
    pub failed: usize,
}

/// 将任务卡里的路径收窄为可移植的仓库相对路径。
///
/// 只接受 `/` 分隔的普通 component；绝对路径、`.`、`..`、重复分隔符和尾随
/// 分隔符一律拒绝，避免 `Path::join` 在 absolute path 上丢弃可信 base，或经
/// parent traversal 逃出仓库/worktree。
fn canonical_repo_relative(raw: &str, field: &str) -> Result<PathBuf> {
    if raw.is_empty() {
        bail!("seed {field} 不能为空");
    }
    if raw.contains('\\') {
        bail!("seed {field} 只接受可移植的 '/' 分隔符: {raw:?}");
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        bail!("seed {field} 必须是仓库相对路径，拒绝绝对路径: {raw:?}");
    }

    let mut parts = Vec::new();
    let mut normalized = PathBuf::new();
    for component in path.components() {
        let Component::Normal(part) = component else {
            bail!("seed {field} 含非规范 component（只允许普通相对路径）: {raw:?}");
        };
        let part = part
            .to_str()
            .with_context(|| format!("seed {field} 含非 UTF-8 component: {raw:?}"))?;
        parts.push(part);
        normalized.push(part);
    }
    if parts.is_empty() || parts.join("/") != raw {
        bail!("seed {field} 不是规范仓库相对路径: {raw:?}");
    }
    Ok(normalized)
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf> {
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("解析 {label} 真实路径失败: {}", path.display()))?;
    if !fs::metadata(&canonical)
        .with_context(|| format!("读取 {label} 元数据失败: {}", canonical.display()))?
        .is_dir()
    {
        bail!("{label} 不是目录: {}", canonical.display());
    }
    Ok(canonical)
}

/// 在可信 base 下逐 component 做 `symlink_metadata`，拒绝任一中间链接。
fn checked_regular_source(
    base: &Path,
    relative: &Path,
    display: &str,
) -> Result<(PathBuf, fs::Metadata)> {
    let components = relative.components().collect::<Vec<_>>();
    let mut current = base.to_path_buf();
    let mut final_metadata = None;
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(part) = component else {
            bail!("seed src 内部校验遇到非规范 component: {display:?}");
        };
        current.push(part);
        let metadata = fs::symlink_metadata(&current)
            .with_context(|| format!("读取 seed src 失败: {display} ({})", current.display()))?;
        if metadata.file_type().is_symlink() {
            bail!(
                "seed src 禁止符号链接（含父目录）: {display} ({})",
                current.display()
            );
        }
        if index + 1 < components.len() && !metadata.is_dir() {
            bail!(
                "seed src 父 component 不是目录: {display} ({})",
                current.display()
            );
        }
        final_metadata = Some(metadata);
    }

    let metadata = final_metadata.context("seed src 缺路径 component")?;
    if !metadata.is_file() {
        bail!(
            "seed src 必须是 regular file: {display} ({})",
            current.display()
        );
    }
    let resolved = fs::canonicalize(&current)
        .with_context(|| format!("解析 seed src 真实路径失败: {display}"))?;
    if !resolved.starts_with(base) {
        bail!(
            "seed src 解析后逃出仓库: {display} → {}",
            resolved.display()
        );
    }
    Ok((current, metadata))
}

#[cfg(unix)]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[cfg(not(unix))]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
        && left.created().ok() == right.created().ok()
}

fn read_checked_seed(
    base: &Path,
    relative: &Path,
    display: &str,
) -> Result<(Vec<u8>, fs::Permissions)> {
    let (path, expected) = checked_regular_source(base, relative, display)?;
    let mut file = fs::File::open(&path)
        .with_context(|| format!("打开 seed src 失败: {display} ({})", path.display()))?;
    let opened = file
        .metadata()
        .with_context(|| format!("读取已打开 seed src 元数据失败: {display}"))?;
    if !opened.is_file() || !same_file_identity(&expected, &opened) {
        bail!("seed src 在检查与打开之间发生替换: {display}");
    }

    // 打开后再走一次路径校验，缩小父目录被替换成 symlink 的 TOCTOU 窗口。
    let (_, after_open) = checked_regular_source(base, relative, display)?;
    if !same_file_identity(&opened, &after_open) {
        bail!("seed src 在打开后发生路径替换: {display}");
    }

    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("读取 seed src 内容失败: {display}"))?;
    let handle_after = file
        .metadata()
        .with_context(|| format!("复核已打开 seed src 元数据失败: {display}"))?;
    let (_, path_after) = checked_regular_source(base, relative, display)?;
    if !same_file_identity(&opened, &handle_after)
        || !same_file_identity(&handle_after, &path_after)
        || handle_after.len() != bytes.len() as u64
    {
        bail!("seed src 读取期间 bytes/inode/path 发生漂移: {display}");
    }
    Ok((bytes, opened.permissions()))
}

pub(crate) fn read_bound_seed_bytes(root: &Path, source: &str) -> Result<Vec<u8>> {
    let canonical_root = canonical_directory(root, "仓库 root")?;
    let relative = canonical_repo_relative(source, "src")?;
    read_checked_seed(&canonical_root, &relative, source).map(|(bytes, _)| bytes)
}

fn inspect_target_path(base: &Path, relative: &Path, display: &str) -> Result<()> {
    let components = relative.components().collect::<Vec<_>>();
    let mut current = base.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(part) = component else {
            bail!("seed target 内部校验遇到非规范 component: {display:?}");
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    bail!(
                        "seed target 禁止符号链接（含父目录）: {display} ({})",
                        current.display()
                    );
                }
                let is_last = index + 1 == components.len();
                if is_last && !metadata.is_file() {
                    bail!("seed target 已存在且不是 regular file: {display}");
                }
                if !is_last && !metadata.is_dir() {
                    bail!(
                        "seed target 父 component 不是目录: {display} ({})",
                        current.display()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("检查 seed target 失败: {display} ({})", current.display())
                });
            }
        }
    }
    Ok(())
}

fn create_checked_target_parent(base: &Path, relative: &Path, display: &str) -> Result<PathBuf> {
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    let mut current = base.to_path_buf();
    for component in parent.components() {
        let Component::Normal(part) = component else {
            bail!("seed target 父目录含非规范 component: {display:?}");
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    bail!(
                        "seed target 父 component 必须是真实目录: {display} ({})",
                        current.display()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("创建 seed target 父目录失败: {}", current.display())
                        });
                    }
                }
                let metadata = fs::symlink_metadata(&current).with_context(|| {
                    format!("复查 seed target 父目录失败: {}", current.display())
                })?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    bail!(
                        "seed target 新建父 component 被替换成非真实目录: {}",
                        current.display()
                    );
                }
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("检查 seed target 父目录失败: {}", current.display()))
            }
        }
    }
    let resolved = fs::canonicalize(&current)
        .with_context(|| format!("解析 seed target 父目录失败: {}", current.display()))?;
    if !resolved.starts_with(base) {
        bail!(
            "seed target 父目录解析后逃出 worktree: {display} → {}",
            resolved.display()
        );
    }
    Ok(current)
}

/// 在任何 worktree/ledger/spawn 动作前验证种子路径合同。
///
/// `src` 必须是仓库内、全链无 symlink 的 regular file；`target` 必须是规范相对路径，
/// 且属于任务 writeSet、不得命中任务 frozenPaths。重复 target 也会被拒绝，防止后一个
/// seed 静默覆盖前一个。
pub fn validate_seed_paths(root: &Path, c: &card::Card) -> Result<()> {
    let canonical_root = canonical_directory(root, "仓库 root")?;
    let mut targets = HashSet::new();
    for seed in &c.meta.seeds {
        let src = canonical_repo_relative(&seed.src, "src")?;
        let target = canonical_repo_relative(&seed.target, "target")?;
        checked_regular_source(&canonical_root, &src, &seed.src)?;
        if !card::path_matches(&c.meta.write_set, &seed.target) {
            bail!("seed target 不在任务 writeSet: {}", seed.target);
        }
        if card::path_matches(&c.meta.frozen_paths, &seed.target) {
            bail!("seed target 命中任务 frozenPaths: {}", seed.target);
        }
        if targets
            .iter()
            .any(|existing: &PathBuf| target.starts_with(existing) || existing.starts_with(&target))
        {
            bail!("任务卡 seed targets 不得重复或互为祖先: {}", seed.target);
        }
        if !targets.insert(target) {
            bail!("任务卡含重复 seed target: {}", seed.target);
        }
    }
    Ok(())
}

/// 将任务卡 seeds 安全落位到已存在的 worktree。
///
/// 所有源文件会在任何目标写入前完成稳定身份读取；所有现存目标链也会先做无 symlink
/// 预检。单文件通过同目录临时文件 + rename 落位，最终 component 即使被并发换成 symlink
/// 也只会被原子替换，不会跟随写入其指向对象。
pub fn copy_seeds_contained(root: &Path, worktree: &Path, c: &card::Card) -> Result<()> {
    validate_seed_paths(root, c)?;
    let canonical_root = canonical_directory(root, "仓库 root")?;
    let canonical_worktree = canonical_directory(worktree, "oracle worktree")?;

    let mut loaded = Vec::with_capacity(c.meta.seeds.len());
    for seed in &c.meta.seeds {
        let src = canonical_repo_relative(&seed.src, "src")?;
        let target = canonical_repo_relative(&seed.target, "target")?;
        let (bytes, permissions) = read_checked_seed(&canonical_root, &src, &seed.src)?;
        loaded.push((seed, target, bytes, permissions));
    }
    for (seed, target, _, _) in &loaded {
        inspect_target_path(&canonical_worktree, target, &seed.target)?;
    }

    for (seed, target, bytes, permissions) in loaded {
        let parent = create_checked_target_parent(&canonical_worktree, &target, &seed.target)?;
        inspect_target_path(&canonical_worktree, &target, &seed.target)?;
        let destination = canonical_worktree.join(&target);
        let temporary = parent.join(format!(
            ".{}",
            crate::util::unique_scratch_name("orch-seed-copy")
        ));
        let write_result = (|| -> Result<()> {
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .with_context(|| format!("创建 seed 临时文件失败: {}", temporary.display()))?;
            file.write_all(&bytes)
                .with_context(|| format!("写 seed 临时文件失败: {}", temporary.display()))?;
            file.sync_all()
                .with_context(|| format!("同步 seed 临时文件失败: {}", temporary.display()))?;
            drop(file);
            fs::set_permissions(&temporary, permissions)
                .with_context(|| format!("设置 seed 权限失败: {}", temporary.display()))?;

            // rename 前再次确认父链，避免检查后被静态 symlink 替换。
            create_checked_target_parent(&canonical_worktree, &target, &seed.target)?;
            inspect_target_path(&canonical_worktree, &target, &seed.target)?;
            fs::rename(&temporary, &destination)
                .with_context(|| format!("落位 seed 失败: {} → {}", seed.src, seed.target))?;
            Ok(())
        })();
        if write_result.is_err() {
            fs::remove_file(&temporary).ok();
        }
        write_result?;
    }
    Ok(())
}

pub fn parse_suite_counts(log: &str) -> Option<SuiteCounts> {
    // 取最后一个含「Tests 」的行（gate 日志=行首缩进；REPORT §3 叙事=行中缀，都认）。
    // 「Test Files」「(tests 17ms)」等干扰行不含大写开头的「Tests 」子串。
    let raw = log.lines().filter(|l| l.contains("Tests ")).next_back()?;
    let line = &raw[raw.find("Tests ")?..];
    let total = line
        .rsplit_once('(')
        .and_then(|(_, r)| r.split(')').next())
        .and_then(|n| n.trim().parse().ok())?;
    let num_before = |kw: &str| -> usize {
        line.split(kw)
            .next()
            .and_then(|before| before.split_whitespace().next_back())
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    };
    Some(SuiteCounts {
        failed: num_before("failed"),
        passed: num_before("passed"),
        total,
    })
}

pub fn parse_file_counts(log: &str, target: &str) -> Option<FileCounts> {
    // 文件计数行 = 含目标路径 + 「(N test…」（单用例是「(1 test)」单数）。
    // 必须校验括号后首 token 是数字——失败详情区的 `❯ target:15:5` 位置行会伪装成匹配（r6 实测踩坑）
    let line = log.lines().rev().find(|l| {
        l.contains(target)
            && l.contains(" test")
            && l.split_once('(')
                .and_then(|(_, r)| r.split_whitespace().next())
                .is_some_and(|tok| !tok.is_empty() && tok.chars().all(|c| c.is_ascii_digit()))
    })?;
    let inner = line.split_once('(').map(|(_, r)| r)?;
    let tests: usize = inner.split_whitespace().next()?.parse().ok()?;
    let failed = if let Some((_, after)) = inner.split_once('|') {
        after
            .split_whitespace()
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    } else {
        0
    };
    Some(FileCounts { tests, failed })
}

pub fn parse_failed_cases(log: &str) -> Vec<String> {
    log.lines()
        .filter(|l| l.trim_start().starts_with('×'))
        .map(|l| l.trim().trim_start_matches('×').trim().to_string())
        .collect()
}

/// cargo workspace 汇总计数：每个测试二进制各有一行，必须跨行求和。
/// 至少解析出一个真实数字才算计数行——叙述性文本（如「无任何 `test result:` 行」这句话
/// 本身含子串）不得伪装成计数行（E13，r8/B12 误杀实测）。
pub fn parse_cargo_suite_counts(log: &str) -> Option<SuiteCounts> {
    let mut found = false;
    let (mut failed, mut passed) = (0usize, 0usize);
    for line in log.lines().filter(|line| line.contains("test result:")) {
        let number_before = |keyword: &str| -> Option<usize> {
            line.split(keyword)
                .next()
                .and_then(|before| before.split_whitespace().next_back())
                .and_then(|number| number.parse().ok())
        };
        let f = number_before("failed");
        let p = number_before("passed");
        if f.is_none() && p.is_none() {
            continue; // 无任何真实数字：非计数行
        }
        found = true;
        failed += f.unwrap_or(0);
        passed += p.unwrap_or(0);
    }
    found.then_some(SuiteCounts {
        failed,
        passed,
        total: failed + passed,
    })
}

/// cargo 失败行形如 `test module::case ... FAILED`；保留首次出现顺序并去重。
pub fn parse_cargo_failed_cases(log: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut cases = Vec::new();
    for line in log.lines() {
        let trimmed = line.trim();
        let Some(case) = trimmed
            .strip_prefix("test ")
            .and_then(|value| value.strip_suffix(" ... FAILED"))
        else {
            continue;
        };
        if seen.insert(case.to_string()) {
            cases.push(case.to_string());
        }
    }
    cases
}

fn cargo_test_names(seed_source: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut awaiting_fn = false;
    for line in seed_source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("#[test]") {
            awaiting_fn = true;
        }
        if !awaiting_fn {
            continue;
        }
        let tokens = trimmed.split_whitespace().collect::<Vec<_>>();
        let Some(fn_index) = tokens.iter().position(|token| *token == "fn") else {
            continue;
        };
        let Some(raw_name) = tokens.get(fn_index + 1) else {
            continue;
        };
        let name = raw_name.split(['(', '<']).next().unwrap_or_default().trim();
        if !name.is_empty() {
            names.push(name.to_string());
        }
        awaiting_fn = false;
    }
    names
}

/// cargo 没有 per-file 汇总行：从种子源码提取测试函数，再与失败全名做后缀匹配。
pub fn cargo_file_counts(seed_source: &str, failed_cases: &[String]) -> FileCounts {
    let test_names = cargo_test_names(seed_source);
    let failed = failed_cases
        .iter()
        .filter(|case| {
            test_names
                .iter()
                .any(|name| *case == name.as_str() || case.ends_with(&format!("::{name}")))
        })
        .count();
    FileCounts {
        tests: test_names.len(),
        failed,
    }
}

/// 预验/复跑的实测记录（进 SeedOracleVerified.measured / RedProven.replay，机器可比对）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Measured {
    #[serde(rename = "fileFailed")]
    pub file_failed: usize,
    #[serde(rename = "filePassed")]
    pub file_passed: usize,
    #[serde(rename = "totalFailed")]
    pub total_failed: usize,
    #[serde(rename = "totalPassed")]
    pub total_passed: usize,
    pub total: usize,
    #[serde(rename = "failedCases")]
    pub failed_cases: Vec<String>,
    #[serde(rename = "redForm", skip_serializing_if = "Option::is_none")]
    pub red_form: Option<String>,
}

/// One canonical rustc diagnostic used to bind a compile-red oracle.
///
/// Deserialization is intentionally stricter than ordinary archived payload
/// parsing: v2 is a high-water mark, so malformed, unsorted, duplicated, or
/// non-canonical identities must fail closed instead of regaining legacy
/// counts-only semantics.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "RawCompileDiagnostic")]
pub struct CompileDiagnostic {
    pub code: String,
    pub keys: Vec<String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCompileDiagnostic {
    code: String,
    keys: Vec<String>,
}

impl TryFrom<RawCompileDiagnostic> for CompileDiagnostic {
    type Error = String;

    fn try_from(raw: RawCompileDiagnostic) -> Result<Self, Self::Error> {
        if !canonical_rust_code(&raw.code) {
            return Err(format!("non-canonical rustc error code: {:?}", raw.code));
        }
        if raw.keys.is_empty() {
            return Err(format!("{} diagnostic has no identity keys", raw.code));
        }
        if !strictly_sorted(&raw.keys) {
            return Err(format!(
                "{} diagnostic keys are empty, duplicated, or unsorted",
                raw.code
            ));
        }
        for key in &raw.keys {
            validate_compile_key(key)?;
        }
        Ok(Self {
            code: raw.code,
            keys: raw.keys,
        })
    }
}

/// Canonical identity for a set of coded rustc diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "RawCompileRedIdentity")]
pub struct CompileRedIdentity {
    pub dialect: String,
    pub diagnostics: Vec<CompileDiagnostic>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCompileRedIdentity {
    dialect: String,
    diagnostics: Vec<CompileDiagnostic>,
}

impl TryFrom<RawCompileRedIdentity> for CompileRedIdentity {
    type Error = String;

    fn try_from(raw: RawCompileRedIdentity) -> Result<Self, Self::Error> {
        if raw.dialect != "rustc" {
            return Err(format!(
                "compile identity dialect must be rustc, got {:?}",
                raw.dialect
            ));
        }
        if raw.diagnostics.is_empty() {
            return Err("compile identity has no coded rustc diagnostics".into());
        }
        if !strictly_sorted(&raw.diagnostics) {
            return Err("compile diagnostics are duplicated or unsorted".into());
        }
        Ok(Self {
            dialect: raw.dialect,
            diagnostics: raw.diagnostics,
        })
    }
}

/// Machine-recomputed proof that a human `--expected-red` assertion was
/// satisfied by the measured observation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "form", rename_all = "lowercase", deny_unknown_fields)]
pub enum ExpectedRedProof {
    Compile {
        #[serde(rename = "rustCodes")]
        rust_codes: Vec<String>,
        #[serde(rename = "rustKeys")]
        rust_keys: Vec<String>,
    },
    Assertion {
        #[serde(rename = "failedCount")]
        failed_count: usize,
    },
}

/// Task-scoped compile baseline strength used by collect replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompileOracleMatch {
    NoBaseline,
    LegacyCountsOnly,
    IdentityV2,
}

/// Classification of a compile-red symbol identity change between oracle
/// preverification and collect replay.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileIdentityShift {
    Unchanged,
    LegitimateShrink { dropped: Vec<String> },
    Mismatch { reason: String },
}

/// Classify a compile-red symbol-set change without consulting mutable state.
///
/// The ordering of these checks is part of the contract: an unchanged empty
/// set is unchanged, growth always wins over shrink analysis, and a replay
/// that has become empty can never be explained away by upstream work.
#[doc(hidden)]
pub fn classify_compile_identity_shift(
    baseline_symbols: &[String],
    replay_symbols: &[String],
    upstream_provided_symbols: &[String],
) -> CompileIdentityShift {
    let baseline = baseline_symbols.iter().cloned().collect::<BTreeSet<_>>();
    let replay = replay_symbols.iter().cloned().collect::<BTreeSet<_>>();
    let upstream = upstream_provided_symbols
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();

    if baseline == replay {
        return CompileIdentityShift::Unchanged;
    }

    let added = replay.difference(&baseline).cloned().collect::<Vec<_>>();
    if !added.is_empty() {
        return CompileIdentityShift::Mismatch {
            reason: format!("compile identity grew by symbols {added:?}"),
        };
    }

    if replay.is_empty() {
        return CompileIdentityShift::Mismatch {
            reason: "compile identity replay is empty".to_string(),
        };
    }

    let dropped = baseline.difference(&replay).cloned().collect::<Vec<_>>();
    let unexplained = dropped
        .iter()
        .filter(|symbol| !upstream.contains(*symbol))
        .cloned()
        .collect::<Vec<_>>();
    if unexplained.is_empty() {
        CompileIdentityShift::LegitimateShrink { dropped }
    } else {
        CompileIdentityShift::Mismatch {
            reason: format!("compile identity shrink has unexplained symbols {unexplained:?}"),
        }
    }
}

/// Additive observation wrapper. `Measured` remains source-compatible for
/// archived struct literals, while schema-v2 JSON gains `compileIdentity`
/// beside the legacy measured fields through `flatten`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OracleObservation {
    #[serde(flatten)]
    pub measured: Measured,
    #[serde(
        rename = "compileIdentity",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub compile_identity: Option<CompileRedIdentity>,
}

fn strictly_sorted<T: Ord>(values: &[T]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

fn canonical_rust_code(code: &str) -> bool {
    let bytes = code.as_bytes();
    bytes.len() == 5 && bytes[0] == b'E' && bytes[1..].iter().all(u8::is_ascii_digit)
}

fn rust_symbol(token: &str) -> bool {
    if token.is_empty() || token.contains('/') || token.contains('\\') {
        return false;
    }
    token.split("::").all(|segment| {
        let segment = segment.strip_prefix("r#").unwrap_or(segment);
        !segment.is_empty()
            && segment
                .chars()
                .next()
                .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
            && segment
                .chars()
                .all(|character| character == '_' || character.is_ascii_alphanumeric())
    })
}

fn normalize_message(message: &str) -> String {
    let collapsed = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut normalized = String::new();
    let mut number_run = false;
    for raw_word in collapsed.split(' ') {
        if !normalized.is_empty() {
            normalized.push(' ');
        }
        let path_like = raw_word.contains('/')
            || raw_word.contains('\\')
            || raw_word.to_ascii_lowercase().contains(".rs:");
        if path_like {
            normalized.push_str("<path>");
            number_run = false;
            continue;
        }
        for character in raw_word.chars() {
            if character.is_ascii_digit() {
                if !number_run {
                    normalized.push_str("<n>");
                    number_run = true;
                }
            } else {
                normalized.push(character);
                number_run = false;
            }
        }
        number_run = false;
    }
    normalized
}

fn validate_compile_key(key: &str) -> Result<(), String> {
    if let Some(symbol) = key.strip_prefix("symbol:") {
        if rust_symbol(symbol) {
            return Ok(());
        }
        return Err(format!("non-canonical symbol key: {key:?}"));
    }
    if let Some(message) = key.strip_prefix("message:") {
        if !message.is_empty() && normalize_message(message) == message {
            return Ok(());
        }
        return Err(format!("non-canonical message key: {key:?}"));
    }
    Err(format!("unknown compile identity key: {key:?}"))
}

fn strip_ansi_csi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' || chars.peek() != Some(&'[') {
            output.push(character);
            continue;
        }
        chars.next();
        for next in chars.by_ref() {
            if ('@'..='~').contains(&next) {
                break;
            }
        }
    }
    output
}

fn backtick_symbols(text: &str) -> BTreeSet<String> {
    let mut symbols = BTreeSet::new();
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find('`') else {
            break;
        };
        let token = &after_open[..close];
        if rust_symbol(token) {
            symbols.insert(format!("symbol:{token}"));
        }
        rest = &after_open[close + 1..];
    }
    symbols
}

fn parse_rustc_headline(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("error[")?;
    let close = rest.find("]:")?;
    let raw_code = &rest[..close];
    if raw_code.len() != 5
        || !matches!(raw_code.as_bytes()[0], b'E' | b'e')
        || !raw_code.as_bytes()[1..].iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    let code = format!("E{}", &raw_code[1..]);
    let headline = rest[close + 2..].trim().to_string();
    Some((code, headline))
}

/// Parse and canonicalize coded rustc error headlines.
pub fn canonical_rust_compile_identity(log: &str) -> Result<CompileRedIdentity, String> {
    let clean = strip_ansi_csi(log);
    let mut diagnostics = BTreeSet::new();
    for line in clean.lines() {
        let Some((code, headline)) = parse_rustc_headline(line) else {
            continue;
        };
        let symbols = backtick_symbols(&headline);
        let keys = if symbols.is_empty() {
            let message = normalize_message(&headline);
            if message.is_empty() {
                return Err(format!("{code} rustc diagnostic has an empty headline"));
            }
            vec![format!("message:{message}")]
        } else {
            symbols.into_iter().collect()
        };
        diagnostics.insert(CompileDiagnostic { code, keys });
    }
    if diagnostics.is_empty() {
        return Err("no coded rustc error[Edddd] diagnostics found".into());
    }
    Ok(CompileRedIdentity {
        dialect: "rustc".into(),
        diagnostics: diagnostics.into_iter().collect(),
    })
}

fn expected_compile_parts(expected: &str) -> Result<(Vec<String>, Vec<String>), String> {
    let clean = strip_ansi_csi(expected);
    let bytes = clean.as_bytes();
    let mut codes = BTreeSet::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let Some(relative) = clean[offset..].find("error[") else {
            break;
        };
        let start = offset + relative + "error[".len();
        if start + 6 <= bytes.len()
            && matches!(bytes[start], b'E' | b'e')
            && bytes[start + 1..start + 5].iter().all(u8::is_ascii_digit)
            && bytes[start + 5] == b']'
        {
            codes.insert(format!("E{}", &clean[start + 1..start + 5]));
            offset = start + 6;
        } else {
            offset = start;
        }
    }
    if codes.is_empty() {
        return Err("--expected-red compile assertion must contain error[Edddd]".into());
    }
    Ok((
        codes.into_iter().collect(),
        backtick_symbols(&clean).into_iter().collect(),
    ))
}

/// Validate a compile expectation against the complete canonical identity.
pub fn validate_expected_compile_red(
    expected: &str,
    actual: &CompileRedIdentity,
) -> Result<ExpectedRedProof, String> {
    let (rust_codes, rust_keys) = expected_compile_parts(expected)?;
    let actual_codes = actual
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.code.as_str())
        .collect::<BTreeSet<_>>();
    let actual_keys = actual
        .diagnostics
        .iter()
        .flat_map(|diagnostic| diagnostic.keys.iter().map(String::as_str))
        .collect::<BTreeSet<_>>();
    for code in &rust_codes {
        if !actual_codes.contains(code.as_str()) {
            return Err(format!(
                "expected rustc code {code} is absent from measured compile identity"
            ));
        }
    }
    for key in &rust_keys {
        if !actual_keys.contains(key.as_str()) {
            return Err(format!(
                "expected rustc key {key} is absent from measured compile identity"
            ));
        }
    }
    Ok(ExpectedRedProof::Compile {
        rust_codes,
        rust_keys,
    })
}

fn expected_assertion_count(expected: &str) -> Result<usize, String> {
    let runs = expected
        .split(|character: char| !character.is_ascii_digit())
        .filter(|run| !run.is_empty())
        .collect::<Vec<_>>();
    if runs.len() != 1 {
        return Err(
            "--expected-red assertion must contain exactly one unambiguous positive integer".into(),
        );
    }
    let count = runs[0]
        .parse::<usize>()
        .map_err(|error| format!("invalid assertion failed-count: {error}"))?;
    if count == 0 {
        return Err("--expected-red assertion failed-count must be positive".into());
    }
    Ok(count)
}

fn validate_expected_assertion_red(
    expected: &str,
    measured: &Measured,
) -> Result<ExpectedRedProof, String> {
    let failed_count = expected_assertion_count(expected)?;
    if measured.red_form.as_deref() == Some("compile") {
        return Err("card redForm assertion cannot accept a compile-red observation".into());
    }
    if failed_count != measured.file_failed {
        return Err(format!(
            "expected assertion failed-count {failed_count} != measured fileFailed {}",
            measured.file_failed
        ));
    }
    Ok(ExpectedRedProof::Assertion { failed_count })
}

pub(crate) fn validate_expected_red_syntax(
    red_form: Option<&str>,
    dialect: &str,
    expected: &str,
    record_only: bool,
) -> Result<(), String> {
    if record_only {
        return Err(
            "schema-v2 SeedOracleVerified rejects --record-only because no measured proof exists"
                .into(),
        );
    }
    match red_form {
        Some("compile") => {
            if dialect != "cargo" {
                return Err(format!(
                    "card redForm compile requires cargo oracle dialect, got {dialect:?}"
                ));
            }
            expected_compile_parts(expected)?;
            Ok(())
        }
        Some("assertion") => {
            expected_assertion_count(expected)?;
            Ok(())
        }
        Some(other) => Err(format!(
            "card redForm must be the closed compile|assertion enum, got {other:?}"
        )),
        None => Err("card redForm is required for schema-v2 SeedOracleVerified".into()),
    }
}

pub(crate) fn prove_expected_red(
    red_form: &str,
    expected: &str,
    observation: &OracleObservation,
) -> Result<ExpectedRedProof, String> {
    match red_form {
        "compile" => {
            if observation.measured.red_form.as_deref() != Some("compile") {
                return Err(format!(
                    "card redForm compile != measured redForm {:?}",
                    observation.measured.red_form
                ));
            }
            let identity = observation
                .compile_identity
                .as_ref()
                .ok_or_else(|| "compile-red observation lacks compileIdentity".to_string())?;
            validate_expected_compile_red(expected, identity)
        }
        "assertion" => {
            if observation.measured.red_form.as_deref() != Some("assertion") {
                return Err(format!(
                    "card redForm assertion != measured redForm {:?}",
                    observation.measured.red_form
                ));
            }
            if observation.compile_identity.is_some() {
                return Err(
                    "assertion-red observation unexpectedly contains compileIdentity".into(),
                );
            }
            validate_expected_assertion_red(expected, &observation.measured)
        }
        other => Err(format!("unknown card redForm {other:?}")),
    }
}

fn v2_compile_identity(payload: &serde_json::Value) -> Result<CompileRedIdentity, String> {
    let expected = payload
        .get("expectedRed")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "schema-v2 oracle payload lacks string expectedRed".to_string())?;
    let measured = payload
        .get("measured")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "schema-v2 oracle payload lacks measured object".to_string())?;
    if measured.get("redForm").and_then(serde_json::Value::as_str) != Some("compile") {
        return Err("schema-v2 compile baseline measured.redForm is not compile".into());
    }
    let identity_value = measured
        .get("compileIdentity")
        .cloned()
        .ok_or_else(|| "schema-v2 compile baseline lacks compileIdentity".to_string())?;
    let identity = serde_json::from_value::<CompileRedIdentity>(identity_value)
        .map_err(|error| format!("malformed schema-v2 compileIdentity: {error}"))?;
    let proof_value = payload
        .get("expectedRedProof")
        .cloned()
        .ok_or_else(|| "schema-v2 oracle payload lacks expectedRedProof".to_string())?;
    let proof = serde_json::from_value::<ExpectedRedProof>(proof_value)
        .map_err(|error| format!("malformed schema-v2 expectedRedProof: {error}"))?;
    let recomputed = validate_expected_compile_red(expected, &identity)?;
    if proof != recomputed {
        return Err(format!(
            "schema-v2 expectedRedProof is not self-consistent: stored={proof:?} recomputed={recomputed:?}"
        ));
    }
    Ok(identity)
}

fn compile_oracle_baseline_state(
    task_payloads: &[serde_json::Value],
) -> Result<(CompileOracleMatch, Option<CompileRedIdentity>), String> {
    let mut crossed_v2 = false;
    let mut latest_v2 = None;
    for payload in task_payloads {
        match payload.get("oracleSchemaVersion") {
            None => {
                if crossed_v2 {
                    return Err(
                        "oracle schema downgrade: versionless payload follows schema v2".into(),
                    );
                }
            }
            Some(version) => {
                let version = version.as_u64().ok_or_else(|| {
                    "oracleSchemaVersion must be an integer schema version".to_string()
                })?;
                if version != 2 {
                    return Err(format!("unknown oracleSchemaVersion {version}"));
                }
                crossed_v2 = true;
                latest_v2 = Some(v2_compile_identity(payload)?);
            }
        }
    }

    if crossed_v2 {
        let baseline = latest_v2
            .ok_or_else(|| "schema-v2 high-water has no valid compile baseline".to_string())?;
        return Ok((CompileOracleMatch::IdentityV2, Some(baseline)));
    }

    let Some(latest) = task_payloads.last() else {
        return Ok((CompileOracleMatch::NoBaseline, None));
    };
    let legacy_measured = latest
        .get("measured")
        .and_then(serde_json::Value::as_object)
        .is_some();
    if legacy_measured {
        Ok((CompileOracleMatch::LegacyCountsOnly, None))
    } else {
        Ok((CompileOracleMatch::NoBaseline, None))
    }
}

/// Compare a replay identity with all task-scoped SeedOracleVerified payloads.
///
/// Legacy tasks retain an explicit degraded counts-only state. Once any v2
/// payload appears, every later event must remain a valid v2 payload and the
/// latest canonical identity must exactly equal replay.
pub fn compile_oracle_baseline_match(
    task_payloads: &[serde_json::Value],
    replay: &CompileRedIdentity,
) -> Result<CompileOracleMatch, String> {
    let (matched, baseline) = compile_oracle_baseline_state(task_payloads)?;
    if let Some(baseline) = baseline {
        if &baseline != replay {
            return Err(format!(
                "compile identity mismatch: baseline={baseline:?} replay={replay:?}"
            ));
        }
    }
    Ok(matched)
}

#[derive(Debug)]
struct LegitimateShrinkEvidence {
    baseline_symbols: Vec<String>,
    replay_symbols: Vec<String>,
    dropped: Vec<String>,
    upstream_tasks: Vec<String>,
}

#[derive(Debug)]
struct CompileReplayDecision {
    matched: CompileOracleMatch,
    legitimate_shrink: Option<LegitimateShrinkEvidence>,
}

fn compile_identity_symbols(identity: &CompileRedIdentity) -> Vec<String> {
    identity
        .diagnostics
        .iter()
        .flat_map(|diagnostic| diagnostic.keys.iter())
        .filter(|key| key.starts_with("symbol:"))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn identity_after_dropping_symbols(
    baseline: &CompileRedIdentity,
    dropped: &[String],
) -> CompileRedIdentity {
    let dropped = dropped.iter().collect::<BTreeSet<_>>();
    let diagnostics = baseline
        .diagnostics
        .iter()
        .filter_map(|diagnostic| {
            let keys = diagnostic
                .keys
                .iter()
                .filter(|key| !dropped.contains(key))
                .cloned()
                .collect::<Vec<_>>();
            (!keys.is_empty()).then(|| CompileDiagnostic {
                code: diagnostic.code.clone(),
                keys,
            })
        })
        .collect();
    CompileRedIdentity {
        dialect: baseline.dialect.clone(),
        diagnostics,
    }
}

fn compile_oracle_replay_match(
    task_payloads: &[serde_json::Value],
    effective_rebaseline: Option<&CompileRedIdentity>,
    replay: &CompileRedIdentity,
    upstream_provided_symbols: &[String],
) -> Result<CompileReplayDecision, String> {
    let (matched, seed_baseline) = compile_oracle_baseline_state(task_payloads)?;
    let Some(seed_baseline) = seed_baseline else {
        return Ok(CompileReplayDecision {
            matched,
            legitimate_shrink: None,
        });
    };
    let baseline = effective_rebaseline.unwrap_or(&seed_baseline);
    if baseline == replay {
        return Ok(CompileReplayDecision {
            matched,
            legitimate_shrink: None,
        });
    }

    let baseline_symbols = compile_identity_symbols(baseline);
    let replay_symbols = compile_identity_symbols(replay);
    if let CompileIdentityShift::LegitimateShrink { dropped } = classify_compile_identity_shift(
        &baseline_symbols,
        &replay_symbols,
        upstream_provided_symbols,
    ) {
        // The symbol classifier is intentionally small and pure. The replay
        // seam additionally proves that removing exactly those symbols from
        // the complete coded identity yields replay, so code/message changes
        // cannot hide behind a symbol-only shrink.
        if identity_after_dropping_symbols(baseline, &dropped) == *replay {
            return Ok(CompileReplayDecision {
                matched,
                legitimate_shrink: Some(LegitimateShrinkEvidence {
                    baseline_symbols,
                    replay_symbols,
                    dropped,
                    upstream_tasks: Vec::new(),
                }),
            });
        }
    }

    // Preserve the existing hard-failure diagnostic verbatim for every
    // change that is not completely explained by recorded upstream work.
    Err(format!(
        "compile identity mismatch: baseline={baseline:?} replay={replay:?}"
    ))
}

fn canonical_string_array(payload: &serde_json::Value, key: &str) -> Result<Vec<String>, String> {
    let values = payload
        .get(key)
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| format!("legitimate-shrink evidence lacks array {key}"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("legitimate-shrink evidence {key} contains non-string"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if !strictly_sorted(&values) {
        return Err(format!(
            "legitimate-shrink evidence {key} is empty, duplicated, or unsorted"
        ));
    }
    Ok(values)
}

const LEGITIMATE_SHRINK_REASON: &str = "compile identity legitimate upstream shrink";

fn legitimate_shrink_rebaseline(
    payload: &serde_json::Value,
) -> Result<Option<CompileRedIdentity>, String> {
    if payload.get("stage").and_then(serde_json::Value::as_str) != Some("red-replay")
        || payload.get("reason").and_then(serde_json::Value::as_str)
            != Some(LEGITIMATE_SHRINK_REASON)
    {
        return Ok(None);
    }
    let baseline_symbols = canonical_string_array(payload, "baselineSymbols")?;
    let replay_symbols = canonical_string_array(payload, "replaySymbols")?;
    let dropped = canonical_string_array(payload, "dropped")?;
    let upstream_tasks = canonical_string_array(payload, "upstreamTasks")?;
    if baseline_symbols.is_empty()
        || replay_symbols.is_empty()
        || dropped.is_empty()
        || upstream_tasks.is_empty()
        || baseline_symbols
            .iter()
            .chain(replay_symbols.iter())
            .chain(dropped.iter())
            .any(|symbol| !symbol.starts_with("symbol:") || validate_compile_key(symbol).is_err())
        || upstream_tasks
            .iter()
            .any(|task_id| card::validate_task_id(task_id).is_err())
    {
        return Err("malformed legitimate-shrink evidence".to_string());
    }
    match classify_compile_identity_shift(&baseline_symbols, &replay_symbols, &dropped) {
        CompileIdentityShift::LegitimateShrink {
            dropped: recomputed,
        } if recomputed == dropped => {}
        other => {
            return Err(format!(
                "legitimate-shrink evidence is inconsistent: {other:?}"
            ));
        }
    }
    let identity_value = payload
        .get("rebaselineCompileIdentity")
        .cloned()
        .ok_or_else(|| {
            "legitimate-shrink escalation lacks rebaselineCompileIdentity".to_string()
        })?;
    let identity = serde_json::from_value::<CompileRedIdentity>(identity_value)
        .map_err(|error| format!("malformed rebaseline compileIdentity: {error}"))?;
    if compile_identity_symbols(&identity) != replay_symbols {
        return Err(
            "legitimate-shrink escalation replaySymbols != rebaselineCompileIdentity".to_string(),
        );
    }
    Ok(Some(identity))
}

/// Return the latest durable legitimate-shrink identity after the latest v2
/// SeedOracleVerified event. A later explicit preverification supersedes an
/// automatic rebaseline; malformed rebaseline evidence fails closed.
fn latest_compile_rebaseline(
    events: &[orch_core::EventRecord],
    round: &str,
    task_id: &str,
) -> Result<Option<CompileRedIdentity>, String> {
    let mut v2_seen = false;
    let mut latest = None;
    for event in events.iter().filter(|event| {
        event.task_id.as_deref() == Some(task_id) && event.round.as_deref() == Some(round)
    }) {
        if event.kind == "SeedOracleVerified" {
            if event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("oracleSchemaVersion"))
                .and_then(serde_json::Value::as_u64)
                == Some(2)
            {
                v2_seen = true;
                latest = None;
            }
            continue;
        }
        if event.kind != "EscalationRaised" {
            continue;
        }
        let Some(payload) = event.payload.as_ref() else {
            continue;
        };
        let Some(identity) = legitimate_shrink_rebaseline(payload)? else {
            continue;
        };
        if event.actor != "runtime:orch" {
            return Err("legitimate-shrink escalation has non-canonical actor".to_string());
        }
        if !v2_seen {
            return Err(
                "legitimate-shrink escalation precedes schema-v2 SeedOracleVerified".to_string(),
            );
        }
        latest = Some(identity);
    }
    Ok(latest)
}

fn public_item_name(line: &str) -> Option<String> {
    // rustfmt keeps module-scope declarations at column zero. Restricting the
    // evidence extractor to that canonical form avoids mistaking public impl
    // methods or declarations inside inline modules for crate-module exports.
    let rest = line.strip_prefix("pub ")?;
    let tokens = rest
        .split(|character: char| {
            !(character == '_' || character == '#' || character.is_ascii_alphanumeric())
        })
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    let first = *tokens.first()?;
    let name = match first {
        "fn" | "struct" | "enum" | "type" | "trait" | "union" | "mod" | "static" => {
            *tokens.get(1)?
        }
        "const" if tokens.get(1) == Some(&"fn") => *tokens.get(2)?,
        "const" => *tokens.get(1)?,
        "async" | "unsafe" | "extern" => {
            let fn_index = tokens.iter().position(|token| *token == "fn")?;
            *tokens.get(fn_index + 1)?
        }
        // Re-export syntax may span lines or rename imports. Unsupported
        // shapes deliberately contribute no evidence (fail closed).
        _ => return None,
    };
    rust_symbol(name).then(|| name.to_string())
}

fn public_module_prefix(path: &str) -> Option<String> {
    let parts = path.split('/').collect::<Vec<_>>();
    if parts.len() < 5 || parts[..2] != ["orch", "crates"] || parts[3] != "src" {
        return None;
    }
    let crate_name = parts[2].replace('-', "_");
    let source = &parts[4..];
    let filename = *source.last()?;
    if !filename.ends_with(".rs") || filename == "main.rs" || source.first() == Some(&"bin") {
        return None;
    }
    let mut modules = source[..source.len() - 1].to_vec();
    match filename {
        "lib.rs" if modules.is_empty() => {}
        "mod.rs" => {}
        _ => modules.push(filename.strip_suffix(".rs")?),
    }
    let mut prefix = crate_name;
    for module in modules {
        if !rust_symbol(module) {
            return None;
        }
        prefix.push_str("::");
        prefix.push_str(module);
    }
    Some(prefix)
}

fn public_symbols_in_source(path: &str, source: &str) -> BTreeSet<String> {
    let Some(prefix) = public_module_prefix(path) else {
        return BTreeSet::new();
    };
    source
        .lines()
        .filter_map(public_item_name)
        .map(|name| format!("symbol:{prefix}::{name}"))
        .collect()
}

fn full_git_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn introduced_public_symbols(
    root: &Path,
    merge_sha: &str,
    write_set: &[String],
) -> Option<BTreeSet<String>> {
    if !full_git_sha(merge_sha) || !gitx::is_ancestor(root, merge_sha, "main").ok()? {
        return None;
    }
    let first_parent = gitx::rev_parse(root, &format!("{merge_sha}^1")).ok()?;
    // A recorded ordinary delivery is a no-ff merge. Requiring a second
    // parent prevents a forged single-parent commit from defining evidence.
    gitx::rev_parse(root, &format!("{merge_sha}^2")).ok()?;
    let changed = gitx::diff_names(root, &first_parent, merge_sha)
        .ok()?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let mut before_symbols = BTreeSet::new();
    let mut after_symbols = BTreeSet::new();
    for path in write_set
        .iter()
        .filter(|path| changed.contains(path.as_str()))
        .filter(|path| public_module_prefix(path).is_some())
    {
        let prefix = public_module_prefix(path)?;
        // Only a successful tree lookup with no exact entry proves a new
        // source file.  Every other object/read failure still abstains rather
        // than turning unavailable evidence into forged "new" symbols.
        let existed_before = gitx::tree_path_exists(root, &first_parent, path).ok()?;
        let before = if existed_before {
            String::from_utf8(gitx::show_bytes(root, &first_parent, path).ok()?).ok()?
        } else {
            String::new()
        };
        let after = String::from_utf8(gitx::show_bytes(root, merge_sha, path).ok()?).ok()?;
        before_symbols.extend(public_symbols_in_source(path, &before));
        after_symbols.extend(public_symbols_in_source(path, &after));
        if !existed_before {
            // A newly added source file introduces its module identity as
            // well as its public items.  rustc can diagnose the former as
            // `symbol:<crate>::<module>` without naming an item.
            after_symbols.insert(format!("symbol:{prefix}"));
        }
    }
    Some(after_symbols.difference(&before_symbols).cloned().collect())
}

/// Test-facing wrapper around the fail-closed upstream symbol extractor.
#[doc(hidden)]
pub fn introduced_public_symbols_probe(
    root: &Path,
    merge_sha: &str,
    write_set: &[String],
) -> Option<BTreeSet<String>> {
    introduced_public_symbols(root, merge_sha, write_set)
}

#[derive(Default)]
struct UpstreamSymbolEvidence {
    by_task: BTreeMap<String, BTreeSet<String>>,
}

impl UpstreamSymbolEvidence {
    fn symbols(&self) -> Vec<String> {
        self.by_task
            .values()
            .flat_map(|symbols| symbols.iter().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn tasks_explaining(&self, dropped: &[String]) -> Vec<String> {
        let dropped = dropped.iter().collect::<BTreeSet<_>>();
        self.by_task
            .iter()
            .filter(|(_, symbols)| symbols.iter().any(|symbol| dropped.contains(symbol)))
            .map(|(task, _)| task.clone())
            .collect()
    }
}

fn recorded_upstream_public_symbols(
    root: &Path,
    round: &str,
    c: &card::Card,
    events: &[orch_core::EventRecord],
    ledger_clean: bool,
) -> UpstreamSymbolEvidence {
    let collected = (|| -> Option<UpstreamSymbolEvidence> {
        if !ledger_clean {
            return None;
        }
        let mut by_task = BTreeMap::new();
        for upstream_task in &c.meta.depends_on {
            let recorded = events
                .iter()
                .enumerate()
                .filter(|(_, event)| {
                    event.kind == "TaskRecorded"
                        && event.task_id.as_deref() == Some(upstream_task)
                        && event.round.as_deref() == Some(round)
                        && event.actor == "runtime:orch"
                        && event
                            .payload
                            .as_ref()
                            .and_then(|payload| payload.get("postMergeGates"))
                            .and_then(serde_json::Value::as_str)
                            == Some("all-green")
                })
                .collect::<Vec<_>>();
            if recorded.len() != 1 {
                return None;
            }
            let recorded_index = recorded[0].0;
            let merged = events
                .iter()
                .enumerate()
                .filter(|(index, event)| {
                    *index < recorded_index
                        && event.kind == "MergeExecuted"
                        && event.task_id.as_deref() == Some(upstream_task)
                        && event.round.as_deref() == Some(round)
                        && event.actor == "reviewer:orch-runtime"
                        && event
                            .payload
                            .as_ref()
                            .and_then(|payload| payload.get("policy"))
                            .and_then(serde_json::Value::as_str)
                            == Some("no-ff")
                })
                .collect::<Vec<_>>();
            if merged.len() != 1 {
                return None;
            }
            let merge_sha = merged[0].1.payload.as_ref()?.get("mergeSha")?.as_str()?;
            let upstream_card = card::load(root, round, upstream_task).ok()?;
            let symbols =
                introduced_public_symbols(root, merge_sha, &upstream_card.meta.write_set)?;
            by_task.insert(upstream_task.clone(), symbols);
        }
        Some(UpstreamSymbolEvidence { by_task })
    })();
    collected.unwrap_or_default()
}

fn v2_assertion_payload(payload: &serde_json::Value) -> Result<(), String> {
    let expected = payload
        .get("expectedRed")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "schema-v2 assertion payload lacks string expectedRed".to_string())?;
    let measured_value = payload
        .get("measured")
        .cloned()
        .ok_or_else(|| "schema-v2 assertion payload lacks measured".to_string())?;
    if measured_value.get("compileIdentity").is_some() {
        return Err("schema-v2 assertion payload must not contain compileIdentity".into());
    }
    let measured = serde_json::from_value::<Measured>(measured_value)
        .map_err(|error| format!("malformed schema-v2 assertion measured payload: {error}"))?;
    if measured.red_form.as_deref() != Some("assertion") {
        return Err("schema-v2 assertion measured.redForm is not assertion".into());
    }
    let proof_value = payload
        .get("expectedRedProof")
        .cloned()
        .ok_or_else(|| "schema-v2 assertion payload lacks expectedRedProof".to_string())?;
    let proof = serde_json::from_value::<ExpectedRedProof>(proof_value)
        .map_err(|error| format!("malformed schema-v2 assertion expectedRedProof: {error}"))?;
    let recomputed = validate_expected_assertion_red(expected, &measured)?;
    if proof != recomputed {
        return Err(format!(
            "schema-v2 assertion expectedRedProof is not self-consistent: stored={proof:?} recomputed={recomputed:?}"
        ));
    }
    Ok(())
}

fn assertion_oracle_baseline_match(
    task_payloads: &[serde_json::Value],
) -> Result<CompileOracleMatch, String> {
    let mut crossed_v2 = false;
    for payload in task_payloads {
        match payload.get("oracleSchemaVersion") {
            None => {
                if crossed_v2 {
                    return Err(
                        "oracle schema downgrade: versionless payload follows schema v2".into(),
                    );
                }
            }
            Some(version) => {
                let version = version.as_u64().ok_or_else(|| {
                    "oracleSchemaVersion must be an integer schema version".to_string()
                })?;
                if version != 2 {
                    return Err(format!("unknown oracleSchemaVersion {version}"));
                }
                crossed_v2 = true;
                v2_assertion_payload(payload)?;
            }
        }
    }
    if crossed_v2 {
        return Ok(CompileOracleMatch::IdentityV2);
    }
    if task_payloads
        .last()
        .and_then(|payload| payload.get("measured"))
        .and_then(serde_json::Value::as_object)
        .is_some()
    {
        Ok(CompileOracleMatch::LegacyCountsOnly)
    } else {
        Ok(CompileOracleMatch::NoBaseline)
    }
}

/// 核验 REPORT §3 的断言红计数：声称值必须与种子文件自身的实测计数一致。
///
/// 全工作区聚合计数仅用于诊断与基线复现，不能替代文件内计数；编译红没有可比的
/// `test result:` 数字，因而直接豁免数字比较。
pub fn report_claim_ok(claim: &SuiteCounts, measured: &Measured) -> Result<(), String> {
    if measured.red_form.as_deref() == Some("compile") {
        return Ok(());
    }
    if claim.failed == measured.file_failed && claim.passed == measured.file_passed {
        Ok(())
    } else {
        Err(format!(
            "REPORT §3 声称 {}f/{}p ≠ seed commit 文件内实测 {}f/{}p（报数造假，E12 机拦）",
            claim.failed, claim.passed, measured.file_failed, measured.file_passed
        ))
    }
}

/// 先红复跑命令卡驱动化（B32）：取卡 gates.fast **首个**门名（卡内顺序即优先级）。
/// 空列表 → Err（文案含 `seeded` 字样——seeded 任务必须至少一个快门，不许静默回退默认名）。
pub fn red_replay_gate(fast: &[String]) -> Result<String, String> {
    fast.first()
        .cloned()
        .ok_or_else(|| "seeded 任务卡 gates.fast 为空——至少需要一个快门用于先红复跑".to_string())
}

fn first_coded_rustc_error(log: &str) -> Option<String> {
    let line = strip_ansi_csi(log)
        .lines()
        .find(|line| parse_rustc_headline(line).is_some())?
        .to_string();
    Some(line.chars().take(120).collect())
}

fn measure_cargo_observation(
    log: &str,
    exit_code: i32,
    seed_sources: &[String],
) -> Result<OracleObservation> {
    let failed_cases = parse_cargo_failed_cases(log);
    let file_counts = seed_sources
        .iter()
        .map(|source| cargo_file_counts(source, &failed_cases))
        .collect::<Vec<_>>();
    let file_tests = file_counts.iter().map(|counts| counts.tests).sum::<usize>();

    if parse_cargo_suite_counts(log).is_none() && exit_code != 0 {
        let compile_identity = canonical_rust_compile_identity(log)
            .map_err(anyhow::Error::msg)
            .context("cargo compile-red 缺 canonical coded rustc identity")?;
        let error =
            first_coded_rustc_error(log).context("cargo 编译红缺首条 coded rustc error 行")?;
        return Ok(OracleObservation {
            measured: Measured {
                file_failed: file_tests,
                file_passed: 0,
                total_failed: file_tests,
                total_passed: 0,
                total: file_tests,
                failed_cases: vec![format!("<compile-red>: {error}")],
                red_form: Some("compile".into()),
            },
            compile_identity: Some(compile_identity),
        });
    }

    let suite =
        parse_cargo_suite_counts(log).context("无法从门日志解析 cargo test result 汇总行")?;
    let file_failed = file_counts
        .iter()
        .map(|counts| counts.failed)
        .sum::<usize>();
    if file_failed > file_tests {
        bail!("cargo 种子失败数 {file_failed} > 静态提取测试数 {file_tests}");
    }
    Ok(OracleObservation {
        measured: Measured {
            file_failed,
            file_passed: file_tests - file_failed,
            total_failed: suite.failed,
            total_passed: suite.passed,
            total: suite.total,
            failed_cases,
            red_form: (exit_code != 0 && suite.failed > 0).then(|| "assertion".into()),
        },
        compile_identity: None,
    })
}

#[cfg(test)]
fn measure_cargo(log: &str, exit_code: i32, seed_sources: &[String]) -> Result<Measured> {
    measure_cargo_observation(log, exit_code, seed_sources).map(|observation| observation.measured)
}

fn parse_test_gate_observation(
    b: &binding::Binding,
    workdir: &Path,
    seeds: &[card::SeedSpec],
    g: gate::GateResult,
) -> Result<OracleObservation> {
    let log = fs::read_to_string(&g.log_path)?;
    match b.oracle.dialect.as_str() {
        "cargo" => {
            let seed_sources = seeds
                .iter()
                .map(|seed| {
                    fs::read_to_string(workdir.join(&seed.target)).with_context(|| {
                        format!("读取 cargo 种子源码失败（{}）: {}", seed.target, g.log_path)
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            measure_cargo_observation(&log, g.exit_code, &seed_sources)
                .with_context(|| format!("无法解析 cargo 门日志: {}", g.log_path))
        }
        "vitest" => {
            let suite = parse_suite_counts(&log)
                .with_context(|| format!("无法从门日志解析 vitest 汇总行: {}", g.log_path))?;
            let (mut ft, mut ff) = (0usize, 0usize);
            for s in seeds {
                let fc = parse_file_counts(&log, &s.target).with_context(|| {
                    format!("门日志无种子文件行（{}）: {}", s.target, g.log_path)
                })?;
                ft += fc.tests;
                ff += fc.failed;
            }
            Ok(OracleObservation {
                measured: Measured {
                    file_failed: ff,
                    file_passed: ft - ff,
                    total_failed: suite.failed,
                    total_passed: suite.passed,
                    total: suite.total,
                    failed_cases: parse_failed_cases(&log),
                    // Historical vitest observations omitted redForm. The
                    // schema-v2 round boundary adds the card-declared form
                    // without changing legacy preverify JSON.
                    red_form: None,
                },
                compile_identity: None,
            })
        }
        dialect => bail!("不支持的 oracle dialect: {dialect}"),
    }
}

fn run_test_gate_and_parse_observation_guarded(
    root: &Path,
    round: &str,
    task: &str,
    b: &binding::Binding,
    workdir: &Path,
    tag: &str,
    seeds: &[card::SeedSpec],
    gate_ref: &str,
) -> Result<OracleObservation> {
    let spec = b
        .commands
        .get(gate_ref)
        .with_context(|| format!("绑定缺命令: {gate_ref}（卡 gates.fast[0] 解析）"))?;
    let log_dir = root.join("coordination/runtime/logs");
    // Expected red is a normal non-zero exit; only admission/spawn/timeout is an execution error.
    let g = gate::run_gate_with_audit_identity(
        root,
        round,
        crate::ledger::GateAuditIdentity::PreAttempt { task_id: task },
        gate_ref,
        spec,
        workdir,
        &log_dir,
        tag,
    )?;
    parse_test_gate_observation(b, workdir, seeds, g)
}

#[allow(clippy::too_many_arguments)]
fn run_test_gate_and_parse_observation_with_permit(
    root: &Path,
    round: &str,
    identity: crate::ledger::GateAuditIdentity<'_>,
    permit: &crate::storage::StoragePermit,
    b: &binding::Binding,
    workdir: &Path,
    tag: &str,
    seeds: &[card::SeedSpec],
    gate_ref: &str,
) -> Result<OracleObservation> {
    let spec = b
        .commands
        .get(gate_ref)
        .with_context(|| format!("绑定缺命令: {gate_ref}（卡 gates.fast[0] 解析）"))?;
    let log_dir = root.join("coordination/runtime/logs");
    crate::storage::refresh_gate_permit(
        root,
        round,
        identity,
        permit,
        &[
            workdir.to_path_buf(),
            log_dir.clone(),
            root.join("orch/target"),
        ],
    )?;
    let g = gate::run_gate_with_permit_and_identity(
        permit, identity, gate_ref, spec, workdir, &log_dir, tag,
    )?;
    parse_test_gate_observation(b, workdir, seeds, g)
}

/// Oracle preverification with the schema-v2 observation retained. The public
/// `preverify` compatibility entry point below projects `Measured` from this
/// single gate run.
pub(crate) fn preverify_observation(
    root: &Path,
    c: &card::Card,
    allow_file_passed: usize,
) -> Result<OracleObservation> {
    // 必须先于 binding/git/worktree 等任何外部动作；恶意 seed path 不得制造半成品现场。
    validate_seed_paths(root, c)?;
    let round = crate::current_round(root)?;
    if c.meta
        .round
        .as_deref()
        .is_some_and(|declared| declared != round)
    {
        bail!("oracle preverify card round does not match CURRENT-ROUND");
    }
    let b = binding::load(root)?;
    let base = gitx::rev_parse(root, "main")?;
    let wt = root
        .join(".worktrees")
        .join(format!("_oracle-{}", c.meta.task_id));
    if wt.exists() {
        gitx::worktree_remove(root, &wt).ok(); // 幂等：上次预验残留直接清
    }
    gitx::worktree_add_detached(root, &wt, &base)?;

    // 主体闭包：无论成败最后都清理临时 worktree
    let body = || -> Result<OracleObservation> {
        let nm = root.join("node_modules");
        if (b.oracle.dialect == "vitest" || b.has_ecosystem("node"))
            && nm.exists()
            && !wt.join("node_modules").exists()
        {
            #[cfg(unix)]
            std::os::unix::fs::symlink(&nm, wt.join("node_modules")).ok();
        }
        copy_seeds_contained(root, &wt, c)?;
        let gate_ref = red_replay_gate(&c.meta.gates.fast).map_err(|e| anyhow::anyhow!("{e}"))?;
        run_test_gate_and_parse_observation_guarded(
            root,
            &round,
            &c.meta.task_id,
            &b,
            &wt,
            &format!("_oracle-{}", c.meta.task_id),
            &c.meta.seeds,
            &gate_ref,
        )
    };
    let result = body();
    gitx::worktree_remove(root, &wt).ok();
    let observation = result?;
    let m = &observation.measured;

    // O5 机器化判据
    if m.file_failed == 0 {
        bail!("oracle 预验失败：种子文件内 0 红——种子无检出力（或产品已在库，对照 O5 r4 教训）");
    }
    if m.file_passed > allow_file_passed {
        bail!(
            "oracle 预验失败：种子文件内 {} passed > 允许 {}（空洞通过位，O5）——逐用例核对：{:?}",
            m.file_passed,
            allow_file_passed,
            m.failed_cases
        );
    }
    // 编译红时 workspace 尚未运行；构造值令“总红 == 文件红”恒成立，沿用同一 O5 判据。
    if m.total_failed != m.file_failed {
        bail!(
            "oracle 预验失败：总红 {} ≠ 种子文件红 {}——种子殃及既有用例（基线受扰）",
            m.total_failed,
            m.file_failed
        );
    }
    Ok(observation)
}

/// Legacy measured-only API. This deliberately does not rerun the gate:
/// callers receive a projection of the complete observation.
pub fn preverify(root: &Path, c: &card::Card, allow_file_passed: usize) -> Result<Measured> {
    preverify_observation(root, c, allow_file_passed).map(|observation| observation.measured)
}

/// 从 REPORT 提取 §3（种子/先红）段落文本（`## 3` 起到下一个 `## ` 止）
fn extract_report_section3(report: &str) -> Option<String> {
    let mut in_s3 = false;
    let mut out = String::new();
    for line in report.lines() {
        if line.trim_start().starts_with("## ") {
            if in_s3 {
                break;
            }
            // 标题风格容忍（r16/B28 实证：执行者写 `## §3` 导致段落定位落空、误亮叙事失职黄旗）
            let t = line
                .trim_start()
                .trim_start_matches("## ")
                .trim_start_matches('§');
            in_s3 = t.starts_with('3');
            continue;
        }
        if in_s3 {
            out.push_str(line);
            out.push('\n');
        }
    }
    (!out.is_empty()).then_some(out)
}

fn parse_suite_counts_for_dialect(dialect: &str, log: &str) -> Option<SuiteCounts> {
    match dialect {
        "cargo" => parse_cargo_suite_counts(log),
        "vitest" => parse_suite_counts(log),
        _ => None,
    }
}

/// 机检级先红复跑（收取阶段调用）——E12 零模型兜底的两半：
/// ①事实核验：seed commit 处复跑测试门，与账本预验基线比对（防实现混入/种子漂移/坏基线）；
/// ②叙事核验：REPORT §3 声称的 vitest 计数（若可解析）与实测比对（防报数造假——r6/B9 实测：
///   只核事实拦不住「事实真+叙事谎」，必须读叙事数字）。
/// 不符 = bail（机检 FAIL）；旧轮无 measured 基线则记录并放行（向后兼容）。
pub fn replay_seed_red(
    root: &Path,
    round: &str,
    c: &card::Card,
    branch: &str,
    worktree: &Path,
    attempt_id: &str,
    storage_permit: &crate::storage::StoragePermit,
) -> Result<()> {
    if c.meta.seeds.is_empty() {
        return Ok(());
    }
    validate_seed_paths(root, c)?;
    let b = binding::load(root)?;
    let mb = gitx::merge_base(root, "main", branch)?;
    let seed_sha = gitx::commits_after(root, &mb, branch)?
        .into_iter()
        .next()
        .context("先红复跑：分支无提交（种子 commit 缺失）")?;

    // Task-scoped history is required for schema high-water enforcement; a
    // latest-only lookup cannot detect v2 -> versionless downgrade.
    let lr = read_ledger(&root.join(format!("coordination/rounds/{round}/events.jsonl")))?;
    let task_payloads = lr
        .events
        .iter()
        .filter(|event| {
            event.kind == "SeedOracleVerified" && event.task_id.as_deref() == Some(&c.meta.task_id)
        })
        .filter_map(|event| event.payload.clone())
        .collect::<Vec<_>>();
    let baseline = task_payloads
        .last()
        .and_then(|p| p.get("measured").cloned());

    // detached 到 seed commit → 跑测试门 → 无论成败先回分支头再传播错误（现场必须复原）
    let gate_ref = red_replay_gate(&c.meta.gates.fast).map_err(|e| anyhow::anyhow!("{e}"))?;
    gitx::checkout(worktree, &seed_sha)?;
    let replay = run_test_gate_and_parse_observation_with_permit(
        root,
        round,
        crate::ledger::GateAuditIdentity::Attempt {
            task_id: &c.meta.task_id,
            attempt_id,
        },
        storage_permit,
        &b,
        worktree,
        &format!("{}-redreplay", c.meta.task_id),
        &c.meta.seeds,
        &gate_ref,
    );
    gitx::checkout(worktree, branch)?;
    let observation = replay?;
    let m = &observation.measured;

    // ② 叙事核验：REPORT §3 的声称计数（可解析才比对；无数字行留给 verifier 兜）
    let report_rel = format!(
        "coordination/rounds/{round}/reports/{}-REPORT.md",
        c.meta.task_id
    );
    let report_text = fs::read_to_string(worktree.join(&report_rel))
        .or_else(|_| fs::read_to_string(root.join(&report_rel)))
        .unwrap_or_default();
    if m.red_form.as_deref() == Some("compile") {
        // 编译红形态：§3 无计数可言（记录的是 error 关键行）——数字比对不适用（E13：
        // r8/B12 实测「无任何 `test result:` 行」的叙述句被当计数行解析出 0f/0p 误杀诚实 REPORT）。
        // 叙事核验降为 error 关键行存在性；内容真伪由 verifier 深核。
        let s3 = extract_report_section3(&report_text).unwrap_or_default();
        if s3.contains("error[") || s3.contains("error:") {
            println!("④½ 叙事核验通过: 编译红形态——REPORT §3 含 error 关键行（数字比对不适用）");
        } else {
            println!("⚠️ 编译红棒 REPORT §3 未见 error 关键行——叙事失职嫌疑，留给 verifier 深核");
        }
    } else if let Some(claim) = extract_report_section3(&report_text)
        .and_then(|section| parse_suite_counts_for_dialect(&b.oracle.dialect, &section))
    {
        if let Err(reason) = report_claim_ok(&claim, &m) {
            ledger::append(
                root,
                round,
                &[ledger::event(
                    "EscalationRaised",
                    "runtime:orch",
                    Some(&c.meta.task_id),
                    Some(round),
                    serde_json::json!({"stage": "red-replay-claim", "reason": &reason}),
                )],
            )?;
            bail!(
                "机检 FAIL·REPORT §3 报数造假（E12 零模型拦截）：seed commit {}：{}",
                gitx::short(&seed_sha),
                reason
            );
        }
        println!(
            "④½ 叙事核验通过: REPORT §3 声称 {}f/{}p == seed commit 文件内实测 {}f/{}p",
            claim.failed, claim.passed, m.file_failed, m.file_passed
        );
    }

    let oracle_match_result = if m.red_form.as_deref() == Some("compile") {
        let identity = observation
            .compile_identity
            .as_ref()
            .context("compile replay lacks canonical compileIdentity")?;
        let effective_rebaseline = latest_compile_rebaseline(&lr.events, round, &c.meta.task_id);
        effective_rebaseline.and_then(|effective_rebaseline| {
            let upstream = recorded_upstream_public_symbols(
                root,
                round,
                c,
                &lr.events,
                lr.bad_lines.is_empty(),
            );
            let upstream_symbols = upstream.symbols();
            compile_oracle_replay_match(
                &task_payloads,
                effective_rebaseline.as_ref(),
                identity,
                &upstream_symbols,
            )
            .map(|mut decision| {
                if let Some(evidence) = decision.legitimate_shrink.as_mut() {
                    evidence.upstream_tasks = upstream.tasks_explaining(&evidence.dropped);
                }
                decision
            })
        })
    } else {
        assertion_oracle_baseline_match(&task_payloads).map(|matched| CompileReplayDecision {
            matched,
            legitimate_shrink: None,
        })
    };
    let decision = match oracle_match_result {
        Ok(decision) => decision,
        Err(reason) => {
            ledger::append(
                root,
                round,
                &[ledger::event(
                    "EscalationRaised",
                    "runtime:orch",
                    Some(&c.meta.task_id),
                    Some(round),
                    serde_json::json!({"stage": "red-replay", "reason": &reason}),
                )],
            )?;
            bail!(
                "机检 FAIL·compile oracle identity/high-water 不符：seed commit {}：{}",
                gitx::short(&seed_sha),
                reason
            );
        }
    };
    let oracle_match = decision.matched;
    let legitimate_shrink = decision.legitimate_shrink;

    if let Some(base) = &baseline {
        let want = |key: &str| {
            base.get(key)
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(u64::MAX) as usize
        };
        let want_red_form = base.get("redForm").and_then(serde_json::Value::as_str);
        let red_form_matches = m.red_form.as_deref() == want_red_form
            || (b.oracle.dialect == "vitest"
                && m.red_form.is_none()
                && want_red_form == Some("assertion"));
        if m.file_failed != want("fileFailed")
            || m.file_passed != want("filePassed")
            || m.total_failed != want("totalFailed")
            || !red_form_matches
        {
            let reason = format!(
                "seed commit 实测 {}f/{}p(文件内) 总红{} redForm={:?} ≠ 预验基线 {}",
                m.file_failed, m.file_passed, m.total_failed, m.red_form, base
            );
            ledger::append(
                root,
                round,
                &[ledger::event(
                    "EscalationRaised",
                    "runtime:orch",
                    Some(&c.meta.task_id),
                    Some(round),
                    serde_json::json!({"stage": "red-replay", "reason": &reason}),
                )],
            )?;
            bail!(
                "机检 FAIL·先红复跑不符（E12 零模型拦截）：seed commit {}：{}",
                gitx::short(&seed_sha),
                reason
            );
        }
    }

    let matches_oracle = match oracle_match {
        CompileOracleMatch::IdentityV2 => serde_json::json!(true),
        CompileOracleMatch::LegacyCountsOnly => {
            println!(
                "⚠️ ④½ LEGACY COUNTS-ONLY：任务 {} 尚无 compile identity 证明，仅按历史计数兼容回放",
                c.meta.task_id
            );
            serde_json::json!("legacy-counts-only")
        }
        CompileOracleMatch::NoBaseline => serde_json::json!("no-baseline"),
    };
    let red_proven = ledger::event(
        "RedProven",
        "runtime:orch",
        Some(&c.meta.task_id),
        Some(round),
        serde_json::json!({
        "failedCount": m.file_failed,
        "matchesOracle": matches_oracle,
        "seedCommit": gitx::short(&seed_sha),
        "replay": serde_json::to_value(&observation)?,
        }),
    );
    let events = if let Some(evidence) = &legitimate_shrink {
        let rebaseline = observation
            .compile_identity
            .as_ref()
            .context("legitimate compile shrink lacks rebaseline identity")?;
        vec![
            ledger::event(
                "EscalationRaised",
                "runtime:orch",
                Some(&c.meta.task_id),
                Some(round),
                serde_json::json!({
                    "stage": "red-replay",
                    "reason": LEGITIMATE_SHRINK_REASON,
                    "identityShift": "legitimate-shrink",
                    "baselineSymbols": evidence.baseline_symbols.clone(),
                    "replaySymbols": evidence.replay_symbols.clone(),
                    "dropped": evidence.dropped.clone(),
                    "upstreamTasks": evidence.upstream_tasks.clone(),
                    "rebaselineCompileIdentity": rebaseline,
                }),
            ),
            red_proven,
        ]
    } else {
        vec![red_proven]
    };
    ledger::append(root, round, &events)?;
    if let Some(evidence) = legitimate_shrink {
        println!(
            "④½ compile oracle 合法收缩并已重记基线: dropped={:?} upstreamTasks={:?}",
            evidence.dropped, evidence.upstream_tasks
        );
    }
    println!(
        "④½ 先红复跑通过: seed commit {} 实测 {}f/{}p(文件内)·总 {}f/{}p，oracle={:?}",
        gitx::short(&seed_sha),
        m.file_failed,
        m.file_passed,
        m.total_failed,
        m.total_passed,
        oracle_match
    );
    Ok(())
}

/// O5 种子局部性纯判据（r31/B60）：两条判据全过⇒Ok，否则 Err 指明哪条失败。
/// · 无空洞位：seed 文件内 passed = seed_file.tests - seed_file.failed，须 <= allow_file_passed；
/// · 基线不受扰：suite.failed == seed_file.failed（全部红都局部在种子文件，无附带红）。
/// additive seam——`preverify` 与既有测试不动。
pub fn seed_red_localized(
    suite: &SuiteCounts,
    seed_file: &FileCounts,
    allow_file_passed: usize,
) -> Result<(), String> {
    let file_passed = seed_file.tests - seed_file.failed;
    if file_passed > allow_file_passed {
        return Err(format!(
            "空洞位：种子文件内 {file_passed} passed > 允许 {allow_file_passed}（O5 无空洞位强制）"
        ));
    }
    if suite.failed != seed_file.failed {
        return Err(format!(
            "基线受扰：整套 {} failed ≠ 种子文件 {} failed（有附带红）",
            suite.failed, seed_file.failed
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG_RED: &str = "\
 ❯ test/product.test.ts (5 tests | 5 failed) 3ms
   × product (contract B6) > ① multiplies values 2ms
   × product (contract B6) > ② empty array → 1 (multiplicative identity) 0ms
   × product (contract B6) > ③ zero annihilates 0ms
   × product (contract B6) > ④ handles negatives 0ms
   × product (contract B6) > ⑤ does not mutate the input array 0ms
 Test Files  1 failed | 6 passed (7)
      Tests  5 failed | 33 passed (38)
";
    const LOG_GREEN: &str = "\
 ✓ test/stddev.test.ts (5 tests) 4ms
 Test Files  8 passed (8)
      Tests  43 passed (43)
";

    #[test]
    fn parses_red_suite_and_file() {
        let s = parse_suite_counts(LOG_RED).unwrap();
        assert_eq!((s.failed, s.passed, s.total), (5, 33, 38));
        let f = parse_file_counts(LOG_RED, "test/product.test.ts").unwrap();
        assert_eq!((f.tests, f.failed), (5, 5));
        assert_eq!(parse_failed_cases(LOG_RED).len(), 5);
    }

    #[test]
    fn parses_green_suite_and_file() {
        let s = parse_suite_counts(LOG_GREEN).unwrap();
        assert_eq!((s.failed, s.passed, s.total), (0, 43, 43));
        let f = parse_file_counts(LOG_GREEN, "test/stddev.test.ts").unwrap();
        assert_eq!((f.tests, f.failed), (5, 0));
        assert!(parse_failed_cases(LOG_GREEN).is_empty());
    }

    #[test]
    fn hollow_pass_detected_by_file_counts() {
        // O5 空洞：文件 5 用例只 4 红 → file_passed=1（r1/B3 的 4f/1p 场景可机判）
        let log = " ❯ test/span.test.ts (5 tests | 4 failed) 3ms\n      Tests  4 failed | 29 passed (33)\n";
        let f = parse_file_counts(log, "test/span.test.ts").unwrap();
        assert_eq!((f.tests, f.failed), (5, 4));
    }

    #[test]
    fn single_case_file_singular_form() {
        // vitest 单用例文件是「(1 test)」单数（r6 负向自证实测踩到）
        let log = " ✓ test/hollow.test.ts (1 test) 1ms\n      Tests  44 passed (44)\n";
        let f = parse_file_counts(log, "test/hollow.test.ts").unwrap();
        assert_eq!((f.tests, f.failed), (1, 0));
    }

    #[test]
    fn report_section3_claim_extraction() {
        let report = "\
---
taskId: B9
---
## 1 变更文件清单
略
## 3 种子搬运证据（先红原文）
cmp 一致。seed commit 后：Tests  4 failed | 44 passed (48)
## 4 快门实测
npx vitest run: Tests  48 passed (48)
";
        let claim = extract_report_section3(report)
            .and_then(|s| parse_suite_counts(&s))
            .unwrap();
        // 只取 §3 段内的计数（§4 的 48 passed 不得混入）
        assert_eq!((claim.failed, claim.passed, claim.total), (4, 44, 48));
    }

    #[test]
    fn failure_detail_location_lines_do_not_hijack() {
        // 失败详情区的位置行（❯ target:15:5）不得被当成文件计数行（r6/B9 实测踩坑）
        let log = "\
 ❯ test/sumsq.test.ts (5 tests | 5 failed) 3ms
 Test Files  1 failed | 8 passed (9)
      Tests  5 failed | 43 passed (48)
 FAIL  test/sumsq.test.ts > sumsq (contract B9) > ① sums squares
 ❯ test/sumsq.test.ts:15:5
     15|   it(\"① sums squares\", () => {
";
        let f = parse_file_counts(log, "test/sumsq.test.ts").unwrap();
        assert_eq!((f.tests, f.failed), (5, 5));
    }

    #[test]
    fn cargo_suite_counts_sum_every_test_binary() {
        let log = "\
test result: ok. 3 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n\
test result: FAILED. 7 passed; 2 failed; 1 ignored; 0 measured; 0 filtered out\n";
        let counts = parse_cargo_suite_counts(log).unwrap();
        assert_eq!((counts.failed, counts.passed, counts.total), (3, 10, 13));
    }

    #[test]
    fn cargo_failed_cases_are_deduplicated() {
        let log = "\
test oracle::tests::first ... FAILED\n\
test oracle::tests::second ... FAILED\n\
test oracle::tests::first ... FAILED\n";
        assert_eq!(
            parse_cargo_failed_cases(log),
            vec!["oracle::tests::first", "oracle::tests::second"]
        );
    }

    #[test]
    fn cargo_file_counts_extract_test_functions_across_attributes() {
        let source = "\
#[test]\n\
fn plain_case() {}\n\n\
#[test]\n\
#[should_panic(expected = \"boom\")]\n\
fn panic_case() {}\n";
        let counts = cargo_file_counts(source, &[]);
        assert_eq!((counts.tests, counts.failed), (2, 0));
    }

    #[test]
    fn cargo_file_counts_matches_qualified_failure_suffixes() {
        let source = "#[test]\nfn catches_bug() {}\n#[test]\nfn stays_green() {}\n";
        let failed = vec![
            "oracle::tests::catches_bug".to_string(),
            "unrelated::case".to_string(),
        ];
        let counts = cargo_file_counts(source, &failed);
        assert_eq!((counts.tests, counts.failed), (2, 1));
    }

    #[test]
    fn rust_compile_message_fallback_is_stable_and_not_code_only() {
        let first = canonical_rust_compile_identity(
            "error[E0308]: expected 12 values at /private/tmp/a/seed.rs:9:7\n",
        )
        .unwrap();
        let second = canonical_rust_compile_identity(
            "error[E0308]:  expected 900 values at /another/worktree/seed.rs:77:3\n",
        )
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(
            first.diagnostics[0].keys,
            vec!["message:expected <n> values at <path>"]
        );
    }

    #[test]
    fn malformed_compile_identity_deserialization_fails_closed() {
        for value in [
            serde_json::json!({"dialect":"rustc","diagnostics":[]}),
            serde_json::json!({
                "dialect":"rustc",
                "diagnostics":[{"code":"e0432","keys":["symbol:crate::missing"]}]
            }),
            serde_json::json!({
                "dialect":"rustc",
                "diagnostics":[{"code":"E0432","keys":["symbol:z","symbol:a"]}]
            }),
            serde_json::json!({
                "dialect":"rustc",
                "diagnostics":[{"code":"E0432","keys":["symbol:a","symbol:a"]}]
            }),
            serde_json::json!({
                "dialect":"rustc",
                "diagnostics":[{"code":"E0432","keys":["symbol:a"],"extra":true}]
            }),
        ] {
            assert!(
                serde_json::from_value::<CompileRedIdentity>(value).is_err(),
                "malformed identity was accepted"
            );
        }
    }

    #[test]
    fn expected_red_syntax_and_assertion_count_are_closed() {
        assert!(
            validate_expected_red_syntax(Some("compile"), "cargo", "error[E0432]", false).is_ok()
        );
        assert!(
            validate_expected_red_syntax(Some("compile"), "vitest", "error[E0432]", false).is_err()
        );
        assert!(validate_expected_red_syntax(Some("assertion"), "cargo", "2", false).is_ok());
        assert!(validate_expected_red_syntax(Some("assertion"), "cargo", "2 of 3", false).is_err());
        assert!(validate_expected_red_syntax(Some("unknown"), "cargo", "2", false).is_err());
        assert!(validate_expected_red_syntax(None, "cargo", "error[E0432]", false).is_err());
        assert!(
            validate_expected_red_syntax(Some("compile"), "cargo", "error[E0432]", true).is_err()
        );
    }

    #[test]
    fn unknown_schema_and_v2_downgrade_fail_closed() {
        let identity =
            canonical_rust_compile_identity("error[E0432]: unresolved import `crate::api`\n")
                .unwrap();
        let proof = validate_expected_compile_red("error[E0432]", &identity).unwrap();
        let modern = serde_json::json!({
            "oracleSchemaVersion": 2,
            "expectedRed": "error[E0432]",
            "expectedRedProof": proof,
            "measured": {
                "redForm": "compile",
                "compileIdentity": identity
            }
        });
        let legacy = serde_json::json!({
            "measured": {"fileFailed":1,"redForm":"compile"}
        });
        let unknown = serde_json::json!({"oracleSchemaVersion": 3});
        assert!(compile_oracle_baseline_match(
            &[unknown],
            &canonical_rust_compile_identity("error[E0432]: unresolved import `crate::api`\n")
                .unwrap()
        )
        .is_err());
        assert!(compile_oracle_baseline_match(
            &[modern, legacy],
            &canonical_rust_compile_identity("error[E0432]: unresolved import `crate::api`\n")
                .unwrap()
        )
        .is_err());
    }

    fn compile_identity(code: &str, keys: &[&str]) -> CompileRedIdentity {
        CompileRedIdentity {
            dialect: "rustc".to_string(),
            diagnostics: vec![CompileDiagnostic {
                code: code.to_string(),
                keys: keys.iter().map(|key| (*key).to_string()).collect(),
            }],
        }
    }

    fn v2_compile_payload(identity: &CompileRedIdentity) -> serde_json::Value {
        let proof = validate_expected_compile_red("error[E0432]", identity).unwrap();
        serde_json::json!({
            "oracleSchemaVersion": 2,
            "expectedRed": "error[E0432]",
            "expectedRedProof": proof,
            "measured": {
                "redForm": "compile",
                "compileIdentity": identity,
            }
        })
    }

    #[test]
    fn compile_replay_accepts_only_a_structurally_exact_explained_shrink() {
        let baseline = compile_identity("E0432", &["symbol:crate::kept", "symbol:crate::upstream"]);
        let replay = compile_identity("E0432", &["symbol:crate::kept"]);
        let payload = v2_compile_payload(&baseline);
        let upstream = vec!["symbol:crate::upstream".to_string()];
        let accepted = compile_oracle_replay_match(&[payload.clone()], None, &replay, &upstream)
            .expect("fully explained symbol-only shrink");
        assert_eq!(accepted.matched, CompileOracleMatch::IdentityV2);
        assert_eq!(
            accepted.legitimate_shrink.unwrap().dropped,
            upstream,
            "the durable evidence must enumerate every dropped symbol"
        );

        let unexplained = compile_oracle_replay_match(&[payload.clone()], None, &replay, &[])
            .expect_err("missing upstream evidence must fail closed");
        assert!(unexplained.starts_with("compile identity mismatch: baseline="));

        let changed_code = compile_identity("E0308", &["symbol:crate::kept"]);
        let structural_change =
            compile_oracle_replay_match(&[payload], None, &changed_code, &upstream)
                .expect_err("a code change cannot hide behind a symbol shrink");
        assert!(structural_change.starts_with("compile identity mismatch: baseline="));
    }

    #[test]
    fn legitimate_escalation_round_trips_as_a_rebaseline() {
        let replay = compile_identity("E0432", &["symbol:crate::kept"]);
        let payload = serde_json::json!({
            "stage": "red-replay",
            "reason": LEGITIMATE_SHRINK_REASON,
            "identityShift": "legitimate-shrink",
            "baselineSymbols": ["symbol:crate::kept", "symbol:crate::upstream"],
            "replaySymbols": ["symbol:crate::kept"],
            "dropped": ["symbol:crate::upstream"],
            "upstreamTasks": ["B197"],
            "rebaselineCompileIdentity": replay,
        });
        assert_eq!(
            legitimate_shrink_rebaseline(&payload).unwrap(),
            Some(replay.clone())
        );

        let mut malformed = payload;
        malformed["upstreamTasks"] = serde_json::json!([]);
        assert!(legitimate_shrink_rebaseline(&malformed).is_err());
    }

    #[test]
    fn public_symbol_evidence_excludes_impl_and_restricted_items() {
        let source = r#"pub struct Exported;
impl Exported {
    pub fn method_is_not_a_module_export() {}
}
pub(crate) fn restricted() {}
pub async fn run() {}
pub const VALUE: usize = 1;
"#;
        assert_eq!(
            public_symbols_in_source("orch/crates/orch-host/src/wake.rs", source),
            [
                "symbol:orch_host::wake::Exported".to_string(),
                "symbol:orch_host::wake::VALUE".to_string(),
                "symbol:orch_host::wake::run".to_string(),
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            public_module_prefix("orch/crates/orch-core/src/lib.rs").as_deref(),
            Some("orch_core")
        );
        assert!(public_module_prefix("orch/crates/orch-cli/src/main.rs").is_none());
    }

    #[test]
    fn observation_adds_identity_without_changing_measured_json() {
        let source = "#[test]\nfn missing_api_contract() {}\n".to_string();
        let observation = measure_cargo_observation(
            "error[E0432]: unresolved import `crate::missing_api`\n",
            101,
            &[source],
        )
        .unwrap();
        let observation_json = serde_json::to_value(&observation).unwrap();
        assert!(observation_json.get("compileIdentity").is_some());
        let measured_json = serde_json::to_value(&observation.measured).unwrap();
        assert!(measured_json.get("compileIdentity").is_none());
    }

    #[test]
    fn generic_cargo_error_is_not_a_compile_observation() {
        let source = "#[test]\nfn contract() {}\n".to_string();
        for log in [
            "error: failed to run custom build command\n",
            "clang: error: unknown argument\n",
            "error: could not compile `orch-host`\n",
        ] {
            assert!(measure_cargo_observation(log, 101, std::slice::from_ref(&source)).is_err());
        }
    }

    #[test]
    fn cargo_compile_red_builds_compile_measurement() {
        let source = "#[test]\nfn missing_api_contract() {}\n".to_string();
        let log = "Compiling seeded-test\nerror[E0432]: unresolved import `missing_api`\n";
        let measured = measure_cargo(log, 101, &[source]).unwrap();
        assert_eq!((measured.file_failed, measured.file_passed), (1, 0));
        assert_eq!(
            (measured.total_failed, measured.total_passed, measured.total),
            (1, 0, 1)
        );
        assert_eq!(measured.red_form.as_deref(), Some("compile"));
        assert_eq!(
            measured.failed_cases,
            vec!["<compile-red>: error[E0432]: unresolved import `missing_api`"]
        );
    }

    #[test]
    fn cargo_assertion_red_marks_assertion_form() {
        let source = "#[test]\nfn rejects_bad_value() {}\n".to_string();
        let log = "\
test oracle::tests::rejects_bad_value ... FAILED\n\
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n";
        let measured = measure_cargo(log, 101, &[source]).unwrap();
        assert_eq!((measured.file_failed, measured.file_passed), (1, 0));
        assert_eq!(measured.red_form.as_deref(), Some("assertion"));
    }

    #[test]
    fn report_claim_error_discloses_claim_and_file_counts() {
        let claim = SuiteCounts {
            failed: 2,
            passed: 1,
            total: 3,
        };
        let measured = Measured {
            file_failed: 3,
            file_passed: 0,
            total_failed: 3,
            total_passed: 184,
            total: 187,
            failed_cases: Vec::new(),
            red_form: Some("assertion".to_string()),
        };
        let err = report_claim_ok(&claim, &measured).unwrap_err();
        assert!(err.contains("2f/1p"), "缺声称值: {err}");
        assert!(err.contains("3f/0p"), "缺文件内实测值: {err}");
        assert!(err.contains("文件内"), "未标明计数口径: {err}");
    }

    #[test]
    fn binding_without_oracle_defaults_to_vitest() {
        let b: crate::binding::Binding =
            serde_yaml::from_str("commands: {}\nunknownTopLevelField: tolerated\n").unwrap();
        assert_eq!(b.oracle.dialect, "vitest");
        assert!(b.project.ecosystems.is_empty());
    }

    #[test]
    fn narrative_mention_is_not_a_count_line() {
        // E13（r8/B12 误杀实测）：REPORT §3 的叙述句「无任何 `test result:` 行」自身含子串，
        // 但无真实数字——不得被当成 0f/0p 计数行
        let narrative = "在种子提交态执行 cargo test，exit 101；无任何 `test result:` 行，符合 redForm: compile。\n";
        assert!(parse_cargo_suite_counts(narrative).is_none());
        // 真计数行混在叙述里仍要被解析
        let mixed = "说明文字\ntest result: ok. 3 passed; 2 failed; 0 ignored\n";
        let s = parse_cargo_suite_counts(mixed).unwrap();
        assert_eq!((s.failed, s.passed), (2, 3));
    }

    #[test]
    fn red_replay_gate_empty_errors_with_seeded_keyword() {
        // 空列表 → Err，文案含 "seeded"（seeded 任务必须至少一个快门）
        let err = red_replay_gate(&[]).unwrap_err();
        assert!(err.contains("seeded"), "err should mention seeded: {err}");
    }

    #[test]
    fn red_replay_gate_takes_first_preserving_card_order() {
        // 多门取首（卡内顺序即优先级，非字典序）
        let gates = vec!["zeta".to_string(), "alpha".to_string(), "mid".to_string()];
        assert_eq!(red_replay_gate(&gates).unwrap(), "zeta");
        // 单门直通
        assert_eq!(
            red_replay_gate(&["testFast".to_string()]).unwrap(),
            "testFast"
        );
    }
}
