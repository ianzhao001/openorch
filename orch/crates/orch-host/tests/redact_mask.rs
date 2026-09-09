//! ═══ 红种子契约 · B81（canary · redact 尾部保留打码纯函数）═══
//! 落位: orch/crates/orch-host/tests/redact_mask.rs（逐字节复制）
//! 预期红（redForm: compile）：`redact::mask_tail` 尚不存在 → error[E0425]。
//!
//! 背景：r40 是修复后 run-wave 首次真实双执行者并发 canary 轮。本棒（opencode）与 B80（codex，
//! util.rs）**写集不相交** → plan_waves 归同一波 → run-wave 波内**并发** collect（并行实证）。
//! 本棒轻量 additive：redact.rs 加一个尾部保留打码纯函数（纯 std，不新增依赖），不改既有函数/lib.rs。
//!
//! 契约（落在 orch_host::redact）：
//!   - pub fn mask_tail(s: &str, keep: usize) -> String
//!       保留末 `keep` 个**字符**，其余每字符替换为 '*'。`keep >= 字符数` ⇒ 原样返回。
//!       按字符计（多字节安全）。例：("secret123",3)→"******123"，("ab",5)→"ab"，("abc",0)→"***"。
//! 负向变异：M1 按字节而非字符/计数错 ⇒ masks_all_but_tail 红；M2 keep>=len 不返原样 ⇒
//!   keep_ge_len 红；M3 keep==0 未全打码 ⇒ keep_zero 红。

use orch_host::redact::mask_tail;

#[test]
fn masks_all_but_tail() {
    assert_eq!(mask_tail("secret123", 3), "******123");
}

#[test]
fn keep_ge_len_returns_original() {
    assert_eq!(mask_tail("ab", 5), "ab");
}

#[test]
fn keep_zero_masks_all() {
    assert_eq!(mask_tail("abc", 0), "***");
}
