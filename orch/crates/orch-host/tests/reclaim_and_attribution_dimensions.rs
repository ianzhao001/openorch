#![allow(dead_code)]
//! B272 seeded-red contract：回收与归属的**判据维度**修正（H159 + H151 前半）。
//!
//! Expected red: **compile**。`buildcache::sweep_trial_cache_with_budget` 与
//! `gate::round_scoped_log_tag` 尚不存在 ⇒ `error[E0432]: unresolved import`。
//!
//! ── 两条缺陷同族：判据少了一个维度，于是某一类对象落进永久保留区 ──
//!
//! **H159（planner r71 开轮实测，第二次复发）**
//! `orch sites sweep-trial-cache` 的语义是「保留每个 slot 的**最新一代**」。
//! slot-03 只有一代（`generation-000000`）⇒ 它同时是最旧和最新 ⇒ **结构上永远被保留**，
//! 无论它有多大。r71 开轮实测：
//! ```text
//! $ orch sites sweep-trial-cache
//! orch sites sweep-trial-cache · keepLatest=1 · removed=0 · freedBytes=0
//! ```
//! 而该单目录当时是 **65 GiB**（planner 手工删除后 `.cowork-temp` 79G → 14G）。
//! 策略只有「代次」维度，**完全没有尺寸维度**。
//!
//! 这与 r69 成因 2（24h 挂钟 TTL 与收轮时刻结构性冲突）是**同一型**：
//! 那次是时间维度选错，这次是代次维度选错。
//!
//! **H151（r69 收轮实撞 23 条，r71 开轮复测 24 条）**
//! 门日志路径由 `format!("{tag}-gate-{name}.…")` 构造（`gate.rs:121/869/1028/1029` 四处），
//! `tag` 只含 taskId 与阶段、**不含 round**。同名卡跨轮重复出现时（B246/B247/B248 在 r68 与 r69 都存在），
//! 同一文件名被两轮同时主张 ⇒ `logrotate` 报
//! `CONFLICT …（同时被已收轮 r68/r69 认领，拒绝归属）`。
//!
//! **当前的 fail-closed 行为是对的**（拒绝归属而非错误归属，未污染任何一轮的收轮产物），
//! **但代价是这些门日志成为无主证据**——`orch cost` 与收轮产物核对都无法把它们计入。
//!
//! ── 本种子刻意不做的事 ──
//!
//! 只读 `TrialCacheSweepReport` 的字段、**从不构造**它（加字段不破坏冻结合同，H74）。
//! 不钉整段哈希；源码扫描进普通 `#[test]`；每条断言一个独立 `#[test]`（坑 16）。
//!
//! ── Negative mutations that must turn the named case red ──
//!
//! M1. 尺寸维度不生效：超预算的单代 slot 仍被保留
//!     -> `an_oversized_single_generation_slot_is_reclaimed` 红。
//! M2. 把闸修成全删：未超预算的最新代也被删（缓存永远冷）
//!     -> `a_slot_within_budget_keeps_its_newest_generation` 红。
//! M3. 回收了却不在报告里体现（又一次「只写 journal」）
//!     -> `a_budget_reclaim_is_visible_in_the_report` 红。
//! M4. **接线变异**：预算版存在但收轮路径不调用它
//!     -> `the_round_close_sweep_consults_the_budget_variant` 红。
//! M5. 门日志名仍不含 round
//!     -> `a_gate_log_tag_is_scoped_by_round` 红。
//! M6. **接线变异**：谓词存在但四处日志路径构造不消费它
//!     -> `the_gate_log_paths_consume_the_round_scoped_tag` 红。

use orch_host::buildcache::sweep_trial_cache_with_budget;
use orch_host::gate::round_scoped_log_tag;

const GATE_SRC: &str = include_str!("../src/gate.rs");
const CLOSE_SRC: &str = include_str!("../src/close.rs");

// ── H151：日志 tag 必须带轮号 ────────────────────────────────────────────

