//! ═══ 红种子契约 · B74（Phase D/D4 · liveness 冷启动 boot-tolerance 根修 O8 慢冷启动假死）═══
//! 落位: orch/crates/orch-host/tests/liveness_cold_boot.rs（逐字节复制）
//! 预期红（redForm: compile）：`LivenessOpts` 尚无 `cold_boot_grace` 字段 → error[E0560]
//!   （随后行为断言亦须实现 judge 冷启动分支才转绿）。
//!
//! 背景（O8 实证 · r37/B73）：直接唤醒的执行者（`codex exec resume {message}`）**不跑
//! wait-dispatch.sh**，故整个 attempt 处于 `!acked` 等待相位，既无 GO.ack 也无"waiting"心跳；
//! liveness 只能靠 worktree 活动。冷启动（resume 会话 + 读协议 + 建 worktree）常需 ~2min，
//! 超过现 `boot_grace=90s` → 在首个 worktree 活动出现前被**误判死亡**（B73 实测 121s 建 worktree，
//! 90s 窗口太短）。修法：等待相位下，当**尚无任何活动痕迹**（`last_activity_age == None`，即
//! worktree 还没建出来）时，用更长的**冷启动窗口** `cold_boot_grace`（默认 300s）；一旦有过活动
//! （无论新鲜或陈旧），沿用原 `boot_grace`。既延长慢冷启动容忍，又不永久豁免、也不放宽"起过步
//! 又静默"的真滞留判定。
//!
//! 目标契约（改 orch_host::liveness，勿动 lib.rs、不新增依赖）：
//!   (1) LivenessOpts 新增 `pub cold_boot_grace: Duration`，`Default` 置 300s（其余字段不变）。
//!   (2) `judge` 等待相位（`!acked`）的启动宽限判据改为：
//!         let boot_window = if s.last_activity_age.is_none() { o.cold_boot_grace } else { o.boot_grace };
//!         if s.elapsed < boot_window { Healthy("booting") }
//!       —— 置于"活动新鲜→working-no-ack"判据之后、dead 三条件之前（保持既有顺序与其它分支不变）。
//!
//! 负向变异下界（转绿后逐条自证）：
//!   M1 不区分冷启动（启动窗口仍单用 boot_grace）⇒ cold_boot_no_activity_stays_booting 红；
//!   M2 冷启动永久豁免（无上界 / elapsed 不比 cold_boot_grace）⇒ cold_boot_dies_after_cold_grace 红；
//!   M3 对"有过陈旧活动"也套 cold_boot_grace ⇒ stale_activity_not_extended 红。

use orch_host::liveness::{judge, Judgement, LivenessOpts, ProbeSnapshot};
use std::time::Duration;

fn opts() -> LivenessOpts {
    // 显式钉住两窗口，避免默认值漂移影响断言；其余走 Default（grace40/stall20min/confirm2…）。
    LivenessOpts {
        boot_grace: Duration::from_secs(90),
        cold_boot_grace: Duration::from_secs(300),
        ..Default::default()
    }
}

/// 造等待相位快照（!acked）：elapsed、心跳 age、活动 age（None=还没建 worktree）。
fn waiting(elapsed_s: u64, hb_age_s: Option<u64>, activity_s: Option<u64>) -> ProbeSnapshot {
    ProbeSnapshot {
        elapsed: Duration::from_secs(elapsed_s),
        hb_age: hb_age_s.map(Duration::from_secs),
        hb_pid_alive: None,
        acked: false,
        ack_age: None,
        last_activity_age: activity_s.map(Duration::from_secs),
    }
}

// ── 核心修复：冷启动（无 ack/无心跳/无活动），elapsed=150s 越过旧 boot_grace(90) ──
// 旧口径会误杀；新 cold_boot_grace(300) 应仍判 booting。
#[test]
fn cold_boot_no_activity_stays_booting_past_old_boot_grace() {
    assert_eq!(
        judge(&waiting(150, None, None), &opts()),
        Judgement::Healthy("booting"),
        "冷启动 150s(<cold_boot_grace 300)应仍 booting，不得误杀"
    );
}

// ── 冷启动不永久豁免：elapsed=350s 越过 cold_boot_grace(300) → 判死（真·从未起步）──
#[test]
fn cold_boot_dies_after_cold_grace() {
    assert!(
        matches!(judge(&waiting(350, None, None), &opts()), Judgement::DeadCandidate(_)),
        "冷启动超 cold_boot_grace 仍须能判死，不得无限豁免"
    );
}

// ── 有过活动但已陈旧(>stall=1200s)：非冷启动，套 boot_grace(90)，elapsed=150>90 → 判死 ──
#[test]
fn stale_activity_not_extended() {
    assert!(
        matches!(judge(&waiting(150, None, Some(1300)), &opts()), Judgement::DeadCandidate(_)),
        "起过步又静默(陈旧活动)不得享受冷启动延长"
    );
}

// ── 既有行为不变①：活动新鲜(<=stall) → working-no-ack ──
#[test]
fn fresh_activity_healthy_unchanged() {
    assert_eq!(
        judge(&waiting(150, None, Some(30)), &opts()),
        Judgement::Healthy("working-no-ack")
    );
}

// ── 既有行为不变②：心跳新鲜(<=grace) → waiting ──
#[test]
fn fresh_heartbeat_waiting_unchanged() {
    assert_eq!(
        judge(&waiting(10, Some(5), None), &opts()),
        Judgement::Healthy("waiting")
    );
}
