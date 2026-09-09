//! ═══ 红种子契约 · B70（util 百分比格式化纯函数 · 压测棒）═══
//! 落位: orch/crates/orch-host/tests/util_pct.rs（逐字节复制）
//! 预期红（redForm: compile）：`util::format_pct` 尚不存在 → error[E0425]。
//!
//! 契约（落在 orch_host::util，勿动 lib.rs、不新增依赖）：
//!   - pub fn format_pct(part: usize, whole: usize) -> String
//!       四舍五入到整数百分比并加 "%"；whole==0 ⇒ "0%"（不得除零 panic）。
//! 负向变异：M1 除零未防 / M2 不四舍五入(截断 33→…)差异 ⇒ 对应用例红。

use orch_host::util::format_pct;

#[test]
fn formats_rounded_percent() {
    assert_eq!(format_pct(3, 4), "75%");
    assert_eq!(format_pct(1, 3), "33%");
    assert_eq!(format_pct(2, 3), "67%");
}

#[test]
fn zero_whole_is_zero_pct() {
    assert_eq!(format_pct(0, 0), "0%");
    assert_eq!(format_pct(5, 0), "0%");
}
