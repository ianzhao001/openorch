//! ═══ 红种子契约 · B87（双根 BLOCKED 选择）═══
//! 落位: orch/crates/orch-host/tests/blocked_dual_root.rs（逐字节复制）
//! 预期红（redForm: compile）：`tierf::select_current_blocked_path` 尚不存在 → error[E0432]。
//!
//! 负向变异下界：
//! M1 永远取 main 首项 → stale_main_cannot_mask_fresh_worktree 红；
//! M2 用 >= 接受同刻旧证据 → equal_marker_is_stale 红；
//! M3 无 marker 时丢弃证据 → missing_marker_remains_conservative 红；
//! M4 选最新 mtime 而不是候选顺序中首个 current → first_current_preserves_root_order 红。

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use orch_host::tierf::select_current_blocked_path;

fn t(seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
}

#[test]
fn stale_main_cannot_mask_fresh_worktree() {
    let candidates = vec![
        (PathBuf::from("/repo/main/B87-BLOCKED.md"), Some(t(10))),
        (
            PathBuf::from("/repo/.worktrees/B87/B87-BLOCKED.md"),
            Some(t(30)),
        ),
    ];
    assert_eq!(
        select_current_blocked_path(&candidates, Some(t(20))),
        Some(candidates[1].0.clone())
    );
}

#[test]
fn equal_marker_is_stale() {
    let candidates = vec![(PathBuf::from("equal"), Some(t(20)))];
    assert_eq!(select_current_blocked_path(&candidates, Some(t(20))), None);
}

#[test]
fn missing_marker_remains_conservative() {
    let candidates = vec![(PathBuf::from("present"), Some(t(1)))];
    assert_eq!(
        select_current_blocked_path(&candidates, None),
        Some(PathBuf::from("present"))
    );
}

#[test]
fn first_current_preserves_root_order() {
    let candidates = vec![
        (PathBuf::from("main-current"), Some(t(30))),
        (PathBuf::from("worktree-newer"), Some(t(40))),
    ];
    assert_eq!(
        select_current_blocked_path(&candidates, Some(t(20))),
        Some(PathBuf::from("main-current"))
    );
}
