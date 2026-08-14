//! ═══ 红种子契约 · B30 ═══（落位: orch/crates/orch-host/tests/redact_kv.rs，逐字节复制）
//! 预期红（redForm: compile）：redact::redact_kv_secrets 尚不存在（E0425，文件级编译红）。
//! 背景：B15q 落了 sk-/Bearer 滤网；本棒扩展 KV 形态——env 风格赋值里键名以
//!   TOKEN/SECRET/PASSWORD/KEY 结尾（不区分大小写）的，值打码为 ***；其余键原样。
//!   纪律同 B15q：纯 std，幂等（打码结果再过滤网不变）。
//! 变异清单（E9 下界）: M1 恒原样返回 → ① 红；M2 所有 KV 一律打码 → ② 红；M3 幂等破坏（对已打码值再包一层） → ③ 红
use orch_host::redact;

#[test]
fn secret_suffix_keys_are_masked() {
    // ① 敏感后缀键（大小写混合）→ 值打码
    assert_eq!(redact::redact_kv_secrets("API_TOKEN=abc123"), "API_TOKEN=***");
    assert_eq!(redact::redact_kv_secrets("db_password=hunter2"), "db_password=***");
}

#[test]
fn ordinary_keys_untouched() {
    // ② 普通键原样——打码面不许扩大
    assert_eq!(redact::redact_kv_secrets("PATH=/usr/bin:/bin"), "PATH=/usr/bin:/bin");
    assert_eq!(redact::redact_kv_secrets("RUST_LOG=debug"), "RUST_LOG=debug");
}

#[test]
fn idempotent_masking() {
    // ③ 幂等：打码结果再过滤网不变
    let once = redact::redact_kv_secrets("AWS_SECRET=xyz");
    assert_eq!(once, "AWS_SECRET=***");
    assert_eq!(redact::redact_kv_secrets(&once), once);
}

#[test]
fn multiple_pairs_in_one_line() {
    // ④ 同行多对：各自独立判定
    assert_eq!(
        redact::redact_kv_secrets("A_KEY=k1 PATH=/bin GH_TOKEN=t9"),
        "A_KEY=*** PATH=/bin GH_TOKEN=***"
    );
}
