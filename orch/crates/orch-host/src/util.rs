//! 人读输出工具：字符串中段截断、秒数时长格式化。
//! B15p 填充（util 探测棒）。
//! B108：临时路径卫生——`unique_scratch_name`（pid+模块级计数器，无时钟依赖）
//! 与 `test_scratch_dir`（测试 scratch 一律落在本 worktree `orch/target/test-tmp`）。

/// 中段截断：超长时头尾各留若干字符，中间插入省略号 U+2026，结果按字符计 ≤ `max`。
/// 不超限原样返回。按**字符**计数（防多字节字符按字节截断导致 panic/错位）。
pub fn truncate_middle(s: &str, max: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    const ELLIPSIS: char = '…'; // U+2026
    let budget = max - 1; // head + tail 可用字符预算
    let head_len = budget / 2;
    let tail_len = budget - head_len;
    let head: String = s.chars().take(head_len).collect();
    let tail: String = s
        .chars()
        .skip(char_count - tail_len)
        .take(tail_len)
        .collect();
    format!("{head}{ELLIPSIS}{tail}")
}

/// 把秒数格式化为 `h/m/s` 分段字符串，零段省略（输入 0 → "0s"）。
pub fn format_secs(secs: u64) -> String {
    if secs == 0 {
        return "0s".to_string();
    }
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    let mut out = String::new();
    if h > 0 {
        out.push_str(&format!("{h}h"));
    }
    if m > 0 {
        out.push_str(&format!("{m}m"));
    }
    if s > 0 {
        out.push_str(&format!("{s}s"));
    }
    out
}

/// 把 `part / whole` 四舍五入为整数百分比；空分母按 0% 处理。
pub fn format_pct(part: usize, whole: usize) -> String {
    if whole == 0 {
        return "0%".to_string();
    }
    let part = part as u128;
    let whole = whole as u128;
    let pct = (part * 100 + whole / 2) / whole;
    format!("{pct}%")
}

/// 格式化带单位的计数：仅 1 使用单数，其余计数使用简单 `s` 复数。
pub fn format_count(n: usize, unit: &str) -> String {
    let suffix = if n == 1 { "" } else { "s" };
    format!("{n} {unit}{suffix}")
}

/// 模块级单调计数器：临时名唯一性的唯一来源（线程安全，无时钟依赖）。
static SCRATCH_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 生成进程内唯一的临时名 `{label}-{pid}-{seq}`。
///
/// 唯一性 = 进程号 + 模块级 `AtomicU64.fetch_add` 单调序号；不读时钟，
/// 避免 pid+nanos/时间戳 在并发或粗时钟粒度下撞名（B108 契约）。
pub fn unique_scratch_name(label: &str) -> String {
    let seq = SCRATCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{label}-{}-{seq}", std::process::id())
}

/// 测试 scratch 目录：`<调用 worktree>/orch/target/test-tmp/<唯一名>`，已创建。
///
/// 禁止 `std::env::temp_dir()`（指向 /tmp，Tier F 沙箱下会被 auto-reject）。
/// 以编译期 `CARGO_MANIFEST_DIR`（= orch/crates/orch-host）定位所属 worktree
/// 的 `orch/target/test-tmp`；target/ 已被 gitignore，不污染工作树。
pub fn test_scratch_dir(tag: &str) -> std::path::PathBuf {
    let orch_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("CARGO_MANIFEST_DIR 应形如 <worktree>/orch/crates/orch-host");
    let dir = orch_root
        .join("target")
        .join("test-tmp")
        .join(unique_scratch_name(tag));
    std::fs::create_dir_all(&dir).expect("创建测试 scratch 目录失败");
    dir
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScratchSweepReport {
    pub removed: Vec<std::path::PathBuf>,
    pub failed: Vec<(std::path::PathBuf, String)>,
    pub freed_bytes: u64,
}

impl ScratchSweepReport {
    /// B222 compatibility surface: callers which only inspect successful
    /// removals keep seeing the old list semantics.  Failures remain available
    /// through the public `failed` field and are never hidden from summaries.
    pub fn iter(&self) -> std::slice::Iter<'_, std::path::PathBuf> {
        self.removed.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.removed.is_empty()
    }

    pub fn is_total_failure(&self) -> bool {
        self.removed.is_empty() && !self.failed.is_empty()
    }
}

const REMOVE_DIR_ALL_MAX_ATTEMPTS: usize = 2;

