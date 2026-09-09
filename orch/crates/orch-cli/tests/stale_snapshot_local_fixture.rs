//! B274 · stale-snapshot 只使用本地物化树轮态 —— 冻结契约种子。
//!
//! **首红：compile `error[E0583]: file for module `stale_snapshot_local_fixture_support` not found`。**
//! 共享 support 同时被本种子与 `stale_binary_cli.rs` 声明；首红因此直接钉住唯一可观察接线。
//!
//! ## 合同边界
//!
//! `ORCH_BUILD_GIT_SHA` + `git read-tree` 仍负责物化真实构建树。本卡只把该物化树里的
//! **轮态改造成 scratch-local 状态**：从传入 root 里选最新 production-validated IR 模板；
//! 若模板已关闭，只在 scratch 内移除 `RoundClosed`，然后写该 scratch 的 CURRENT-ROUND。
//! 绝不从当前生产仓寻找“恰好开放”的轮，也不伪造一份无法通过 IR crosscheck 的新 IR。
//!
//! ## M 变异 ↔ 测试
//!
//! | M | 注入（载体均在 writeSet） | 必红用例 |
//! |---|---|---|
//! | M1 | 从 `stale_binary_cli.rs` 删除共享 support 声明、把选择逻辑搬回本文件 | `the_round_selection_lives_in_the_shared_support_module` |
//! | M2 | support 忽略传入 root，改读 `CARGO_MANIFEST_DIR` 对应生产仓 | `the_synthesized_round_is_absent_from_the_real_repository` |
//! | M3 | support 直接返回 `r90001`，不检查文件事实 | `the_selected_round_is_the_synthesized_one` |
//! | M4 | support 不写 scratch 的 `CURRENT-ROUND` | `the_selected_round_is_the_synthesized_one` |
//! | M5 | 删除“所有模板均已关闭”时的 scratch-local 重开 | `the_scenario_passes_with_every_round_closed_in_the_scratch_repo` |
//! | M6 | 把 build stamp / `read-tree` 物化改成合成输入 | `the_materialisation_still_comes_from_the_build_stamp` |

#![allow(dead_code)]

mod stale_snapshot_local_fixture_support;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

use stale_snapshot_local_fixture_support::prepare_local_valid_materialized_round;

const HOST: &str = "orch/crates/orch-cli/tests/stale_binary_cli.rs";
const SUPPORT: &str = "orch/crates/orch-cli/tests/stale_snapshot_local_fixture_support/mod.rs";
const SYNTHETIC_ROUND: &str = "r90001";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR 上溯三级应为仓根")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    fs::read_to_string(repo_root().join(rel))
        .unwrap_or_else(|error| panic!("读 {rel} 失败: {error}"))
}

