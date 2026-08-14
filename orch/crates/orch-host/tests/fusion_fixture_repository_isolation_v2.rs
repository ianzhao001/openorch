//! B253 · worktree-backed Fusion 单元夹具必须使用独立 Git common-dir。
//!
//! # 实撞
//!
//! `fusion::tests::temp_root` 只在外层仓的 `orch/target/test-tmp` 下创建普通目录；
//! `git -C <fixture> ...` 因而向上发现外层仓 `.git`。全量门并发时，四个成员的串行
//! `worktree add` 会与其他测试共享 B249 的 canonical common-dir 初始化锁，30 秒整单
//! `totalWallSecs` 可在 adapter 启动前被耗尽。
//!
//! # 本种子只钉测试夹具，不改生产预算
//!
//! 正确修复是在 `fusion.rs` 的 `#[cfg(test)]` 模块里为所有真实 worktree-backed 用例
//! 初始化独立仓库。生产 `run_fusion_in_skeleton` 的计时顺序、30 秒总预算和 1 秒成员
//! 预算必须保持；把 deadline 移到 provisioning 后或放宽预算都不能让本种子转绿。
//!
//! # B251 恢复订正
//!
//! B251 的冻结 seed 在整个 production prefix 上从头搜索
//! `provision_consult_sites(root`，首匹配了函数定义，与它同时冻结的 prefix hash
//! 机械矛盾。本新 task/new target 保留同一 production hash，但只在 `started` 与
//! `std::thread::scope` 之间的窄窗口搜索真实 provisioning call，并要求该窗口内恰一次匹配。

use sha2::{Digest, Sha256};

const SOURCE: &str = include_str!("../src/fusion.rs");

const fn source_contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    let mut start = 0;
    while start + needle.len() <= haystack.len() {
        let mut offset = 0;
        while offset < needle.len() && haystack[start + offset] == needle[offset] {
            offset += 1;
        }
        if offset == needle.len() {
            return true;
        }
        start += 1;
    }
    false
}

// This compile-time sentinel is deliberate.  Before plan sign-off the workspace's own unsigned-IR
// fixture may fail before an assertion-red integration target is executed.  E0080 therefore makes
// the real missing helper the first stable red without adding a production API or weakening gates.
const _: () = assert!(
    source_contains(
        SOURCE.as_bytes(),
        b"fn isolated_fusion_repo(name: &str) -> PathBuf"
    ),
    "B253: worktree-backed fusion unit tests must create a private git common-dir"
);

fn between<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    let (_, tail) = source
        .split_once(start)
        .unwrap_or_else(|| panic!("B253: missing source anchor {start:?}"));
    let (body, _) = tail
        .split_once(end)
        .unwrap_or_else(|| panic!("B253: missing source anchor {end:?}"));
    body
}

