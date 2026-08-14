//! ═══ 红种子契约 · B15p ══════════════════════════════════════════════
//! 落位: orch/crates/orch-host/tests/util_fmt.rs （逐字节复制）
//! 预期红（redForm: compile）：orch_host::util 为占位空模块，函数缺席。
//! 负向变异清单（E9 下界）:
//!   M1 truncate_middle 只截头不保尾 → ① 红
//!   M2 format_secs 丢小时段         → ③ 红
//!   M3 truncate_middle 对短串也截    → ② 红
//! ══════════════════════════════════════════════════════════════════
use orch_host::util;

#[test]
fn truncates_middle_keeping_both_ends() {
    // ① 总长≤max，头尾各保留，中间省略号 U+2026
    assert_eq!(util::truncate_middle("abcdefghijklmn", 9), "abcd…klmn");
}

#[test]
fn short_strings_untouched() {
    // ② 不超限原样返回
    assert_eq!(util::truncate_middle("abc", 9), "abc");
}

#[test]
fn formats_hours_minutes_seconds() {
    // ③ 3661s → 1h1m1s（零段省略）
    assert_eq!(util::format_secs(3661), "1h1m1s");
    assert_eq!(util::format_secs(61), "1m1s");
}

#[test]
fn formats_zero_and_seconds_only() {
    // ④ 边界
    assert_eq!(util::format_secs(0), "0s");
    assert_eq!(util::format_secs(59), "59s");
}
