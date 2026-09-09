//! B208 seeded-red contract: trial 构建缓存的身份绑定 + 独立超时预算。
//!
//! Expected red: compile. `orch_host::buildcache` 在 B208 之前不存在。
//!
//! 设计依据：r63 fusion 的 planner judge（consultations/01KZ409HF3FPC2B12YFV5VPEYK/judge.md）。
//! H88 实证：同一 CARGO_TARGET_DIR 先编主仓、再编固定 review worktree 会复用错误 root 的
//! 陈旧 rmeta，正确源码仍报 E0432——**门因此会假红，而假红比慢更贵**。
//!
//! Negative mutations that must turn the named case red:
//! M1. marker 只绑 source root（不绑 toolchain/lockfile/构建配置）
//!     -> `marker_binds_the_whole_build_identity_not_just_the_root` 红。
//! M2. marker 不符时就地 clean 后继续用
//!     -> `mismatch_opens_a_fresh_generation_and_never_cleans_in_place` 红：
//!        就地 clean 会销毁「不匹配」这一事实本身的证据。
//! M3. 拒绝发生在建日志/spawn 之后
//!     -> `mismatch_is_refused_before_any_log_or_spawn` 红。
//! M4. trial 沿用本体超时 -> `trial_timeout_is_independent_and_defaults_to_three_times_base` 红。
//! M5. 把 trial target 注入 main / 任务 worktree / fixed review root
//!     -> `trial_slots_never_bind_to_a_fixed_review_root` 红（H88 的直接复发路径）。

use orch_host::buildcache::{
    resolve_trial_timeout_secs, BuildIdentity, CacheDecision, SlotKind, TrialSlot,
};

fn identity(root: &str, lock: &str) -> BuildIdentity {
    BuildIdentity {
        schema: 1,
        kind: SlotKind::TrialStaging,
        canonical_source_root: root.into(),
        canonical_common_dir: format!("{root}/.git"),
        rustc_version: "1.99.0".into(),
        cargo_version: "1.99.0".into(),
        target_triple: "aarch64-apple-darwin".into(),
        cargo_lock_sha256: lock.into(),
        build_config_digest: "cfg-0001".into(),
    }
}

#[test]
fn marker_binds_the_whole_build_identity_not_just_the_root() {
    let base = identity("/repo/.cowork-temp/trial-1", "lock-aaa");

    // 同 root、不同 lockfile ⇒ 必须判不匹配。只绑 root 的实现会在这里放行，
    // 而依赖树变了却复用旧 rmeta 正是 H88 的形状。
    let other_lock = identity("/repo/.cowork-temp/trial-1", "lock-bbb");
    assert!(
        !base.matches(&other_lock),
        "Cargo.lock 变化必须使缓存身份失效"
    );

    // 同 root 同 lock、不同 toolchain ⇒ 必须判不匹配。
    let mut other_rustc = base.clone();
    other_rustc.rustc_version = "1.98.0".into();
    assert!(!base.matches(&other_rustc), "toolchain 变化必须使缓存身份失效");

    let mut other_cfg = base.clone();
    other_cfg.build_config_digest = "cfg-0002".into();
    assert!(!base.matches(&other_cfg), "构建配置变化必须使缓存身份失效");

    assert!(base.matches(&identity("/repo/.cowork-temp/trial-1", "lock-aaa")));
}

#[test]
fn mismatch_opens_a_fresh_generation_and_never_cleans_in_place() {
    let existing = identity("/repo/.cowork-temp/trial-1", "lock-aaa");
    let incoming = identity("/repo/.cowork-temp/trial-2", "lock-aaa");

    match TrialSlot::decide(Some(&existing), &incoming) {
        CacheDecision::FreshGeneration { reason, .. } => {
            assert!(
                reason.contains("source_root") || reason.contains("identity"),
                "拒绝理由必须点名不匹配的字段，实际: {reason}"
            );
        }
        other => panic!("身份不符时必须另开空 generation，实际: {other:?}"),
    }

    // 非空目录但完全没有 marker：同样不得直接用，也不得就地 clean 后用。
    assert!(matches!(
        TrialSlot::decide(None, &incoming),
        CacheDecision::FreshGeneration { .. }
    ));

    // 身份一致才允许复用增量。
    assert!(matches!(
        TrialSlot::decide(Some(&incoming), &incoming),
        CacheDecision::ReuseIncremental
    ));
}

#[test]
fn mismatch_is_refused_before_any_log_or_spawn() {
    // 拒绝必须发生在建日志与 spawn 之前——否则「拒绝」这件事本身已经产生了副作用，
    // 且门日志里会出现一次根本没跑起来的构建记录。
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/buildcache.rs"),
    )
    .expect("读 src/buildcache.rs 失败");
    let decide = src
        .split("pub fn decide")
        .nth(1)
        .expect("decide 必须存在");
    for banned in ["create_dir_all", "spawn(", "File::create", "remove_dir_all"] {
        assert!(
            !decide.contains(banned),
            "decide 是纯判定，不得有副作用 `{banned}`（尤其不得就地 clean）"
        );
    }
}

#[test]
fn trial_slots_never_bind_to_a_fixed_review_root() {
    // H88 的直接复发路径：把 trial 的共享 target 注入固定审查现场。
    for forbidden in [
        SlotKind::MainWorktree,
        SlotKind::TaskWorktree,
        SlotKind::FixedReviewRoot,
    ] {
        assert!(
            !forbidden.may_share_trial_target(),
            "{forbidden:?} 绝不允许共享 trial target（H88）"
        );
    }
    assert!(SlotKind::TrialStaging.may_share_trial_target());
}

#[test]
fn trial_timeout_is_independent_and_defaults_to_three_times_base() {
    // 冷编译撞门是 H77：trial 跑在无缓存的新克隆里，却沿用为「有缓存工作树」定的预算。
    assert_eq!(resolve_trial_timeout_secs(1500, None), 4500, "缺省 = 本体 × 3");
    assert_eq!(
        resolve_trial_timeout_secs(1500, Some(2400)),
        2400,
        "显式配置键必须覆盖缺省"
    );
    // 本体预算不得被 trial 消耗：两者是独立预算，不是同一个池子。
    assert_eq!(resolve_trial_timeout_secs(600, None), 1800);
    // 溢出不得回绕成一个荒谬的小值（那会把门变成随机失败源）。
    assert!(resolve_trial_timeout_secs(u64::MAX, None) >= u64::MAX / 2);
}
