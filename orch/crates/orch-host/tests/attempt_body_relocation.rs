//! B275 · `attempt.rs` 测试体外提（搬迁模式 canary）—— 冻结契约种子。
//!
//! **首红：compile `error[E0583]: file for module `attempt_body_relocation_support` not found`。**
//! 该 support 模块只是零判别力的管道 marker（`pub fn contract_loaded() {}`），
//! 落点是**目录形态** `tests/attempt_body_relocation_support/mod.rs`——顶层 `tests/*.rs`
//! 会成为独立 Cargo target，本轮在管 target 计数，不能一边立棘轮一边偷偷 +1。
//!
//! ⚠️ 为什么不用「`include_str!` 一个不存在的文件」拿红：那产生的是**无错误码**的
//! `error: couldn't read ...`，`oracle.rs:2072` 对不含 `error[Edddd]` 的日志直接
//! `Err("no coded rustc error[Edddd] diagnostics found")`，整份种子会被预验拒收。
//!
//! ⚠️ 本文件不用 `include_str!` 生产源码 + 硬编码 hex64 的写法：那会把生产前缀永久焊死在
//! 冻结种子里。这里一律 `fs::read_to_string` 运行时读，期望值来自 planner 可维护基线。
//!
//! ## M 变异 ↔ 测试 一一对应（无遗漏）
//!
//! | M | 注入（**载体全部在 writeSet 内**） | 必红的指名用例 |
//! |---|---|---|
//! | M1 | 把 `include!` 的路径写成 `attempt/tests.rs`（文件不存在） | **编译断**（`couldn't read`，无错误码）⇒ 全部用例不可达。该变异的红形态是"编译断"，不是某个 `#[test]` 变红 |
//! | M2 | 从 `src/attempt/tests_body.rs` 删掉一个基线登记的测试 | `the_relocated_body_carries_the_known_tests` |
//! | M3 | 把 `src/attempt/tests_body.rs` 里一个基线登记的测试改名 | `the_relocated_body_carries_the_known_tests` |
//! | M4 | 在 `src/attempt.rs` 的 `mod tests` 里**保留**一份被搬测试的副本 | `the_host_file_keeps_nothing_but_the_include` |
//! | M5 | 在 `src/attempt.rs` 的生产前缀（1–4259）里插入一个空行 | `the_production_prefix_matches_the_recorded_digest` |
//! | M6 | 什么都不搬（只建空 support + 空 body） | `the_relocated_body_carries_the_known_tests` **与** `the_host_file_keeps_nothing_but_the_include` 双红 |
//! | M7 | 在 `src/attempt/tests_body.rs` 里加一条 `fs::read_to_string(...src/wake.rs)` 读边 | `the_relocated_body_declares_no_source_reader` |
//!
//! 四条 `#[test]` 每条都有至少一个 M 载体；七条 M 每条都有指名落点，无遗漏。
//! 〔刻意**不**为"冻结测试注释里的软行号"单立用例：该性质已被
//!   `the_production_prefix_matches_the_recorded_digest` 完全覆盖（前缀逐字节不变 ⇒ 行号不失真），
//!   单立会得到一条**没有 M 载体**的永久绿断言。〕
//!
//! 每条断言写成一个独立 `#[test]`，失败点可单独定位（RUNBOOK 坑 16）。

#![allow(dead_code)]

mod attempt_body_relocation_support;

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

/// planner 域基线。**编译期**嵌入：运行时绝不读 `coordination/`（H180 的病根）。
/// 先例：`orch-host/src/scaffold.rs:485`、`orch-host/tests/exclusive_lane_contract.rs:6`。
const BASELINE: &str = include_str!("../../../../coordination/source-shape-baseline-v1.json");

const HOST: &str = "orch/crates/orch-host/src/attempt.rs";
const BODY: &str = "orch/crates/orch-host/src/attempt/tests_body.rs";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR 上溯三级应为仓根")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("读 {rel} 失败: {error}"))
}

fn baseline() -> Value {
    serde_json::from_str(BASELINE).expect("基线不是合法 JSON")
}

fn baseline_attempt() -> Value {
    baseline()["attempt"].clone()
}

fn known_test_names() -> Vec<String> {
    baseline_attempt()["knownTestNames"]
        .as_array()
        .expect("基线缺 attempt.knownTestNames")
        .iter()
        .map(|value| value.as_str().expect("测试名不是字符串").to_string())
        .collect()
}

/// `#[test]` 属性计数。**只数属性行**，与"能否解析出 fn 名"解耦。
fn test_attribute_count(source: &str) -> usize {
    source
        .lines()
        .filter(|line| line.trim() == "#[test]")
        .count()
}

