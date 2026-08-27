//! orch-host · IO 宿主层（M1）：git/worktree、Tier S 适配器 spawn、门执行、账本追加、run-task 编排。
//! 纪律：一切 git/文件操作钉死主仓绝对 root（errata E6）；步骤小步原子、失败即停（E7）；
//! 账本条目在动作成功后引用已验证值写入（E8）。

pub mod activity;
pub mod adapter;
pub mod agent_profile;
pub mod approval;
pub mod attempt;
pub mod binding;
pub mod buildcache;
pub mod budget;
pub mod card;
pub mod cas;
pub mod close;
pub mod collect;
pub mod consult;
pub mod cost;
pub mod gate;
pub mod failure;
pub mod gitx;
// H32 预置（r77，planner）：B293 的 writeSet 含 src/harness.rs 而 lib.rs 对执行者冻结，
// 无声明则该模块永不被编译、卡内结构性无法实现（B18/B238/B247/B273 同型先例）。
// 本行只保证可达性；实现属 B293，当前是可编译空壳。
pub mod harness;
pub mod hooks;
pub mod judge;
pub mod ledger;
pub mod liveness;
pub mod logrotate;
pub mod mech;
pub mod observed;
pub mod oracle;
pub mod fusion;
pub mod plan;
pub mod pricing;
// H32 预置（r66，planner）：B238 的 writeSet 含 src/reclaim.rs 而 lib.rs 对它冻结，
// 无声明则该模块永不被编译、卡内结构性无法实现。实现属 B237，本行只保证可达性。
pub mod reclaim;
pub mod round;
pub mod scaffold;
// r68/B247 planner 预置（H32）：与 src/stall.rs 的可编译空 stub 同一个原子提交落库，
// 使 B247 的落位种子红在 E0432 而非 E0583。实现由 B247 执行者填充。
pub mod stall;
pub mod runtask;
pub mod scheduler;
pub mod reconcile;
pub mod redact;
pub mod runloop;
pub mod inbox;
pub mod inbox_meta;
pub mod mcp;
pub mod preset;
pub mod probe;
pub mod quality;
pub mod registry;
pub mod serve;
pub mod sites;
pub mod chanhealth;
pub mod closed_round_audit;
pub mod snapshot;
// H32 预置（r72，planner）：B273 的 writeSet 含 src/srcshape.rs 而 lib.rs 对执行者冻结，
// 无声明则该模块永不被编译、卡内结构性无法实现（B18/B238/B247 同型先例）。
// 本行只保证可达性；实现属 B273，当前是可编译空壳。
pub mod srcshape;
pub mod staleness;
pub mod storage;
pub mod tierf;
pub mod util;
pub mod verify;
pub mod wake;
pub mod wave;

pub use close::{run_merge, run_record, MergeOutcome, RecordOutcome};
pub use runtask::{run_task, RunOutcome};
pub use verify::{run_verify, VerifyOutcome};

use std::path::Path;

use anyhow::{Context, Result};

/// 当前轮指针（runtime/CURRENT-ROUND）
pub fn current_round(root: &Path) -> Result<String> {
    Ok(std::fs::read_to_string(root.join("coordination/runtime/CURRENT-ROUND"))
        .context("CURRENT-ROUND 缺失")?
        .trim()
        .to_string())
}

/// BOARD.md 追加（人读账本，append-only 语义）
pub fn board_append(root: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(root.join("coordination/BOARD.md"))
        .context("打开 BOARD.md 失败")?;
    write!(f, "{text}")?;
    Ok(())
}
