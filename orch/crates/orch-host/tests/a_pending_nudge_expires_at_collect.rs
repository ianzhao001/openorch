#![allow(dead_code)]
//! B269 seeded-red contract：pending NUDGE 必须随 REPORT/collect 失效（H165）。
//!
//! Expected red: **compile**。`tierf::pending_nudge_disposition` 与
//! `tierf::NudgeDisposition` 尚不存在 ⇒ `error[E0432]: unresolved import`。
//!
//! ── 实撞（r70/B264-A0002）──
//!
//! planner 在实现期发出 forward-baseline typed NUDGE；**实时注入因 continuation digest
//! changed 被拒**，但 pending `NUDGE.md` 继续留在文件通道里。
//! executor 完成实现与 REPORT、**collect 已经跑过之后**才消费到那条消息。
//! 消息里的 exact implementation HEAD 还被 planner 写错，executor 因而正确地拒绝合并
//! 并追加了一个 commit——但 **task ref 已经从 canonical collect 的 `branchSha=9e4aa2fd`
//! 漂移到 `545d`**，正式 review 的 exact barrier 随即（正确地）拒收。
//!
//! 恢复代价：planner 先补落两条 `ActionRejected`，再 non-clobber 建 archive ref，
//! 移除 clean task worktree，用 exact old OID 把 `task/B264` CAS 回 `9e4`，重建现场。
//!
//! ── 缺陷的两个面 ──
//!
//! ① **实时注入失败与文件留存产生了两个互相矛盾的可观测状态**：
//!    一边报「拒绝」，一边留下一条**未来仍可执行**的命令。
//! ② `coordination/scripts/wait-dispatch.sh:60-61`（同 `scaffold.rs:140-141`）
//!    无条件消费 `NUDGE.md`——**没有相位检查、没有前置条件**：
//!    ```sh
//!    if [ -f "$disp/NUDGE.md" ]; then
//!      echo NUDGE; cat "$disp/NUDGE.md"; mv "$disp/NUDGE.md" "$disp/NUDGE.md.seen"; exit 4
//!    ```
//!
//! ── 修法落在 runtime 侧（刻意不改冻结的 wait 脚本）──
//!
//! `coordination/**` 对执行者冻结，因此本卡**不动 wait-dispatch.sh**。
//! 改为由 runtime 在 collect 完成时**中和**尚未消费的 NUDGE：
//! 让文件根本不再出现在执行者面前，同时落 durable `NudgeSuperseded`。
//!
//! ── 一条必须写死的原则：身份，不是邻近 ──
//!
//! 只有**同一个 attempt** 的 `ReportObserved` / `ReportCollectCompleted` 才能使其失效。
//! 另一个 attempt 的同类事件**不得**让本条 NUDGE 失效——这与 H165 自身的教训同源：
//! **时间邻近不是身份**（r70 还在 wake supersession 上栽过同一型）。
//!
//! ── Negative mutations that must turn the named case red ──
//!
//! M1. collect 之后仍判可消费（回到 r70 的形态）
//!     -> `a_nudge_is_superseded_once_its_attempt_reported` 红。
//! M2. collect 完成事件不使其失效
//!     -> `a_nudge_is_superseded_once_collect_completed` 红。
//! M3. 用「有没有出现过这类事件」代替「是不是同一个 attempt 的」
//!     -> `another_attempts_report_never_supersedes_this_nudge` 红。
//! M4. **接线变异**：谓词存在但 tierf 生产路径不调用它
//!     -> `the_runtime_consults_the_pending_nudge_disposition` 红。
//! M5. 失效了却不落 durable 事实（只删文件，不留痕）
//!     -> `the_runtime_emits_a_durable_nudge_supersession` 红。
//! M6. 把闸修成全拦：实现期的正常 NUDGE 也被判失效
//!     -> `a_nudge_before_any_report_is_still_consumable` 红。

use orch_host::tierf::{pending_nudge_disposition, NudgeDisposition};

const TIERF_SRC: &str = include_str!("../src/tierf.rs");

/// `(kind, attemptId)`：刻意用元组而不是结构体字面量，避免把字段集钉进冻结文件（H74）。
fn later(events: &[(&str, &str)]) -> Vec<(String, String)> {
    events
        .iter()
        .map(|(k, a)| ((*k).to_string(), (*a).to_string()))
        .collect()
}

// ── ① 实现期的正常 NUDGE 必须仍可消费（M6）──────────────────────────────

#[test]
fn a_nudge_before_any_report_is_still_consumable() {
    let disposition = pending_nudge_disposition(
        "B264-A0002",
        &later(&[("DispatchAcked", "B264-A0002"), ("AttemptStarted", "B264-A0002")]),
    );
    assert!(
        matches!(disposition, NudgeDisposition::Consumable),
        "B269 M6: 实现期的 NUDGE 是催醒的正常手段，不得被一并拦死；实得 {disposition:?}"
    );
}

// ── ② REPORT 已被观察 ⇒ 失效（M1）───────────────────────────────────────

#[test]
fn a_nudge_is_superseded_once_its_attempt_reported() {
    let disposition =
        pending_nudge_disposition("B264-A0002", &later(&[("ReportObserved", "B264-A0002")]));
    match disposition {
        NudgeDisposition::Superseded { superseded_by } => {
            assert_eq!(
                superseded_by, "ReportObserved",
                "B269: 必须如实回报是哪一个事实结束了它的窗口"
            );
        }
        other => panic!(
            "B269 M1: 交卷之后到达的命令能把 task ref 推离 immutable collect HEAD；实得 {other:?}"
        ),
    }
}

// ── ③ collect 已完成 ⇒ 失效（M2）────────────────────────────────────────

#[test]
fn a_nudge_is_superseded_once_collect_completed() {
    let disposition = pending_nudge_disposition(
        "B264-A0002",
        &later(&[("ReportCollectCompleted", "B264-A0002")]),
    );
    assert!(
        matches!(disposition, NudgeDisposition::Superseded { .. }),
        "B269 M2: collect 之后 branchSha 已经 immutable，任何后续 commit 都会打破 exact barrier；\
         实得 {disposition:?}"
    );
}

// ── ④ 身份，不是邻近（M3）───────────────────────────────────────────────

#[test]
fn another_attempts_report_never_supersedes_this_nudge() {
    let disposition = pending_nudge_disposition(
        "B264-A0002",
        &later(&[
            ("ReportObserved", "B264-A0001"),
            ("ReportCollectCompleted", "B263-A0004"),
        ]),
    );
    assert!(
        matches!(disposition, NudgeDisposition::Consumable),
        "B269 M3: 时间邻近不是身份——别的 attempt 交卷不能作废本 attempt 的 NUDGE；\
         这与 r70 在 wake supersession 上栽的是同一型；实得 {disposition:?}"
    );
}

// ── ⑤ 接线变异（M4）：生产路径必须真的问它 ───────────────────────────────

#[test]
fn the_runtime_consults_the_pending_nudge_disposition() {
    assert!(
        TIERF_SRC.contains("pending_nudge_disposition("),
        "B269 M4: 谓词写对了但生产路径不调它，NUDGE 照样会在 collect 之后被消费"
    );
}

// ── ⑥ 失效必须留痕（M5）─────────────────────────────────────────────────

#[test]
fn the_runtime_emits_a_durable_nudge_supersession() {
    assert!(
        TIERF_SRC.contains("NudgeSuperseded"),
        "B269 M5: 只删文件不落账 = 又一个「拒收只写 journal」的重演（r69 成因 3）；\
         失效必须是可被账本审计的一等事实"
    );
}