#[test]
fn a_gate_log_tag_is_scoped_by_round() {
    let scoped = round_scoped_log_tag("r71", "B248-A0001-root-verdict");
    assert!(
        scoped.contains("r71"),
        "B272 M5: 跨轮同名卡会让同一文件名被两轮同时主张——\
         r69 收轮实撞 23 条 CONFLICT，r71 开轮复测 24 条。实得 {scoped:?}"
    );
    assert!(
        scoped.contains("B248-A0001-root-verdict"),
        "B272: 加轮号不得吞掉原有 tag 信息，否则归因反而更难。实得 {scoped:?}"
    );
    assert_ne!(
        round_scoped_log_tag("r68", "B248"),
        round_scoped_log_tag("r69", "B248"),
        "B272 M5: 两轮的同名卡必须得到**不同**的日志 tag，这正是 CONFLICT 的根"
    );
}

#[test]
fn the_gate_log_paths_consume_the_round_scoped_tag() {
    assert!(
        GATE_SRC.contains("round_scoped_log_tag"),
        "B272 M6: 谓词写对了但四处 format!(\"{{tag}}-gate-…\") 不消费它，\
         日志名照旧跨轮碰撞——这是 r64/H116 的形态"
    );
}

// ── H159：回收必须有尺寸维度 ─────────────────────────────────────────────

#[test]
fn an_oversized_single_generation_slot_is_reclaimed() {
    let root = orch_host::util::test_scratch_dir("b272-oversized");
    let slot = root
        .join(".cowork-temp/trial-cache/slots/slot-00/generations/generation-000000/target");
    std::fs::create_dir_all(&slot).expect("B272: 建 slot 失败");
    std::fs::write(slot.join("blob.bin"), vec![0u8; 4096]).expect("B272: 写占位文件失败");

    // keep_latest=1（与收轮路径一致），但槽预算只有 1 KiB ⇒ 该单代必须被回收
    let report = sweep_trial_cache_with_budget(&root, 1, 1024).expect("B272: 带预算清扫失败");

    assert!(
        report.removed >= 1,
        "B272 M1: slot 只有一代时它同时是最旧和最新 ⇒ 结构上永远被保留，\
         无论它有多大（r71 实测 65 GiB、产品路径回收 0 字节）。实得 removed={}",
        report.removed
    );
}

#[test]
fn a_budget_reclaim_is_visible_in_the_report() {
    let root = orch_host::util::test_scratch_dir("b272-visible");
    let slot = root
        .join(".cowork-temp/trial-cache/slots/slot-00/generations/generation-000000/target");
    std::fs::create_dir_all(&slot).expect("B272: 建 slot 失败");
    std::fs::write(slot.join("blob.bin"), vec![0u8; 4096]).expect("B272: 写占位文件失败");

    let report = sweep_trial_cache_with_budget(&root, 1, 1024).expect("B272: 带预算清扫失败");
    assert!(
        report.freed_bytes > 0,
        "B272 M3: 回收了却不在报告里体现 = 又一次「拒收只写 journal」（r69 成因 3）；\
         收轮摘要必须让膨胀可见——今天它完全无声"
    );
}

#[test]
fn a_slot_within_budget_keeps_its_newest_generation() {
    let root = orch_host::util::test_scratch_dir("b272-within");
    let slot = root
        .join(".cowork-temp/trial-cache/slots/slot-00/generations/generation-000000/target");
    std::fs::create_dir_all(&slot).expect("B272: 建 slot 失败");
    std::fs::write(slot.join("blob.bin"), vec![0u8; 16]).expect("B272: 写占位文件失败");

    let report = sweep_trial_cache_with_budget(&root, 1, 1024 * 1024).expect("B272: 带预算清扫失败");
    assert_eq!(
        report.removed, 0,
        "B272 M2: 未超预算的最新代必须保留——把闸修成全删会让缓存永远冷，\
         那是比膨胀更贵的代价"
    );
}

// ── 接线变异（M4）：收轮路径必须用带预算的那一个 ─────────────────────────

#[test]
fn the_round_close_sweep_consults_the_budget_variant() {
    assert!(
        CLOSE_SRC.contains("sweep_trial_cache_with_budget"),
        "B272 M4: 收轮路径继续调用无预算的旧版，等于本卡什么都没修——\
         r71 开轮实测就是收轮跑过清扫、却回收 0 字节"
    );
}
