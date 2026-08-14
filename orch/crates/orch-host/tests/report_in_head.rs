//! ═══ 红种子契约 · B27 ═══（落位: orch/crates/orch-host/tests/report_in_head.rs，逐字节复制）
//! 预期红（redForm: compile）：mech::report_committed 尚不存在（E0425，文件级编译红）。
//! 背景：E15（r15/B24 实证）——收取链读 REPORT 走文件系统，未核「已 commit 在分支头」，
//!   未提交的盘上文件骗过④/④½直到 verifier 从 git 视角拦截。本棒补机检判据：
//!   输入=git ls-tree -r --name-only <分支HEAD> 输出，REPORT 相对路径必须精确在列。
//! 变异清单（E9 下界）: M1 恒 true → ② 红；M2 子串 contains 匹配 → ③ 红；M3 恒 false → ① 红
use orch_host::mech;

#[test]
fn present_exact_path_is_committed() {
    // ① 精确路径在树中 → true
    let tree = "README.md\ncoordination/rounds/r15/reports/B24-REPORT.md\nsrc/lib.rs\n";
    assert!(mech::report_committed(tree, "coordination/rounds/r15/reports/B24-REPORT.md"));
}

#[test]
fn absent_report_is_violation() {
    // ② 树中无 REPORT → false（E15 主场景：写盘未 commit）
    let tree = "README.md\nsrc/lib.rs\n";
    assert!(!mech::report_committed(tree, "coordination/rounds/r15/reports/B24-REPORT.md"));
}

#[test]
fn substring_lookalike_does_not_count() {
    // ③ 近似路径（后缀/前缀衍生名）不算——必须整行精确匹配，禁 contains 子串
    let tree = "coordination/rounds/r15/reports/B24-REPORT.md.bak\nprefix/coordination/rounds/r15/reports/B24-REPORT.md2\n";
    assert!(!mech::report_committed(tree, "coordination/rounds/r15/reports/B24-REPORT.md"));
}

#[test]
fn tolerates_crlf_and_blank_lines() {
    // ④ 行尾 \r 与空行容忍（git 输出经不同管道的鲁棒性）
    let tree = "\nREADME.md\r\ncoordination/rounds/r15/reports/B24-REPORT.md\r\n\n";
    assert!(mech::report_committed(tree, "coordination/rounds/r15/reports/B24-REPORT.md"));
}
