//! 通道健康：把「注入发出去了，但那一轮其实没干成活」这件事变成可观测的判据。
//!
//! 背景（r45 实战）：`orch wake` 后台 spawn 子进程后**不等待**（注入 turn 可达十几分钟，
//! 同步等待会把 dispatch 阻塞成执行者时长）。代价是子进程如何结束**无人记录**——
//! r45 内先后发生四类静默失败，全部只能靠 planner 人肉翻日志才发现：
//!
//! 1. 沙箱自动拒绝（读 `/tmp/*`、读 `~/.cargo/registry/**`）→ **当场终结 turn**（3 次）；
//! 2. 常驻会话 rollout 记录丢失 → `resume` 仍握手成功但消息写不进去（errata O32）；
//! 3. 常驻会话上下文耗尽 → 执行者自报「上下文已满，无法在本会话完成」（B102）；
//! 4. 账号级会话上限 / 无容量 / 读超时（agy 连续两次）。
//!
//! 本模块只做**纯判据**（吃日志文本吐结论）与一个最小 IO 探针（pid 是否存活），
//! 判据与 IO 分层沿用 `liveness.rs` 的 probe/judge 先例。
//!
//! **不做**的事：不自动重发、不自动降级、不改 `agents.yaml`——处置权仍在 planner
//! （与决策 4「假死告警只提示、不自动恢复」一致）。

use std::path::{Path, PathBuf};

/// A single metadata observation of an exact per-attempt wake log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WakeLogSample {
    pub len: u64,
    pub modified: std::time::SystemTime,
}

/// Probe SmartClaw's durable log proxy.
///
/// Only observed forward progress is affirmative. A first sample, a stagnant
/// log, metadata failure, or clock/size regression is ambiguous (`None`) and is
/// never collapsed into backend death (`Some(false)`).
pub fn probe_wake_log_progress(
    log_path: &Path,
    previous: &mut Option<WakeLogSample>,
) -> Option<bool> {
    let metadata = std::fs::metadata(log_path).ok()?;
    let current = WakeLogSample {
        len: metadata.len(),
        modified: metadata.modified().ok()?,
    };
    let advanced = previous
        .as_ref()
        .is_some_and(|old| current.len > old.len || current.modified > old.modified);
    *previous = Some(current);
    advanced.then_some(true)
}

/// 通道失效形态。命中即说明「这一次注入大概率没产出有效工作」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChannelFault {
    /// 工具权限被自动拒绝（沙箱边界）——r45 实测会**当场终结 turn**。
    SandboxRejected,
    /// 会话存储损坏：resume 成功但消息追加不进去（O32）。
    ThreadNotFound,
    /// 上下文窗口耗尽，执行者自报无法在本会话完成。
    ContextExhausted,
    /// 账号级会话上限 / 配额 / 无容量。
    SessionLimit,
    /// 读超时、无响应。
    Timeout,
}

impl ChannelFault {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChannelFault::SandboxRejected => "sandbox-rejected",
            ChannelFault::ThreadNotFound => "thread-not-found",
            ChannelFault::ContextExhausted => "context-exhausted",
            ChannelFault::SessionLimit => "session-limit",
            ChannelFault::Timeout => "timeout",
        }
    }

    /// 面向 planner 的处置建议（**只复制不执行**，与 TUI 只读铁律一致）。
    pub fn hint(&self) -> &'static str {
        match self {
            ChannelFault::SandboxRejected => {
                "注入 turn 被沙箱拒绝终结：提示词需前置声明沙箱边界（禁 /tmp、禁读依赖源码），然后重发"
            }
            ChannelFault::ThreadNotFound => {
                "常驻会话已损坏（O32）：改为每次新建会话，或 orch session set <agent> <新 id>"
            }
            ChannelFault::ContextExhausted => {
                "会话上下文耗尽：换新会话并用自含富注入（O10）续跑，现场保留在 worktree"
            }
            ChannelFault::SessionLimit => "账号级上限/无容量：换通道或等配额恢复，不要空转重试",
            ChannelFault::Timeout => "通道无响应：先探活再决定是否改派，不要连发注入",
        }
    }
}

