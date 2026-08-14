//! ═══ 红种子契约 · B69（redact 秘密探测纯函数 · 压测棒）═══
//! 落位: orch/crates/orch-host/tests/redact_has_secret.rs（逐字节复制）
//! 预期红（redForm: compile）：`redact::has_secret` 尚不存在 → error[E0425]。
//!
//! 契约（落在 orch_host::redact，勿动 lib.rs、不新增依赖）：
//!   - pub fn has_secret(line: &str) -> bool  ——  `redact_full(line) != line`（有内容被脱敏即 true）。
//! 负向变异：M1 恒返 false / M2 恒返 true ⇒ 对应用例红。

use orch_host::redact::has_secret;

#[test]
fn detects_masked_secret() {
    assert!(has_secret("API_KEY=sk-live-abc123"));
}

#[test]
fn plain_line_has_no_secret() {
    assert!(!has_secret("just a plain log line, nothing sensitive"));
}
