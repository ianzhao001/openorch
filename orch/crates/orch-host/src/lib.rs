//! orch-host · harness 调用通道、手动任务门与 Git/工作树/账本 IO。
//! 纪律：一切 git/文件操作钉死主仓绝对 root（errata E6）；步骤小步原子、失败即停（E7）；
//! 账本条目在动作成功后引用已验证值写入（E8）。

//! Retired automatic/task-control writers have no public import path.
//! ```compile_fail
//! use orch_host::wake::run_fresh_planner_wake;
//! ```
//! ```compile_fail
//! use orch_host::tierf::run_dispatch_signed_candidate;
//! ```
//! ```compile_fail
//! use orch_host::tierf::run_nudge;
//! ```
//! ```compile_fail
//! use orch_host::tierf::run_resume;
//! ```

pub mod activity;
pub mod adapter;
#[cfg(feature = "selfhost")]
pub mod agent_profile;
#[cfg(feature = "selfhost")]
pub mod attempt;
#[cfg(feature = "selfhost")]
pub mod binding;
#[cfg(feature = "selfhost")]
pub mod buildcache;
#[cfg(feature = "selfhost")]
pub mod budget;
#[cfg(feature = "selfhost")]
pub mod card;
#[cfg(feature = "selfhost")]
pub mod cas;
/// Unified code-owned harness invocation channel.
pub mod channel;
#[cfg(feature = "selfhost")]
pub mod close;
#[cfg(feature = "selfhost")]
pub mod collect;
pub mod consult;
/// Read-only invocation observation surface, independent of task execution controls.
pub mod observation;
#[cfg(feature = "selfhost")]
pub mod cost;
#[cfg(feature = "selfhost")]
pub mod gate;
pub mod failure;
pub mod gitx;
pub mod harness;
/// Immutable snapshots of gitignored local harness configuration.
pub mod harness_config;
#[cfg(feature = "selfhost")]
pub mod hooks;
#[cfg(feature = "selfhost")]
pub mod judge;
#[cfg(feature = "selfhost")]
pub mod ledger;
/// Fixed-commit compatibility readers for closed historical rounds.
#[cfg(feature = "selfhost")]
pub mod legacy;
#[cfg(feature = "selfhost")]
pub mod liveness;
#[cfg(feature = "selfhost")]
pub mod logrotate;
#[cfg(feature = "selfhost")]
pub mod mech;
#[cfg(feature = "selfhost")]
pub mod observed;
#[cfg(feature = "selfhost")]
pub mod oracle;
#[cfg(feature = "selfhost")]
pub mod fusion;
#[cfg(feature = "selfhost")]
pub mod plan;
#[cfg(feature = "selfhost")]
pub mod pricing;
#[cfg(feature = "selfhost")]
pub mod reclaim;
#[cfg(feature = "selfhost")]
pub mod round;
#[cfg(feature = "selfhost")]
pub mod stall;
#[cfg(feature = "selfhost")]
pub mod reconcile;
pub mod redact;
#[cfg(feature = "selfhost")]
pub mod preset;
pub mod probe;
#[cfg(feature = "selfhost")]
pub mod quality;
#[cfg(feature = "selfhost")]
pub mod registry;
#[cfg(feature = "selfhost")]
pub mod sites;
#[cfg(feature = "selfhost")]
pub mod chanhealth;
#[cfg(feature = "selfhost")]
pub mod closed_round_audit;
#[cfg(feature = "selfhost")]
pub mod snapshot;
pub mod staleness;
#[cfg(feature = "selfhost")]
pub mod storage;
#[cfg(feature = "selfhost")]
pub mod tierf;
pub mod util;
#[cfg(feature = "selfhost")]
pub mod verify;
/// Selfhost task/review layer; shared transport stays in `channel` for default builds.
#[cfg(feature = "selfhost")]
pub use channel::managed::selfhost as wake;
/// Generic, actorless review facts used by schema 3 rounds.
#[cfg(feature = "selfhost")]
pub mod generic_review;

#[cfg(feature = "selfhost")]
pub use close::{run_merge, run_record, MergeOutcome, RecordOutcome};
#[cfg(feature = "selfhost")]
pub use verify::{run_verify, VerifyOutcome};

use std::path::Path;

use anyhow::{Context, Result};

/// 当前轮指针（runtime/CURRENT-ROUND）
#[cfg(feature = "selfhost")]
pub fn current_round(root: &Path) -> Result<String> {
    Ok(std::fs::read_to_string(root.join("coordination/runtime/CURRENT-ROUND"))
        .context("CURRENT-ROUND 缺失")?
        .trim()
        .to_string())
}

/// BOARD.md 追加（人读账本，append-only 语义）
#[cfg(feature = "selfhost")]
pub fn board_append(root: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(root.join("coordination/BOARD.md"))
        .context("打开 BOARD.md 失败")?;
    write!(f, "{text}")?;
    Ok(())
}

/// Detect persisted selfhost inputs without parsing or trusting their contents.
/// Default callers use this before any invocation write; corrupt or dangling
/// markers cannot silently turn a selfhost project into a standalone project.
pub fn has_selfhost_state(root: &Path) -> Result<bool> {
    fn present(path: &Path) -> Result<bool> {
        match std::fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).with_context(|| format!("inspect selfhost marker {}", path.display())),
        }
    }
    for relative in ["coordination/runtime/CURRENT-ROUND", "coordination/PROJECT-BINDING.yaml"] {
        if present(&root.join(relative))? { return Ok(true); }
    }
    let rounds = root.join("coordination/rounds");
    if !present(&rounds)? { return Ok(false); }
    let metadata = std::fs::symlink_metadata(&rounds)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() { return Ok(true); }
    for entry in std::fs::read_dir(&rounds)? {
        let entry = entry?;
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() { return Ok(true); }
        if metadata.is_dir() && (present(&entry.path().join("ROUND-IR.yaml"))?
            || present(&entry.path().join("events.jsonl"))?) { return Ok(true); }
    }
    Ok(false)
}
