//! B196 · consult 测试的 worktree 判据必须是**作用域**的，不是主仓全局的（H59），
//! 并把「测试自己建的现场自己清」（H38 已落地的纪律）固化成可复用的支撑件。
//!
//! 首红形态：**compile**。本文件顶部声明的 `support_worktree` 模块尚不存在
//! （目标文件 `orch/crates/orch-host/tests/support_worktree/mod.rs`），rustc 报
//! `error[E0583]: file not found for module `support_worktree``。
//! 不得以建空壳、改本文件、或把本测试排除出门的方式伪造红绿。
//!
//! scratch 一律落在本 worktree 的 `orch/target/test-tmp`（协议约束，不污染工作树、
//! 不落 OS 共享 temp）。本文件不引用任何 orch 生产 API：H59 是测试判据缺陷，
//! 修复面就在测试层，不得借机改动生产行为。

mod support_worktree;

use std::path::{Path, PathBuf};
use std::process::Command;

use support_worktree::{scoped_worktrees, ScopedWorktreeGuard};

/// 本仓 `orch/` 目录（`CARGO_MANIFEST_DIR` = `<repo>/orch/crates/orch-host`）。
fn orch_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("CARGO_MANIFEST_DIR 上溯两级应为 orch 工作区根")
        .to_path_buf()
}

fn scratch_root(tag: &str) -> PathBuf {
    let root = orch_dir()
        .join("target/test-tmp")
        .join(format!("b196-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("scratch 根应可创建");
    root
}

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git 应可执行");
    assert!(
        output.status.success(),
        "git {args:?} 失败: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// 造一个**独立的**临时仓：本套断言绝不碰主仓，否则就重演 H59。
fn scratch_repo(tag: &str) -> PathBuf {
    let root = scratch_root(tag).join("repo");
    std::fs::create_dir_all(&root).expect("临时仓目录应可创建");
    git(&root, &["init", "--quiet"]);
    git(&root, &["config", "user.email", "b196@example.invalid"]);
    git(&root, &["config", "user.name", "b196"]);
    std::fs::write(root.join("seed.txt"), b"b196\n").expect("写入应成功");
    git(&root, &["add", "seed.txt"]);
    git(&root, &["commit", "--quiet", "-m", "base"]);
    root
}

/// H59 的正解：判据必须只看**本次测试自己建的**现场，按名字前缀过滤；
/// 主仓（或任何共享仓）里别人建的现场对本测试不可见，也就无从漂移。
#[test]
fn scoped_view_ignores_worktrees_outside_the_prefix() {
    let repo = scratch_repo("scoped-view");
    let mut guard = ScopedWorktreeGuard::new(&repo, "b196-scope-");

    let mine_a = guard.add("b196-scope-alpha");
    let mine_b = guard.add("b196-scope-beta");

    // 模拟「并发的审查建场」：同一个仓里出现一个不属于本测试的现场。
    let foreign = repo.join(".worktrees").join("review-foreign");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "--detach",
            foreign.to_str().expect("路径应为 UTF-8"),
        ],
    );

    let seen = scoped_worktrees(&repo, "b196-scope-");
    assert_eq!(
        seen.len(),
        2,
        "作用域视图必须恰好看到本测试建的两个现场，实际: {seen:?}"
    );
    assert!(seen.contains(&mine_a), "缺少自建现场 {mine_a:?}: {seen:?}");
    assert!(seen.contains(&mine_b), "缺少自建现场 {mine_b:?}: {seen:?}");
    assert!(
        !seen.iter().any(|path| path == &foreign),
        "外来现场必须不可见，否则并发建场会让判据随机漂移: {seen:?}"
    );

    // 全局计数在同一时刻是 4（主 + 2 自建 + 1 外来），它正是**不该被断言**的量。
    let global = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .expect("git worktree list 应可执行");
    let global_count = String::from_utf8(global.stdout)
        .expect("git 输出应为 UTF-8")
        .lines()
        .filter(|line| line.starts_with("worktree "))
        .count();
    assert_eq!(
        global_count, 4,
        "前置条件：本用例确实制造了作用域外的漂移源"
    );
    assert_ne!(
        seen.len(),
        global_count,
        "作用域判据与全局计数必须是两个量；相等则说明过滤没生效"
    );
}

/// H38 的纪律要可复用：guard 析构只清自己建的，绝不误伤别人的现场。
#[test]
fn guard_drop_removes_only_its_own_worktrees() {
    let repo = scratch_repo("guard-drop");

    let foreign = repo.join(".worktrees").join("review-foreign");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "--detach",
            foreign.to_str().expect("路径应为 UTF-8"),
        ],
    );

    {
        let mut guard = ScopedWorktreeGuard::new(&repo, "b196-drop-");
        guard.add("b196-drop-one");
        guard.add("b196-drop-two");
        assert_eq!(
            scoped_worktrees(&repo, "b196-drop-").len(),
            2,
            "guard 存活期内自建现场应可见"
        );
    }

    assert!(
        scoped_worktrees(&repo, "b196-drop-").is_empty(),
        "guard 析构后自建现场必须全部注销"
    );
    assert!(
        foreign.exists(),
        "外来现场必须不受影响——清理只能是作用域内的"
    );
}

/// 回归锁：legacy fusion tests 不得重新成为 schema-3 consult 的 live driver。
#[test]
fn consult_fusion_no_longer_asserts_primary_repo_global_count() {
    let fusion = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/consult_fusion.rs");
    let source = std::fs::read_to_string(&fusion).expect("consult_fusion.rs 应可读");

    assert!(
        !source.contains("registered_worktree_count"),
        "consult_fusion.rs 仍在使用主仓全局计数助手，H59 未修"
    );
    assert!(
        !source.contains("must not change the primary repo's worktree registration count"),
        "consult_fusion.rs 仍保留全局计数断言文案，H59 未修"
    );
    for retired in ["run_consultation", "ConsultArgs", "JudgeMode"] {
        assert!(
            !source.contains(retired),
            "consult_fusion.rs 不得复活旧 live consult 入口 {retired}"
        );
    }
}

/// 同族排查（H59 修法③）：除生产代码外，测试层不得再有对共享仓全局 worktree 状态的断言。
#[test]
fn no_test_asserts_on_shared_repo_global_worktree_state() {
    let tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut offenders = Vec::new();
    let entries = std::fs::read_dir(&tests_dir).expect("tests 目录应可读");
    for entry in entries {
        let path = entry.expect("目录项应可读").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        if path.file_name().and_then(|name| name.to_str()) == Some("consult_worktree_scope.rs") {
            continue; // 本文件自身要引用这些字面量作为判据
        }
        let source = std::fs::read_to_string(&path).expect("测试源应可读");
        if source.contains("registered_worktree_count") {
            offenders.push(path);
        }
    }
    assert!(
        offenders.is_empty(),
        "以下测试仍对共享仓全局 worktree 状态下断言: {offenders:?}"
    );
}