fn module_declaration_count(source: &str) -> usize {
    source
        .lines()
        .filter(|line| line.trim() == "mod stale_snapshot_local_fixture_support;")
        .count()
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let root = repo_root()
            .join("orch/target/test-tmp")
            .join(format!("b274-{tag}-{}-{seq}", std::process::id()));
        fs::create_dir_all(&root).expect("创建 B274 scratch 失败");
        Self(root)
    }

    fn root(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn write_round(root: &Path, round: &str, closed: bool) {
    let dir = root.join("coordination/rounds").join(round);
    fs::create_dir_all(&dir).expect("创建合成 round 失败");
    fs::write(
        dir.join("ROUND-IR.yaml"),
        format!("schemaVersion: 2\nround: {round}\nrevision: 1\n"),
    )
    .expect("写合成 ROUND-IR 失败");

    let mut lines = vec![serde_json::json!({
        "id": format!("validated-{round}"),
        "type": "TaskValidated",
        "actor": "runtime:orch",
        "payload": {"validationDigest": "a".repeat(64)},
    })
    .to_string()];
    if closed {
        lines.push(
            serde_json::json!({
                "id": format!("closed-{round}"),
                "type": "RoundClosed",
                "actor": "runtime:orch",
                "payload": {},
            })
            .to_string(),
        );
    }
    fs::write(dir.join("events.jsonl"), format!("{}\n", lines.join("\n")))
        .expect("写合成 events 失败");
}

fn round_events(root: &Path, round: &str) -> Vec<Value> {
    fs::read_to_string(
        root.join("coordination/rounds")
            .join(round)
            .join("events.jsonl"),
    )
    .expect("读合成 events 失败")
    .lines()
    .filter(|line| !line.trim().is_empty())
    .map(|line| serde_json::from_str(line).expect("合成 events 不是合法 JSONL"))
    .collect()
}

fn assert_selected_files(root: &Path, selected: &str) {
    assert_eq!(selected, SYNTHETIC_ROUND, "必须选择最新的合成 IR 模板");
    let round_root = root.join("coordination/rounds").join(SYNTHETIC_ROUND);
    assert!(round_root.join("ROUND-IR.yaml").is_file());

    let events = round_events(root, SYNTHETIC_ROUND);
    assert!(events.iter().any(|event| {
        event["type"] == "TaskValidated"
            && event["actor"] == "runtime:orch"
            && event
                .pointer("/payload/validationDigest")
                .and_then(Value::as_str)
                .is_some_and(|digest| {
                    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
    }));
    assert!(
        events.iter().all(|event| event["type"] != "RoundClosed"),
        "scratch-local 模板必须处于开放态"
    );
    assert_eq!(
        fs::read_to_string(root.join("coordination/runtime/CURRENT-ROUND"))
            .expect("support 必须写 scratch CURRENT-ROUND")
            .trim(),
        SYNTHETIC_ROUND
    );
}

#[test]
fn the_round_selection_lives_in_the_shared_support_module() {
    let host = read(HOST);
    let support = read(SUPPORT);
    assert_eq!(
        module_declaration_count(&host),
        1,
        "stale_binary_cli.rs 必须以独立模块声明行引用共享 support 恰一次"
    );
    assert_eq!(
        module_declaration_count(include_str!("stale_snapshot_local_fixture.rs")),
        1,
        "种子必须以独立模块声明行引用同一共享 support 恰一次"
    );
    assert!(
        !host.contains("fn current_valid_materialized_round("),
        "旧的私有轮选择实现必须移出 host"
    );
    assert!(
        support.contains("pub fn prepare_local_valid_materialized_round("),
        "共享 support 必须承载唯一轮态准备入口"
    );
}

#[test]
fn the_selected_round_is_the_synthesized_one() {
    let scratch = Scratch::new("selected");
    write_round(scratch.root(), "r89999", true);
    write_round(scratch.root(), SYNTHETIC_ROUND, false);
    let selected =
        prepare_local_valid_materialized_round(scratch.root()).expect("合成开放轮必须可选");
    assert_selected_files(scratch.root(), &selected);
}

#[test]
fn the_synthesized_round_is_absent_from_the_real_repository() {
    assert!(
        !repo_root()
            .join("coordination/rounds")
            .join(SYNTHETIC_ROUND)
            .exists(),
        "r90001 只能存在于本用例的 scratch，真实仓不得已有同名轮"
    );
    let support = read(SUPPORT);
    for forbidden in ["CARGO_MANIFEST_DIR", "env!(", "repo_root()"] {
        assert!(
            !support.contains(forbidden),
            "共享 support 只能使用调用方传入的 scratch root，命中 {forbidden:?}"
        );
    }
}

#[test]
fn the_scenario_passes_with_every_round_closed_in_the_scratch_repo() {
    let scratch = Scratch::new("all-closed");
    write_round(scratch.root(), "r89999", true);
    write_round(scratch.root(), SYNTHETIC_ROUND, true);
    let selected = prepare_local_valid_materialized_round(scratch.root())
        .expect("全部模板已关闭时必须在 scratch 内明确重开最新有效模板");
    assert_selected_files(scratch.root(), &selected);
}

#[test]
fn the_scenario_passes_with_an_open_round_in_the_scratch_repo() {
    let scratch = Scratch::new("already-open");
    write_round(scratch.root(), "r89999", true);
    write_round(scratch.root(), SYNTHETIC_ROUND, false);
    let before = fs::read_to_string(
        scratch
            .root()
            .join("coordination/rounds/r90001/events.jsonl"),
    )
    .expect("读开放轮失败");
    let selected =
        prepare_local_valid_materialized_round(scratch.root()).expect("已有开放轮时必须成功");
    assert_selected_files(scratch.root(), &selected);
    let after = fs::read_to_string(
        scratch
            .root()
            .join("coordination/rounds/r90001/events.jsonl"),
    )
    .expect("回读开放轮失败");
    assert_eq!(before, after, "已有开放轮不得被无故改写");
}

#[test]
fn the_materialisation_still_comes_from_the_build_stamp() {
    let host = read(HOST);
    assert_eq!(
        host.matches("option_env!(\"ORCH_BUILD_GIT_SHA\")").count(),
        1,
        "stale fixture 必须继续使用编译期 build stamp"
    );
    assert!(
        host.contains("read-tree") && host.contains("&build_sha"),
        "物化必须继续由 git read-tree build_sha 驱动"
    );
    assert!(
        host.contains("prepare_local_valid_materialized_round(&"),
        "snapshot 场景必须把物化树根传给 scratch-local 轮态准备入口"
    );
}

#[test]
fn the_five_existing_case_names_remain_a_subset() {
    let host = read(HOST);
    for name in [
        "stale_state_change_is_rejected_and_override_is_audited",
        "stale_read_only_doctor_remains_available_and_warns",
        "stale_ledger_recover_distinguishes_dry_run_from_apply",
        "stale_snapshot_distinguishes_read_from_write",
        "missing_git_metadata_degrades_with_an_explanation",
    ] {
        assert!(
            host.contains(&format!("fn {name}(")),
            "既有 stale-binary 用例 {name} 不得删除或改名"
        );
    }
}
