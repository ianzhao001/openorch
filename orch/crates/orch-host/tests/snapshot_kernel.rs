//! B102 契约种子 · 快照内核 `OrchSnapshot`（r45）
//!
//! 落位：`orch/crates/orch-host/tests/snapshot_kernel.rs`（逐字节搬运，禁止修改）
//! 预期红形态：**compile**（`orch_host::snapshot::*` 的项尚不存在 → E0432/E0425）
//!
//! ## 契约
//!
//! `build_snapshot(inputs: &SnapshotInputs) -> OrchSnapshot`（**纯函数**，IO 采样在别处）
//!
//! ### 「空闲」判定（本卡最容易做错的地方）
//! 心跳 `phase` 只有 `waiting` 一个取值，且被 wake 直接注入的执行者**永远没有心跳**，
//! 所以不能「无心跳 = 空闲」也不能「有心跳 = 忙」。规则：
//!   - 有派发中任务 → 由 `liveness::judge` 的判定映射（booting/working/working-no-ack/stalled/dead）
//!   - 无派发中任务 + 心跳新鲜 → `Idle`
//!   - 无派发中任务 + 无心跳/心跳过期 → **`Unknown`（不是 Idle）**
//! 阈值必须复用 `liveness::LivenessOpts`，**禁止另造**（否则重演 O8 假死误判）。
//!
//! ### 其他不变量
//!   - `schema_version` 与 `generated_at` 必填
//!   - `round.budget` **三维齐全**（usd / wallMins / wakes），任一维缺失即违约
//!   - dead/stalled 必须产 `critical` 告警，并带**可复制的建议命令**（面板只复制不执行）
//!   - `write_snapshot` 必须原子（tmp + rename），且只由 daemon 调用
//!
//! ## 负向变异清单（REPORT §5 逐条自证）
//! 1. 无心跳且无派发判成 Idle → `no_heartbeat_no_dispatch_is_unknown` 红
//! 2. 绕开 liveness 判据自造阈值 → `agent_state_follows_liveness_judgement` 红
//! 3. 快照缺 generated_at/schema_version → `snapshot_is_versioned_and_stamped` 红
//! 4. 预算三维少一维 → `round_budget_has_three_dimensions` 红
//! 5. dead 不产 critical 告警或不带建议命令 → `dead_agent_raises_critical_alert` 红

use orch_host::liveness::{Judgement, LivenessOpts, ProbeSnapshot};
use orch_host::snapshot::{
    build_snapshot, AgentInput, AgentState, BudgetInput, Severity, SnapshotInputs,
};
use std::time::Duration;

fn opts() -> LivenessOpts {
    LivenessOpts::default()
}

fn probe(hb_age: Option<Duration>, acked: bool, activity: Option<Duration>) -> ProbeSnapshot {
    ProbeSnapshot {
        elapsed: Duration::from_secs(600),
        hb_age,
        hb_pid_alive: None,
        acked,
        ack_age: if acked { Some(Duration::from_secs(30)) } else { None },
        last_activity_age: activity,
    }
}

fn agent(id: &str, task: Option<&str>, probe: ProbeSnapshot) -> AgentInput {
    AgentInput {
        agent_id: id.to_string(),
        current_task: task.map(str::to_string),
        probe,
        judgement: None,
    }
}

fn inputs(agents: Vec<AgentInput>) -> SnapshotInputs {
    SnapshotInputs {
        round_id: "r45".to_string(),
        generated_at: "2026-07-26T00:00:00Z".to_string(),
        events: Vec::new(),
        agents,
        budget: BudgetInput {
            max_usd: Some(8.0),
            max_wall_minutes: Some(900),
            max_model_wakes: Some(24),
            spent_usd: 1.25,
            spent_wall_minutes: 30,
            spent_model_wakes: 3,
        },
        activity: Vec::new(),
        liveness_opts: opts(),
    }
}

fn state_of(snap: &orch_host::snapshot::OrchSnapshot, agent_id: &str) -> AgentState {
    snap.agents
        .iter()
        .find(|a| a.agent_id == agent_id)
        .unwrap_or_else(|| panic!("快照缺 agent {agent_id}"))
        .state
}

#[test]
fn snapshot_is_versioned_and_stamped() {
    let snap = build_snapshot(&inputs(Vec::new()));
    assert!(!snap.schema_version.is_empty(), "快照必须带 schema_version（供 WebUI 兼容）");
    assert_eq!(snap.generated_at, "2026-07-26T00:00:00Z", "快照必须带生成时刻");
}

#[test]
fn idle_requires_fresh_heartbeat_and_no_dispatch() {
    let snap = build_snapshot(&inputs(vec![agent(
        "executor-opencode",
        None,
        probe(Some(Duration::from_secs(5)), false, None),
    )]));
    assert_eq!(
        state_of(&snap, "executor-opencode"),
        AgentState::Idle,
        "无派发 + 心跳新鲜 = 空闲"
    );
}

#[test]
fn no_heartbeat_no_dispatch_is_unknown() {
    let snap = build_snapshot(&inputs(vec![agent("executor-desktop", None, probe(None, false, None))]));
    assert_eq!(
        state_of(&snap, "executor-desktop"),
        AgentState::Unknown,
        "被 wake 直接注入的执行者永远没有心跳——无心跳只能判 Unknown，绝不能当成 Idle"
    );

    let stale = build_snapshot(&inputs(vec![agent(
        "executor-desktop",
        None,
        probe(Some(opts().grace + Duration::from_secs(1)), false, None),
    )]));
    assert_eq!(
        state_of(&stale, "executor-desktop"),
        AgentState::Unknown,
        "心跳已过 grace 同样只能判 Unknown"
    );
}

