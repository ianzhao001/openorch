//! ═══ 红种子契约 · B40 ═══（落位: orch/crates/orch-host/tests/inbox_priority.rs，逐字节复制）
//! 预期红（redForm: compile）：inbox::order_by_priority 尚不存在（E0425 cannot find function），文件级编译红。
//! 背景：design/11 §4——daemon 用 priority 给待处理队列排序(高优先先处理)。本棒做优先级排序纯函数。
//!   入参 = (文件名, 该文件 priority)对(priority 由 inbox_meta::parse_instruction 抽取,None=无 frontmatter/缺 priority)。
//!   序:priority 降序(高者先);None 排在所有显式 priority 之后;同 priority 按文件名升序(文件名含 unix_ts,即时序)。
//! 变异清单（E9 下界）:
//!   M1 priority 升序(低者先) → ① 红；M2 None 排到前面 → ② 红；M3 同优先级不按文件名 tiebreak → ③ 红
use orch_host::inbox;

#[test]
fn higher_priority_first_then_filename() {
    // ① priority 降序:5 先于 1;同为 5 时按文件名(时序)升序
    let entries = vec![
        ("1002-a.md".to_string(), Some(1u8)),
        ("1000-b.md".to_string(), Some(5u8)),
        ("1001-c.md".to_string(), Some(5u8)),
    ];
    assert_eq!(
        inbox::order_by_priority(entries),
        vec!["1000-b.md".to_string(), "1001-c.md".to_string(), "1002-a.md".to_string()]
    );
}

#[test]
fn none_priority_sorts_after_any_explicit() {
    // ② 无 priority(None)排在所有显式 priority 之后
    let entries = vec![
        ("1000-a.md".to_string(), None),
        ("1001-b.md".to_string(), Some(1u8)),
    ];
    assert_eq!(
        inbox::order_by_priority(entries),
        vec!["1001-b.md".to_string(), "1000-a.md".to_string()]
    );
}

#[test]
fn same_priority_orders_by_filename_ascending() {
    // ③ 同优先级按文件名升序(unix_ts 前缀 → 先到先处理)
    let entries = vec![
        ("1005-late.md".to_string(), Some(3u8)),
        ("1002-early.md".to_string(), Some(3u8)),
    ];
    assert_eq!(
        inbox::order_by_priority(entries),
        vec!["1002-early.md".to_string(), "1005-late.md".to_string()]
    );
}
