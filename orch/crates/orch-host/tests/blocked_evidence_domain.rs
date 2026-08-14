//! ═══ 红种子契约 · B84（mech-check 合法 BLOCKED 证据白名单 · R-a）═══
//! 落位: orch/crates/orch-host/tests/blocked_evidence_domain.rs（逐字节复制）
//! 预期红（redForm: compile）：`mech::task_evidence_path` 尚不存在 → error[E0425]。
//!
//! 背景：执行者按协议写 `<task>-BLOCKED.md` 后，经 nudge 解阻完成时，该证据仍在分支。
//! path_guard 当前只对白名单 `<task>-REPORT.md` 例外，合法 BLOCKED 会被当作 protected/domain
//! 越界，逼执行者删除事故证据。本棒只放行“本 task REPORT 的精确 BLOCKED 兄弟路径”。
//!
//! 负向变异下界：
//! M1 仍只放行 REPORT → own_blocked_is_a_legal_task_evidence_path 红；
//! M2 用 contains/starts_with/suffix 宽匹配 → lookalikes_and_other_tasks_are_rejected 红；
//! M3 只加分类函数但 path_guard 未接线 → path_guard_allows_only_exact_task_evidence 红。

use orch_host::mech;

const REPORT: &str = "coordination/rounds/r41/reports/B84-REPORT.md";
const BLOCKED: &str = "coordination/rounds/r41/reports/B84-BLOCKED.md";

#[test]
fn own_blocked_is_a_legal_task_evidence_path() {
    assert!(mech::task_evidence_path(REPORT, REPORT));
    assert!(mech::task_evidence_path(BLOCKED, REPORT));
}

#[test]
fn lookalikes_and_other_tasks_are_rejected() {
    for path in [
        "coordination/rounds/r41/reports/B83-BLOCKED.md",
        "coordination/rounds/r41/reports/B84-BLOCKED.md.bak",
        "prefix/coordination/rounds/r41/reports/B84-BLOCKED.md",
        "coordination/rounds/r41/reports/B84-SUMMARY.md",
    ] {
        assert!(!mech::task_evidence_path(path, REPORT), "误放行 {path}");
    }
}

#[test]
fn malformed_report_path_only_allows_its_exact_value() {
    let malformed = "coordination/rounds/r41/reports/B84.md";
    assert!(mech::task_evidence_path(malformed, malformed));
    assert!(!mech::task_evidence_path(BLOCKED, malformed));
}

#[test]
fn path_guard_allows_only_exact_task_evidence() {
    let protected = vec!["coordination/**".to_string()];
    let frozen = vec!["coordination/**".to_string()];

    mech::path_guard(
        &[BLOCKED.to_string(), REPORT.to_string()],
        &[],
        &frozen,
        &protected,
        REPORT,
    )
    .expect("本任务 REPORT/BLOCKED 均应作为证据例外放行");

    let err = mech::path_guard(
        &["coordination/rounds/r41/reports/B83-BLOCKED.md".to_string()],
        &[],
        &frozen,
        &protected,
        REPORT,
    )
    .unwrap_err();
    assert!(err.to_string().contains("protected"));
}
