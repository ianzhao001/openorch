#![allow(dead_code)]
//! B268 seeded-red contract：让 `dependsOn` 成为**真实的派发准入闸**，
//! 并在依赖已落地而 attempt base 落后时给出 typed forward-baseline 修复信号（H164）。
//!
//! Expected red: **compile**。`plan::dependency_dispatch_admission` 与
//! `plan::DependencyAdmission` 尚不存在 ⇒ `error[E0432]: unresolved import`。
//!
//! ── 这张卡是本仓「可达性盲区」的教科书样本 ──
//!
//! r51/B147 已经交付了一个**正确的**依赖谓词：
//! `plan::task_dependency_blockers`（`plan.rs:1733`，`pub fn`），
//! 还配了契约测试 `tests/dependency_eligibility.rs`。
//!
//! **但 planner 复核全仓命中后发现：它的调用者全是测试。**
//! `tierf.rs` 与 `scheduler.rs` 对 `depends_on` / `dependency` 的命中数是 **0**；
//! 已有的派发准入函数 `require_runtime_dispatch_admission`
//! （`tierf.rs:3265`，调用点 `:3609` 与 `:3931`，窗口 1359 字节）
//! 窗口内 `depends_on` 命中同样是 **0**。
//!
//! ⇒ **谓词是对的，测试是绿的，而生产派发路径从来没问过它。**
//! 这正是本仓反复咬人的形态：契约测试自调，使「有调用者」在测试视角下永远成立。
//!
//! ── 实撞（r70/B264-A0002）──
//!
//! B264 卡面声明依赖 B263，且必须修改 B263 新接入的 exit/disposition 路径。
//! 但 A0001 可以在 B263 `Recorded` **之前**以 `baseSha=6dd149c…` 派发。
//! A0001 因旧合同诚实 BLOCKED 后，B263 已合入当前 main `6653d1f…`，
//! 而 plain blocked-successor dispatch 仍按设计让 A0002 **继承首派 base**，
//! worktree HEAD 继续是 `34f80c23`——**里面没有 B263 的实现**。
//! `dispatch --new-attempt` 不是该状态的重锚入口，机械拒绝且零状态变更。
//!
//! ── 修法的两个层次（缺一不可）──
//!
//! ① **依赖未 Recorded ⇒ 派发被拒**（把 B147 的谓词真正接上）；
//! ② 依赖已 Recorded、但 attempt base **不含**其 merge ⇒ 落 typed
//!    `ForwardBaselineRequired`，**不是**静默放行、也**不是**含糊的 BLOCKED。
//!    r70 的教训是：恢复机制其实存在（`mech::check` 允许 task 分支向前吸收受信 main 祖先链），
//!    但**没有任何东西告诉执行者「现在该合了」**，于是安全窗口在几分钟内消失。
//!
//! ── 为什么谓词收一个 `base_contains` 闭包 ──
//!
//! 让「base 是否包含某 merge」这件需要 git IO 的事留在调用方，
//! 谓词本身保持**纯函数**：可独立测试、无夹具、无 IO。
//! 生产调用方负责传入真实的祖先链判定。
//!
//! ── Negative mutations that must turn the named case red ──
//!
//! M1. 依赖未 Recorded 仍放行（回到 r70 的形态）
//!     -> `an_unrecorded_dependency_blocks_dispatch` 红。
//! M2. 依赖已 Recorded 但 base 落后时静默放行
//!     -> `a_recorded_dependency_missing_from_the_base_demands_a_forward_baseline` 红。
//! M3. 把「base 落后」误判成「依赖未完成」（两种状态混为一谈，运维无从下手）
//!     -> `a_stale_base_is_not_reported_as_an_incomplete_dependency` 红。
//! M4. **接线变异**：谓词存在但 `require_runtime_dispatch_admission` 不调用它
//!     -> `the_runtime_dispatch_admission_consults_the_dependency_predicate` 红。
//! M5. 无依赖的卡被误拦（把闸修成全拦）
//!     -> `a_task_without_dependencies_is_always_admitted` 红。
//! M6. 依赖齐备且 base 已含其 merge 时仍不放行
//!     -> `a_satisfied_dependency_with_a_current_base_is_admitted` 红。

// 谓词落在 `scheduler.rs` 而不是 `plan.rs`：准入本来就是 scheduler 的职责域
// （`capacity_admits_in_domain` 等已在此），且这样能把 `plan.rs` 的同轮写者数量
// 控制在 2 张卡以内，减少前向基线搅动——本卡自己修的就是这一族问题。
use orch_host::plan::{dependency_dispatch_admission, DependencyAdmission};

const TIERF_SRC: &str = include_str!("../src/tierf.rs");

