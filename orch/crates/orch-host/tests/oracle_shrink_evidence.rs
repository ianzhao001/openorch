//! B242 · H126 修复契约：上游新增文件的符号证据 + REPORT/BLOCKED 逃生舱。
//!
//! 首红形态：**compile** —— `introduced_public_symbols_probe` 与 `await_poll_with_escape`
//! 尚不存在（`error[E0432]`）。种子字节冻结；改种子 = 整棒 FAIL。
//!
//! 用例 ↔ 验收映射：
//!   M1 upstream_new_file_emits_module_and_item_symbols   —— 修法①正向（新文件⇒模块符号+条目符号）
//!   M2 preexisting_module_symbol_is_not_reintroduced     —— 修法①口径（旧文件只算新增条目）
//!   M3 non_utf8_upstream_still_abstains                  —— 自认限制另一半原样保留
//!   M4 b233_historical_merge_replays_the_b234_shape      —— r66/B234 实景：当年返回 None 的
//!                                                            确切输入，修后必须给出模块级符号
//!   M5 non_merge_sha_still_abstains                      —— 防伪造面不回退
//!   M6 escape_hatch_decision_matrix                      —— 修法②六格真值表
//!
//! scratch 一律落 `orch/target/test-tmp`（H38），测试自建现场自己清（B196 纪律）。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use orch_host::oracle::introduced_public_symbols_probe;
use orch_host::tierf::{await_poll_with_escape, AwaitPoll};

/// 本仓根（CARGO_MANIFEST_DIR = <repo>/orch/crates/orch-host，上溯三级）。
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
        .join(format!("b242-{tag}-{}", std::process::id()));
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
        .env("GIT_AUTHOR_NAME", "b242")
        .env("GIT_AUTHOR_EMAIL", "b242@seed")
        .env("GIT_COMMITTER_NAME", "b242")
        .env("GIT_COMMITTER_EMAIL", "b242@seed")
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

fn write_file(repo: &Path, rel: &str, bytes: &[u8]) {
    let path = repo.join(rel);
    std::fs::create_dir_all(path.parent().expect("有父目录")).expect("建父目录");
    std::fs::write(path, bytes).expect("写文件");
}

/// 建一个「main 基座 + topic 分支（改旧文件 + 新增文件）+ no-ff 合回」的夹具仓。
/// 返回 (repo, merge_sha)。`extra_new_file` 允许 M3 注入非 UTF-8 新文件。
fn fixture_merge(tag: &str, extra_new_file: Option<(&str, &[u8])>) -> (PathBuf, String) {
    let repo = scratch_root(tag).join("repo");
    std::fs::create_dir_all(&repo).expect("建仓目录");
    git(&repo, &["init", "--quiet"]);
    git(&repo, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    write_file(
        &repo,
        "orch/crates/orch-host/src/b242_seed_alpha.rs",
        b"pub fn alpha_base() {}\n",
    );
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "--quiet", "--no-verify", "-m", "base"]);
    git(&repo, &["checkout", "--quiet", "-b", "topic"]);
    write_file(
        &repo,
        "orch/crates/orch-host/src/b242_seed_alpha.rs",
        b"pub fn alpha_base() {}\npub fn alpha_added() {}\n",
    );
    write_file(
        &repo,
        "orch/crates/orch-host/src/b242_new_module.rs",
        b"pub fn beta_probe() {}\npub struct BetaThing;\n",
    );
    if let Some((rel, bytes)) = extra_new_file {
        write_file(&repo, rel, bytes);
    }
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "--quiet", "--no-verify", "-m", "topic"]);
    git(&repo, &["checkout", "--quiet", "main"]);
    git(&repo, &["merge", "--quiet", "--no-ff", "--no-verify", "-m", "merge", "topic"]);
    let merge_sha = git_stdout(&repo, &["rev-parse", "HEAD"]);
    (repo, merge_sha)
}

fn cleanup(tag: &str) {
    let _ = std::fs::remove_dir_all(
        repo_root()
            .join("orch/target/test-tmp")
            .join(format!("b242-{tag}-{}", std::process::id())),
    );
}

#[test]
fn upstream_new_file_emits_module_and_item_symbols() {
    let (repo, merge_sha) = fixture_merge("m1", None);
    let write_set = vec![
        "orch/crates/orch-host/src/b242_seed_alpha.rs".to_string(),
        "orch/crates/orch-host/src/b242_new_module.rs".to_string(),
    ];
    let symbols: BTreeSet<String> = introduced_public_symbols_probe(&repo, &merge_sha, &write_set)
        .expect("上游含新增文件时不得整体弃权（H126 根因）");
    // 新文件：模块级符号 + 全部 pub 条目符号（模块级即 B234 掉的那种诊断身份）。
    assert!(symbols.contains("symbol:orch_host::b242_new_module"), "缺新文件模块级符号: {symbols:?}");
    assert!(symbols.contains("symbol:orch_host::b242_new_module::beta_probe"), "缺新文件条目符号: {symbols:?}");
    assert!(symbols.contains("symbol:orch_host::b242_new_module::BetaThing"), "缺新文件类型符号: {symbols:?}");
    // 旧文件：新增条目照旧收集。
    assert!(symbols.contains("symbol:orch_host::b242_seed_alpha::alpha_added"), "缺旧文件新增条目: {symbols:?}");
    cleanup("m1");
}

