//! B289 · 冻结契约：B285 的 amendment transaction 夹具**不得**把调用者主仓的 live 仓态
//! 当成测试输入，而真实的 barrier / drift / 未签核 / 已收轮拒绝语义**一条都不许放宽**。
//!
//! 来源：`coordination/HARDENING-BACKLOG.md` 的 H193，外加 r76 开轮前实撞的第二个触发。
//!
//! 现场根因（复核到卡面冻结时的 HEAD）：
//! `orch/crates/orch-host/tests/audited_agent_pin_amendment_support/mod.rs:56-89` 的
//! `fixture_root` 从 `repo_root().join("coordination")` 复制 `agents.yaml` /
//! `PROJECT-BINDING.yaml` / `frozen-contract-baseline-v1.json` / `modes/relay-selfhost.yaml`
//! 与整个 `rounds/r75`（`const ROUND: &str = "r75"` 硬编码），再要求这份克隆通过
//! `require_active_round_ir`。于是调用者仓的**任何**合法演进都会打红一个与它无关的用例；
//! 而 r75 已闭轮、永不再 `orch plan` ⇒ 这条红是**永久**的。
//!
//! ## 本契约的两层防线（r76 计划审核后加强）
//!
//! 计划审核指出：**只做源码标记扫描是可以被混淆绕过的**——把
//! `repo_root().join("coordination")` 写成 `Path::new("coordination")`、把 `"r75"` 写成
//! `format!("r{}", 75)`，扫描就失效而 live 耦合仍在。因此本契约把判据做成两层：
//!
//! - **第一层（便宜）**：源码窗口标记扫描，抓最直白的回退。
//! - **第二层（不可混淆绕过）**：**行为独立性**——合成现场的 registry 字节必须与调用者仓
//!   **不同**，且合成轮名在调用者仓里**根本不存在**。只要还在复制 live 仓，这两条就红。
//!
//! 另一条纪律：**support 只负责造现场，所有判定必须来自生产 `pub` 入口**
//! （`orch_host::plan::require_active_round_ir` / `orch_host::registry::run_agent_pin_amendment`），
//! 不得由 support 自报结论。
//!
//! 本契约钉的是**窄不变量**，**不钉整段字节哈希**——整段哈希钉死正是 H182 / H190 / B284
//! 三个死结的成因（RUNBOOK 坑 19 连带）。每条断言一个独立 `#[test]`。

#![allow(dead_code)]

mod b289_fixture_round_isolation_support;

use std::path::{Path, PathBuf};

use b289_fixture_round_isolation_support as support;
use orch_core::{read_ledger, EventRecord};

/// 被约束的夹具源码（B285 support 模块）。窗口扫描，不是哈希钉死。
const B285_SUPPORT_SOURCE: &str = include_str!("audited_agent_pin_amendment_support/mod.rs");

/// r75/B285 的冻结落位种子。本卡只读它、不改它。
const B285_FROZEN_SEED_SOURCE: &str = include_str!("audited_agent_pin_amendment.rs");

/// 真实轮名不得出现在夹具的轮常量里——尤其不得是已闭轮的 r75。
const FORBIDDEN_ROUND_LITERALS: [&str; 3] = ["\"r75\"", "\"r74\"", "\"r76\""];

/// 调用者仓根。只用于**证明合成现场与它无关**，不作为任何夹具输入。
fn live_repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR 上溯三级应为仓根")
        .to_path_buf()
}

fn ledger_len(root: &Path) -> usize {
    ledger_events(root).len()
}

fn registry_bytes(root: &Path) -> Vec<u8> {
    std::fs::read(root.join("coordination/agents.yaml"))
        .expect("合成现场必须包含可读的 coordination/agents.yaml")
}

fn ledger_events(root: &Path) -> Vec<EventRecord> {
    let path = root.join(format!(
        "coordination/rounds/{}/events.jsonl",
        support::SYNTHETIC_ROUND
    ));
    let ledger = read_ledger(&path).expect("合成现场账本必须可读");
    assert!(ledger.bad_lines.is_empty(), "合成现场账本不得有坏行");
    ledger.events
}

// ── 第一层：源码标记扫描（便宜，抓直白回退） ─────────────────────────────────

#[test]
fn the_b285_fixture_never_copies_a_live_repository_source() {
    let source = B285_SUPPORT_SOURCE;
    for marker in [
        "repo_root().join(\"coordination\")",
        "source.join(\"rounds\")",
        "join(\"PROJECT-BINDING.yaml\")",
    ] {
        assert!(
            !source.contains(marker),
            "B285 夹具仍在复制调用者主仓的 live 输入（命中标记 {marker:?}）。\
             夹具必须自己合成现场——把调用者当前业务状态当测试输入，\
             会让任何一次合法的编组演进永久打红一个无关用例（H193 第二触发）。"
        );
    }
}

#[test]
fn the_b285_fixture_pins_no_real_round_name() {
    for literal in FORBIDDEN_ROUND_LITERALS {
        assert!(
            !B285_SUPPORT_SOURCE.contains(literal),
            "B285 夹具把真实轮名 {literal} 当成常量。已闭轮的 IR 永远不会再 orch plan，\
             硬编码它等于把这条红焊死。"
        );
    }
}

// ── 第二层：行为独立性（不可混淆绕过） ───────────────────────────────────────

#[test]
fn the_synthesized_root_shares_no_registry_bytes_with_the_live_repository() {
    let root = support::synthesize_signed_round("b289-independence");
    let synthesized = registry_bytes(&root);
    let live = std::fs::read(live_repo_root().join("coordination/agents.yaml"))
        .expect("读调用者仓 agents.yaml 失败");
    assert_ne!(
        synthesized, live,
        "合成现场的 registry 与调用者主仓逐字节相同 ⇒ 它就是复制来的。\
         源码标记扫描可以被 Path::new / concat! 混淆绕过，这条不行：\
         只要还在拷 live 仓，它就红。"
    );
    assert!(
        !synthesized.is_empty(),
        "合成现场必须真的有一份 registry，不能用空文件躲开上面的不等断言"
    );
}

