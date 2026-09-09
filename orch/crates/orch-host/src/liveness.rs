//! O7：await-report 与执行者 liveness 联动（design/03 §4.3 探测阶梯的运行时实现）。
//! probe = 纯 IO 采样（stat/shell 零模型）；judge = 纯函数判定（host 层首批单测对象）。
//! 防误杀根本律（§4.3 working 行）：**工作相位不强求心跳**——wait 脚本 GO_FOUND 后
//! exit 0（$$ 即死）是常态，故 ack 之后心跳/pid 绝不参与判定；dead 仅在等待相位、
//! 且「心跳过期 ∧ 无 ack ∧ 无 worktree 活动」三条件齐备时成立。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use orch_core::EventRecord;

use crate::gitx;

pub struct LivenessOpts {
    /// 心跳新鲜阈值（design/03 §4.3：mtime ≤40s = waiting 活着）
    pub grace: Duration,
    /// workingStallMinutes（§4.3 stalled 行，默认 20min）
    pub stall: Duration,
    /// 探测周期（每 N 个 2s tick 一探）
    pub probe_every: Duration,
    /// 启动宽限：bootstrap 粘贴 → 客户端读协议 → 首跳的时间窗
    pub boot_grace: Duration,
    /// 尚无任何 worktree 活动痕迹时的冷启动宽限
    pub cold_boot_grace: Duration,
    /// 候选态连续确认次数（去抖）
    pub confirm: u8,
}

