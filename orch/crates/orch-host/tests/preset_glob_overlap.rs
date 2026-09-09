//! ═══ 红种子契约 · B56（Parallel 预设：glob/前缀感知 writeSet 交集原语）═══
//! 落位: orch/crates/orch-host/tests/preset_glob_overlap.rs（逐字节复制）
//! 预期红（redForm: compile）：下列 API 尚不存在 → error[E0432]/error[E0425]。
//!
//! 背景：B49 的 `plan_waves` 首切片按**路径字符串精确相等**判 writeSet 交集，函数头
//! 注释明确「glob 语义留后续切片」「不展开 glob，也不推断目录前缀」。本棒是该后续
//! 切片的**纯原语**：一个 glob/前缀感知的路径冲突判定 `write_sets_overlap_glob`，
//! **additive**——不改 `plan_waves`、不改其现有测试、不改私有 `write_sets_overlap`，
//! 不新增依赖。（后续切片再决定是否让 plan_waves 改用本原语。）
//!
//! 目标契约（落在 orch_host::preset，模块已 pub 导出，勿动 lib.rs）：
//!  - pub fn write_sets_overlap_glob(left: &[String], right: &[String]) -> bool
//!      · 存在 l ∈ left、r ∈ right 使二者冲突时返回 true，否则 false；
//!      · 单个路径冲突判据 path_pair_conflicts(a, b)：
//!          1. a == b（精确相等，向后兼容）⇒ 冲突；
//!          2. `dir/**` 递归 glob 与任何以 `dir/` 为前缀的具体路径冲突（对称：
//!             左右任一侧是 glob 均算）；
//!          3. 其余（两个不等的具体路径 / 不覆盖的 glob）⇒ 不冲突。
//!      · `**` 是递归的：`src/**` 覆盖 `src/a.rs` 与 `src/deep/nested.rs`。
//!
//! 负向变异下界（转绿后逐条自证）：
//!  M1 忽略 glob（退化成纯精确相等）
//!       ⇒ glob_conflicts_with_covered_concrete 红；
//!  M2 glob 判定不对称（只认 left 是 glob）
//!       ⇒ glob_overlap_is_symmetric 红；
//!  M3 glob 前缀误判（把不同目录也算冲突，或非递归）
//!       ⇒ glob_prefix_is_scoped_and_recursive 红。

use orch_host::preset::write_sets_overlap_glob;

fn v(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|s| s.to_string()).collect()
}

#[test]
fn glob_conflicts_with_covered_concrete() {
    // src/** 覆盖 src/main.rs ⇒ 冲突
    assert!(write_sets_overlap_glob(&v(&["src/**"]), &v(&["src/main.rs"])));
    // src/** 不覆盖 lib/main.rs ⇒ 不冲突
    assert!(!write_sets_overlap_glob(&v(&["src/**"]), &v(&["lib/main.rs"])));
    // 纯精确相等仍冲突（向后兼容）
    assert!(write_sets_overlap_glob(&v(&["a.rs"]), &v(&["a.rs"])));
    // 两个不等具体路径不冲突
    assert!(!write_sets_overlap_glob(&v(&["a.rs"]), &v(&["b.rs"])));
}

#[test]
fn glob_overlap_is_symmetric() {
    // glob 在 right 侧同样生效
    assert!(write_sets_overlap_glob(&v(&["src/main.rs"]), &v(&["src/**"])));
    assert!(write_sets_overlap_glob(&v(&["src/**"]), &v(&["src/main.rs"])));
}

#[test]
fn glob_prefix_is_scoped_and_recursive() {
    // 递归覆盖深层路径
    assert!(write_sets_overlap_glob(
        &v(&["src/**"]),
        &v(&["src/deep/nested/mod.rs"])
    ));
    // 前缀必须是目录边界：src/** 不冲突 srcbin/x.rs（非 src/ 前缀）
    assert!(!write_sets_overlap_glob(&v(&["src/**"]), &v(&["srcbin/x.rs"])));
    // 多路径集合里只要有一对冲突即 true
    assert!(write_sets_overlap_glob(
        &v(&["docs/readme.md", "src/**"]),
        &v(&["tests/t.rs", "src/util.rs"])
    ));
}
