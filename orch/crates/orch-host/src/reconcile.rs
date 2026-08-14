//! 崩溃重建六步和解核心（design/02 §6）：账本折叠 vs 文件系统地面真值逐项和解。
//!
//! 与 §6 步骤2 比对对的对称增补：账本或文件系统**任一侧**多出来的「幽灵记录」都必报——
//! §6「绝不猜测成功」明文要求「一致 → 缓存；不一致 → needs_reconciliation」，语义对双向不对称
//! 都得敏感，不能只查「文件多、账本少」、漏查「账本多、文件少」。本模块是纯函数核心；
//! fold 与 IO 采集（events.jsonl / 目录扫描 / `git worktree list`）由调用方完成，
//! 喂入 [`TaskTruth`] 后由 [`diff_task`] 产出 mismatch 列表交给策略决策 attach/retry/adopt/cancel。

/// 一项账本与文件系统地面真值的不一致（§6 步骤2 逐项）；变体可扩。
///
/// 变体语义对照 §6 步骤2 三个比对对：`ReportNotObserved` ↔ reports/、`AckNotLogged` ↔
/// dispatch/、`OrphanBranch` ↔ git worktree list；另加一个镜像变体
/// [`Mismatch::ReportObservedButMissing`] 显式承担「账本比真值多」的方向。
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Mismatch {
    /// REPORT 文件在而账本无 `ReportObserved`（运行时死在观察前）—— §6 步骤2 reports/。
    ReportNotObserved,
    /// `.ack` 在而账本无 `DispatchAcked`（agent 已消费但运行时未落账）—— §6 步骤2 dispatch/。
    AckNotLogged,
    /// 分支在而账本无 `DispatchIssued`（手工残留 / 派发账丢）—— §6 步骤2 git worktree list。
    OrphanBranch,
    /// 账本已落 `ReportObserved` 但文件系统找不到 REPORT 文件——「账本比真值多」的幽灵观察。
    ///
    /// §6「绝不猜测成功」的镜像：不光文件系统多东西账本要查，账本多出来的也要查——账本错写、
    /// 文件被误删、跨 worktree 据点漂移 都会引入此态，重建时不替它打圆场。这是本棒的「展示对 §6
    /// 的理解」新变体——它在六步里没被明写出来，但若漏查会让 attach/retry 决策把账本错记当真。
    ReportObservedButMissing,
}

/// 单任务的六维真值快照：账本折叠 ↔ 文件系统地面真值逐项比对。
///
/// 字段两两成对，对应 §6 步骤2 三个观测对：
///   - `report_exists` ↔ `report_observed_logged`（reports/）
///   - `ack_exists` ↔ `ack_logged`（dispatch/ GO.ack）
///   - `branch_exists` ↔ `dispatch_logged`（git worktree list ↔ DispatchIssued）
pub struct TaskTruth {
    pub task_id: String,
    /// 文件系统：REPORT 文件存在。
    pub report_exists: bool,
    /// 账本：已落 `ReportObserved`。
    pub report_observed_logged: bool,
    /// 文件系统：`.ack` 文件存在（agent 已消费 GO）。
    pub ack_exists: bool,
    /// 账本：已落 `DispatchAcked`。
    pub ack_logged: bool,
    /// 文件系统：git 分支 / worktree 存在。
    pub branch_exists: bool,
    /// 账本：已落 `DispatchIssued`。
    pub dispatch_logged: bool,
}

/// 逐项和解：对单任务比对其六维真值，任一方向不对称 → 触发对应 [`Mismatch`]；一致 → 空 Vec。
///
/// **绝不猜测成功**：每项不对称都必报，不替账本或文件系统做默认推断；多项不对称同时存在
/// 时各自落账独立的 `Mismatch`（§6「逐项」语义——交还完整 mismatch 列表给用户/策略决定
/// attach / retry / adopt artifact / cancel），不做短路。
pub fn diff_task(t: &TaskTruth) -> Vec<Mismatch> {
    let mut out = Vec::new();
    // §6 步骤2 reports/: "REPORT 是否存在而账本无 ReportObserved"
    if t.report_exists && !t.report_observed_logged {
        out.push(Mismatch::ReportNotObserved);
    }
    // §6 步骤2 dispatch/: "GO 是否有 .ack" → 已消费但账本未落 DispatchAcked
    if t.ack_exists && !t.ack_logged {
        out.push(Mismatch::AckNotLogged);
    }
    // §6 步骤2 git worktree list ± DispatchIssued: "分支在而无派发账"
    if t.branch_exists && !t.dispatch_logged {
        out.push(Mismatch::OrphanBranch);
    }
    // §6「绝不猜测成功」镜像：账本有 ReportObserved 而文件系统无 REPORT 文件
    if !t.report_exists && t.report_observed_logged {
        out.push(Mismatch::ReportObservedButMissing);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> TaskTruth {
        TaskTruth {
            task_id: "X".into(),
            report_exists: false,
            report_observed_logged: false,
            ack_exists: false,
            ack_logged: false,
            branch_exists: false,
            dispatch_logged: false,
        }
    }

    #[test]
    fn report_observed_but_missing_is_phantom_observation() {
        // §6 反向校验：账本写了 ReportObserved 但文件系统 REPORT 文件找不到——账本比真值多。
        // 不替账本打圆场（绝不猜测成功）；这是 B17 自创的镜像 Mismatch 变体。
        let mut t = base();
        t.report_observed_logged = true;
        t.report_exists = false;
        let m = diff_task(&t);
        assert!(m.contains(&Mismatch::ReportObservedButMissing),
                "应报 ReportObservedButMissing: {m:?}");
        // 反向不对称时不该顺手误报 ReportNotObserved（report_exists=false）
        assert!(!m.contains(&Mismatch::ReportNotObserved),
                "report_exists=false 不该误报 ReportNotObserved: {m:?}");
    }

    #[test]
    fn multiple_mismatches_collected_not_short_circuit() {
        // §6「逐项和解」：多项不对称同时存在 → 各自落账独立 Mismatch，不短路其一。
        // 交还完整列表给用户决策 attach / retry / adopt artifact / cancel。
        let mut t = base();
        t.report_exists = true;          // → ReportNotObserved
        t.ack_exists = true;             // → AckNotLogged
        t.branch_exists = true;          // → OrphanBranch (dispatch_logged=false)
        let m = diff_task(&t);
        assert!(m.contains(&Mismatch::ReportNotObserved));
        assert!(m.contains(&Mismatch::AckNotLogged));
        assert!(m.contains(&Mismatch::OrphanBranch));
        assert_eq!(m.len(), 3, "三项不一致一次性全收、不短路: {m:?}");
    }
}
