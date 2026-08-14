//! ═══ 红种子契约 · B12 ═══════════════════════════════════════════════
//! 落位路径: orch/crates/orch-host/tests/protected_paths.rs （逐字节复制，不得改动）
//!
//! 预期红（redForm: compile，planner oracle 预验 2026-07-22）:
//!   编译红——`binding::Binding` 尚无 `scope`/`git` 字段、`mech::path_guard`
//!   尚不存在，本文件整体编译失败（error[E0599]/E0609 类），种子 3 用例全红。
//!   既有测试因整个 workspace 编译失败同样无法运行——编译红语义下
//!   「基线不受扰」判据以两侧同为编译红为等价（oracle cargo 方言 B11 已定义）。
//!
//! 负向变异自证清单（下界语义，errata E9）:
//!   M1 path_guard 删去 protectedPaths 命中检查   → ③ 红
//!   M2 binding scope 解析丢失（字段名拼错）      → ① 红
//!   M3 binding git.pushPolicy 解析丢失           → ② 红
//! ══════════════════════════════════════════════════════════════════

use orch_host::{binding, mech};

const BINDING_YAML: &str = r#"
project:
  ecosystems: [rust]
commands:
  testFast: {argv: [cargo, test]}
scope:
  protectedPaths: ["coordination/**", "design/**", "HANDOFF.md"]
  reviewRequiredPaths: ["orch/Cargo.toml"]
git:
  implementerMayCommit: true
  implementerMayMerge: false
  pushPolicy: forbidden
  mergePolicy: ff-only-else-no-ff
"#;

#[test]
fn parses_scope_protected_paths() {
    let b: binding::Binding = serde_yaml::from_str(BINDING_YAML).expect("binding parses");
    assert!(b.scope.protected_paths.iter().any(|p| p == "coordination/**"));
    assert!(b.scope.protected_paths.iter().any(|p| p == "HANDOFF.md"));
    assert_eq!(b.scope.review_required_paths, vec!["orch/Cargo.toml"]);
}

#[test]
fn parses_git_push_policy() {
    let b: binding::Binding = serde_yaml::from_str(BINDING_YAML).expect("binding parses");
    assert_eq!(b.git.push_policy, "forbidden");
    assert!(!b.git.implementer_may_merge);
}

#[test]
fn path_guard_rejects_protected_touch_but_allows_report() {
    let protected = vec!["coordination/**".to_string(), "design/**".to_string()];
    let write_set = vec!["orch/crates/orch-host/src/mech.rs".to_string()];
    let report_rel = "coordination/rounds/r8/reports/B12-REPORT.md";

    // 域内文件 + REPORT 例外：放行
    mech::path_guard(
        &["orch/crates/orch-host/src/mech.rs".to_string(), report_rel.to_string()],
        &write_set,
        &[],
        &protected,
        report_rel,
    )
    .expect("in-set + report passes");

    // 触碰 protectedPaths（design/ 下任意文件）：拒绝
    let err = mech::path_guard(
        &["design/00-overview-and-decisions.md".to_string()],
        &["design/00-overview-and-decisions.md".to_string()], // 即使 writeSet 声明了也不放行
        &[],
        &protected,
        report_rel,
    )
    .expect_err("protected touch must be rejected");
    assert!(err.to_string().contains("protected"), "错误信息应指明 protected：{err}");
}
