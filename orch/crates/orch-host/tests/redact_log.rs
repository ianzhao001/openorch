//! ═══ 红种子契约 · B15q ══════════════════════════════════════════════
//! 落位: orch/crates/orch-host/tests/redact_log.rs （逐字节复制）
//! 预期红（redForm: compile）：orch_host::redact 为占位空模块，函数缺席。
//! 负向变异清单（E9 下界）:
//!   M1 删 sk- 前缀模式   → ① 红
//!   M2 删 Bearer 模式    → ② 红
//!   M3 打码后仍残留原文  → ①② 红（断言包含 *** 且不含原 secret）
//! ══════════════════════════════════════════════════════════════════
use orch_host::redact;

#[test]
fn redacts_api_key_prefix() {
    // ① sk- 系 API key 打码：保留前缀标识，密文不外泄
    let out = redact::redact_line("export OPENAI_KEY=sk-abc123XYZsecret888");
    assert!(out.contains("sk-***"), "应打码为 sk-***：{out}");
    assert!(!out.contains("abc123XYZsecret888"), "原密文不得残留：{out}");
}

#[test]
fn redacts_bearer_tokens() {
    // ② Authorization Bearer 打码
    let out = redact::redact_line("Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.payload.sig");
    assert!(out.contains("Bearer ***"), "应打码为 Bearer ***：{out}");
    assert!(!out.contains("eyJhbGciOiJIUzI1NiJ9"), "token 不得残留：{out}");
}

#[test]
fn benign_lines_untouched() {
    // ③ 无敏感内容原样
    assert_eq!(redact::redact_line("cargo test ok 53 passed"), "cargo test ok 53 passed");
}

#[test]
fn redaction_is_idempotent() {
    // ④ 幂等：二次打码不变
    let once = redact::redact_line("token=sk-abc123XYZsecret888");
    assert_eq!(redact::redact_line(&once), once);
}