/// 纯判据：扫描一次注入的 wake 日志文本，给出命中的失效形态（去重、稳定序）。
///
/// 判据全部来自 r45 的**真实日志原文**，不臆造：
/// - `permission requested: ... auto-rejecting`（opencode 沙箱）
/// - `failed to record rollout items` / `thread ... not found`（codex 会话损坏）
/// - `上下文已满` / `context window` + `full`（执行者自报）
/// - `session limit` / `No capacity available` / `UNAVAILABLE`
/// - `timeout waiting for response` / `读超时`
pub fn scan_wake_log(text: &str) -> Vec<ChannelFault> {
    let mut hits: Vec<ChannelFault> = Vec::new();
    let push = |f: ChannelFault, hits: &mut Vec<ChannelFault>| {
        if !hits.contains(&f) {
            hits.push(f);
        }
    };
    // **逐行 + 同行共现**（r45 实测教训）：早期版本在整份日志里裸匹配子串，
    // 结果把执行者**自己写的代码注释**（`// ...auto-reject）。`）当成了沙箱拒绝——
    // wake 日志里含 agent 读写的文件内容，裸子串必然误报。
    // 判据一律要求同一行上出现 harness 的**完整签名**，而不是单个词。
    for line in text.lines() {
        let l = line.to_lowercase();
        // opencode: `! permission requested: external_directory (/tmp/*); auto-rejecting`
        if l.contains("permission requested") && l.contains("reject") {
            push(ChannelFault::SandboxRejected, &mut hits);
        }
        // codex: `ERROR codex_core::session: failed to record rollout items: thread <id> not found`
        if l.contains("failed to record rollout items")
            || (l.contains("thread ") && l.contains("not found") && l.contains("error"))
        {
            push(ChannelFault::ThreadNotFound, &mut hits);
        }
        // 执行者自报（B102 原文）：「上下文窗口已满，我无法在这个会话中完成 …」
        if (line.contains("上下文") && (line.contains("已满") || line.contains("耗尽")))
            || l.contains("context window is full")
        {
            push(ChannelFault::ContextExhausted, &mut hits);
        }
        // 账号级：`You've hit your session limit · resets ...` / `No capacity available` / `UNAVAILABLE (code 503)`
        if l.contains("session limit")
            || l.contains("no capacity available")
            || (l.contains("unavailable") && l.contains("503"))
        {
            push(ChannelFault::SessionLimit, &mut hits);
        }
        // agy: `Error: timeout waiting for response`
        if l.contains("timeout waiting for response") || line.contains("[multica] 读超时") {
            push(ChannelFault::Timeout, &mut hits);
        }
    }
    hits.sort();
    hits
}

/// 五个 provider 的统一身份。B110 合入前各 provider 的 spawn/engaged 判据
/// 散在 adapter 里、口径不一（r45 实测同一种「进程退出」被两个 adapter 判成不同态）。
/// 本枚举把身份归一，但**不**内嵌判据——判据在 `normalize_provider_state` 里。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum ProviderKind {
    Codex,
    OpenCode,
    SmartClaw,
    Agy,
    Dclaw,
}

impl ProviderKind {
    /// 稳定字符串（告警 / snapshot 投影用；不参与判据）。
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderKind::Codex => crate::harness::HarnessId::Codex.as_str(),
            ProviderKind::OpenCode => crate::harness::HarnessId::OpenCode.as_str(),
            ProviderKind::SmartClaw => crate::harness::HarnessId::SmartClaw.as_str(),
            ProviderKind::Agy => crate::harness::HarnessId::Agy.as_str(),
            ProviderKind::Dclaw => crate::harness::HarnessId::Dclaw.as_str(),
        }
    }
}

/// provider 的归一化状态。优先级：**Fault > Engaged > Pending**。
///
/// 语义铁律（任务卡 B111）：
/// - spawn/pid alive 只能证明 `Pending`（进程起来了 ≠ 在干活）；
/// - 只有 provider-specific 的**活动事实**（turn.started / 工具调用等 action 日志）
///   才能升 `Engaged`；
/// - `Fault` 带原因串，命中 `scan_wake_log` 的 harness 完整签名即升 Fault，
///   即使同时有活动事实也以 Fault 为准（fault 终结 turn，活动是残影）。
///
/// `Unknown` 仅用于「形状未知」——例如上层还没传齐事实、或 provider 形状尚未识别，
/// **不**用于「无活动」（无活动是 `Pending`）。snapshot 投影对未知 provider 显式 Unknown，
/// 不猜 Engaged。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum ProviderState {
    /// 进程在 / 已 spawn，但没有任何 provider-specific 活动事实。
    Pending,
    /// 命中 provider-specific 活动事实（turn.started 等 action 日志切片）。
    Engaged,
    /// 命中通道失效（带原因串，复用 `scan_wake_log` 的 harness 完整签名判据）。
    Fault(String),
    /// 形状未知：上层未提供足够事实、或 provider 形状尚未识别。绝不用于「无活动」。
    Unknown,
}