#[test]
fn the_synthetic_round_is_absent_from_the_live_repository() {
    let live_round_dir = live_repo_root()
        .join("coordination/rounds")
        .join(support::SYNTHETIC_ROUND);
    assert!(
        !live_round_dir.exists(),
        "合成轮名 {:?} 在调用者仓里真实存在（{}）⇒ 夹具仍可能在复制那个轮目录。\
         合成轮名必须是仓里根本没有的名字。",
        support::SYNTHETIC_ROUND,
        live_round_dir.display()
    );
}

// ── 合成现场必须自成一体且真的“已签核且活跃”（判定来自生产入口） ─────────────

#[test]
fn a_signed_active_round_is_synthesized_without_any_repository_input() {
    let root = support::synthesize_signed_round("b289-synth-active");
    let events = ledger_events(&root);
    orch_host::plan::require_active_round_ir(&root, support::SYNTHETIC_ROUND, &events)
        .expect("合成现场必须是一个已签核且活跃的轮；它不得依赖调用者主仓");
}

#[test]
fn the_synthesized_round_is_deterministic() {
    let left = support::synthesize_signed_round("b289-determinism-a");
    let right = support::synthesize_signed_round("b289-determinism-b");
    assert_eq!(
        registry_bytes(&left),
        registry_bytes(&right),
        "同一构造器两次生成的 registry 必须逐字节相同，否则变异自证无法定位"
    );
}

// ── 真实拒绝语义一条都不许放宽（全部直调生产 pub 入口） ──────────────────────

#[test]
fn an_unresolved_merge_barrier_still_refuses_the_amendment() {
    let root = support::synthesize_signed_round("b289-barrier");
    support::inject_unresolved_merge_barrier(&root, "B999");
    let before = registry_bytes(&root);
    let events_before = ledger_len(&root);

    let outcome = orch_host::registry::run_agent_pin_amendment(
        &root,
        support::AMENDABLE_AGENT,
        None,
        Some("b289-barrier-canary"),
        None,
        "B289 barrier 负例",
    );

    assert!(
        outcome.is_err(),
        "现场存在未闭合 MergeStarted 屏障时必须拒绝修订；夹具不得为了绿门放宽它"
    );
    assert_eq!(
        registry_bytes(&root),
        before,
        "被拒的修订不得改动 registry 字节"
    );
    assert_eq!(ledger_len(&root), events_before, "被拒的修订不得写账本");
}

#[test]
fn a_hand_edited_registry_still_drifts_without_an_amendment_event() {
    let root = support::synthesize_signed_round("b289-drift");
    support::hand_edit_registry_without_event(&root);
    let events = ledger_events(&root);
    assert!(
        orch_host::plan::require_active_round_ir(&root, support::SYNTHETIC_ROUND, &events).is_err(),
        "手改 registry 而不落 AgentPinAmended 必须仍然失配——\
         这条证明的是事件在起作用，而不是校验被关掉。\
         注意夹具**不得**在手改后顺手把 IR 摘要一起改掉来掩盖漂移。"
    );
}

#[test]
fn an_unsigned_or_closed_round_still_refuses_the_amendment() {
    for label in ["b289-unsigned", "b289-closed"] {
        let root = if label == "b289-unsigned" {
            support::synthesize_unsigned_round(label)
        } else {
            support::synthesize_closed_round(label)
        };
        let before = registry_bytes(&root);
        let outcome = orch_host::registry::run_agent_pin_amendment(
            &root,
            support::AMENDABLE_AGENT,
            None,
            Some("b289-state-canary"),
            None,
            "B289 状态负例",
        );
        assert!(outcome.is_err(), "{label}: 未签核 / 已收轮的轮必须拒绝修订");
        assert_eq!(
            registry_bytes(&root),
            before,
            "{label}: 被拒的修订不得改动 registry 字节"
        );
    }
}

// ── 合法路径仍然走得通（证明上面几条不是靠“全都拒绝”蒙混过关）────────────────

#[test]
fn a_legal_amendment_on_the_synthesized_round_still_commits() {
    let root = support::synthesize_signed_round("b289-happy");
    let events_before = ledger_len(&root);
    orch_host::registry::run_agent_pin_amendment(
        &root,
        support::AMENDABLE_AGENT,
        None,
        Some("b289-happy-canary"),
        None,
        "B289 正例",
    )
    .expect("合成的已签核活跃轮上，合法修订必须成功提交");
    assert!(
        ledger_len(&root) > events_before,
        "成功的修订必须落一条 durable 事件"
    );
}

// ── 冻结落位种子一个字节都不许动，接线也不许断 ──────────────────────────────

#[test]
fn the_frozen_b285_seed_is_still_wired_to_its_support_module() {
    assert!(
        B285_FROZEN_SEED_SOURCE.contains("mod audited_agent_pin_amendment_support;"),
        "r75/B285 的冻结落位种子必须仍然声明它的 support 模块；\
         接线断了等于把 B285 的整份契约悄悄下架。\
         （该文件的**字节不变**由 oracle::tests::signed_landed_seed_manifest_matches_the_current_tree\
         独立打红，本条只保证接线。）"
    );
}

#[test]
fn the_support_module_is_wired() {
    assert_eq!(
        support::CONTRACT_ID,
        "B289",
        "support 模块必须由本卡交付并自报契约身份"
    );
}
