//! ═══ 红种子契约 · B62（plan_waves 采用 glob/前缀感知交集）═══
//! 落位: orch/crates/orch-host/tests/preset_waves_glob_integration.rs（逐字节复制）
//! 预期红（redForm: assertion）：plan_waves 现按**精确相等**判交集，故被 glob 覆盖的
//! 具体路径与其 `dir/**` 父不冲突、被并进同一波 → 下列「分波」断言 FAIL。
//!
//! 背景：B56 已落地 `write_sets_overlap_glob`（glob/目录前缀感知）。本棒把 `plan_waves`
//! 的交集判据从私有精确匹配 `write_sets_overlap` 换成 `write_sets_overlap_glob`，
//! 使并行分波真正尊重 `dir/**` 写集覆盖——一个真实正确性升级。
//!
//! 契约（只改 preset.rs）：
//!  - plan_waves 第 19 行 `write_sets_overlap(...)` → `write_sets_overlap_glob(...)`；
//!  - 删除随之死代码的私有 `write_sets_overlap`（唯一调用点即第 19 行）；
//!  - 更新 plan_waves 头注释（不再是「精确相等」）。
//!  - **planner 授权的既有测试更新**（见任务卡）：内联 `exact_strings_do_not_expand_globs_or_prefixes`
//!    断言旧「不展开」契约，本升级后必然变红——按卡给出的精确新断言更新它（这是
//!    planner 明示的契约演进，非消红式弱化）。
//!
//! 本种子只含**判别性分波用例**（glob 感知与精确匹配结果不同者），全部当前红：
//!  M1 忽略 glob（仍精确相等）⇒ 三个分波断言全红；
//!  M2 glob 非递归（`/**` 只覆盖一层）⇒ recursive_glob_splits_deep_file 红；
//!  M3 多路径集合只按首条判交集 ⇒ glob_parent_splits_multiple_covered 红。

use orch_host::preset;

fn t(id: &str, ws: &[&str]) -> (String, Vec<String>) {
    (id.to_string(), ws.iter().map(|s| s.to_string()).collect())
}

#[test]
fn recursive_glob_splits_covered_file() {
    // src/** 覆盖 src/a.rs ⇒ 二者必须分到相邻两波（父在前）
    let waves = preset::plan_waves(&[t("W", &["src/**"]), t("F", &["src/a.rs"])]);
    assert_eq!(waves, vec![vec!["W".to_string()], vec!["F".to_string()]]);
}

#[test]
fn recursive_glob_splits_deep_file() {
    // ** 递归覆盖深层路径
    let waves = preset::plan_waves(&[t("W", &["src/**"]), t("F", &["src/deep/nested/mod.rs"])]);
    assert_eq!(waves, vec![vec!["W".to_string()], vec!["F".to_string()]]);
}

#[test]
fn glob_parent_splits_multiple_covered() {
    // P=x/** 覆盖 A、B；A 与 B 互不精确相等⇒不冲突，二者同在 P 的下一波
    let waves = preset::plan_waves(&[
        t("P", &["x/**"]),
        t("A", &["x/1.rs"]),
        t("B", &["x/2.rs"]),
    ]);
    assert_eq!(
        waves,
        vec![vec!["P".to_string()], vec!["A".to_string(), "B".to_string()]]
    );
}