fn remove_dir_all_with_enotempty_retry_using<F>(
    path: &std::path::Path,
    mut remove: F,
) -> std::io::Result<()>
where
    F: FnMut(&std::path::Path) -> std::io::Result<()>,
{
    for attempt in 0..REMOVE_DIR_ALL_MAX_ATTEMPTS {
        match remove(path) {
            Ok(()) => return Ok(()),
            Err(error)
                if error.kind() == std::io::ErrorKind::DirectoryNotEmpty
                    && attempt + 1 < REMOVE_DIR_ALL_MAX_ATTEMPTS =>
            {
                std::thread::yield_now();
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("bounded removal loop always returns")
}

/// Remove a directory tree, retrying `ENOTEMPTY` exactly once.  Other errors
/// are never retried, so permission refusals and dirty-site safeguards remain
/// loud and deterministic.
pub(crate) fn remove_dir_all_with_enotempty_retry(path: &std::path::Path) -> std::io::Result<()> {
    remove_dir_all_with_enotempty_retry_using(path, |candidate| std::fs::remove_dir_all(candidate))
}

fn apparent_tree_bytes(root: &std::path::Path) -> std::io::Result<u64> {
    let mut total = 0_u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            for entry in std::fs::read_dir(&path)? {
                pending.push(entry?.path());
            }
        } else {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

/// 删除 scratch 根下超过 `ttl` 且未被 `keep` 保护的直接条目，并返回逐项报告。
/// 根不存在时视为已经清理完成；单项失败被隔离，后续条目仍继续处理。
pub fn sweep_test_scratch_root(
    root: &std::path::Path,
    ttl: std::time::Duration,
    keep: &[&std::path::Path],
) -> std::io::Result<ScratchSweepReport> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ScratchSweepReport::default())
        }
        Err(error) => return Err(error),
    };
    let now = std::time::SystemTime::now();
    let mut report = ScratchSweepReport::default();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                report.failed.push((root.to_path_buf(), error.to_string()));
                continue;
            }
        };
        let path = entry.path();
        let protected = keep
            .iter()
            .any(|kept| *kept == path || kept.starts_with(&path));
        if protected {
            continue;
        }
        let modified = match entry.metadata().and_then(|metadata| metadata.modified()) {
            Ok(modified) => modified,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                report.failed.push((path, error.to_string()));
                continue;
            }
        };
        let expired = now.duration_since(modified).unwrap_or_default() > ttl;
        if !expired {
            continue;
        }
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                report.failed.push((path, error.to_string()));
                continue;
            }
        };
        // Accounting is best-effort: a stat/read race must never become a new
        // reason to skip an otherwise deletable entry.  Removal remains the
        // authority for both progress and refusal reporting.
        let bytes = apparent_tree_bytes(&path).unwrap_or(0);
        let result = if file_type.is_dir() {
            remove_dir_all_with_enotempty_retry(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match result {
            Ok(()) => {
                report.removed.push(path);
                report.freed_bytes = report.freed_bytes.saturating_add(bytes);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => report.failed.push((path, error.to_string())),
        }
    }
    report.removed.sort();
    report.failed.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_middle_handles_multibyte_unicode() {
        // 中文为多字节字符；必须按字符而非字节截断，否则错位或 panic
        // "你好世界测试X"（7 字符），max=5 → head=2, tail=2，结果 5 字符
        assert_eq!(truncate_middle("你好世界测试X", 5), "你好…试X");
        // 极短多字节串落在不超限分支，原样返回
        assert_eq!(truncate_middle("你好", 5), "你好");
    }

    #[test]
    fn format_secs_handles_large_values_and_hour_only() {
        // 90061 = 25h1m1s（大数值时长）
        assert_eq!(format_secs(90061), "25h1m1s");
        // 仅小时（3600s → "1h"，零段省略）
        assert_eq!(format_secs(3600), "1h");
    }

    #[test]
    fn unique_scratch_name_is_unique_across_512_concurrent_calls() {
        // B108 自证：8 线程 × 64 = 并发 512 次无重名，且全部携带 pid
        use std::collections::HashSet;
        use std::sync::Mutex;

        let names = std::sync::Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let names = std::sync::Arc::clone(&names);
            handles.push(std::thread::spawn(move || {
                let local: Vec<String> = (0..64).map(|_| unique_scratch_name("conc")).collect();
                names.lock().unwrap().extend(local);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let names = names.lock().unwrap();
        assert_eq!(names.len(), 512);
        let unique: HashSet<&String> = names.iter().collect();
        assert_eq!(unique.len(), 512, "并发 512 次出现重名");
        let pid = std::process::id().to_string();
        assert!(names.iter().all(|n| n.contains(&pid)));
    }

    #[test]
    fn test_scratch_dir_lands_under_workspace_target_test_tmp() {
        let dir = test_scratch_dir("util-self");
        assert!(dir.is_dir());
        let text = dir.to_string_lossy().replace('\\', "/");
        assert!(
            text.contains("/orch/target/test-tmp/"),
            "scratch 必须落在 orch/target/test-tmp，实际: {text}"
        );
        assert!(!text.starts_with("/tmp"), "禁止落 /tmp: {text}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn directory_not_empty_retry_is_bounded_and_error_specific() {
        let path = std::path::Path::new("unused-retry-fixture");
        let mut succeeds_on_retry = 0;
        remove_dir_all_with_enotempty_retry_using(path, |_| {
            succeeds_on_retry += 1;
            if succeeds_on_retry == 1 {
                Err(std::io::Error::from(std::io::ErrorKind::DirectoryNotEmpty))
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(succeeds_on_retry, 2);

        let mut stays_nonempty = 0;
        let error = remove_dir_all_with_enotempty_retry_using(path, |_| {
            stays_nonempty += 1;
            Err(std::io::Error::from(std::io::ErrorKind::DirectoryNotEmpty))
        })
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::DirectoryNotEmpty);
        assert_eq!(stays_nonempty, REMOVE_DIR_ALL_MAX_ATTEMPTS);

        let mut permission_denied = 0;
        let error = remove_dir_all_with_enotempty_retry_using(path, |_| {
            permission_denied += 1;
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        })
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(permission_denied, 1);

        let mut missing = 0;
        let error = remove_dir_all_with_enotempty_retry_using(path, |_| {
            missing += 1;
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        })
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(missing, 1);
    }
}
