//! ═══ 红种子契约 · B38 ═══（落位: orch/crates/orch-host/tests/inbox_meta.rs，逐字节复制）
//! 预期红（redForm: compile）：inbox_meta::{Instruction, parse_instruction} 尚不存在
//!   （E0432 unresolved import + E0425），文件级编译红。
//! 背景：design/11 §4 INBOX 指令文件——可选 frontmatter(priority/round-hint) + 正文=自然语言指令。
//!   本棒做 frontmatter 解析纯函数(daemon 用 priority 给队列排序;round-hint 供规划参考)。
//! 变异清单（E9 下界）:
//!   M1 无 frontmatter 时不把整体当 body → ② 红；M2 priority 不解析(恒 None) → ① 红；
//!   M3 正文内的 --- 被当分隔符截断 → ③ 红
use orch_host::inbox_meta;

#[test]
fn parses_optional_frontmatter_and_trims_body() {
    // ① 有 frontmatter:抽 priority(u8)/round-hint(String),body=正文去首尾空白
    let src = "---\npriority: 3\nround-hint: r22\n---\n给 orch 加个 X 功能\n";
    let ins = inbox_meta::parse_instruction(src);
    assert_eq!(ins.priority, Some(3));
    assert_eq!(ins.round_hint.as_deref(), Some("r22"));
    assert_eq!(ins.body, "给 orch 加个 X 功能");
}

#[test]
fn absent_frontmatter_treats_whole_as_body() {
    // ② 无 frontmatter:整体即正文,priority/round-hint 皆 None
    let ins = inbox_meta::parse_instruction("直接写指令没有 frontmatter\n");
    assert_eq!(ins.priority, None);
    assert_eq!(ins.round_hint, None);
    assert_eq!(ins.body, "直接写指令没有 frontmatter");
}

#[test]
fn triple_dash_inside_body_is_preserved() {
    // ③ 只有首个 --- ... --- 是 frontmatter;正文内的 --- 原样保留,不被截断
    let src = "---\npriority: 1\n---\n第一行\n---\n第二行\n";
    let ins = inbox_meta::parse_instruction(src);
    assert_eq!(ins.priority, Some(1));
    assert_eq!(ins.body, "第一行\n---\n第二行");
}
