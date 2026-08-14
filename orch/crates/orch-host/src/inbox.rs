//! INBOX 指令队列(design/11 §4,r21/B37)——planner 预置占位,消 lib.rs 热点。
//! 一条指令一个文件,生命周期 coordination/inbox/ → processing/ → done/。
//! 任意本地终端写一条指令文件 → daemon 唤醒主控(中心化半套)。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};

/// 指令文件的三态生命周期。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxStage {
    Pending,
    Processing,
    Done,
}

/// 三态 → 三目录（互不串味）。
pub fn stage_dir(stage: InboxStage) -> &'static str {
    match stage {
        InboxStage::Pending => "coordination/inbox",
        InboxStage::Processing => "coordination/inbox/processing",
        InboxStage::Done => "coordination/inbox/done",
    }
}

/// 状态转移目标目录：stage_dir + "/" + filename。
pub fn relocate_target(filename: &str, to: InboxStage) -> String {
    format!("{}/{}", stage_dir(to), filename)
}

/// User-supplied inbox names are identifiers, not paths.  Keeping this check
/// next to the filesystem primitive prevents every caller (CLI or daemon)
/// from turning `advance` into an arbitrary rename outside the inbox.
pub fn validate_filename(filename: &str) -> Result<()> {
    let stem = filename
        .strip_suffix(".md")
        .filter(|stem| {
            !stem.is_empty()
                && stem
                    .bytes()
                    .next()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric())
                && stem.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                })
        })
        .with_context(|| {
            format!("inbox filename 必须匹配 [A-Za-z0-9][A-Za-z0-9._-]*.md: {filename:?}")
        })?;
    let mut components = Path::new(filename).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None) if !name.is_empty() && !stem.is_empty() => Ok(()),
        _ => bail!("inbox filename 必须是单一安全 basename: {filename:?}"),
    }
}

fn canonical_root(root: &Path) -> Result<PathBuf> {
    fs::canonicalize(root).with_context(|| format!("解析 inbox root 失败: {}", root.display()))
}

/// Resolve/create one of the three fixed stage directories without ever
/// following a symlink component.  The stage names are compile-time constants;
/// only the final filename is user-controlled and is validated separately.
fn ensure_stage_dir(root: &Path, stage: InboxStage) -> Result<PathBuf> {
    let root = canonical_root(root)?;
    let mut cursor = root.clone();
    for component in Path::new(stage_dir(stage)).components() {
        let Component::Normal(name) = component else {
            bail!("internal inbox stage path 非 canonical");
        };
        cursor.push(name);
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            }
            Ok(_) => bail!("inbox stage component 必须是真实目录: {}", cursor.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&cursor)
                    .with_context(|| format!("创建 inbox stage 目录失败: {}", cursor.display()))?;
                let metadata = fs::symlink_metadata(&cursor)?;
                if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                    bail!("新建 inbox stage component 身份异常: {}", cursor.display());
                }
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("检查 inbox stage 失败: {}", cursor.display()))
            }
        }
    }
    let canonical = fs::canonicalize(&cursor)?;
    if !canonical.starts_with(&root) || canonical != cursor {
        bail!("inbox stage 目录逃逸 root: {}", cursor.display());
    }
    Ok(cursor)
}

fn existing_stage_dir(root: &Path, stage: InboxStage) -> Result<Option<PathBuf>> {
    let root = canonical_root(root)?;
    let mut cursor = root.clone();
    for component in Path::new(stage_dir(stage)).components() {
        let Component::Normal(name) = component else {
            bail!("internal inbox stage path 非 canonical");
        };
        cursor.push(name);
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            }
            Ok(_) => bail!("inbox stage component 必须是真实目录: {}", cursor.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("检查 inbox stage 失败: {}", cursor.display()))
            }
        }
    }
    let canonical = fs::canonicalize(&cursor)?;
    if !canonical.starts_with(&root) || canonical != cursor {
        bail!("inbox stage 目录逃逸 root: {}", cursor.display());
    }
    Ok(Some(cursor))
}

/// 单向推进：Pending→Processing→Done，Done 是终态。
pub fn next_stage(s: InboxStage) -> Option<InboxStage> {
    match s {
        InboxStage::Pending => Some(InboxStage::Processing),
        InboxStage::Processing => Some(InboxStage::Done),
        InboxStage::Done => None,
    }
}

/// 既有 `next_stage` 的逆：Done→Some(Processing)，Processing→Some(Pending)，Pending→None。
pub fn prev_stage(s: InboxStage) -> Option<InboxStage> {
    match s {
        InboxStage::Done => Some(InboxStage::Processing),
        InboxStage::Processing => Some(InboxStage::Pending),
        InboxStage::Pending => None,
    }
}

/// 指令文本 → fs 安全 slug。
/// 规则：取 ASCII 字母数字词（非 ASCII 如中文剔除），非字母数字→`-`，小写，去重连字符，截断 ≤40；
/// 空/全符号/全非 ASCII → 兜底 `"instruction"`。
pub fn slugify(instruction: &str) -> String {
    let mut slug: String = instruction
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    // 去重连字符
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    // 去首尾连字符
    let slug = slug.trim_matches('-').to_string();
    // 空/全符号 → 兜底
    if slug.is_empty() {
        return "instruction".to_string();
    }
    // 截断 ≤40
    if slug.len() > 40 {
        slug.chars().take(40).collect()
    } else {
        slug
    }
}

