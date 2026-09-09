//! B231 seeded-red contract: 主仓 HEAD 不得被 checkout 切走（H118，P0 通道安全）。
//!
//! Expected red: assertion（预期 1 条红）。`.githooks/reference-transaction`（B162/H28 交付）
//! 今天对 `HEAD` 的 reference 更新**放行**——r64 实测审查者在主仓 `git checkout <sha>` 把
//! 工作树切走（reflog 逐字 `checkout: moving from main to 73545ca`），planner 三笔提交落
//! detached 链、账本工作树副本被回退（幸有 WAL 兜底）。本卡在既有守卫上加第二条规则：
//! **主仓（primary git-dir）的 HEAD 直接更新（detach）一律拒绝**，`ORCH_MAIN_GUARD_BYPASS=1`
//! 为显式逃生舱（沿 B162 既有约定）。
//!
//! 机械事实（钉住不回退的三条自由度）：
//! - linked worktree 的 HEAD 更新**必须继续放行**——审查现场就是 detached worktree，
//!   建场机械（`git worktree add --detach`）每天都在做这件事。
//! - 主仓 topic 分支 ref 更新不受影响（守卫只看 HEAD 与受保护 ref）。
//! - git 没有 pre-checkout 钩子；reference-transaction 是唯一能在 prepared 阶段
//!   否决 HEAD 更新的机械位置（B162 已实证该通道可用）。
//!
//! Negative mutations that must turn the named case red:
//! M1. 删除/绕过新规则（HEAD 更新重新放行）
//!     -> `primary_head_detach_is_blocked` 红。
//! M2. 规则误伤 linked worktree（把审查建场一起拦死）
//!     -> `linked_worktree_head_update_stays_free` 红。
//! M3. 逃生舱失效（bypass 下仍拦，恢复操作无法进行）
//!     -> `bypass_env_allows_primary_head_update` 红。
//! M4. 规则外溢到普通分支 ref（主仓日常提交被拦）
//!     -> `topic_branch_update_stays_free` 红。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use orch_host::util::test_scratch_dir;

fn hook_path() -> PathBuf {
    // CARGO_MANIFEST_DIR = <repo>/orch/crates/orch-host
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("定位仓根失败")
        .join(".githooks/reference-transaction")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=b231", "-c", "user.email=b231@test"])
        .args(args)
        .output()
        .expect("git 调用失败");
    assert!(
        out.status.success(),
        "git {args:?} 失败: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// 建一个含两个提交的主仓夹具，返回 (仓路径, 旧提交, 新提交=main tip)。
fn fixture_repo(tag: &str) -> (PathBuf, String, String) {
    let root = test_scratch_dir(tag);
    let repo = root.join("primary");
    std::fs::create_dir_all(&repo).expect("建夹具目录失败");
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["commit", "--allow-empty", "-m", "c1"]);
    let c1 = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["commit", "--allow-empty", "-m", "c2"]);
    let c2 = git(&repo, &["rev-parse", "HEAD"]);
    (repo, c1, c2)
}

/// 以 prepared 状态调用真实钩子，喂一行 reference-transaction 更新，返回退出码。
fn run_hook(cwd: &Path, envs: &[(&str, &str)], line: &str) -> i32 {
    let mut cmd = Command::new(hook_path());
    cmd.arg("prepared")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let mut child = cmd.spawn().expect("spawn 钩子失败");
    child
        .stdin
        .as_mut()
        .expect("钩子 stdin")
        .write_all(line.as_bytes())
        .expect("写钩子 stdin 失败");
    child.wait().expect("等待钩子退出失败").code().unwrap_or(-1)
}

#[test]
fn primary_head_detach_is_blocked() {
    // M1：H118 的事故形态逐字复刻——主仓 HEAD 从 main tip 移到历史提交（checkout <sha>）。
    let (repo, c1, c2) = fixture_repo("b231-detach");
    let code = run_hook(&repo, &[], &format!("{c2} {c1} HEAD\n"));
    assert_ne!(code, 0, "主仓 HEAD detach 必须被 prepared 阶段否决");
}

#[test]
fn bypass_env_allows_primary_head_update() {
    // M3：显式逃生舱（书面理由 + 人工设 env）必须仍然可用——否则合法恢复操作也做不了。
    let (repo, c1, c2) = fixture_repo("b231-bypass");
    let code = run_hook(
        &repo,
        &[("ORCH_MAIN_GUARD_BYPASS", "1")],
        &format!("{c2} {c1} HEAD\n"),
    );
    assert_eq!(code, 0, "ORCH_MAIN_GUARD_BYPASS=1 必须放行");
}

#[test]
fn linked_worktree_head_update_stays_free() {
    // M2：审查现场 = detached linked worktree，其 HEAD 更新是建场机械的日常动作，不得拦。
    let (repo, c1, c2) = fixture_repo("b231-linked");
    let wt = repo.parent().expect("夹具父目录").join("linked-wt");
    git(&repo, &["worktree", "add", "--detach", wt.to_str().expect("utf8 路径"), &c1]);
    let code = run_hook(&wt, &[], &format!("{c1} {c2} HEAD\n"));
    assert_eq!(code, 0, "linked worktree 的 HEAD 更新必须继续放行");
}

#[test]
fn topic_branch_update_stays_free() {
    // M4：主仓普通分支 ref 更新不受新规则影响（守卫不外溢）。
    let (repo, c1, c2) = fixture_repo("b231-topic");
    let code = run_hook(&repo, &[], &format!("{c1} {c2} refs/heads/topic\n"));
    assert_eq!(code, 0, "topic 分支 ref 更新必须放行");
}
