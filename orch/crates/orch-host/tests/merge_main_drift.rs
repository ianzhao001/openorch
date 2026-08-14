//! B172 seed (H46 ③+⑤) — merge 授权链放宽的纯判据契约。
//!
//! 背景：r57 三张双审 PASS 的卡只有一张合得进去。planner 在三条 verdict 之间插了
//! wake 并提交账本，三条裁决各自钉死在三个已成历史的 main 上；`validate_repo_tuple`
//! 要求 `actual_main == main_sha` 严格相等，而 B162 的写保护（正确地）不让 main 后退，
//! `orch dispatch` 在 approved 态又是纯空操作铸不出新 attempt ⇒ 两张卡整轮作废。
//!
//! 本 seed 钉死放宽之后**仍然必须成立**的安全性质。放宽的方向只有一个：
//! main 在裁决之后**只前进了纯协调产物**时，允许合并。除此之外一律仍拒。
//!
//! M1：删掉「区间不得含 `orch/**`」这一条 ⇒ `a_compilation_input_change_still_blocks_merge` 红。
//! M2：删掉「区间不得含本卡绑定的审查/证据文件」这一条 ⇒
//!     `a_tampered_review_after_verdict_still_blocks_merge` 红（**安全命门**）。
//! M3：删掉祖先判定（接受分叉）⇒ `a_forked_main_still_blocks_merge` 红。

use orch_host::verify::{classify_main_advance, MainAdvanceVerdict};

/// 本卡绑定的审查/证据路径（形态取自 r57 真实产物命名）。
fn bound_artifacts() -> Vec<String> {
    vec![
        "coordination/rounds/r58/reviews/B172-A0001-primary-executor-claw.md".to_string(),
        "coordination/rounds/r58/reviews/B172-A0001-secondary-executor-opencode.md".to_string(),
        "coordination/rounds/r58/evidence/B172-a-coordination-only-main-advance-no-longer-blocks-merge.json"
            .to_string(),
    ]
}

#[test]
fn an_unmoved_main_is_still_admitted() {
    // 零回归：`main_sha == actual_main` 是放宽后的平凡特例，必须继续通过。
    // 若实现把「相等」也一并改坏，本用例会红——放宽不等于换一套判据。
    let verdict = classify_main_advance("aaaa", "aaaa", true, &[], &bound_artifacts());
    assert_eq!(
        verdict,
        MainAdvanceVerdict::Unmoved,
        "main 未移动必须继续放行（零回归）"
    );
}

#[test]
fn a_coordination_only_advance_is_admitted() {
    // r57 的真实场景回放：裁决之后 planner 只提交了账本与审查产物之外的纯文书。
    // 这正是本卡要解开的那一格。
    let changed = vec![
        "coordination/rounds/r58/events.jsonl".to_string(),
        "coordination/HARDENING-BACKLOG.md".to_string(),
        "coordination/BOARD.md".to_string(),
    ];
    let verdict = classify_main_advance("aaaa", "bbbb", true, &changed, &bound_artifacts());
    assert_eq!(
        verdict,
        MainAdvanceVerdict::CoordinationOnly,
        "裁决后只前进纯协调产物必须放行——这是本卡存在的理由"
    );
}

#[test]
fn a_compilation_input_change_still_blocks_merge() {
    // M1：门是在 main_sha 上跑的。区间一旦碰了 orch/**，那份门结论就不再描述当前 main。
    // 放宽绝不能放宽到这里。
    let changed = vec![
        "coordination/BOARD.md".to_string(),
        "orch/crates/orch-host/src/verify.rs".to_string(),
    ];
    let verdict = classify_main_advance("aaaa", "bbbb", true, &changed, &bound_artifacts());
    assert_eq!(
        verdict,
        MainAdvanceVerdict::RefusedCompilationInput(
            "orch/crates/orch-host/src/verify.rs".to_string()
        ),
        "区间改动编译输入必须仍然拒绝，并点名是哪个文件"
    );
}

#[test]
fn a_tampered_review_after_verdict_still_blocks_merge() {
    // M2 —— **安全命门**。
    // verify.rs 读审查/证据字节用的是 `committed_regular_blob_bytes(root, main_sha, …)`，
    // 即 **main_sha 上**的版本。若区间内这些文件被改过而我们仍放行，
    // 等于允许「裁决通过 → 篡改审查 → 照样合并」。
    // 注意这条与 M1 是**独立**的：被改的文件全在 coordination/ 下，
    // 只看「有没有 orch/**」的实现会漏掉它，所以两条判据都必须存在。
    let tampered = "coordination/rounds/r58/reviews/B172-A0001-primary-executor-claw.md";
    let changed = vec!["coordination/BOARD.md".to_string(), tampered.to_string()];
    let verdict = classify_main_advance("aaaa", "bbbb", true, &changed, &bound_artifacts());
    assert_eq!(
        verdict,
        MainAdvanceVerdict::RefusedBoundArtifact(tampered.to_string()),
        "裁决后改动被绑定的审查文件必须仍然拒绝，并点名该文件"
    );
}

#[test]
fn a_forked_main_still_blocks_merge() {
    // M3：只接受顺推。分叉意味着裁决时那条历史已经不在 main 上，
    // 「区间」这个概念本身失去意义。
    let changed = vec!["coordination/BOARD.md".to_string()];
    let verdict = classify_main_advance("aaaa", "bbbb", false, &changed, &bound_artifacts());
    assert_eq!(
        verdict,
        MainAdvanceVerdict::RefusedNotAncestor,
        "钉点不是当前 main 的祖先时必须拒绝（不接受分叉）"
    );
}

#[test]
fn refusal_precedence_names_the_compilation_input_first() {
    // 一个区间可能同时踩两条。判据必须是确定性的：先报编译输入（更根本，
    // 它意味着门结论作废），再报绑定产物。不确定的报错顺序会让审查者无法复现。
    let tampered = "coordination/rounds/r58/reviews/B172-A0001-primary-executor-claw.md";
    let changed = vec![
        tampered.to_string(),
        "orch/crates/orch-host/src/wake.rs".to_string(),
    ];
    let verdict = classify_main_advance("aaaa", "bbbb", true, &changed, &bound_artifacts());
    assert_eq!(
        verdict,
        MainAdvanceVerdict::RefusedCompilationInput("orch/crates/orch-host/src/wake.rs".to_string()),
        "同时命中两条时必须优先报编译输入，且顺序确定"
    );
}