#[test]
fn preexisting_module_symbol_is_not_reintroduced() {
    let (repo, merge_sha) = fixture_merge("m2", None);
    let write_set = vec![
        "orch/crates/orch-host/src/b242_seed_alpha.rs".to_string(),
        "orch/crates/orch-host/src/b242_new_module.rs".to_string(),
    ];
    let symbols = introduced_public_symbols_probe(&repo, &merge_sha, &write_set)
        .expect("同 M1，不得弃权");
    // 旧文件的模块符号与既有条目都不是「新引入」。
    assert!(!symbols.contains("symbol:orch_host::b242_seed_alpha"), "旧文件模块符号不得算新引入");
    assert!(!symbols.contains("symbol:orch_host::b242_seed_alpha::alpha_base"), "既有条目不得算新引入");
    cleanup("m2");
}

#[test]
fn non_utf8_upstream_still_abstains() {
    let (repo, merge_sha) = fixture_merge(
        "m3",
        Some(("orch/crates/orch-host/src/b242_bad_bytes.rs", &[0xFFu8, 0xFE, 0x00, 0x9F][..])),
    );
    let write_set = vec!["orch/crates/orch-host/src/b242_bad_bytes.rs".to_string()];
    assert!(
        introduced_public_symbols_probe(&repo, &merge_sha, &write_set).is_none(),
        "非 UTF-8 源仍是不支持的证据形状，必须整体弃权（自认限制另一半原样保留）"
    );
    cleanup("m3");
}

#[test]
fn b233_historical_merge_replays_the_b234_shape() {
    // r66 实景：B233 的 merge 在第一父上新增 registry.rs；当年 introduced_public_symbols
    // 对它整体返回 None，使 B234 掉的 `symbol:orch_host::registry` 无从解释。
    // 修后：对同一历史输入必须给出模块级符号 + 条目符号。
    let root = repo_root();
    let merge_sha = "bb1a3029a208d6edb9daf198668818540b23ad17";
    let write_set = vec!["orch/crates/orch-host/src/registry.rs".to_string()];
    let symbols = introduced_public_symbols_probe(&root, merge_sha, &write_set)
        .expect("B233 历史 merge 是标准 no-ff 双亲提交，不得弃权");
    assert!(
        symbols.contains("symbol:orch_host::registry"),
        "必须能解释 B234-r66 掉的模块级诊断身份: {symbols:?}"
    );
    assert!(
        symbols.iter().any(|s| s.starts_with("symbol:orch_host::registry::")),
        "新文件的 pub 条目符号也应在集合内: {symbols:?}"
    );
}

#[test]
fn non_merge_sha_still_abstains() {
    let (repo, _merge) = fixture_merge("m5", None);
    // HEAD^2 = topic 侧支提交：有第一父（base）、无第二父——恰好打在「双亲要求」判据上
    // （r67 种子修订：初版误用 HEAD~2，三提交 DAG 的 first-parent 链只有两级，该 rev 不存在）。
    let side = git_stdout(&repo, &["rev-parse", "HEAD^2"]);
    let write_set = vec!["orch/crates/orch-host/src/b242_seed_alpha.rs".to_string()];
    assert!(
        introduced_public_symbols_probe(&repo, &side, &write_set).is_none(),
        "单亲提交不是 recorded no-ff 交付，防伪造面不得回退"
    );
    cleanup("m5");
}

#[test]
fn escape_hatch_decision_matrix() {
    // await_poll_with_escape(report_found, blocked_found,
    //                        report_collect_hard_rejected, blocked_is_current)
    // 判据 (a) report_collect_hard_rejected 必须来自账本事实（本 attempt 的收取机检硬拒终态），
    // 判据 (b) blocked_is_current 复用 tierf 既有原语义——两者的推导在生产调用点，本表只钉决策。
    // 逃生舱开启：仅当 REPORT 在场、已被硬拒、且 BLOCKED 是严格更新证据。
    assert_eq!(await_poll_with_escape(true, true, true, true), AwaitPoll::Blocked);
    // REPORT 未被拒 ⇒ 常规优先级一字不动。
    assert_eq!(await_poll_with_escape(true, true, false, true), AwaitPoll::ReportFound);
    // BLOCKED 是陈旧证据 ⇒ 不得遮蔽 REPORT。
    assert_eq!(await_poll_with_escape(true, true, true, false), AwaitPoll::ReportFound);
    // 无 REPORT 的基线行为与 await_poll 逐字等价。
    assert_eq!(await_poll_with_escape(false, true, false, false), AwaitPoll::Blocked);
    assert_eq!(await_poll_with_escape(true, false, false, false), AwaitPoll::ReportFound);
    assert_eq!(await_poll_with_escape(false, false, false, false), AwaitPoll::Waiting);
}