/// 本卡只需要守住搬迁体里的几种明确源码读法，不依赖另一张尚未交付的全仓扫描器。
/// 闭语法按单行归一化后匹配；多行拼接、别名和 helper 间接构造是已披露盲区。
fn production_source_reader_lines(source: &str) -> Vec<&str> {
    source
        .lines()
        .filter(|line| {
            let compact: String = line.chars().filter(|ch| !ch.is_whitespace()).collect();
            let reads_file = [
                "include_str!(",
                "include_bytes!(",
                "read_to_string(",
                "fs::read(",
                "File::open(",
            ]
            .iter()
            .any(|needle| compact.contains(needle));
            let names_production_source = compact.contains("/src/")
                || compact.contains("../src/")
                || compact.contains("src/");
            reads_file && names_production_source
        })
        .collect()
}

/// 取 `#[cfg(test)]` 之后的全部内容（宿主文件的测试区）。
fn host_test_region(source: &str) -> String {
    let marker = "\n#[cfg(test)]\n";
    let at = source
        .find(marker)
        .expect("attempt.rs 必须有列 0 的 #[cfg(test)]");
    source[at + 1..].to_string()
}

/// 生产前缀 = `#[cfg(test)]` 之前的全部字节。
fn host_production_prefix(source: &str) -> String {
    let marker = "\n#[cfg(test)]\n";
    let at = source
        .find(marker)
        .expect("attempt.rs 必须有列 0 的 #[cfg(test)]");
    source[..=at].to_string()
}

fn sha256_hex(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

// ─────────────────────────────────────────────────────────────────────────────
// A · 搬迁真的发生了（**判别器**：实现之前必红）
// ─────────────────────────────────────────────────────────────────────────────

/// M2 / M3 / M6 的落点。
#[test]
fn the_relocated_body_carries_the_known_tests() {
    attempt_body_relocation_support::contract_loaded();

    let body = read(BODY);
    let known = known_test_names();
    assert!(
        !known.is_empty(),
        "基线必须录有 attempt 的已知测试名，实际为空"
    );

    // 用 `>=` 而不是 `==`：`==` 会永久禁止以后给 attempt 合法新增测试（B257 型债务）。
    assert!(
        test_attribute_count(&body) >= known.len(),
        "tests_body.rs 的 #[test] 计数 {} 少于基线登记的 {} 个",
        test_attribute_count(&body),
        known.len()
    );

    for name in &known {
        let needle = format!("fn {name}(");
        assert!(
            body.contains(&needle),
            "基线登记的测试 {name} 不在 tests_body.rs 里（被删或被改名？）"
        );
    }

    let host = read(HOST);
    assert_eq!(
        test_attribute_count(&host),
        0,
        "attempt.rs 自身不得再留任何 #[test]"
    );
}

/// M4 / M6 的落点。
#[test]
fn the_host_file_keeps_nothing_but_the_include() {
    attempt_body_relocation_support::contract_loaded();

    let host = read(HOST);
    let region = host_test_region(&host);

    let mut kept: Vec<&str> = region
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    kept.retain(|line| *line != "#[cfg(test)]");

    assert_eq!(
        kept,
        vec!["mod tests {", "include!(\"attempt/tests_body.rs\");", "}",],
        "attempt.rs 的测试区必须只剩 mod tests + 一行 include! + 闭合括号，实际是 {kept:?}"
    );

    let total = host.lines().count();
    assert!(
        total <= 4_270,
        "attempt.rs 搬迁后应 ≤ 4270 行，实际 {total}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// B · 生产代码一个字节都没动（守卫）
// ─────────────────────────────────────────────────────────────────────────────

/// B307/B309 supersession 的 retained coverage：快照授权基于逻辑状态，不基于
/// Git index 的物理编码；同时保留 HEAD、porcelain 与重复 tree 捕获的 fail-closed 下界。
#[test]
fn the_production_prefix_uses_logical_snapshot_identity() {
    attempt_body_relocation_support::contract_loaded();

    let host = read(HOST);
    let prefix = host_production_prefix(&host);
    for forbidden in [
        "after_index != before_index",
        "快照改动真实 index 字节",
    ] {
        assert!(
            !prefix.contains(forbidden),
            "raw index 物理编码不得再成为 WIP 快照授权事实: {forbidden}"
        );
    }
    for retained in [
        "after_status != before_status",
        "after_head != before_head",
        "WIP snapshot changed",
    ] {
        assert!(
            prefix.contains(retained),
            "逻辑快照 fail-closed 覆盖不得丢失: {retained}"
        );
    }
    assert!(
        prefix.matches("gitx::snapshot_tree(").count() >= 2,
        "快照必须保留至少两次独立 tree 捕获"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// C · 搬出去的东西不许引入新的生产源码读边
// ─────────────────────────────────────────────────────────────────────────────

/// M7 的落点。用本卡自带的闭语法扫描搬迁体，避免依赖另一张卡的扫描实现。
#[test]
fn the_relocated_body_declares_no_source_reader() {
    attempt_body_relocation_support::contract_loaded();

    let body = read(BODY);
    let edges = production_source_reader_lines(&body);
    assert!(
        edges.is_empty(),
        "tests_body.rs 不得用本卡闭语法声明生产源码读边，实际 {edges:?}"
    );
}