/// 把 provider 的**事实切片**归一成统一状态。纯函数，不碰文件系统、不按 mtime 猜文件。
///
/// 参数语义（任务卡 B111：「归一化器接受精确 action 日志切片」）：
/// - `kind`：provider 身份（仅决定它属于同一状态机，不改变判据本身——
///   五 provider 共享一套 Pending/Engaged/Fault，避免任一漏判）。
/// - `spawn_seen`：是否见到 spawn / pid alive 事实。**只能证明 Pending**，
///   不得单独升 Engaged（M2 反例：把 spawn 当 Engaged → `spawn_is_only_pending` 红）。
/// - `activity_seen`：是否见到 provider-specific 的活动事实（turn.started / 工具调用等
///   由调用方从精确 action 日志切片里判定后传入；本函数不自行扫 mtime）。
/// - `action_log_slice`：一次注入的**精确 action 日志切片**（非整份 wake 日志的裸子串——
///   调用方负责切片，避免把模型正文 / 代码注释里提到的 fault 关键词误报成 Fault）。
///
/// 优先级：**Fault > Engaged > Pending**。
/// - 见到 Fault 签名即 Fault，即便 `activity_seen=true` 也以 Fault 为准（M1 反例）。
/// - 无 Fault 且 `activity_seen=true` → Engaged。
/// - 无 Fault 且无活动，但 `spawn_seen=true` → Pending。
/// - 都没有 → Pending（provider 还没起来也是 Pending，不是 Unknown）。
///
/// 注意：本函数**不返回 Unknown**——Unknown 是 snapshot 投影对「未知 provider / 形状未识别」
/// 的显式标记，归一化器对已识别 provider 永远吐出 Pending/Engaged/Fault 三态之一。
pub fn normalize_provider_state(
    _kind: ProviderKind,
    spawn_seen: bool,
    activity_seen: bool,
    action_log_slice: &str,
) -> ProviderState {
    // Fault 判据复用 scan_wake_log（harness 完整签名同行共现，防误报）。
    // 调用方传入的是精确 action 日志切片，不是整份 wake 日志的裸子串，
    // 因此模型正文 / 代码注释里提到 "auto-reject" 不会进到这里被误判。
    let faults = scan_wake_log(action_log_slice);
    if let Some(f) = faults.first() {
        return ProviderState::Fault(f.as_str().to_string());
    }
    // M1 铁律：Fault 优先于 Engaged。这里不得把 activity_seen 放到 fault 判据之前，
    // 否则「fault + 残影活动」会被误判成 Engaged（fault_always_wins 红）。
    if activity_seen {
        return ProviderState::Engaged;
    }
    // M2 铁律：spawn/pid alive 只能证明 Pending。这里不得因 spawn_seen=true 就升 Engaged，
    // 也不得因 spawn_seen=false 且无活动就返回 Unknown（无活动 = Pending）。
    let _ = spawn_seen; // spawn_seen 不改变结论：有它 Pending，没它也 Pending。
    ProviderState::Pending
}

/// IO 探针：进程是否仍存活（`kill(pid, 0)` 语义）。
///
/// 注意：pid 复用理论上存在误判，但注入子进程生命周期以分钟计、pid 空间以万计，
/// 误判概率极低；且本判据**只用于产出提示**，不驱动任何自动动作。
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // 不引入新依赖：用 kill -0 的 shell 语义等价物。
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 一次注入的健康结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeHealth {
    pub agent: String,
    pub log_path: PathBuf,
    pub pid: Option<u32>,
    /// 子进程是否仍在跑（None = 日志里没记 pid，无法判断）
    pub alive: Option<bool>,
    pub faults: Vec<ChannelFault>,
}

impl WakeHealth {
    /// 「注入已结束但没干成活」——进程已退出且命中了失效形态。
    /// 单纯进程退出**不算**失败（正常完成也会退出），必须叠加失效证据，
    /// 避免重演 O8 那类假死误判。
    pub fn ended_without_progress(&self) -> bool {
        self.alive == Some(false) && !self.faults.is_empty()
    }
}

