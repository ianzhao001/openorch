//! ═══ 红种子契约 · B80（canary · util 计数复数化纯函数）═══
//! 落位: orch/crates/orch-host/tests/util_count.rs（逐字节复制）
//! 预期红（redForm: compile）：`util::format_count` 尚不存在 → error[E0425]。
//!
//! 背景：r40 是修复后 run-wave 首次真实双执行者并发 canary 轮。本棒（codex）与 B81（opencode，
//! redact.rs）**写集不相交** → plan_waves 归同一波 → run-wave 波内**并发** collect（并行实证）。
//! 本棒轻量 additive：util.rs 加一个复数化计数纯函数，不改既有函数/依赖/lib.rs。
//!
//! 契约（落在 orch_host::util）：
//!   - pub fn format_count(n: usize, unit: &str) -> String
//!       返回 "{n} {unit}"，n != 1 时 unit 加 "s"（n==1 单数）。例：(1,"file")→"1 file"，
//!       (3,"file")→"3 files"，(0,"file")→"0 files"。
//! 负向变异：M1 不复数化（恒单数）⇒ plural 红；M2 n==0 当单数 ⇒ zero_is_plural 红。

use orch_host::util::format_count;

#[test]
fn singular() {
    assert_eq!(format_count(1, "file"), "1 file");
}

#[test]
fn plural() {
    assert_eq!(format_count(3, "file"), "3 files");
}

#[test]
fn zero_is_plural() {
    assert_eq!(format_count(0, "file"), "0 files");
}
