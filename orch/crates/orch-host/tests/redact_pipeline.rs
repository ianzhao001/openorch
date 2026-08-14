//! ═══ 红种子契约 · B33 ═══（落位: orch/crates/orch-host/tests/redact_pipeline.rs，逐字节复制）
//! 预期红（redForm: compile）：redact::redact_full 尚不存在（E0425，文件级编译红）。
//! 背景：B15q(sk-/Bearer)与 B30(KV) 两张滤网已各自服役，但日志落盘尚未串联。
//!   本棒组合两网为单入口 redact_full，并在 adapter 日志写盘处接线。
//! 变异清单（E9 下界）: M1 只走 sk-/Bearer 网跳过 KV → ② 红；M2 只走 KV 网跳过 sk-/Bearer → ① 红；M3 组合后幂等破坏 → ④ 红
use orch_host::redact;

#[test]
fn bearer_and_sk_masked() {
    // ① sk-/Bearer 网生效
    assert_eq!(redact::redact_full("auth: Bearer abc123"), "auth: Bearer ***");
    assert_eq!(redact::redact_full("key=sk-live999 ok"), "key=sk-*** ok");
}

#[test]
fn kv_secrets_masked() {
    // ② KV 网生效
    assert_eq!(redact::redact_full("API_TOKEN=abc123"), "API_TOKEN=***");
}

#[test]
fn combined_line_both_masked() {
    // ③ 复合行两网都打、普通内容不动
    assert_eq!(
        redact::redact_full("GH_TOKEN=t9 Bearer xyz PATH=/bin"),
        "GH_TOKEN=*** Bearer *** PATH=/bin"
    );
}

#[test]
fn full_pipeline_idempotent() {
    // ④ 组合幂等：打码结果再过全网不变
    let once = redact::redact_full("A_SECRET=x Bearer y sk-z1");
    assert_eq!(redact::redact_full(&once), once);
}