/// 落盘辅助：生成 `inbox/<unix_ts>-<slug>.md` 写指令（mkdir 自愈 O6）。
/// 返回相对文件名（不含目录前缀）。
pub fn add(root: &Path, instruction: &str, ts: u64) -> Result<String> {
    let slug = slugify(instruction);
    let filename = format!("{ts}-{slug}.md");
    validate_filename(&filename)?;
    let dir = ensure_stage_dir(root, InboxStage::Pending)?;
    let path = dir.join(&filename);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("create-new inbox 文件失败: {}", path.display()))?;
    file.write_all(format!("# INBOX 指令\n\n{instruction}\n").as_bytes())
        .with_context(|| format!("写 inbox 文件失败: {}", path.display()))?;
    Ok(filename)
}

/// 列出 Pending 目录下的指令文件名。
pub fn list_pending(root: &Path) -> Result<Vec<String>> {
    let Some(dir) = existing_stage_dir(root, InboxStage::Pending)? else {
        return Ok(Vec::new());
    };
    let mut files: Vec<String> = fs::read_dir(&dir)
        .with_context(|| format!("读 inbox 目录失败: {}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_type()
                .is_ok_and(|kind| kind.is_file() && !kind.is_symlink())
        })
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|filename| validate_filename(filename).is_ok())
        .collect();
    files.sort();
    Ok(files)
}

/// 状态转移：git mv 语义的文件移动（源目录推断→目标目录）。
/// filename 为相对文件名（不含目录前缀）。
pub fn advance(root: &Path, filename: &str, to: InboxStage) -> Result<()> {
    validate_filename(filename)?;
    // 在三态目录中查找唯一 regular/non-symlink 源文件。
    let stages = [
        InboxStage::Pending,
        InboxStage::Processing,
        InboxStage::Done,
    ];
    let mut candidates = Vec::new();
    for stage in stages {
        let Some(dir) = existing_stage_dir(root, stage)? else {
            continue;
        };
        let path = dir.join(filename);
        match fs::symlink_metadata(&path) {
            Ok(metadata)
                if metadata.file_type().is_file() && !metadata.file_type().is_symlink() =>
            {
                candidates.push(path)
            }
            Ok(_) => bail!("inbox 源必须是 regular non-symlink: {}", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("检查 inbox 源失败: {}", path.display()))
            }
        }
    }
    if candidates.len() != 1 {
        bail!(
            "inbox 文件必须恰好存在于一个 stage: {filename}（实际 {}）",
            candidates.len()
        );
    }
    let src = candidates.pop().expect("len checked");
    let target_dir = ensure_stage_dir(root, to)?;
    let dst = target_dir.join(filename);
    if fs::symlink_metadata(&dst).is_ok() {
        bail!("inbox 目标已存在，拒绝覆盖: {}", dst.display());
    }
    fs::rename(&src, &dst)
        .with_context(|| format!("移动 inbox 文件失败: {} → {}", src.display(), dst.display()))?;
    let metadata = fs::symlink_metadata(&dst)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!("inbox rename 后目标身份异常: {}", dst.display());
    }
    Ok(())
}

/// 优先级排序纯函数(design/11 §4,r22/B40):入参=(文件名, 该文件 priority)对。
/// 序:priority 降序(高者先);None(无 frontmatter/缺 priority)排在所有显式 priority 之后;
/// 同 priority(含同为 None)按文件名升序 tiebreak(文件名含 unix_ts 前缀,即先到先处理)。
pub fn order_by_priority(entries: Vec<(String, Option<u8>)>) -> Vec<String> {
    let mut entries = entries;
    // Option 序:None < Some(_);反向 cmp 得"显式优先、高者先",None 自然殿后。
    entries.sort_by(|(fa, pa), (fb, pb)| pb.cmp(pa).then_with(|| fa.cmp(fb)));
    entries.into_iter().map(|(f, _)| f).collect()
}