const ADMISSION_ANCHOR: &str = "fn require_runtime_dispatch_admission(";

fn admission_window() -> &'static str {
    let start = TIERF_SRC
        .find(ADMISSION_ANCHOR)
        .expect("B268: missing source anchor for require_runtime_dispatch_admission");
    let tail = &TIERF_SRC[start + ADMISSION_ANCHOR.len()..];
    let end = tail
        .find("\nfn ")
        .or_else(|| tail.find("\npub fn "))
        .unwrap_or(tail.len());
    &TIERF_SRC[start..start + ADMISSION_ANCHOR.len() + end]
}

fn owned(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

fn recorded_with_merge(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(task, sha)| ((*task).to_string(), (*sha).to_string()))
        .collect()
}

// ── ① 依赖未 Recorded ⇒ 拒（M1）──────────────────────────────────────────

#[test]
fn an_unrecorded_dependency_blocks_dispatch() {
    let admission = dependency_dispatch_admission(
        &owned(&["B263"]),
        &recorded_with_merge(&[]),
        Some("34f80c23"),
        &|_merge| true,
    );
    match admission {
        DependencyAdmission::BlockedByDependency { blockers } => {
            assert_eq!(
                blockers,
                owned(&["B263"]),
                "B268: 必须点名是哪一个依赖没完成"
            );
        }
        other => panic!(
            "B268 M1: 依赖未 Recorded 就派发，执行者会拿着不含依赖实现的树开工；实得 {other:?}"
        ),
    }
}

// ── ② 依赖已 Recorded 但 base 落后 ⇒ typed 前向基线修复（M2）─────────────

#[test]
fn a_recorded_dependency_missing_from_the_base_demands_a_forward_baseline() {
    let admission = dependency_dispatch_admission(
        &owned(&["B263"]),
        &recorded_with_merge(&[("B263", "6653d1f0")]),
        Some("34f80c23"),
        // base 不含该 merge —— r70/B264-A0002 的确切形态
        &|_merge| false,
    );
    match admission {
        DependencyAdmission::ForwardBaselineRequired {
            dependency,
            merge_sha,
            attempt_base_sha,
        } => {
            assert_eq!(dependency, "B263");
            assert_eq!(merge_sha, "6653d1f0");
            assert_eq!(attempt_base_sha, "34f80c23");
        }
        other => panic!(
            "B268 M2: r70 的恢复机制其实存在，缺的是「现在该合了」这句话——\
             静默放行会让安全窗口在几分钟内消失；实得 {other:?}"
        ),
    }
}

// ── ③ 两种状态不得混为一谈（M3）──────────────────────────────────────────

#[test]
fn a_stale_base_is_not_reported_as_an_incomplete_dependency() {
    let admission = dependency_dispatch_admission(
        &owned(&["B263"]),
        &recorded_with_merge(&[("B263", "6653d1f0")]),
        Some("34f80c23"),
        &|_merge| false,
    );
    assert!(
        !matches!(admission, DependencyAdmission::BlockedByDependency { .. }),
        "B268 M3: 「依赖没做完」与「依赖做完了但你的树是旧的」需要的动作完全不同——\
         混为一谈就把归因成本又转嫁给人"
    );
}

// ── ④ 接线变异（M4）：生产准入路径必须真的问它 ────────────────────────────

#[test]
fn the_runtime_dispatch_admission_consults_the_dependency_predicate() {
    assert!(
        admission_window().contains("dependency_dispatch_admission("),
        "B268 M4: r51/B147 交付过一个正确的依赖谓词、配了契约测试，\
         却从来没有生产调用者——这条断言就是为了不让同一件事发生第二次"
    );
}

// ── ⑤ 不得把闸修成全拦（M5）──────────────────────────────────────────────

#[test]
fn a_task_without_dependencies_is_always_admitted() {
    let admission =
        dependency_dispatch_admission(&[], &recorded_with_merge(&[]), Some("34f80c23"), &|_| false);
    assert!(
        matches!(admission, DependencyAdmission::Admitted),
        "B268 M5: 无 dependsOn 的卡必须畅通——r69 的级联阻塞是真实代价"
    );
}

// ── ⑥ 满足条件时必须放行（M6）────────────────────────────────────────────

#[test]
fn a_satisfied_dependency_with_a_current_base_is_admitted() {
    let admission = dependency_dispatch_admission(
        &owned(&["B263"]),
        &recorded_with_merge(&[("B263", "6653d1f0")]),
        Some("6653d1f0"),
        &|merge| merge == "6653d1f0",
    );
    assert!(
        matches!(admission, DependencyAdmission::Admitted),
        "B268 M6: 依赖齐备且 base 已含其 merge 时必须放行"
    );
}