#[test]
fn agent_state_follows_liveness_judgement() {
    // 有派发中任务时，状态必须由 liveness::judge 决定，而不是快照侧另造阈值
    let mut working = agent(
        "executor-opencode",
        Some("B102"),
        probe(None, true, Some(Duration::from_secs(10))),
    );
    working.judgement = Some(Judgement::Healthy("working"));
    let snap = build_snapshot(&inputs(vec![working]));
    assert_eq!(state_of(&snap, "executor-opencode"), AgentState::Working);

    let mut stalled = agent(
        "executor-opencode",
        Some("B102"),
        probe(None, true, Some(opts().stall + Duration::from_secs(1))),
    );
    stalled.judgement = Some(Judgement::StalledCandidate("ack 后长期零活动".to_string()));
    let snap = build_snapshot(&inputs(vec![stalled]));
    assert_eq!(state_of(&snap, "executor-opencode"), AgentState::Stalled);
}

#[test]
fn dead_agent_raises_critical_alert() {
    let mut dead = agent("executor-desktop", Some("B101"), probe(None, false, None));
    dead.judgement = Some(Judgement::DeadCandidate("心跳缺失 ∧ 无 ack ∧ 无 worktree 活动".to_string()));
    let snap = build_snapshot(&inputs(vec![dead]));
    assert_eq!(state_of(&snap, "executor-desktop"), AgentState::Dead);

    let alert = snap
        .alerts
        .iter()
        .find(|a| a.subject == "executor-desktop")
        .expect("判死必须产告警");
    assert_eq!(alert.severity, Severity::Critical, "判死是 critical 级");
    let cmd = alert
        .suggested_command
        .as_ref()
        .expect("告警必须带可复制的建议命令（面板只复制不执行）");
    assert!(
        cmd.contains("orch retry-dead") || cmd.contains("orch handshake"),
        "建议命令应指向既有恢复入口，实际: {cmd}"
    );
}

#[test]
fn round_budget_has_three_dimensions() {
    let snap = build_snapshot(&inputs(Vec::new()));
    let budget = &snap.round.budget;
    assert_eq!(budget.usd.spent, 1.25);
    assert_eq!(budget.usd.max, Some(8.0));
    assert_eq!(budget.wall_minutes.spent, 30);
    assert_eq!(budget.wall_minutes.max, Some(900));
    assert_eq!(budget.model_wakes.spent, 3);
    assert_eq!(budget.model_wakes.max, Some(24));
    assert_eq!(
        budget.model_wakes.remaining,
        Some(21),
        "剩余额度必须给出，面板要显示三维余量"
    );
}

// ── B111：provider 归一化状态接入 snapshot ──
// AgentInput 被 B102 种子字节锁定、无法承载 provider 事实，build_snapshot 路径
// 拿不到 spawn/activity/fault 切片。本投影对「没事实」必须诚实：provider_state=None，
// 绝不因「没事实」就猜成 Engaged（任务卡「未知 provider/未知形状显式 Pending/Unknown，不猜 Engaged」）。
#[test]
fn provider_state_is_none_when_no_facts_never_engaged() {
    // 有派发中任务 + 工作中迹象（probe 上有 last_activity）——但 provider 事实缺位。
    let ai = agent(
        "executor-opencode",
        Some("B111"),
        probe(None, true, Some(Duration::from_secs(10))),
    );
    let snap = build_snapshot(&inputs(vec![ai]));
    let a = snap
        .agents
        .iter()
        .find(|a| a.agent_id == "executor-opencode")
        .unwrap();
    assert_eq!(
        a.provider_state, None,
        "无 provider 事实时 provider_state 必须为 None（诚实缺省），不得猜成 Engaged"
    );
    // 反向断言：绝不能是 Some(Engaged) ——「没事实就猜 Engaged」是 B111 要堵的退化。
    assert!(
        !matches!(a.provider_state, Some(orch_host::chanhealth::ProviderState::Engaged)),
        "没 provider 事实不得猜 Engaged"
    );
}

// 未知 provider / 未知形状：归一化器本身对已识别 provider 永远吐 Pending/Engaged/Fault；
// 「形状未知」由 snapshot 投影用 None 表达（等价 Unknown，但 None 更诚实——字段未填）。
// normalize_provider_state 侧另由 provider_state.rs 契约覆盖，这里只验投影口径。
#[test]
fn provider_state_field_exists_and_serializes() {
    use orch_host::snapshot::OrchSnapshot;
    let snap = build_snapshot(&inputs(vec![agent(
        "executor-desktop",
        None,
        probe(Some(Duration::from_secs(5)), false, None),
    )]));
    let json = serde_json::to_string(&snap).expect("snapshot 序列化");
    assert!(
        json.contains("provider_state"),
        "快照 JSON 必须含 provider_state 字段（B111 接线）：{json}"
    );
    // 无 provider 事实 → null（None），不得是 "Engaged"
    assert!(
        !json.contains("\"Engaged\""),
        "无 provider 事实时 JSON 不得出现 Engaged：{json}"
    );
    let _ = OrchSnapshot::empty("r45", "t");
}
