//! ═══ 红种子契约 · B73（wall 预算冻结假象根修 · 活跃-attempt-窗口求和）═══
//! 落位: orch/crates/orch-host/tests/budget_active_wall.rs（逐字节复制）
//! 预期红（redForm: compile）：`budget::active_attempt_wall_mins` 尚不存在 → error[E0425]/E0433。
//!
//! 背景（Phase D · D3）：现 `check_before_model_wake` 用 `spent_wall_mins =
//! (now − epoch_started_at)/60`（budget.rs 单点相减，锚在 RoundOpened 或首个 post-close
//! 事件）。该口径把**轮内冻结/空闲小时也计入 wall**——一旦跨冻结再恢复就冲到 100%，
//! 逼停派发（r36 实证：冻结数小时后 wall 假象触顶，需人工放行）。本棒引入一个**纯折叠**
//! 计算"活跃 attempt 窗口之和"，只累计执行者真正在环的时间，冻结/空闲缝隙不计。
//!
//! ⚠️ 确定性注意：`ledger::event()` 会读环境变量并盖"当下"时间戳，无法造受控 ts。故本种子
//! **直接用结构体字面量构造 EventRecord**（同 B61 先例），保证纯函数无环境/时钟依赖。
//!
//! 目标契约（落在 orch_host::budget，模块已 pub 导出，勿动 lib.rs、不新增依赖）：
//!   - pub fn active_attempt_wall_mins(events: &[orch_core::EventRecord], now_rfc3339: &str) -> u64
//!       语义 = 逐条扫描，按 taskId 维护"打开的 attempt 窗口"：
//!         · 遇 `DispatchIssued`（该 task）→ 以其 ts 打开窗口（若已开则先按本 ts 结清旧窗口再开新）；
//!         · 遇终态 `TaskRecorded` | `AttemptCrashed` | `AttemptTimedOut`（该 task）→ 结清该 task
//!           打开的窗口，累加 (终态.ts − 打开.ts) 的**分钟数**（向下取整）；
//!         · 扫描结束仍打开的窗口（in-flight）→ 累加 (now_rfc3339 − 打开.ts)；
//!       —— 因此：首个 dispatch 之前（含 RoundOpened→首 dispatch）、以及两次 attempt 之间的
//!          空闲/冻结缝隙，都**不落在任何窗口内 ⇒ 不计**。`ReportObserved` 不是终态（其后仍有
//!          verify/merge/record 的活跃时间）。ts 不可解析者跳过；空切片 ⇒ 0。
//!
//! 负向变异下界（转绿后逐条自证）：
//!   M1 用 (now − 首事件.ts)/RoundOpened 单点相减（保留旧假象）⇒ idle_before_first_dispatch_excluded
//!      与 freeze_gap_between_attempts_excluded 红；
//!   M2 把两 attempt 之间的空闲缝隙也计入（不按窗口分段）⇒ freeze_gap_between_attempts_excluded 红；
//!   M3 忽略 in-flight 打开窗口（仅计已闭合窗口）⇒ in_flight_open_window_counts_to_now 红。

use orch_core::EventRecord;
use orch_host::budget::active_attempt_wall_mins;

/// 造一条受控事件（kind + ts + 可选 taskId），绕开 ledger::event() 的时钟/环境依赖。
fn ev(kind: &str, ts: &str, task: Option<&str>) -> EventRecord {
    EventRecord {
        event_id: "id".to_string(),
        ts: ts.to_string(),
        actor: "runtime:orch".to_string(),
        kind: kind.to_string(),
        task_id: task.map(|t| t.to_string()),
        round: Some("r37".to_string()),
        payload: None,
        extra: serde_json::Map::new(),
    }
}

// ── 用例 1：单次 attempt = dispatch→terminal 的分钟差 ──
#[test]
fn single_attempt_counts_dispatch_to_terminal() {
    let events = [
        ev("DispatchIssued", "2026-07-24T11:00:00Z", Some("B73")),
        ev("TaskRecorded", "2026-07-24T11:30:00Z", Some("B73")),
    ];
    // 窗口 [11:00, 11:30] = 30 分钟
    assert_eq!(
        active_attempt_wall_mins(&events, "2026-07-24T12:00:00Z"),
        30,
        "单窗口应为 dispatch→terminal 的 30min"
    );
}

// ── 用例 2：首 dispatch 之前的空闲（含 RoundOpened→首 dispatch）不计 ──
// 这是冻结假象的核心：旧口径 now−RoundOpened=120min，新口径只计 5min。
#[test]
fn idle_before_first_dispatch_excluded() {
    let events = [
        ev("RoundOpened", "2026-07-24T10:00:00Z", None),
        ev("DispatchIssued", "2026-07-24T11:00:00Z", Some("B73")),
        ev("TaskRecorded", "2026-07-24T11:05:00Z", Some("B73")),
    ];
    // 仅窗口 [11:00, 11:05] = 5；RoundOpened→首 dispatch 的 60min 不计。
    assert_eq!(
        active_attempt_wall_mins(&events, "2026-07-24T12:00:00Z"),
        5,
        "首 dispatch 前的空闲/RoundOpened 缝隙不得计入"
    );
}

// ── 用例 3：两次 attempt 之间的冻结缝隙不计（钱途所在）──
#[test]
fn freeze_gap_between_attempts_excluded() {
    let events = [
        ev("DispatchIssued", "2026-07-24T11:00:00Z", Some("B73")),
        ev("AttemptCrashed", "2026-07-24T11:10:00Z", Some("B73")),
        // —— 冻结 ~3 小时（无任何活跃 attempt）——
        ev("DispatchIssued", "2026-07-24T14:00:00Z", Some("B73")),
        ev("TaskRecorded", "2026-07-24T14:15:00Z", Some("B73")),
    ];
    // 窗口 [11:00,11:10]=10 + [14:00,14:15]=15 = 25；中间 170min 冻结缝隙不计。
    assert_eq!(
        active_attempt_wall_mins(&events, "2026-07-24T14:20:00Z"),
        25,
        "两 attempt 之间的冻结缝隙不得计入，只求各活跃窗口之和"
    );
}

// ── 用例 4：in-flight（dispatch 后无终态）窗口计到 now ──
#[test]
fn in_flight_open_window_counts_to_now() {
    let events = [ev("DispatchIssued", "2026-07-24T11:00:00Z", Some("B73"))];
    // 未闭合窗口 [11:00, now=11:20] = 20 分钟
    assert_eq!(
        active_attempt_wall_mins(&events, "2026-07-24T11:20:00Z"),
        20,
        "in-flight 打开窗口应计到 now"
    );
}

// ── 用例 5：空切片 ⇒ 0 ──
#[test]
fn empty_is_zero() {
    assert_eq!(active_attempt_wall_mins(&[], "2026-07-24T12:00:00Z"), 0);
}
