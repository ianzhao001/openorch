//! ═══ 红种子契约 · B37 ═══（落位: orch/crates/orch-host/tests/inbox_state.rs，逐字节复制）
//! 预期红（redForm: compile）：inbox::slugify / InboxStage / stage_dir / relocate_target 尚不存在（E0433/E0425，文件级编译红）。
//! 背景：design/11 §4 INBOX 指令队列——一条指令一个文件,生命周期 inbox/ → processing/ → done/。
//! 变异清单（E9 下界）: M1 slugify 不截断超长 → ② 红；M2 stage_dir 三态串味 → ③ 红；M3 relocate_target 丢文件名 → ④ 红
use orch_host::inbox::{self, InboxStage};

#[test]
fn slugify_is_filesystem_safe_and_bounded() {
    // ① 指令文本 → 安全短 slug(非字母数字转 -,小写,截断上界)
    assert_eq!(inbox::slugify("给 orch 加个 X 功能!"), "orch-x");
    let long = "a".repeat(100);
    assert!(inbox::slugify(&long).len() <= 40);
    assert_eq!(inbox::slugify(""), "instruction"); // 空 → 兜底名
}

#[test]
fn stages_map_to_distinct_dirs() {
    // ② 三态目录互不串味
    assert_eq!(inbox::stage_dir(InboxStage::Pending), "coordination/inbox");
    assert_eq!(inbox::stage_dir(InboxStage::Processing), "coordination/inbox/processing");
    assert_eq!(inbox::stage_dir(InboxStage::Done), "coordination/inbox/done");
}

#[test]
fn relocate_preserves_filename_across_stages() {
    // ③ 状态转移保留文件名,只换目录
    let p = inbox::relocate_target("1784800000-orch-x.md", InboxStage::Processing);
    assert_eq!(p, "coordination/inbox/processing/1784800000-orch-x.md");
    let d = inbox::relocate_target("1784800000-orch-x.md", InboxStage::Done);
    assert_eq!(d, "coordination/inbox/done/1784800000-orch-x.md");
}

#[test]
fn next_stage_advances_pending_processing_done() {
    // ④ 状态推进单向:Pending→Processing→Done,Done 是终态
    assert_eq!(inbox::next_stage(InboxStage::Pending), Some(InboxStage::Processing));
    assert_eq!(inbox::next_stage(InboxStage::Processing), Some(InboxStage::Done));
    assert_eq!(inbox::next_stage(InboxStage::Done), None);
}