/// 读盘包壳(design/11 §4,r22/B40):Pending 队列按 priority 排序后的文件名列表。
/// 尽力容错:单文件读盘失败/消失 → 该文件按 None(殿后)处理,不拖垮整队。
pub fn list_pending_by_priority(root: &Path) -> Result<Vec<String>> {
    let files = list_pending(root)?;
    let entries: Vec<(String, Option<u8>)> = files
        .into_iter()
        .map(|f| {
            let path = root.join(relocate_target(&f, InboxStage::Pending));
            let priority = fs::read_to_string(&path)
                .ok()
                .and_then(|content| crate::inbox_meta::parse_instruction(&content).priority);
            (f, priority)
        })
        .collect();
    Ok(order_by_priority(entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_chinese_stripped_ascii_words_joined() {
        // 中文剔除后按剩余 ASCII 词拼接（去重连字符、小写）
        assert_eq!(slugify("给 orch 加个 X 功能!"), "orch-x");
        // 多词
        assert_eq!(slugify("Add a new gate!"), "add-a-new-gate");
        // 全符号/全非 ASCII → 兜底
        assert_eq!(slugify("！！！"), "instruction");
        assert_eq!(slugify(""), "instruction");
    }

    #[test]
    fn slugify_truncates_long_input() {
        let long = "a".repeat(100);
        let s = slugify(&long);
        assert!(s.len() <= 40);
        assert_eq!(s.len(), 40);
    }

    #[test]
    fn advance_moves_file_between_stages() {
        let root = crate::util::test_scratch_dir("inbox-test");
        let fname = add(&root, "test instruction here", 1784800000).unwrap();
        assert_eq!(fname, "1784800000-test-instruction-here.md");
        // Pending → Processing
        advance(&root, &fname, InboxStage::Processing).unwrap();
        assert!(root
            .join(relocate_target(&fname, InboxStage::Processing))
            .is_file());
        assert!(!root
            .join(relocate_target(&fname, InboxStage::Pending))
            .is_file());
        // Processing → Done
        advance(&root, &fname, InboxStage::Done).unwrap();
        assert!(root
            .join(relocate_target(&fname, InboxStage::Done))
            .is_file());
        // list_pending 不含已移走的
        let pending = list_pending(&root).unwrap();
        assert!(!pending.contains(&fname));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn advance_rejects_path_traversal_without_moving_victim() {
        let root = crate::util::test_scratch_dir("inbox-containment");
        let victim = root.join("victim.md");
        fs::write(&victim, "keep-me").unwrap();
        for bad in [
            "../victim.md",
            "../../../victim.md",
            "/victim.md",
            "..",
            "a\\b.md",
        ] {
            assert!(advance(&root, bad, InboxStage::Done).is_err(), "{bad}");
            assert_eq!(fs::read_to_string(&victim).unwrap(), "keep-me");
        }
        fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn advance_rejects_symlink_source_and_destination_parent() {
        use std::os::unix::fs::symlink;

        let root = crate::util::test_scratch_dir("inbox-symlink-containment");
        let pending = ensure_stage_dir(&root, InboxStage::Pending).unwrap();
        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let victim = outside.join("victim.md");
        fs::write(&victim, "keep-me").unwrap();
        symlink(&victim, pending.join("link.md")).unwrap();
        assert!(advance(&root, "link.md", InboxStage::Done).is_err());
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep-me");

        fs::remove_file(pending.join("link.md")).unwrap();
        fs::write(pending.join("safe.md"), "safe").unwrap();
        let inbox = root.join("coordination/inbox");
        symlink(&outside, inbox.join("done")).unwrap();
        assert!(advance(&root, "safe.md", InboxStage::Done).is_err());
        assert!(pending.join("safe.md").is_file());
        assert!(!outside.join("safe.md").exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn order_all_none_falls_back_to_filename_ascending() {
        // 全 None(无 frontmatter):退化为纯文件名升序(unix_ts 前缀 → 时序)
        let entries = vec![
            ("1003-c.md".to_string(), None),
            ("1001-a.md".to_string(), None),
            ("1002-b.md".to_string(), None),
        ];
        assert_eq!(
            order_by_priority(entries),
            vec![
                "1001-a.md".to_string(),
                "1002-b.md".to_string(),
                "1003-c.md".to_string()
            ]
        );
    }

    #[test]
    fn order_mixed_priority_stable_and_none_last() {
        // 混合:高优先先;同 priority 按文件名升序;None 全部殿后且内部仍按文件名升序
        let entries = vec![
            ("1004-none-b.md".to_string(), None),
            ("1000-p2.md".to_string(), Some(2u8)),
            ("1003-none-a.md".to_string(), None),
            ("1002-p9-b.md".to_string(), Some(9u8)),
            ("1001-p9-a.md".to_string(), Some(9u8)),
        ];
        assert_eq!(
            order_by_priority(entries),
            vec![
                "1001-p9-a.md".to_string(),
                "1002-p9-b.md".to_string(),
                "1000-p2.md".to_string(),
                "1003-none-a.md".to_string(),
                "1004-none-b.md".to_string()
            ]
        );
    }

    #[test]
    fn list_pending_by_priority_reads_frontmatter_from_disk() {
        let root = crate::util::test_scratch_dir("inbox-prio-test");
        let dir = root.join(stage_dir(InboxStage::Pending));
        fs::create_dir_all(&dir).unwrap();
        // 低优先显式 / 无 frontmatter(None) / 高优先显式
        fs::write(dir.join("1000-low.md"), "---\npriority: 1\n---\n低优先\n").unwrap();
        fs::write(dir.join("1001-none.md"), "# INBOX 指令\n\n无 frontmatter\n").unwrap();
        fs::write(dir.join("1002-high.md"), "---\npriority: 9\n---\n高优先\n").unwrap();
        let ordered = list_pending_by_priority(&root).unwrap();
        assert_eq!(
            ordered,
            vec![
                "1002-high.md".to_string(),
                "1000-low.md".to_string(),
                "1001-none.md".to_string()
            ]
        );
        fs::remove_dir_all(&root).ok();
    }
}
