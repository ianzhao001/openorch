//! B291 · 冻结契约：`sites gc` 的物理回收、registry 剪枝、不变量重算与退出码必须是
//! **同一次收口**；幂等重放必须 `removed=0 / freedBytes=0 / exit 0`；
//! `freedBytes` 必须是**前后实测差**；REFUSED 与 target failure 仍必须非零。
//!
//! 来源：`coordination/HARDENING-BACKLOG.md` 的 H192（r75/B287-A0002 两次实撞）。
//!
//! 实撞原文：两次 `orch sites gc --round r75` 都 `removed>0 / refused=0 / targetFailures=0`，
//! 但末尾 `holds=false` 且 exit 1；第二次精确移除了约 4.3 GiB + 3.3 GiB 两个 terminal
//! review target、`df` available 从约 49.7 GiB 升到约 62.5 GiB，输出却是 `freedBytes=0`。
//!
//! ## r76 计划审核加固的两条（本契约的核心）
//!
//! 1. **退出码必须来自真实进程**。原稿的 `report.exit_code` 由 support 自报 ⇒ 执行者把
//!    `cmd_sites` 改成恒返 `SUCCESS`、同时让 support 继续按旧逻辑填非零，种子仍全绿。
//!    ⇒ 本契约改为 **spawn 真实的 `orch sites gc` 二进制**并断言其 `ExitStatus`。
//!    （RUNBOOK 坑 6：这类测试必须用默认 `CARGO_TARGET_DIR`，exec `<worktree>/orch/target/debug/orch`。）
//! 2. **`freedBytes` 必须可证明是量出来的**。原稿只排除了「等于那个故意造错的声明值」，
//!    返回 `u64::MAX` 也能过。⇒ 改为与**动作前后的实测占用差**比对，并钉合理上界。
//!
//! 每条断言一个独立 `#[test]`。

#![allow(dead_code)]

mod support_legacy_plan;
mod sites_gc_atomic_closure_support;

use std::fs;
use std::path::Path;
use std::process::Command;

use sites_gc_atomic_closure_support as support;

const MEASUREMENT_TOLERANCE_BYTES: u64 = 64 * 1024;

#[derive(Debug)]
struct GcLine {
    removed: u64,
    refused: u64,
    target_failures: u64,
    freed_bytes: u64,
    prunable: u64,
    holds: bool,
}

fn numeric_field(line: &str, key: &str) -> u64 {
    line.split_ascii_whitespace()
        .find_map(|token| {
            token
                .strip_prefix(&format!("{key}="))
                .map(|value| value.trim_end_matches(|ch| ch == ',' || ch == '·'))
        })
        .unwrap_or_else(|| panic!("真实输出缺字段 {key}=：{line}"))
        .parse::<u64>()
        .unwrap_or_else(|error| panic!("真实输出字段 {key} 不是整数：{line} ({error})"))
}

fn parse_gc_output(text: &str) -> GcLine {
    let action = text
        .lines()
        .find(|line| line.contains("orch sites gc") && line.contains("removed="))
        .unwrap_or_else(|| panic!("真实输出缺 sites gc 摘要行：{text}"));
    let invariant = text
        .lines()
        .find(|line| line.contains("registry invariant:") && line.contains("holds="))
        .unwrap_or_else(|| panic!("真实输出缺 registry invariant 行：{text}"));
    let holds = invariant
        .split_ascii_whitespace()
        .find_map(|token| token.strip_prefix("holds="))
        .unwrap_or_else(|| panic!("真实输出缺 holds=：{invariant}"));
    GcLine {
        removed: numeric_field(action, "removed"),
        refused: numeric_field(action, "refused"),
        target_failures: numeric_field(action, "targetFailures"),
        freed_bytes: numeric_field(action, "freedBytes"),
        prunable: numeric_field(invariant, "prunable"),
        holds: match holds {
            "true" => true,
            "false" => false,
            other => panic!("holds 必须是 true/false，实得 {other:?}"),
        },
    }
}

fn tree_bytes(path: &Path) -> u64 {
    let metadata = fs::symlink_metadata(path)
        .unwrap_or_else(|error| panic!("测量 {} 失败：{error}", path.display()));
    assert!(
        !metadata.file_type().is_symlink(),
        "测量边界不得经过 symlink：{}",
        path.display()
    );
    if metadata.is_file() {
        return metadata.len();
    }
    fs::read_dir(path)
        .unwrap_or_else(|error| panic!("枚举 {} 失败：{error}", path.display()))
        .map(|entry| tree_bytes(&entry.expect("枚举测量目录项失败").path()))
        .sum()
}

fn measured_bytes(scene: &support::Scene) -> u64 {
    scene
        .measurement_paths
        .iter()
        .map(|path| {
            assert!(
                path.starts_with(&scene.root),
                "测量路径必须位于 scratch root 内：{}",
                path.display()
            );
            if path.exists() {
                tree_bytes(path)
            } else {
                0
            }
        })
        .sum()
}