#[test]
fn worktree_backed_fusion_tests_use_private_git_common_dirs_v2() {
    let (production, tests) = SOURCE
        .split_once("#[cfg(test)]\nmod tests {")
        .expect("B253: fusion.rs must keep one cfg(test) module boundary");

    assert_eq!(
        format!("{:x}", Sha256::digest(production.as_bytes())),
        "f1bcbc72087f8e45ddccdb3f0e876f9d9818ae08bd6a6b83b3ca6eb4f9b8df4f",
        "B253: fusion production prefix must remain byte-identical to the signed baseline"
    );

    assert!(
        tests.contains("fn isolated_fusion_repo(name: &str) -> PathBuf"),
        "B253: worktree-backed fusion unit tests must create a private git common-dir"
    );

    let helper = between(
        tests,
        "fn isolated_fusion_repo(name: &str) -> PathBuf",
        "fn limits() -> ConsultLimits",
    );
    assert!(
        helper.contains("--initial-branch=main"),
        "B253: the private fixture repository must expose an exact main branch"
    );
    assert!(
        helper.contains("--git-common-dir"),
        "B253: helper must observe and assert its canonical private common-dir"
    );
    assert!(
        helper.contains("--show-toplevel"),
        "B253: helper must prove Git resolves the fixture itself as the repository root"
    );
    assert!(
        helper.contains("canonicalize"),
        "B253: common-dir identity must be compared as a canonical physical path"
    );
    for required in [
        "sentinel",
        ".arg(\"add\")",
        "user.name=",
        "user.email=",
        ".arg(\"commit\")",
        "rev_parse",
        "\"main\"",
        "\"HEAD\"",
    ] {
        assert!(
            helper.contains(required),
            "B253: private repo initializer is missing required commit/main proof {required:?}"
        );
    }
    assert!(
        !helper.contains("config"),
        "B253: fixture identity must be command-local; git config writes are forbidden"
    );
    assert!(
        helper.matches("\"-c\"").count() >= 2,
        "B253: user.name and user.email must each be supplied with command-local -c"
    );

    for forbidden in ["#[ignore", "serial_test", "Mutex", "test_threads", "test-threads"] {
        assert!(
            !tests.contains(forbidden),
            "B253: fusion unit module may not hide this regression with {forbidden:?}"
        );
    }

    let limits = between(
        tests,
        "fn limits() -> ConsultLimits",
        "#[test]\n    fn site_plans_reject_unsafe_roots_and_names()",
    );
    assert!(
        limits.contains("per_member_timeout_secs: 1")
            && limits.contains("total_wall_secs: 30")
            && limits.contains("max_members: 8"),
        "B253: limits() must remain exactly 1s member / 30s total / 8 members"
    );

    let fanout = between(
        tests,
        "fn production_fanout_keeps_sites_answers_and_failure_states_distinct()",
        "fn worktree_creation_failure_is_member_local_and_all_failed_names_members()",
    );
    assert!(
        fanout.contains("isolated_fusion_repo(\"fanout\")"),
        "B253: the four-member fanout regression must use the isolated repository"
    );
    assert!(fanout.contains("limits()"), "B253: fanout must use limits()");
    assert!(
        tests.contains(
            "#[test]\n    fn production_fanout_keeps_sites_answers_and_failure_states_distinct()"
        ),
        "B253: fanout must remain an ordinary default-lane test"
    );
    assert!(
        !fanout.contains("Mutex")
            && !fanout.contains("serial_test")
            && !fanout.contains("test_threads"),
        "B253: fanout may not be hidden behind process-global serialization"
    );

    let site_failure = tests
        .split_once("fn worktree_creation_failure_is_member_local_and_all_failed_names_members()")
        .expect("B253: missing worktree failure regression")
        .1;
    assert!(
        site_failure.contains("isolated_fusion_repo(\"site-failure\")"),
        "B253: member-local worktree failure regression must use the isolated repository"
    );
    let ancestor_lock = between(
        tests,
        "#[test]\n    fn isolated_fixture_does_not_wait_for_an_ancestor_worktree_lock()",
        "#[test]\n    fn production_fanout_keeps_sites_answers_and_failure_states_distinct()",
    );
    for required in [
        "orch-worktree-init.lock",
        "fd_lock::RwLock",
        "try_write",
        "canonical_worktree_common_dir",
        "initialize_fusion_repo_at",
        "provision_consult_sites",
        "rev_parse",
        "starts_with",
        "drop(parent_lock_guard)",
        "remove_dir_all",
    ] {
        assert!(
            ancestor_lock.contains(required),
            "B253: ancestor-lock regression is missing executable proof {required:?}"
        );
    }
    assert!(
        !ancestor_lock.contains("thread::spawn") && !ancestor_lock.contains("thread::sleep"),
        "B253: ancestor-lock oracle must be synchronous and threshold-free"
    );
    assert!(
        ancestor_lock.matches("assert_ne!").count() >= 2,
        "B253: parent, inner and outer common-dir identities must be proven distinct"
    );
    let identity_check = ancestor_lock
        .find("assert_ne!")
        .expect("B253: missing pre-lock identity check");
    let lock_open = ancestor_lock
        .find("orch-worktree-init.lock")
        .expect("B253: missing parent lock open");
    let provision = ancestor_lock
        .find("provision_consult_sites")
        .expect("B253: missing inner provisioning");
    let unlock = ancestor_lock
        .find("drop(parent_lock_guard)")
        .expect("B253: missing explicit parent unlock");
    let cleanup = ancestor_lock
        .find("remove_dir_all")
        .expect("B253: missing test-owned repository cleanup");
    assert!(
        identity_check < lock_open
            && lock_open < provision
            && provision < unlock
            && unlock < cleanup,
        "B253: prove identities, lock, provision inner, unlock, then clean the parent"
    );

    let run_fusion = between(
        production,
        "pub fn run_fusion_in_skeleton(",
        "\nstruct RegisteredConsultWorktree {",
    );
    let started_anchor = "let started = Instant::now();";
    let started = run_fusion
        .find(started_anchor)
        .expect("B253: total-wall clock start disappeared");
    let after_started = &run_fusion[started + started_anchor.len()..];
    let provisioning_anchor =
        "if let Err(error) = provision_consult_sites(root, std::slice::from_ref(&worktree), sha)";
    assert_eq!(
        after_started.matches(provisioning_anchor).count(),
        1,
        "B253: run_fusion must contain exactly one post-start production provisioning call"
    );
    let provisioning_after_started = after_started
        .find(provisioning_anchor)
        .expect("B253: post-start production provisioning call disappeared");
    let fanout_after_started = after_started
        .find("std::thread::scope")
        .expect("B253: adapter fanout disappeared");
    assert!(
        provisioning_after_started < fanout_after_started,
        "B253: totalWallSecs must still include serial site provisioning"
    );
}
