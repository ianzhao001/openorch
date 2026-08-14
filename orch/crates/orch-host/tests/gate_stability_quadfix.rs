//! B243 · 门/建场稳定性四修契约（H127 + H128①③ + H130①）。
//!
//! 首红形态：**compile** —— `reap_errno_tolerated` 与 `provision_consult_sites`
//! 尚不存在（`error[E0432]`）。种子字节冻结；改种子 = 整棒 FAIL。
//!
//! 用例 ↔ 验收映射：
//!   M1 esrch_and_eperm_are_tolerated_others_hard_fail —— H128①（跨组收割 EPERM 归「非本组、跳过」）
//!   M2 consult_sites_provision_serially_in_one_call   —— H130①（建场先串行完成，adapter 才并发）
//!   M3 provisioning_refuses_to_clobber_existing_site  —— H130① 负向（不得静默覆盖既有现场）
//!   M4 provisioning_precedes_member_concurrency       —— H130① 接线存在性（弱钉，见下）
//!
//! 覆盖披露（审查者按此对账，不要在种子里找 H127/H128③）：
//!   - H127（b177 等待循环把 fail-closed 当硬失败）与 H128③（fusion 测试族建场幂等）的修点
//!     都在 `wake.rs`/`fusion.rs` 的 `#[cfg(test)]` 内部，集成测试够不着——由 requiredEvidence
//!     的定向重放与满负载整跑覆盖，不由本种子覆盖。
//!   - M4 是**弱钉**：只断言 `fusion.rs` 文本序上「provision_consult_sites(」先于「thread::scope」
//!     首现。真正的机械验收是 requiredEvidence 的「三成员 preset 一次 consult membersOk=3」。
//!
//! scratch 一律落 `orch/target/test-tmp`（H38），测试自建现场自己清（B196 纪律）。

use std::path::{Path, PathBuf};
use std::process::Command;

use orch_host::fusion::provision_consult_sites;
use orch_host::gate::reap_errno_tolerated;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("CARGO_MANIFEST_DIR 上溯三级应为仓根")
        .to_path_buf()
}

fn scratch_root(tag: &str) -> PathBuf {
    let root = repo_root()
        .join("orch/target/test-tmp")
        .join(format!("b243-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("scratch 根应可创建");
    root
}

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "b243")
        .env("GIT_AUTHOR_EMAIL", "b243@seed")
        .env("GIT_COMMITTER_NAME", "b243")
        .env("GIT_COMMITTER_EMAIL", "b243@seed")
        .output()
        .expect("git 应可执行");
    assert!(
        output.status.success(),
        "git {args:?} 失败: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stdout(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git 应可执行");
    assert!(output.status.success(), "git {args:?} 失败");
    String::from_utf8(output.stdout).expect("git stdout 应为 UTF-8").trim().to_string()
}

fn fixture_repo(tag: &str) -> (PathBuf, String) {
    let repo = scratch_root(tag).join("repo");
    std::fs::create_dir_all(&repo).expect("建仓目录");
    git(&repo, &["init", "--quiet"]);
    git(&repo, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    std::fs::write(repo.join("seed.txt"), b"b243\n").expect("写文件");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "--quiet", "--no-verify", "-m", "base"]);
    let sha = git_stdout(&repo, &["rev-parse", "HEAD"]);
    (repo, sha)
}

fn cleanup(tag: &str) {
    let _ = std::fs::remove_dir_all(
        repo_root()
            .join("orch/target/test-tmp")
            .join(format!("b243-{tag}-{}", std::process::id())),
    );
}

#[test]
fn esrch_and_eperm_are_tolerated_others_hard_fail() {
    // ESRCH(3)：组已消失——B227 既有语义，一字不动。
    assert!(reap_errno_tolerated(3), "ESRCH 必须保持容忍");
    // EPERM(1)：打到的不是本进程可辖的组（r66 三门红物证 `Operation not permitted`）
    // ——「非本组、跳过」，不再作为门硬失败。
    assert!(reap_errno_tolerated(1), "EPERM 必须归入「非本组、跳过」");
    // 其余 errno 仍是真故障，必须硬失败：收割语义不许静默放水。
    assert!(!reap_errno_tolerated(22), "EINVAL 仍须硬失败");
    assert!(!reap_errno_tolerated(4), "EINTR 仍须硬失败");
    assert!(!reap_errno_tolerated(0), "0 不是可容忍 errno");
}

#[test]
fn consult_sites_provision_serially_in_one_call() {
    let (repo, sha) = fixture_repo("m2");
    let site_a = repo.join(".worktrees/consult-b243-0-a");
    let site_b = repo.join(".worktrees/consult-b243-1-b");
    provision_consult_sites(&repo, &[site_a.clone(), site_b.clone()], &sha)
        .expect("串行建场应对 ≥2 个成员一次成功");
    for site in [&site_a, &site_b] {
        assert!(site.is_dir(), "现场目录应存在: {}", site.display());
        assert_eq!(
            git_stdout(site, &["rev-parse", "HEAD"]),
            sha,
            "现场应 detach 在指定 SHA"
        );
    }
    cleanup("m2");
}

#[test]
fn provisioning_refuses_to_clobber_existing_site() {
    let (repo, sha) = fixture_repo("m3");
    let site = repo.join(".worktrees/consult-b243-0-solo");
    provision_consult_sites(&repo, &[site.clone()], &sha).expect("首次建场应成功");
    assert!(
        provision_consult_sites(&repo, &[site.clone()], &sha).is_err(),
        "对既有现场必须拒绝，不得静默覆盖（覆盖=毁掉在飞成员的工作目录）"
    );
    cleanup("m3");
}

#[test]
fn provisioning_precedes_member_concurrency() {
    // 弱钉（见文件头覆盖披露）：建场调用在文本序上必须先于成员并发段。
    let src = include_str!("../src/fusion.rs");
    let provision = src
        .find("provision_consult_sites(")
        .expect("fusion.rs 必须接线 provision_consult_sites");
    let scope = src
        .find("thread::scope")
        .expect("fusion.rs 应仍以 thread::scope 并发成员");
    assert!(
        provision < scope,
        "建场必须在成员并发段之前完成（H130①：≥2 并发 worktree add 会撞 B231 判别器）"
    );
}