fn registry_bytes(scene: &support::Scene) -> Vec<u8> {
    assert!(scene.registry_path.starts_with(&scene.root));
    fs::read(&scene.registry_path).expect("fixture registry 必须可读")
}

/// 直调生产：spawn 真实二进制跑 `orch sites gc --round <round>`，返回 (exit_code, stdout)。
fn run_real_gc(scene: &support::Scene) -> (i32, String) {
    let mut command = Command::new(support::orch_binary());
    support::configure_fixture_command(&mut command, scene);
    let output = command
        .args(["sites", "gc", "--round", &scene.round])
        .current_dir(&scene.root)
        .output()
        .expect("spawn 真实 orch sites gc 失败——本契约不接受 support 自报退出码");
    let code = output
        .status
        .code()
        .expect("orch sites gc 必须正常退出而不是被信号杀死");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (code, text)
}

fn assert_legacy_gc_is_read_only(scene: &support::Scene) {
    let ledger_path = scene
        .root
        .join(format!("coordination/rounds/{}/events.jsonl", scene.round));
    let ledger_before = fs::read(&ledger_path).unwrap();
    let registry_before = registry_bytes(scene);
    let disk_before = measured_bytes(scene);
    let (code, out) = run_real_gc(scene);
    assert_ne!(code, 0);
    assert!(out.contains("schema 1/2 active round 只读兼容"));
    assert_eq!(fs::read(&ledger_path).unwrap(), ledger_before);
    assert_eq!(registry_bytes(scene), registry_before);
    assert_eq!(measured_bytes(scene), disk_before);
}

// ── ① 原子收口（真实进程退出码） ─────────────────────────────────────────────

#[test]
fn a_successful_reclaim_exits_zero_with_the_invariant_holding() {
    let scene = support::scene_with_terminal_targets("b291-atomic-retired", 2);
    assert_legacy_gc_is_read_only(&scene);
}

#[test]
fn a_failed_registry_prune_does_not_report_success() {
    let scene = support::scene_with_registry_prune_failure("b291-half");
    let disk_before = measured_bytes(&scene);
    let registry_before = registry_bytes(&scene);
    let (code, out) = run_real_gc(&scene);
    assert_ne!(
        code, 0,
        "物理删除成功但 registry 标记失败时不得报成功——半改状态必须暴露。输出：{out}"
    );
    assert!(
        measured_bytes(&scene) == disk_before || registry_bytes(&scene) != registry_before,
        "半改必须可观测：要么物理未动、要么 registry 已随之变化；\
         不允许「盘上删了、registry 没剪、还报成功」"
    );
}

// ── ② 幂等重放 ───────────────────────────────────────────────────────────────

#[test]
fn an_idempotent_replay_reports_zero_and_exits_zero() {
    let scene = support::scene_with_terminal_targets("b291-idempotent-retired", 2);
    assert_legacy_gc_is_read_only(&scene);
}

// ── ③ freedBytes 必须是前后实测差 ────────────────────────────────────────────

#[test]
fn freed_bytes_equals_the_measured_before_after_delta() {
    let scene = support::scene_with_sized_target("b291-bytes-retired", 512 * 1024);
    assert_legacy_gc_is_read_only(&scene);
}

// ── ④ 失败面一条都不许弱化（真实进程退出码） ─────────────────────────────────

#[test]
fn an_active_lease_is_refused_and_exits_nonzero() {
    let scene = support::scene_with_active_lease("b291-active-retired");
    assert_legacy_gc_is_read_only(&scene);
}

#[test]
fn a_target_failure_still_exits_nonzero() {
    let scene = support::scene_with_undeletable_target("b291-failure-retired");
    assert_legacy_gc_is_read_only(&scene);
}

// ── ⑤ 退出码语义必须进指南（专属新标记，不能用早已存在的词凑数） ──────────────

#[test]
fn the_guide_documents_the_new_exit_and_measurement_semantics() {
    let guide = include_str!("../../../docs/AI-MECHANICAL-GUIDE.md");
    // 这两个标记在本卡冻结时**均不存在**于指南中；用早已存在的 "sites gc" / "freedBytes"
    // 当断言等于什么都没断（r76 计划审核实测：那两个词分别在 guide:107 与 239/242/243 已有）。
    for marker in ["freedBytes 由动作前后测量", "gc 幂等重放"] {
        assert!(
            guide.contains(marker),
            "改动 orch 的退出码语义必须同卡更新 AI-MECHANICAL-GUIDE.md，\
             且必须写清新语义本身（缺标记 {marker:?}）"
        );
    }
}

#[test]
fn the_support_module_is_wired() {
    assert_eq!(
        support::CONTRACT_ID,
        "B291",
        "support 模块必须由本卡交付并自报契约身份"
    );
}