impl Default for LivenessOpts {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(40),
            stall: Duration::from_secs(20 * 60),
            probe_every: Duration::from_secs(10),
            boot_grace: Duration::from_secs(90),
            cold_boot_grace: Duration::from_secs(300),
            confirm: 2,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProbeSnapshot {
    /// await 开始至今
    pub elapsed: Duration,
    /// None = 心跳文件不存在（注意：缺失 ≠ 新鲜——None 走「无心跳」分支）
    pub hb_age: Option<Duration>,
    /// None = 心跳里解不出 pid
    pub hb_pid_alive: Option<bool>,
    pub acked: bool,
    pub ack_age: Option<Duration>,
    /// 活动多信号取最小 age：worktree 写入 ∪ provider 日志增长。
    /// None = 两类活动都无可读痕迹（≠ 有新活动）。
    pub last_activity_age: Option<Duration>,
}

/// 工作相位的 idle 是 ACK、worktree 写入与 provider 输出三类活动年龄的最小值。
/// 任一信号缺失只表示该来源未知；三者皆缺失才返回 `None`，由调用方保守处理。
#[doc(hidden)]
pub fn working_idle_age(
    ack: Option<Duration>,
    worktree: Option<Duration>,
    output: Option<Duration>,
) -> Option<Duration> {
    [ack, worktree, output].into_iter().flatten().min()
}

/// 构造已 ACK 的工作相位快照。provider 输出年龄折叠进既有
/// `last_activity_age`，保持 `ProbeSnapshot` 的公开字段集不变。
#[doc(hidden)]
pub fn probe_working(
    elapsed: Duration,
    ack_age: Option<Duration>,
    worktree_age: Option<Duration>,
    output_age: Option<Duration>,
) -> ProbeSnapshot {
    ProbeSnapshot {
        elapsed,
        hb_age: None,
        hb_pid_alive: None,
        acked: true,
        ack_age,
        last_activity_age: working_idle_age(None, worktree_age, output_age),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Judgement {
    Healthy(&'static str),
    DeadCandidate(String),
    StalledCandidate(String),
}

/// Convert a dead result to a non-penalizing stall when the immutable ledger contains an
/// unresolved storage interruption for this attempt. No filesystem state participates.
pub fn suppress_dead_for_storage(
    judgement: Judgement,
    events: &[EventRecord],
    task: &str,
    attempt: Option<&str>,
) -> Judgement {
    match (judgement, attempt) {
        (Judgement::DeadCandidate(reason), Some(attempt_id))
            if crate::storage::storage_interruption_active(events, task, attempt_id) =>
        {
            Judgement::StalledCandidate(format!("storage-exhaustion: {reason}"))
        }
        (judgement, _) => judgement,
    }
}

/// Keep the short-lived wait-script identity separate from the provider's
/// durable identity. `None` remains JSON null and therefore cannot masquerade
/// as backend death.
pub fn last_signal_json(
    wait_pid_alive: Option<bool>,
    durable_alive: Option<bool>,
) -> serde_json::Value {
    serde_json::json!({
        "waitPidAlive": wait_pid_alive,
        "durableAlive": durable_alive,
        "pidAlive": wait_pid_alive,
        "pidAliveNote": "deprecated: wait-script PID only; does not indicate backend death",
    })
}

/// Probe the wait-script PID only while the heartbeat still says `waiting`.
/// A `working` heartbeat contains the provider spawn PID, which must not be
/// mislabeled as the deprecated wait-script signal.
pub fn probe_wait_pid_alive(root: &Path, agent: &str) -> Option<bool> {
    let path = root.join(format!("coordination/runtime/heartbeats/{agent}.json"));
    let value: serde_json::Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    if value.get("phase").and_then(serde_json::Value::as_str) != Some("waiting") {
        return None;
    }
    let pid = value.get("pid").and_then(serde_json::Value::as_i64)?;
    Some(
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false),
    )
}

/// 纯 IO 采样：心跳 mtime+pid、GO.ack、worktree 活动（目录/HEAD/index/writeSet/最后 commit）
/// elapsed 取 `started.elapsed()`（进程内计时）——等价于
/// `probe_with_durable_elapsed(.., None)`。snapshot 投影 / orch-cli 等不跨重启
/// 恢复的调用方走本入口，签名与行为均不变。
pub fn probe(
    root: &Path,
    agent: &str,
    task_id: &str,
    go_path: Option<&Path>,
    write_set: &[String],
    started: Instant,
) -> ProbeSnapshot {
    probe_with_durable_elapsed(root, agent, task_id, go_path, write_set, started, None)
}

/// B112-A0004：`durable_elapsed = Some(d)` 时快照 `elapsed` 直接取 `d`——d 由
/// 调用方用 `attempt_current_elapsed` 算好（账本 DispatchIssued.ts 恢复的
/// durable_base + 本进程恢复后的 monotonic delta）。durable elapsed 是账本派生的
/// `Duration` 数值，合法地可以大于 monotonic uptime，**不得**经由可能下溢的
/// `Instant::checked_sub` 间接表示（刚开机恢复长期 attempt 时下溢会把 grace
/// 静默清零）。`None` ⇒ `started.elapsed()`（进程内计时，legacy 行为）。
/// 生产唯一消费者：tierf::run_await_with_hook（跨重启恢复 attempt 时钟）。
pub fn probe_with_durable_elapsed(
    root: &Path,
    agent: &str,
    task_id: &str,
    go_path: Option<&Path>,
    write_set: &[String],
    started: Instant,
    durable_elapsed: Option<Duration>,
) -> ProbeSnapshot {
    let now = SystemTime::now();
    let age_of = |p: &Path| -> Option<Duration> {
        now.duration_since(p.metadata().ok()?.modified().ok()?).ok()
    };

    let hb_path = root.join(format!("coordination/runtime/heartbeats/{agent}.json"));
    let hb_age = age_of(&hb_path);
    let hb_json = fs::read_to_string(&hb_path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
    let hb_pid_alive = hb_json
        .as_ref()
        .and_then(|v| v.get("pid").and_then(|p| p.as_i64()))
        .map(|pid| {
            // shelled-out kill -0（项目风格，不引 libc）
            Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        });

    let (acked, ack_age) = match go_path {
        Some(gp) => {
            let ack = PathBuf::from(format!("{}.ack", gp.display()));
            (ack.is_file(), age_of(&ack))
        }
        None => (false, None),
    };

    let wt = root.join(".worktrees").join(task_id);
    let mut ages: Vec<Duration> = Vec::new();
    for p in [
        wt.clone(),
        root.join(".git/worktrees").join(task_id).join("HEAD"),
        root.join(".git/worktrees").join(task_id).join("index"),
    ] {
        if let Some(a) = age_of(&p) {
            ages.push(a);
        }
    }
    for ws in write_set {
        if let Some(a) = age_of(&wt.join(ws)) {
            ages.push(a);
        }
    }
    if let Ok(t) = gitx::commit_unix_time(root, &format!("task/{task_id}")) {
        let committed = SystemTime::UNIX_EPOCH + Duration::from_secs(t.max(0) as u64);
        if let Ok(a) = now.duration_since(committed) {
            ages.push(a);
        }
    }
    let worktree_activity_age = ages.into_iter().min();

    ProbeSnapshot {
        elapsed: durable_elapsed.unwrap_or_else(|| started.elapsed()),
        hb_age,
        hb_pid_alive,
        acked,
        ack_age,
        last_activity_age: worktree_activity_age,
    }
}

/// 纯函数判定（探测阶梯 §4.3）。Option 语义显式：`None <= Some(_)` 的隐式比较是禁区。
///
/// B112：`hb_identity` 为 probe 从心跳文件解析出的身份（round/generation/ordinal），
/// `modern` 为当前现代 dispatch 身份。两者都存在时，heartbeat 必须同时满足
/// 「mtime 新鲜」与「round/generation 精确匹配当前身份」才算 fresh——旧轮、
/// 旧 attempt、字段缺失/类型错的心跳一律 stale，不得救活当前 attempt。
/// `modern=None`（账本无现代 dispatch 身份）走 legacy 回退：仅看 mtime 新鲜。
pub fn judge_with_identity(
    s: &ProbeSnapshot,
    o: &LivenessOpts,
    hb_identity: Option<&HeartbeatIdentity>,
    modern: Option<ModernIdentity>,
) -> Judgement {
    if !s.acked {
        // —— 等待相位：心跳应当新鲜（wait 脚本 20s 一跳）——
        // B112：现代身份存在时，mtime 新鲜 ∧ identity 精确匹配才算 fresh。
        let hb_fresh = matches!(s.hb_age, Some(a) if a <= o.grace)
            && heartbeat_matches_identity(hb_identity, modern);
        if hb_fresh {
            if s.hb_pid_alive == Some(false) {
                return Judgement::DeadCandidate("心跳尚新鲜但写者 pid 已死".into());
            }
            return Judgement::Healthy("waiting");
        }
        // 防误杀①：客户端可能未走 ack 流程直接干活（活动新鲜即健康）
        if matches!(s.last_activity_age, Some(a) if a <= o.stall) {
            return Judgement::Healthy("working-no-ack");
        }
        // 防误杀②：启动宽限（粘贴 → 读协议 → 首跳）
        let boot_window = if s.last_activity_age.is_none() {
            o.cold_boot_grace
        } else {
            o.boot_grace
        };
        if s.elapsed < boot_window {
            return Judgement::Healthy("booting");
        }
        // dead 三条件齐备：心跳过期/缺失 ∧ 无 ack ∧ 无活动
        let hb_desc = match s.hb_age {
            Some(a) => format!("过期 {}s", a.as_secs()),
            None => "缺失".to_string(),
        };
        Judgement::DeadCandidate(format!("心跳{hb_desc} ∧ 无 ack ∧ 无 worktree/provider 活动"))
    } else {
        // —— 工作相位：心跳/pid 绝不参与判定（GO_FOUND 后脚本退出是常态）——
        // provider 输出已经由生产 probe 折叠进 `last_activity_age`；第三参为
        // None 时逐字保持旧的 ACK/worktree 语义。三者皆 None 则保守不升级。
        let idle = working_idle_age(s.ack_age, s.last_activity_age, None);
        match idle {
            Some(i) if i >= o.stall => {
                Judgement::StalledCandidate(format!("ack 后 {}s 零活动", i.as_secs()))
            }
            _ => Judgement::Healthy("working"),
        }
    }
}

/// Legacy judge 入口（无现代 dispatch 身份）。等价于
/// `judge_with_identity(.., None, None)`。保留给 snapshot 投影等不持有当前
/// attempt 身份的调用方——它们走 legacy 回退。
pub fn judge(s: &ProbeSnapshot, o: &LivenessOpts) -> Judgement {
    judge_with_identity(s, o, None, None)
}

/// B112 内部：判定 heartbeat 身份是否匹配现代 dispatch 身份。
/// `modern=None` ⇒ true（legacy 回退：仅看 mtime）。
/// `modern=Some` 但 `hb_identity=None`（字段缺失/类型错/无心跳）⇒ false
/// （fail-closed：identity 缺失不得形成 fresh）。
/// 两者都存在时调用 `heartbeat_is_current` 做精确匹配。
fn heartbeat_matches_identity(
    hb_identity: Option<&HeartbeatIdentity>,
    modern: Option<ModernIdentity>,
) -> bool {
    match modern {
        None => true, // legacy：仅看 mtime
        Some(modern) => match hb_identity {
            None => false, // 现代身份存在但心跳缺身份字段 → stale
            Some(hb) => heartbeat_is_current(modern.round, modern.attempt_id, hb),
        },
    }
}

/// B112：从心跳文件解析现代身份（round/generation/ordinal）。
/// 字段缺失/类型错/文件不存在 ⇒ None（fail-closed）。run_await 在 judge 前
/// 调用此函数，把结果连同当前现代 dispatch 身份传给 `judge_with_identity`。
/// 与 `probe` 共享心跳路径约定 `coordination/runtime/heartbeats/{agent}.json`。
pub fn probe_heartbeat_identity(root: &Path, agent: &str) -> Option<HeartbeatIdentity> {
    let hb_path = root.join(format!("coordination/runtime/heartbeats/{agent}.json"));
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&hb_path).ok()?).ok()?;
    let round = v.get("round")?.as_str()?;
    let generation = v.get("generation")?.as_str()?;
    let ordinal = v.get("ordinal").and_then(|o| o.as_u64()).unwrap_or(0) as usize;
    // 字段非空校验（与 HeartbeatIdentity::new 一致，fail-closed）。
    HeartbeatIdentity::new(round, generation, ordinal).ok()
}

/// macOS 通知（升级阶梯 §4.4 ②档）——best-effort，失败不阻断
pub fn notify_macos(title: &str, text: &str) {
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        text.replace('"', "\\\""),
        title.replace('"', "\\\"")
    );
    let _ = Command::new("osascript")
        .args(["-e", &script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

// ─────────────────────── B112：跨重启 attempt 时钟与 heartbeat identity ────────────────────────

/// Attempt elapsed 从当前 attempt 最新 `DispatchIssued.ts` 恢复（durable）。
/// daemon/await 重启不得重置 boot/cold grace——elapsed 由账本派发时刻与 now 决定，
/// 而非 daemon 启动时刻。时间倒退/坏 ts fail-closed 并留可诊断错误。
///
/// `dispatch_ts` 与 `now_ts` 均为 RFC3339（与 `ledger::ts_health` 同解析器：humantime）。
/// 返回 `now - dispatch`。`now < dispatch`（时间倒退）⇒ Err（fail-closed）。
pub fn attempt_elapsed_from_dispatch(dispatch_ts: &str, now_ts: &str) -> Result<Duration> {
    let dispatch = humantime::parse_rfc3339(dispatch_ts)
        .with_context(|| format!("dispatch ts 解析失败: {dispatch_ts:?}"))?;
    let now = humantime::parse_rfc3339(now_ts)
        .with_context(|| format!("now ts 解析失败: {now_ts:?}"))?;
    now.duration_since(dispatch).map_err(|_| {
        anyhow::anyhow!(
            "attempt elapsed 时间倒退: dispatch={dispatch_ts} now={now_ts}（now < dispatch）"
        )
    })
}

/// B112-A0004：attempt 当前 elapsed 的统一计算（timeout 与 liveness 同源）。
/// `clock = Some((durable_base, captured_at))`：`durable_base` 是恢复时从账本
/// `DispatchIssued.ts` 派生的 `Duration` 数值（可合法大于 monotonic uptime，
/// 不经可能下溢的 `Instant`）；`captured_at` 是本进程恢复时刻。当前 elapsed =
/// `durable_base + captured_at.elapsed()`——既不下溢（durable 从不进入
/// `Instant::checked_sub`），也不冻结为不增长快照（恢复后随本进程 monotonic
/// delta 继续累计）。`None`（账本无 DispatchIssued）⇒ `session_start.elapsed()`
/// （无派发则无 durable elapsed，退化为本次 await 会话的进程内计时）。
pub fn attempt_current_elapsed(
    clock: Option<(Duration, Instant)>,
    session_start: Instant,
) -> Duration {
    match clock {
        Some((durable_base, captured_at)) => durable_base + captured_at.elapsed(),
        None => session_start.elapsed(),
    }
}

/// 心跳身份（B112）：scaffold 生成的 heartbeat JSON 增 `round` 与 `generation`。
/// `generation` 值为 attemptId（如 `B112-A0001`）；无定向 attempt 时显式 "waiting"。
/// `ordinal` 为 attempt 序号（诊断/审计用，不参与 current 判定——身份只看 round+generation）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatIdentity {
    pub round: String,
    pub generation: String,
    pub ordinal: usize,
}

impl HeartbeatIdentity {
    /// 构造心跳身份。`round` 与 `generation` 必须非空（空串 ⇒ Err，fail-closed）。
    /// `ordinal` 仅做诊断，不做 current 判定。
    pub fn new(round: &str, generation: &str, ordinal: usize) -> Result<Self> {
        if round.is_empty() {
            bail!("HeartbeatIdentity round 不能为空");
        }
        if generation.is_empty() {
            bail!("HeartbeatIdentity generation 不能为空");
        }
        Ok(Self {
            round: round.to_string(),
            generation: generation.to_string(),
            ordinal,
        })
    }
}

/// 判定 heartbeat 是否 current：round 与 generation 都必须精确匹配当前 attempt。
/// 旧轮、旧 attempt、字段缺失/类型错 heartbeat 均不得救活当前 attempt。
/// legacy heartbeat（无现代 dispatch 身份）只在无现代 dispatch 身份时兼容——
/// 此处 current 判定要求两者都精确匹配，故 legacy（字段缺失）永远 false。
pub fn heartbeat_is_current(
    current_round: &str,
    current_attempt: &str,
    hb: &HeartbeatIdentity,
) -> bool {
    hb.round == current_round && hb.generation == current_attempt
}

/// 现代 dispatch 身份（B112）：judge 在此身份存在时调用统一 heartbeat identity 判定。
/// `round` = 当前轮（如 "r47"），`attempt_id` = 当前 attemptId（如 "B112-A0002"）。
/// None 表示账本中不存在现代 dispatch 身份，judge 走 legacy 回退（旧行为）。
#[derive(Debug, Clone, Copy)]
pub struct ModernIdentity<'a> {
    pub round: &'a str,
    pub attempt_id: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(
        elapsed_s: u64,
        hb_age_s: Option<u64>,
        pid_alive: Option<bool>,
        acked: bool,
        ack_age_s: Option<u64>,
        act_age_s: Option<u64>,
    ) -> ProbeSnapshot {
        ProbeSnapshot {
            elapsed: Duration::from_secs(elapsed_s),
            hb_age: hb_age_s.map(Duration::from_secs),
            hb_pid_alive: pid_alive,
            acked,
            ack_age: ack_age_s.map(Duration::from_secs),
            last_activity_age: act_age_s.map(Duration::from_secs),
        }
    }

    fn o() -> LivenessOpts {
        LivenessOpts::default() // grace 40s / stall 20min / boot_grace 90s
    }

    #[test]
    fn waiting_fresh_heartbeat_alive() {
        assert_eq!(judge(&snap(300, Some(10), Some(true), false, None, None), &o()), Judgement::Healthy("waiting"));
    }

    #[test]
    fn fresh_heartbeat_but_pid_dead_is_dead_candidate() {
        assert!(matches!(judge(&snap(300, Some(10), Some(false), false, None, None), &o()), Judgement::DeadCandidate(_)));
    }

    #[test]
    fn no_ack_but_recent_activity_is_healthy() {
        // 防误杀①：心跳过期但 worktree 有新活动 → working-no-ack
        assert_eq!(judge(&snap(600, Some(500), Some(true), false, None, Some(30)), &o()), Judgement::Healthy("working-no-ack"));
    }

    #[test]
    fn heartbeat_missing_with_activity_is_healthy() {
        // None 心跳 ≠ dead：活动新鲜即健康
        assert_eq!(judge(&snap(600, None, None, false, None, Some(60)), &o()), Judgement::Healthy("working-no-ack"));
    }

    #[test]
    fn boot_grace_holds_early_silence() {
        // 防误杀②：完全静默但仍在启动宽限内
        assert_eq!(judge(&snap(30, None, None, false, None, None), &o()), Judgement::Healthy("booting"));
    }

    #[test]
    fn missing_heartbeat_no_ack_no_activity_past_grace_is_dead() {
        // None 语义：心跳缺失 ≠ 新鲜（若用 None<=Some 隐式比较此例会误判 Healthy）
        assert!(matches!(judge(&snap(350, None, None, false, None, None), &o()), Judgement::DeadCandidate(_)));
    }

    #[test]
    fn expired_heartbeat_stale_activity_is_dead() {
        // 活动存在但已陈旧（>stall）不能救命
        assert!(matches!(judge(&snap(2000, Some(300), Some(true), false, None, Some(1500)), &o()), Judgement::DeadCandidate(_)));
    }

    #[test]
    fn acked_missing_heartbeat_recent_activity_is_working() {
        // 防误杀③（核心）：工作相位心跳缺失是常态，5min 前有活动 → Healthy
        assert_eq!(judge(&snap(900, None, None, true, Some(600), Some(300)), &o()), Judgement::Healthy("working"));
    }

    #[test]
    fn acked_dead_pid_recent_activity_is_working() {
        // 工作相位 pid 死亡不参与判定
        assert_eq!(judge(&snap(900, Some(700), Some(false), true, Some(600), Some(100)), &o()), Judgement::Healthy("working"));
    }

    #[test]
    fn acked_idle_past_stall_is_stalled() {
        assert!(matches!(judge(&snap(2000, None, None, true, Some(1300), Some(1250)), &o()), Judgement::StalledCandidate(_)));
    }

    #[test]
    fn acked_old_ack_but_fresh_activity_is_working() {
        // min 语义：ack 很久以前，但活动新鲜 → 不 stalled
        assert_eq!(judge(&snap(3000, None, None, true, Some(2500), Some(30)), &o()), Judgement::Healthy("working"));
    }

    #[test]
    fn acked_no_mtimes_is_conservative_working() {
        // (None, None) 不可达路径的保守语义：不升级
        assert_eq!(judge(&snap(3000, None, None, true, None, None), &o()), Judgement::Healthy("working"));
    }

    #[test]
    fn unresolved_storage_fact_turns_dead_into_replayable_stall() {
        let started = crate::ledger::event(
            "AttemptStarted",
            "runtime:orch",
            Some("B206"),
            Some("r63"),
            serde_json::json!({"attemptId":"B206-A0001"}),
        );
        let storage = crate::ledger::event(
            "EscalationRaised",
            "runtime:orch",
            Some("B206"),
            Some("r63"),
            serde_json::json!({"stage":"storage", "probe":"low"}),
        );
        let events = vec![started, storage];
        for _ in 0..3 {
            assert!(matches!(
                suppress_dead_for_storage(
                    Judgement::DeadCandidate("silent".into()),
                    &events,
                    "B206",
                    Some("B206-A0001"),
                ),
                Judgement::StalledCandidate(reason) if reason.contains("storage-exhaustion")
            ));
        }
    }
}
