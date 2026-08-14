//! ═══ 红种子契约 · B17 ═══（落位: orch/crates/orch-host/tests/reconcile_algo.rs，逐字节复制）
//! 预期红（redForm: compile）：orch_host::reconcile 为占位空模块。
//! 变异清单（E9 下界）: M1 删 REPORT 失察分支 → ① 红；M2 删 ack 失账分支 → ② 红；M3 一致时也报 mismatch → ③ 红
use orch_host::reconcile::{self, Mismatch, TaskTruth};

fn base() -> TaskTruth {
    TaskTruth {
        task_id: "B1".into(),
        report_exists: false,
        report_observed_logged: false,
        ack_exists: false,
        ack_logged: false,
        branch_exists: false,
        dispatch_logged: false,
    }
}

#[test]
fn report_on_disk_but_not_logged() {
    // ① REPORT 文件在而账本无 ReportObserved（运行时死在观察前）→ ReportNotObserved
    let mut t = base();
    t.dispatch_logged = true;
    t.branch_exists = true;
    t.report_exists = true;
    let m = reconcile::diff_task(&t);
    assert!(m.contains(&Mismatch::ReportNotObserved), "应报 ReportNotObserved: {m:?}");
}

#[test]
fn ack_on_disk_but_not_logged() {
    // ② .ack 在而账本无 DispatchAcked → AckNotLogged
    let mut t = base();
    t.dispatch_logged = true;
    t.ack_exists = true;
    let m = reconcile::diff_task(&t);
    assert!(m.contains(&Mismatch::AckNotLogged), "应报 AckNotLogged: {m:?}");
}

#[test]
fn consistent_state_is_clean() {
    // ③ 账实一致 → 零 mismatch
    let mut t = base();
    t.dispatch_logged = true;
    t.ack_exists = true;
    t.ack_logged = true;
    t.branch_exists = true;
    t.report_exists = true;
    t.report_observed_logged = true;
    assert!(reconcile::diff_task(&t).is_empty());
}

#[test]
fn orphan_branch_detected() {
    // ④ 分支在而账本无 DispatchIssued → OrphanBranch（手工残留/账丢）
    let mut t = base();
    t.branch_exists = true;
    let m = reconcile::diff_task(&t);
    assert!(m.contains(&Mismatch::OrphanBranch), "应报 OrphanBranch: {m:?}");
}
