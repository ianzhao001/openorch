//! B193 · H48 应急直修（`14ec008`，合入 `728df89`）的**转正审计留证**。
//!
//! backlog 对这笔的转正要求是「逐行审计，**不得只把既有测试再跑一遍**」。因此本文件
//! 刻意**不复用** `tests/merge_boundary_recovery.rs`（那 822 行是补丁自带的测试），
//! 而是从**外部**、按补丁所**声称的不变量**重新写一遍判据：授权边界、合并提交形状、
//! 放宽面的上界、历史链校验。两套测试若对同一不变量给出不同结论，就是审计要抓的东西。
//!
//! 首红形态：**compile**。本文件顶部声明的 `support_boundary` 模块尚不存在
//! （目标文件 `orch/crates/orch-host/tests/support_boundary/mod.rs`），rustc 报
//! `error[E0583]: file not found for module `support_boundary``。
//! 不得以建空壳、改本文件、或把本测试排除出门的方式伪造红绿。
//!
//! **被审对象（`verify.rs` / `close.rs` / `seal.sh`）全在 frozenPaths 内，本卡不改一个字节。**
//! scratch 一律落在本 worktree 的 `orch/target/test-tmp`。

mod support_boundary;

use orch_host::verify::{
    classify_main_advance, validate_merge_commit_shape,
    validate_root_boundary_recovery_authorization, MainAdvanceVerdict,
};
use support_boundary::{event, LedgerDraft, RepoFixture};

/// 放宽面的**上界**：H46/H48 把 merge 的 main 判据从「严格相等」放宽为
/// 「钉点是当前 main 的祖先 **且** 区间内只有协调产物」。本用例逐格锁死这条线——
/// 放宽只对协调产物成立，任何编译输入或被绑定产物的推进都必须被拒。
#[test]
fn main_advance_relaxation_admits_coordination_only_and_nothing_else() {
    let coordination = vec![
        "coordination/rounds/r60/events.jsonl".to_string(),
        "coordination/BOARD.md".to_string(),
        "coordination/CURRENT.md".to_string(),
    ];

    assert!(
        matches!(
            classify_main_advance("abc", "abc", true, &[], &[]),
            MainAdvanceVerdict::Unmoved
        ),
        "main 未动必须判 Unmoved"
    );
    assert!(
        matches!(
            classify_main_advance("abc", "def", true, &coordination, &[]),
            MainAdvanceVerdict::CoordinationOnly
        ),
        "区间内只有协调产物时才允许放宽"
    );
    assert!(
        matches!(
            classify_main_advance("abc", "def", false, &coordination, &[]),
            MainAdvanceVerdict::RefusedNotAncestor
        ),
        "钉点不是当前 main 的祖先必须拒——这是放宽的第一个前提"
    );
    assert!(
        matches!(
            classify_main_advance(
                "abc",
                "def",
                true,
                &["orch/crates/orch-host/src/wake.rs".to_string()],
                &[]
            ),
            MainAdvanceVerdict::RefusedCompilationInput(_)
        ),
        "编译输入被推进必须拒：门的结论会失效"
    );
    assert!(
        matches!(
            classify_main_advance(
                "abc",
                "def",
                true,
                &["coordination/rounds/r60/reviews/B193-A0001-primary-executor-claw.md".to_string()],
                &["coordination/rounds/r60/reviews/B193-A0001-primary-executor-claw.md".to_string()],
            ),
            MainAdvanceVerdict::RefusedBoundArtifact(_)
        ),
        "被 verdict 绑定的产物被改动必须拒（与 H66 同一族的完整性判据）"
    );
    assert!(
        matches!(
            classify_main_advance("abc", "def", true, &["README.md".to_string()], &[]),
            MainAdvanceVerdict::RefusedNonCoordinationPath(_)
        ),
        "非协调路径必须拒——放宽面不得外溢"
    );
}