/// 采样一个 agent 最近一次注入的健康度：取 per-attempt 日志（B100 起）或 legacy 日志。
pub fn probe_wake_health(root: &Path, agent: &str, pid: Option<u32>) -> Option<WakeHealth> {
    let dir = root.join("coordination/runtime/logs");
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let path = entry.path();
        let name = path.file_name()?.to_str()?.to_string();
        if !name.starts_with(&format!("wake-{agent}")) {
            continue;
        }
        let Ok(mt) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if best.as_ref().map(|(t, _)| mt > *t).unwrap_or(true) {
            best = Some((mt, path));
        }
    }
    let (_, log_path) = best?;
    let text = std::fs::read_to_string(&log_path).ok()?;
    Some(WakeHealth {
        agent: agent.to_string(),
        pid,
        alive: pid.map(pid_alive),
        faults: scan_wake_log(&text),
        log_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_rejection_is_detected() {
        // r45 实测原文（opencode）
        let log = "! permission requested: external_directory (/tmp/*); auto-rejecting\n";
        assert_eq!(scan_wake_log(log), vec![ChannelFault::SandboxRejected]);
    }

    #[test]
    fn codex_rollout_loss_is_detected() {
        // r45 实测原文（codex，O32）
        let log = "ERROR codex_core::session: failed to record rollout items: thread 019f8e57 not found\n";
        assert!(scan_wake_log(log).contains(&ChannelFault::ThreadNotFound));
    }

    #[test]
    fn context_exhaustion_is_detected() {
        let log = "上下文窗口已满，我无法在这个会话中完成 B102。\n";
        assert!(scan_wake_log(log).contains(&ChannelFault::ContextExhausted));
    }

    #[test]
    fn agy_timeout_is_detected() {
        assert!(scan_wake_log("Error: timeout waiting for response\n")
            .contains(&ChannelFault::Timeout));
    }

    #[test]
    fn code_comment_mentioning_reject_is_not_a_fault() {
        // r45 实测误报：执行者在自己的源码注释里写了 auto-reject，
        // 文件内容经工具输出进了 wake 日志 → 裸子串匹配把它当成沙箱拒绝。
        let log = "{\"type\":\"tool_use\",\"text\":\"// 读 /tmp 会被 auto-reject）。本 crate 测试一律用 .tmp-* 子目录\"}\n";
        assert!(
            scan_wake_log(log).is_empty(),
            "代码注释里提到 auto-reject 不得判成通道失效（需 harness 完整签名同行共现）"
        );
    }

    #[test]
    fn healthy_log_yields_no_fault() {
        // 正常注入：thread.started / turn.started / 工具调用
        let log = "{\"type\":\"thread.started\"}\n{\"type\":\"turn.started\"}\n{\"type\":\"item.completed\"}\n";
        assert!(scan_wake_log(log).is_empty(), "健康日志不得报失效");
    }

    #[test]
    fn process_alive_alone_is_not_a_failure() {
        // 防误杀：进程已退出但没有失效证据 → 不判「未干成活」（正常完成也会退出）
        let h = WakeHealth {
            agent: "executor-x".into(),
            log_path: PathBuf::from("/dev/null"),
            pid: Some(1),
            alive: Some(false),
            faults: vec![],
        };
        assert!(!h.ended_without_progress());

        let h2 = WakeHealth {
            faults: vec![ChannelFault::SandboxRejected],
            ..h
        };
        assert!(h2.ended_without_progress());
    }

    #[test]
    fn pid_zero_is_never_alive() {
        assert!(!pid_alive(0));
    }

    #[test]
    fn wake_log_stagnation_is_ambiguous_never_dead() {
        use std::io::Write as _;

        let dir = crate::util::test_scratch_dir("wake-log-progress");
        let path = dir.join("wake.jsonl");
        std::fs::write(&path, b"start\n").unwrap();
        let mut sample = None;
        assert_eq!(probe_wake_log_progress(&path, &mut sample), None);
        assert_eq!(probe_wake_log_progress(&path, &mut sample), None);

        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"progress\n").unwrap();
        file.sync_all().unwrap();
        assert_eq!(probe_wake_log_progress(&path, &mut sample), Some(true));
        assert_eq!(probe_wake_log_progress(&path, &mut sample), None);
    }
}