/// 合并提交形状：H48 的红是「合并**后**的 parent 形状校验未随 H46 一同放宽」。
/// 审计要确认放宽之后它仍然拒绝真正错误的形状，而不是变成橡皮图章。
#[test]
fn merge_commit_shape_binds_both_parents_exactly() {
    let repo = RepoFixture::new("shape");
    let base = repo.commit("base", &[("coordination/BOARD.md", "base\n")]);
    let task_head = repo.branch_commit(
        "task/B193",
        &base,
        &[("orch/crates/orch-host/src/tierf.rs", "work\n")],
    );
    let merge = repo.merge_no_ff("main", &task_head);

    let proof = validate_merge_commit_shape(repo.path(), &merge, &base, &task_head, &[])
        .expect("正常的 no-ff 合并必须被接受");
    assert_eq!(proof.first_parent_sha, base, "first parent 必须是钉住的 main");
    assert_eq!(proof.task_head_sha, task_head, "second parent 必须是任务 head");

    assert!(
        validate_merge_commit_shape(repo.path(), &merge, &task_head, &task_head, &[]).is_err(),
        "expected_main 与 first parent 不符必须拒"
    );
    assert!(
        validate_merge_commit_shape(repo.path(), &merge, &base, &base, &[]).is_err(),
        "task head 与 second parent 不符必须拒"
    );

    let ff = repo.branch_commit(
        "task/B193-ff",
        &merge,
        &[("orch/crates/orch-host/src/tierf.rs", "ff\n")],
    );
    assert!(
        validate_merge_commit_shape(repo.path(), &ff, &merge, &ff, &[]).is_err(),
        "单 parent 的提交不是合并提交，必须拒"
    );
}

/// 授权边界：boundary recovery 是**恢复**入口，不是绕过审批的后门。
/// 它必须同时要求 root actor、屏障真实存在、且任务尚未 Recorded。
#[test]
fn boundary_recovery_authorization_requires_barrier_root_and_open_task() {
    let repo = RepoFixture::new("auth");

    let authorized = LedgerDraft::new("r60", "B193")
        .push(event("root", "PlanSignedOff", None))
        .push(event("root", "VerdictIssued", Some("B193")))
        .push(event("root", "MergeStarted", Some("B193")))
        .write(repo.path());
    validate_root_boundary_recovery_authorization(repo.path(), "r60", "B193", &authorized)
        .expect("root + 真实屏障 + 未 Recorded 必须放行");

    let no_barrier = LedgerDraft::new("r60", "B193")
        .push(event("root", "PlanSignedOff", None))
        .push(event("root", "VerdictIssued", Some("B193")))
        .write(repo.path());
    assert!(
        validate_root_boundary_recovery_authorization(repo.path(), "r60", "B193", &no_barrier)
            .is_err(),
        "没有 MergeStarted 屏障就不存在要恢复的边界，必须拒"
    );

    let not_root = LedgerDraft::new("r60", "B193")
        .push(event("root", "PlanSignedOff", None))
        .push(event("root", "VerdictIssued", Some("B193")))
        .push(event("executor-desktop", "MergeStarted", Some("B193")))
        .write(repo.path());
    assert!(
        validate_root_boundary_recovery_authorization(repo.path(), "r60", "B193", &not_root)
            .is_err(),
        "非 root 开出的屏障不得由恢复入口认领"
    );

    let already_recorded = LedgerDraft::new("r60", "B193")
        .push(event("root", "PlanSignedOff", None))
        .push(event("root", "VerdictIssued", Some("B193")))
        .push(event("root", "MergeStarted", Some("B193")))
        .push(event("root", "TaskRecorded", Some("B193")))
        .write(repo.path());
    assert!(
        validate_root_boundary_recovery_authorization(
            repo.path(),
            "r60",
            "B193",
            &already_recorded
        )
        .is_err(),
        "已 Recorded 的任务不得再走恢复入口（幂等边界）"
    );
}

/// 恢复入口不得成为「换个任务号就能用」的通用后门：
/// 授权必须与**具体那张卡**的屏障绑定，不接受同轮其他卡的屏障。
#[test]
fn boundary_recovery_authorization_is_bound_to_the_barriered_task() {
    let repo = RepoFixture::new("bind");
    let other_task_barrier = LedgerDraft::new("r60", "B193")
        .push(event("root", "PlanSignedOff", None))
        .push(event("root", "VerdictIssued", Some("B194")))
        .push(event("root", "MergeStarted", Some("B194")))
        .write(repo.path());

    assert!(
        validate_root_boundary_recovery_authorization(
            repo.path(),
            "r60",
            "B193",
            &other_task_barrier
        )
        .is_err(),
        "B194 的屏障不得授权 B193 的恢复"
    );
}
