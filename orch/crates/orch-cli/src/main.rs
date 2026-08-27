//! orch · 多 Agent 协同运行时 CLI（M1 首切片）
//! 纪律（errata E6/E7）：启动即把 --root 规范化为绝对路径；每个子命令单一职责。

mod guide;

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use orch_core::{doctor, fold, read_ledger, CheckStatus, TaskState};

#[derive(Parser)]
#[command(name = "orch", version, about = "多 Agent 协同机制运行时（M1 切片）")]
struct Cli {
    /// 目标仓库根（将被规范化为绝对路径——errata E6）
    #[arg(long, default_value = ".")]
    root: PathBuf,
    /// 显式逃生舱：允许陈旧二进制执行状态变更命令（会在 stderr 留证）
    #[arg(long, global = true)]
    allow_stale_binary: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Internal one-shot detached provider owner. Launch spec is inherited on stdin.
    #[command(name = "__wake-supervise", hide = true)]
    WakeSupervise,
    /// 幂等注入 coordination/ 骨架、gitignore/attributes 规则与等待脚本
    Init,
    /// 只读探测项目生态并打印 PROJECT-BINDING.yaml 提议（不写盘）
    Bind,
    /// coordination/ 布局体检（gitignore 三行 / union merge / wait 脚本 / 账本可解析）
    Doctor,
    /// 只读停滞体检：账本 + git + 进程 + wake 日志 → 确定性介入判定
    #[command(name = "stall-check")]
    StallCheck,
    /// WAL 对账恢复：缺省干跑，只有 strict-prefix 账本允许逐字节回灌
    Ledger {
        #[command(subcommand)]
        action: LedgerCmd,
    },
    /// Reconcile durable review lifecycle facts from committed artifacts.
    Review {
        #[command(subcommand)]
        action: ReviewCmd,
    },
    /// Lease-gated review-site lifecycle operations.
    Sites {
        #[command(subcommand)]
        action: SitesCmd,
    },
    /// Typed agent/tool registry inspection.
    Agent {
        #[command(subcommand)]
        action: AgentCmd,
    },
    /// Activate a signed dormant policy or deactivate its exact active generation.
    #[command(name = "runtime-policy")]
    RuntimePolicy {
        #[command(subcommand)]
        action: RuntimePolicyCmd,
    },
    /// 折叠当前轮事件账本为任务状态投影（事件是事实，状态是投影）
    Status,
    /// 打印协议 v0 的 EventRecord JSON Schema
    Schema,
    /// 输出编译进二进制的 AI 机械契约指南（零项目依赖、零写盘）
    Guide {
        /// 只输出一个稳定章节
        #[arg(long, conflicts_with = "check")]
        section: Option<String>,
        /// 将指南标记与当前公开 CLI 命令树做精确覆盖检查
        #[arg(long)]
        check: bool,
    },
    /// 驱动一整棒（Tier S）：lease worktree → spawn 执行者 → 收 REPORT → 机检 → 跑门 → 落账
    RunTask {
        /// 任务 ID（当前轮任务卡须存在）
        task: String,
        /// 覆盖任务卡的 agent（内置：opencode / codex / claude）
        #[arg(long)]
        agent: Option<String>,
        /// 执行者超时（秒）
        #[arg(long, default_value = "900")]
        timeout_secs: u64,
    },
    /// 独立机检（不 spawn agent）：域/提交形状/种子 SHA——故障注入验证用
    Check { task: String },
    /// legacy verifier（model 只从 signed ROUND-IR 读取；root-manual 下拒绝）
    Verify {
        task: String,
        #[arg(long, default_value = "600")]
        timeout_secs: u64,
    },
    /// Root-only mechanical fixed-HEAD verdict; derives review/evidence paths from ROUND-IR
    Verdict {
        task: String,
        #[arg(long)]
        attempt: String,
        #[arg(long)]
        expected_head: String,
        #[arg(long)]
        expected_main: String,
        #[arg(long, value_parser = ["pass", "fail", "blocked"])]
        verdict: String,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// One-command normal close: capture main -> root PASS -> merge -> record -> prove postconditions
    Seal {
        task: String,
        #[arg(long)]
        attempt: String,
        #[arg(long)]
        expected_head: String,
    },
    /// 调试/恢复兼容入口；正常收口请使用 seal
    Merge {
        task: String,
        /// H29 释放臂：合并已落但合后门红、补记门永不可能转绿时，
        /// 三条件 fail-closed 后闭合屏障解冻全轮（**不落 TaskRecorded**）。
        /// 屏障悬空（merge 从未发生）时走的仍是 B153 的三条件恢复。
        #[arg(long)]
        recover: bool,
    },
    /// 补记（R45S/H29）：默认在 merge SHA 跑门；显式 --at-tip 可在留证后放宽到 main 尖端
    Record {
        task: String,
        /// 显式逃生舱：在 current main tip 实跑全部 fast gate，并落完整 RecordGateRelaxed 留证
        #[arg(long)]
        at_tip: bool,
    },
    /// 快照（B102 接线）：采样心跳/GO/worktree → 纯聚合 OrchSnapshot；只读，不写账本
    Snapshot {
        /// 输出完整 JSON（默认打人读摘要）
        #[arg(long)]
        json: bool,
        /// 额外原子写到 coordination/runtime/snapshot.json（约定仅 daemon 用）
        #[arg(long)]
        write: bool,
    },
    /// Tier F 派发：写 GO 信号文件（E5：worktree 命令直接给 SHA）+ 落账 + 唤醒分发
    Dispatch {
        task: String,
        /// 逃生舱：跳过 wake 注入，仅走 POKE 备用道
        #[arg(long)]
        no_wake: bool,
        /// planner 显式接管：即使上一 attempt 的 durable 活性仍不明确也允许改派
        #[arg(long)]
        override_ambiguous_active: bool,
        /// Explicitly terminate an approved, pre-merge attempt and mint its successor
        #[arg(long)]
        new_attempt: bool,
        /// Required non-empty audit reason for --new-attempt
        #[arg(long, requires = "new_attempt")]
        reason: Option<String>,
    },
    /// Tier F 收取：双根 stat 轮询等 REPORT/BLOCKED；REPORT → 机检 → 门；O7 liveness 联动
    /// （退出码：0=收取成功 2=入口普通错误 3=执行者死亡 4=停滞/整段超时/内层拒绝 6=执行者阻塞）
    AwaitReport {
        task: String,
        #[arg(long)]
        timeout_secs: Option<u64>,
        /// 心跳新鲜阈值（秒，design/03 §4.3 waiting 行）
        #[arg(long, default_value = "40")]
        liveness_grace_secs: u64,
        /// 关闭 liveness 探测（回到纯 REPORT 轮询旧行为）
        #[arg(long)]
        no_liveness: bool,
    },
    /// await-report 判死后有界退避重派；GiveUp 才交 planner 改派或 BLOCKED
    RetryDead {
        task: String,
        #[arg(long, default_value = "3")]
        max_retries: u8,
        #[arg(long, default_value = "30")]
        base_secs: u64,
    },
    /// 渲染 Tier F 客户端 bootstrap 提示词（--copy 进剪贴板）
    Bootstrap {
        agent: String,
        #[arg(long)]
        copy: bool,
    },
    /// 轮生命周期：open / sign-off / seed-verified / close（消除人肉开轮收轮）
    Round {
        #[command(subcommand)]
        action: RoundCmd,
    },
    /// 升级阶梯①：写 NUDGE 催醒信号（客户端回到 wait 循环时消费）+ NudgeIssued + 唤醒分发
    Nudge {
        agent: String,
        /// 必须显式指定 task；生产 nudge 不按 agent 的最近任务猜测 attempt 身份
        #[arg(long)]
        task: String,
        /// 显式消息文本；值 `-` 表示从 stdin 读取；省略则用默认提示词
        #[arg(long)]
        message: Option<String>,
        /// 从文件读取消息（与 --message 互斥；空文件是显式空消息，不回退默认）
        #[arg(long)]
        message_file: Option<PathBuf>,
        /// 覆盖尚未被消费的旧 NUDGE
        #[arg(long)]
        force: bool,
        /// 逃生舱：跳过 wake 注入，仅走 POKE 备用道
        #[arg(long)]
        no_wake: bool,
    },
    /// 向已登记的常驻会话注入一条唤醒消息（design/10 §4）
    Wake {
        agent: String,
        /// status/attach/cancel/declare-dead action 的 managed wakeId；普通 wake 禁止第二 positional
        wake_id: Option<String>,
        /// attach/cancel action 的有界原因；普通 wake 禁止使用
        #[arg(long)]
        reason: Option<String>,
        /// attach action 的显式模式；resume 失败绝不自动 fallback 到 fork
        #[arg(long, value_parser = ["resume", "fork"])]
        mode: Option<String>,
        /// status action 输出 redacted JSON
        #[arg(long)]
        json: bool,
        /// 显式消息文本；值 `-` 表示从 stdin 读取；省略则用默认提示词
        #[arg(long)]
        message: Option<String>,
        /// 从文件读取消息（与 --message 互斥；空文件是显式空消息，不回退默认）
        #[arg(long)]
        message_file: Option<PathBuf>,
        /// 显式重投一个已声明死亡的 managed OpenCode formal-review wake
        #[arg(long, value_name = "SOURCE_WAKE_ID")]
        reissue: Option<String>,
        /// 将本次真实 wake 记为指定 task 的审查请求
        #[arg(long)]
        review_for: Option<String>,
        /// 被审查的 attempt identity（与 --review-for/--role 成组）
        #[arg(long)]
        attempt: Option<String>,
        /// 审查角色（与 --review-for/--attempt 成组）
        #[arg(long, value_parser = ["primary", "secondary", "nongate"])]
        role: Option<String>,
        /// 审查超期秒数；0 表示关闭超期，省略时按 signed ROUND-IR 的 role × evidence 推导
        #[arg(long)]
        deadline_secs: Option<u64>,
    },
    /// 派发前探活：planner/daemon 应先 handshake 成功再 dispatch，失败不耗 attempt
    Handshake {
        agent: String,
        #[arg(long, default_value = "60")]
        timeout_secs: u64,
        /// 逃生舱：跳过 probe wake（无注入即无精确 logPath，通道显式 Pending 退出）
        #[arg(long)]
        no_wake: bool,
    },
    /// 升级阶梯③：从账本+git 重建现场渲染 RESUME 提示词（粘贴进新会话）+ ResumeIssued
    Resume {
        task: String,
        #[arg(long)]
        copy: bool,
    },
    /// 成本报表 v1（决策 12）：折叠账本 verifier $/门耗时/agent 时长/tokens/升级次数
    Cost {
        /// 指定轮（默认当前轮）
        #[arg(long)]
        round: Option<String>,
    },
    /// 只读调度决策面（B135）：升级链 / next_candidate / 审查链——零副作用零落账
    Schedule {
        /// 任务 ID（当前轮任务卡须存在）
        task: String,
    },
    /// 编译当前轮 ModeConfig + 任务卡为经过静态校验的 ROUND-IR
    Plan,
    /// 方案期本地多模型咨询；PlanSignedOff 后机械拒绝
    Consult {
        /// 包含咨询问题的 UTF-8 文件
        question: PathBuf,
        /// consultation preset 名
        #[arg(long, default_value = "default")]
        preset: String,
        /// 仓内 UTF-8 附件（可重复）
        #[arg(long = "attach")]
        attach: Vec<PathBuf>,
        /// planner（缺省）或本地 judge adapter
        #[arg(long, default_value = "planner", conflicts_with = "no_judge")]
        judge: String,
        /// 跳过综合，只保留 fusion 原始回答
        #[arg(long)]
        no_judge: bool,
        /// 覆盖 preset 的单成员超时
        #[arg(long)]
        member_timeout_secs: Option<u64>,
        /// 覆盖 preset 的整单墙钟上限
        #[arg(long)]
        total_wall_secs: Option<u64>,
    },
    /// 为任务记录一项高风险动作的人工审批决定
    Approve {
        task: String,
        #[arg(long, value_parser = ["push", "publish", "delete-recursive", "network", "install"])]
        action: String,
        #[arg(long, value_parser = ["approved", "denied"])]
        decision: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// 前台持续驱动当前轮（每 30 秒重读账本，可 Ctrl-C）
    Run,
    /// 单步驱动当前轮一次
    Step,
    /// B151 只读派生：从账本+git 生成 coordination/CURRENT.md（活轮/main SHA/任务投影；幂等覆写单文件）
    Current,
    /// 常驻 daemon：机械调度 + 判断边界注入主控
    Serve {
        /// 只运行一拍后返回
        #[arg(long)]
        once: bool,
        /// 禁用独立 provider liveness monitor，回退为旧 daemon 行为
        #[arg(long)]
        no_monitor: bool,
    },
    /// 极简同步 MCP stdio 服务（只读工具面）
    Mcp {
        #[command(subcommand)]
        action: McpCmd,
    },
    /// 会话注册表管理：show 列各 agent 注入态/sessionId；set 回填 sessionId
    Session {
        #[command(subcommand)]
        action: SessionCmd,
    },
    /// INBOX 指令队列：add 写指令 / list 列待处理 / done 移至完成
    Inbox {
        #[command(subcommand)]
        action: InboxCmd,
    },
    /// Parallel 波次调度器：枚举当前轮任务 → 分波 → 串行 dispatch/并发 collect/串行 merge
    RunWave {
        #[arg(long, default_value = "1800")]
        timeout_secs: u64,
    },
}

#[derive(Subcommand)]
enum McpCmd {
    /// stdin 逐行接收 JSON-RPC，stdout 逐行返回 MCP 应答
    Serve,
}

#[derive(Subcommand)]
enum LedgerCmd {
    /// 审核或执行 WAL 缺失后缀回灌；分叉永远拒绝
    Recover {
        /// 指定轮（默认 CURRENT-ROUND）
        #[arg(long)]
        round: Option<String>,
        /// 执行原子回灌；省略时只打印计划
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Subcommand)]
enum ReviewCmd {
    /// Append missing exact ReviewDelivered events from one fixed main commit.
    Reconcile {
        task: String,
        #[arg(long)]
        attempt: String,
    },
    /// Promote one staged review before verdict, or route it monotonically after verdict.
    Deliver {
        task: String,
        attempt: String,
        #[arg(long, value_parser = ["primary", "secondary"])]
        role: String,
        #[arg(long)]
        agent: String,
    },
    /// Dynamic review-pool routing commands.
    Panel {
        #[command(subcommand)]
        action: ReviewPanelCmd,
    },
}

#[derive(Subcommand)]
enum ReviewPanelCmd {
    /// Commit one three-seat panel and its routes before spawning providers.
    Select {
        task: String,
        #[arg(long)]
        attempt: String,
        #[arg(long = "seat", required = true)]
        seats: Vec<String>,
    },
    /// Route generation two for one business-invalid seat.
    Retry {
        task: String,
        #[arg(long)]
        attempt: String,
        #[arg(long)]
        seat_id: String,
    },
    /// Route one unused signed candidate for a system-terminal-invalid seat.
    Backfill {
        task: String,
        #[arg(long)]
        attempt: String,
        #[arg(long)]
        seat: String,
    },
}

#[derive(Subcommand)]
enum SitesCmd {
    /// Re-read the ledger under lock and reclaim only released generations.
    Gc {
        /// Round to inspect (defaults to CURRENT-ROUND).
        #[arg(long)]
        round: Option<String>,
    },
    /// Remove expired entries from orch/target/test-tmp (keep-aware host primitive).
    SweepScratch {
        /// Entries newer than this many hours are preserved.
        #[arg(long, default_value = "24")]
        ttl_hours: u64,
    },
    /// Remove old trial-cache generations, preserving the newest generation in every slot.
    SweepTrialCache,
    /// Gzip logs attributable to ledger-terminal rounds; open-round logs are preserved.
    RotateLogs,
    /// Remove stale review/consult target directories without touching debug or active leases.
    SweepTargets {
        /// Entries newer than this many hours are preserved.
        #[arg(long, default_value = "24")]
        ttl_hours: u64,
    },
}

#[derive(Subcommand)]
enum AgentCmd {
    /// List registered identity, tool, model/effort source and responsibility.
    List,
    /// Parse and validate the complete registry and every referenced tool.
    Lint,
    /// Amend only provider/model/effort under the active signed plan and append an audit fact.
    #[command(name = "set-pin")]
    SetPin {
        /// Exact registered agent identity to amend.
        agent: String,
        /// Replacement provider pin; omitted means unchanged.
        #[arg(long)]
        provider: Option<String>,
        /// Replacement model pin; omitted means unchanged.
        #[arg(long)]
        model: Option<String>,
        /// Replacement effort pin; omitted means unchanged.
        #[arg(long)]
        effort: Option<String>,
        /// Non-empty attribution recorded with the durable amendment event.
        #[arg(long)]
        reason: String,
    },
}

#[derive(Subcommand)]
enum RuntimePolicyCmd {
    /// Activate a signed policy after its owner task is Recorded.
    Activate {
        /// Exact key under PROJECT-BINDING.runtimePolicies.policies.
        policy: String,
    },
    /// Deactivate an active policy for future dispatch bases.
    Deactivate {
        /// Exact signed policy key.
        policy: String,
        /// Non-blank durable audit reason.
        #[arg(long)]
        reason: Option<String>,
    },
}

#[derive(Subcommand)]
enum InboxCmd {
    /// 写一条指令到 inbox（生成时间戳-slug.md）
    Add {
        /// 指令文本
        instruction: String,
    },
    /// 列出 Pending 状态的指令
    List,
    /// 将指令移至 Done
    Done {
        /// 文件名（含 .md）
        file: String,
    },
}

#[derive(Subcommand)]
enum SessionCmd {
    /// 列注册表各 agent 的 configured/effective/overlay
    Show,
    /// 显式配置某 agent 的 mode/sessionId（自动故障降级不会改 registry）
    Set {
        agent: String,
        session_id: String,
        #[arg(long)]
        mode: Option<String>,
    },
}

#[derive(Subcommand)]
enum RoundCmd {
    /// 开轮：目录骨架(O6) → RoundOpened → CURRENT-ROUND → BOARD 开版行
    Open {
        id: String,
        #[arg(long, default_value = "")]
        purpose: String,
        /// Legacy flag retained for syntax compatibility; currently fail-closed（先 plan 再 sign-off）
        #[arg(long)]
        signed_off: bool,
        #[arg(long)]
        sign_note: Option<String>,
        /// 上一轮未收也强行开新轮
        #[arg(long)]
        force: bool,
    },
    /// 计划签核 PlanSignedOff（actor=user）——HITL#1 显式命令面，永不被其他命令隐式触发
    SignOff {
        #[arg(long)]
        note: Option<String>,
    },
    /// 核对种子 SHA + 真跑 oracle 预验（隔离 worktree 落位种子跑测试门，O5 机器化判据）
    SeedVerified {
        task: String,
        #[arg(long)]
        expected_red: String,
        /// 逃生舱：仅记录人肉预验结论，不真跑（M1 旧行为）
        #[arg(long)]
        record_only: bool,
        /// 允许种子文件内 passed 数（默认 0 = O5 无空洞位强制）
        #[arg(long, default_value = "0")]
        allow_file_passed: usize,
    },
    /// 收轮：核账（全 Recorded）→ dispatch/DONE.md → RoundClosed → BOARD 收轮行 → O1 提醒
    Close {
        #[arg(long)]
        force: bool,
        #[arg(long)]
        note: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WakeControlAction {
    Status,
    Attach,
    Cancel,
    DeclareDead,
}

impl WakeControlAction {
    const ACTIONS: [(&'static str, Self); 4] = [
        ("attach", Self::Attach),
        ("cancel", Self::Cancel),
        ("declare-dead", Self::DeclareDead),
        ("status", Self::Status),
    ];

    fn parse(value: &str) -> Option<Self> {
        Self::ACTIONS
            .iter()
            .find_map(|(name, action)| (*name == value).then_some(*action))
    }

    fn names() -> Vec<&'static str> {
        Self::ACTIONS.iter().map(|(name, _)| *name).collect()
    }
}

fn cli_error_exit_code(error: &anyhow::Error) -> u8 {
    orch_host::failure::cli_error_exit_code(error)
}

/// Round close may have already reconciled receipts, archived a snapshot, or
/// reclaimed storage before a later guard rejects. Bind that command boundary
/// conservatively so a nested zero-effect leaf cannot misdescribe the whole run.
fn bind_round_close_failure(error: anyhow::Error) -> anyhow::Error {
    orch_host::failure::with_cli_disposition(
        error,
        orch_host::failure::CliDisposition::EffectUnknown,
    )
}

fn active_round_contract(
    root: &std::path::Path,
) -> Result<(String, orch_host::plan::ReadonlyIrValidation)> {
    let round = orch_host::current_round(root)?;
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let ledger = read_ledger(&ledger_path)
        .with_context(|| format!("入口 preflight 读取账本失败: {}", ledger_path.display()))?;
    if !ledger.bad_lines.is_empty() {
        bail!(
            "入口 preflight 拒绝坏账本（{} 行；首坏行 #{})",
            ledger.bad_lines.len(),
            ledger.bad_lines[0].0
        );
    }
    let active = orch_host::plan::require_active_round_ir(root, &round, &ledger.events)?;
    Ok((round, active))
}

fn validated_round_contract(
    root: &std::path::Path,
) -> Result<(String, orch_host::plan::ReadonlyIrValidation)> {
    let round = orch_host::current_round(root)?;
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let ledger = read_ledger(&ledger_path)
        .with_context(|| format!("入口 preflight 读取账本失败: {}", ledger_path.display()))?;
    if !ledger.bad_lines.is_empty() {
        bail!("入口 preflight 拒绝坏账本");
    }
    let validated = orch_host::plan::require_validated_round_ir(root, &round, &ledger.events)?;
    Ok((round, validated))
}

#[derive(Clone, Copy)]
enum CommandEffectPolicy {
    ReadOnly,
    BootstrapOnly,
    PlanMutation,
    PlanConsultation,
    Signoff,
    RequiresValidated,
    RequiresActive,
    /// H29：恢复出口专用——不校验 IR（漂移本身可能正是冻结原因），
    /// 由恢复臂自带的 fail-closed 判据兜底。
    RecoveryOnly,
    RootManualForbidden,
}

fn command_effect_policy(command: &Cmd) -> CommandEffectPolicy {
    use CommandEffectPolicy::*;
    match command {
        Cmd::WakeSupervise => ReadOnly,
        Cmd::Init => BootstrapOnly,
        Cmd::Bind
        | Cmd::Doctor
        | Cmd::StallCheck
        | Cmd::Status
        | Cmd::Schema
        | Cmd::Guide { .. }
        | Cmd::Check { .. }
        | Cmd::Cost { .. }
        | Cmd::Schedule { .. }
        | Cmd::Mcp { .. }
        | Cmd::Agent {
            action: AgentCmd::List | AgentCmd::Lint,
        } => ReadOnly,
        // B151：orch current 只读派生 + 幂等覆写 coordination/CURRENT.md 单文件，
        // 属运行时投影写（同 BOARD 先例），不落账本、无其他副作用；按 ReadOnly 归类
        // 保持 mutating-CLI 枚举测试绿（写 CURRENT.md 不经 active_round_contract lease）。
        Cmd::Current => ReadOnly,
        Cmd::Bootstrap { copy: false, .. } => ReadOnly,
        Cmd::Bootstrap { copy: true, .. } => RequiresActive,
        Cmd::Plan => PlanMutation,
        Cmd::Consult { .. } => PlanConsultation,
        Cmd::Round {
            action: RoundCmd::Open { .. },
        } => BootstrapOnly,
        Cmd::Round {
            action: RoundCmd::SignOff { .. },
        } => Signoff,
        Cmd::Round {
            action: RoundCmd::SeedVerified { .. },
        } => RequiresValidated,
        Cmd::Session {
            action: SessionCmd::Show,
        }
        | Cmd::Inbox {
            action: InboxCmd::List,
        } => ReadOnly,
        Cmd::Session {
            action: SessionCmd::Set { .. },
        } => RootManualForbidden,
        // H29：恢复出口必须始终可达。IR 漂移本身就是能冻结一轮的原因之一
        // （B156 改了摘要算法 ⇒ 本轮自己那份旧算法签的 IR 立刻失配），若恢复
        // 也要求非漂移 IR，则「屏障常开 + IR 漂移」组合成死锁。释放臂不消费
        // 授权链，自带三条 fail-closed 判据（账本有 post-merge-gate 红 / main
        // 仍含该 merge / 门在 main 尖端复跑全绿），故按 RequiresActive 归类。
        Cmd::Merge { recover: true, .. }
        | Cmd::Ledger {
            action: LedgerCmd::Recover { .. },
        } => RecoveryOnly,
        Cmd::RunTask { .. }
        | Cmd::Review { .. }
        | Cmd::Sites { .. }
        | Cmd::Verify { .. }
        | Cmd::Verdict { .. }
        | Cmd::Seal { .. }
        | Cmd::Merge { .. }
        | Cmd::Record { .. }
        | Cmd::Snapshot { .. }
        | Cmd::Dispatch { .. }
        | Cmd::AwaitReport { .. }
        | Cmd::RetryDead { .. }
        | Cmd::Nudge { .. }
        | Cmd::Wake { .. }
        | Cmd::Handshake { .. }
        | Cmd::Resume { .. }
        | Cmd::Approve { .. }
        | Cmd::Agent {
            action: AgentCmd::SetPin { .. },
        }
        | Cmd::RuntimePolicy { .. }
        | Cmd::Run
        | Cmd::Step
        | Cmd::Serve { .. }
        | Cmd::Inbox {
            action: InboxCmd::Add { .. } | InboxCmd::Done { .. },
        }
        | Cmd::RunWave { .. }
        | Cmd::Round {
            action: RoundCmd::Close { .. },
        } => RequiresActive,
    }
}

/// Production pre-close cleanup, deliberately adjacent to the command-permit
/// classifier arm so the frozen source-wiring contract sees the same calls the
/// real handler executes.
const ROUND_CLOSE_SCRATCH_TTL: Duration = Duration::from_secs(24 * 60 * 60);

fn prepare_round_close_cleanup(root: &std::path::Path) -> Vec<String> {
    let mut site_gc_summary = Vec::new();
    let mut protected_site_ids = None;
    match orch_host::current_round(root) {
        Ok(round) => {
            // Receipt reconciliation may append useful terminal facts, but a
            // stale receipt is diagnostics only: it cannot disable site GC.
            if let Err(error) = orch_host::wake::reconcile_pending_backend_receipts(root, &round) {
                println!(
                    "⚠️ 收轮前 backend receipt 对账失败；已降级为诊断并继续 site GC: {error:#}"
                );
            }

            let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
            match read_ledger(&ledger_path) {
                Ok(ledger) if ledger.bad_lines.is_empty() => {
                    match orch_host::sites::reclaim_report(root, &ledger.events) {
                        Ok(report) => {
                            println!("· 收轮前三态现场报告（只读）：{} 项", report.len());
                            for entry in report {
                                println!(
                                    "  {} {} · {} · {}",
                                    entry.state.label(),
                                    entry.site_id,
                                    entry.worktree,
                                    entry.reason
                                );
                            }
                        }
                        Err(error) => println!("⚠️ 收轮前三态现场报告失败: {error:#}"),
                    }
                }
                Ok(_) => println!("⚠️ 收轮前 reclaim report 拒绝坏账本"),
                Err(error) => println!("⚠️ 收轮前读取 reclaim report 账本失败: {error:#}"),
            }

            match orch_host::sites::reap_released_sites_reported(root, &round) {
                Ok(gc) => {
                    let line = format!(
                        "· 收轮前 site GC：removed={} refused={} freedBytes={}",
                        gc.reaped.len(),
                        gc.refused.len(),
                        gc.freed_bytes
                    );
                    println!("{line}");
                    site_gc_summary.push(line);
                    for (site_id, reason) in &gc.refused {
                        let line = format!("  REFUSED {site_id}: {reason}");
                        println!("{line}");
                        site_gc_summary.push(line);
                    }
                    protected_site_ids = Some(
                        gc.refused
                            .into_iter()
                            .map(|(site_id, _)| site_id)
                            .collect::<Vec<_>>(),
                    );
                }
                Err(error) => println!(
                    "⚠️ 收轮前 site GC 保守留场（不把交付/Recorded 当 release）: {error:#}"
                ),
            }
        }
        Err(error) => println!("⚠️ 收轮前无法解析当前轮；site GC 保守留场: {error:#}"),
    }
    let scratch_root = root.join("orch/target/test-tmp");
    match orch_host::util::sweep_test_scratch_root(&scratch_root, ROUND_CLOSE_SCRATCH_TTL, &[]) {
        Ok(report) => {
            println!(
                "· 收轮前 sweep_test_scratch_root：root={} ttlHours=24 removed={} failed={} freedBytes={}",
                scratch_root.display(),
                report.removed.len(),
                report.failed.len(),
                report.freed_bytes
            );
            for path in &report.removed {
                println!("  removed {}", path.display());
            }
            for (path, reason) in &report.failed {
                println!("  REFUSED {}: {reason}", path.display());
            }
            if report.is_total_failure() {
                println!("⚠️ 收轮前 scratch sweep 未回收到任何对象");
            }
        }
        Err(error) => println!("⚠️ 收轮前 scratch sweep 失败（不阻断协议收轮）: {error}"),
    }
    match orch_host::close::sweep_trial_cache_before_round_close(root) {
        Ok(report) => println!(
            "· 收轮前 sweep_trial_cache：keepLatest=1 removed={} freedBytes={}",
            report.removed, report.freed_bytes
        ),
        Err(error) => println!("⚠️ 收轮前 trial-cache sweep 失败（不阻断协议收轮）: {error}"),
    }
    let reclaimed = match orch_host::reclaim::reclaim_task_sites(root) {
        Ok(report) => {
            println!(
                "· 收轮前 task-site reclaim：verdicts={} freedBytes={}",
                report.verdicts.len(),
                report.freed_bytes
            );
            for verdict in &report.verdicts {
                println!(
                    "  {:?} {} · freedBytes={}",
                    verdict.disposition, verdict.worktree, verdict.freed_bytes
                );
            }
            report
        }
        Err(error) => {
            println!("⚠️ 收轮前 task-site reclaim 失败（保守留场）: {error:#}");
            Default::default()
        }
    };
    let swept = if let Some(protected_site_ids) = protected_site_ids.as_deref() {
        match orch_host::buildcache::sweep_targets_for_round_preserving_sites(
            root,
            Duration::ZERO,
            protected_site_ids,
        ) {
            Ok(report) => {
                println!(
                    "· 收轮前 sweep_targets：removed={} refused={} freedBytes={} debugBytes={}",
                    report.removed.len(),
                    report.refused.len(),
                    report.freed_bytes,
                    report.debug_bytes
                );
                for refusal in report.refused.iter().chain(&report.failures) {
                    println!("  REFUSED {refusal}");
                }
                report
            }
            Err(error) => {
                println!("⚠️ 收轮前 sweep-targets 失败（保守留场）: {error:#}");
                Default::default()
            }
        }
    } else {
        println!("⚠️ 收轮前 site GC 无可靠拒收清单；跳过 target sweep 以保守留场");
        Default::default()
    };
    if reclaimed.incomplete || swept.incomplete {
        println!("⚠️ 删除失败；拒绝输出不完整磁盘三元组");
    }
    match orch_host::reclaim::survey_disk(root, &reclaimed, &swept) {
        Ok(entries) => println!(
            "· 收轮磁盘三元组：{}",
            orch_host::reclaim::summarize_disk(&entries).render()
        ),
        Err(error) => println!("⚠️ 收轮磁盘普查失败（不阻断协议收轮）: {error:#}"),
    }
    site_gc_summary
}

#[cfg(test)]
mod round_close_cleanup_tests {
    use super::*;

    fn git(root: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn real_round_close_reaps_a_fresh_release_and_prints_a_dirty_refusal() {
        let root = orch_host::util::test_scratch_dir("cli-round-close-live-gc");
        git(&root, &["init", "-q"]);
        fs::write(root.join("tracked.txt"), "baseline\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=orch-test",
                "-c",
                "user.email=orch-test@example.invalid",
                "commit",
                "-q",
                "-m",
                "baseline",
            ],
        );
        fs::create_dir_all(root.join("coordination/rounds/rT")).unwrap();
        fs::create_dir_all(root.join("coordination/runtime/ledger-wal")).unwrap();
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "rT\n").unwrap();
        fs::write(root.join("coordination/rounds/rT/events.jsonl"), "").unwrap();
        fs::write(root.join("coordination/runtime/ledger-wal/rT.jsonl"), "").unwrap();
        fs::create_dir_all(root.join(".worktrees")).unwrap();
        fs::create_dir_all(root.join("orch/target")).unwrap();
        let head = orch_host::gitx::rev_parse(&root, "HEAD").unwrap();
        let provision = |task: &str, attempt: &str, agent: &str, wake: &str| {
            orch_host::sites::lease_review_site_with(
                &root,
                "rT",
                task,
                attempt,
                orch_host::sites::SiteRole::Primary,
                agent,
                &head,
                wake,
                |site| {
                    orch_host::gitx::worktree_add_detached(
                        &root,
                        &root.join(&site.worktree),
                        &head,
                    )?;
                    fs::create_dir_all(root.join(&site.target))?;
                    Ok(())
                },
            )
            .unwrap()
        };
        let fresh = provision("BT1", "BT1-A0001", "executor-fresh", "wake-fresh");
        let dirty = provision("BT2", "BT2-A0001", "executor-dirty", "wake-dirty");
        for site in [&fresh, &dirty] {
            orch_host::ledger::append(
                &root,
                "rT",
                &[orch_host::ledger::event(
                    "WorkspaceReleased",
                    "runtime:orch",
                    Some(&site.task_id),
                    Some("rT"),
                    serde_json::json!({
                        "siteId": site.site_id,
                        "generation": site.generation,
                        "attemptId": site.attempt_id,
                        "role": site.role.as_str(),
                        "agent": site.agent,
                        "wakeId": site.wake_id,
                        "completionReceipt": orch_host::sites::MANAGED_COMPLETION_RECEIPT,
                    }),
                )],
            )
            .unwrap();
        }
        fs::write(root.join(&fresh.target).join("fresh-artifact"), b"fresh").unwrap();
        fs::write(root.join(&dirty.worktree).join("tracked.txt"), "dirty\n").unwrap();

        let summary = prepare_round_close_cleanup(&root);
        assert!(!root.join(&fresh.worktree).exists());
        assert!(!root.join(&fresh.target).exists());
        assert!(root.join(&dirty.worktree).exists());
        assert!(root.join(&dirty.target).exists());
        assert!(summary
            .iter()
            .any(|line| line.contains("removed=1 refused=1 freedBytes=")));
        assert!(summary.iter().any(|line| {
            line.contains(&format!("REFUSED {}", dirty.site_id)) && line.contains("tracked/staged")
        }));

        fs::write(root.join(&dirty.worktree).join("tracked.txt"), "baseline\n").unwrap();
        orch_host::gitx::worktree_remove(&root, &root.join(&dirty.worktree)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}

fn compiled_build_stamp() -> orch_host::staleness::BuildStamp {
    orch_host::staleness::BuildStamp {
        commit: option_env!("ORCH_BUILD_GIT_SHA")
            .map(str::trim)
            .filter(|sha| !sha.is_empty())
            .map(str::to_owned),
    }
}

/// Exact read-only classification for the stale-binary guard.
///
/// This intentionally does not change `CommandEffectPolicy`: preflight groups
/// commands by the round/IR contract they require, while staleness must inspect
/// value-level flags that decide whether a particular invocation writes.
fn staleness_command_is_read_only(command: &Cmd) -> bool {
    match command {
        Cmd::WakeSupervise => true,
        Cmd::Ledger {
            action: LedgerCmd::Recover { apply, .. },
        } => !apply,
        Cmd::Snapshot { write, .. } => !write,
        Cmd::Bootstrap { copy, .. } => !copy,
        Cmd::Session {
            action: SessionCmd::Show,
        }
        | Cmd::Inbox {
            action: InboxCmd::List,
        }
        | Cmd::Bind
        | Cmd::Doctor
        | Cmd::StallCheck
        | Cmd::Status
        | Cmd::Schema
        | Cmd::Guide { .. }
        | Cmd::Check { .. }
        | Cmd::Cost { .. }
        | Cmd::Schedule { .. }
        | Cmd::Current
        | Cmd::Mcp { .. }
        | Cmd::Agent {
            action: AgentCmd::List | AgentCmd::Lint,
        } => true,
        _ => false,
    }
}

/// H35: reject state-changing commands when the running binary predates a
/// compiled input on main. Guard classification is deliberately value-level;
/// diagnostics are always reachable.
fn enforce_binary_staleness(
    root: &std::path::Path,
    command: &Cmd,
    allow_stale_binary: bool,
) -> Result<()> {
    use orch_host::staleness::StaleVerdict;

    // The guide is compiled product data and deliberately has no repository
    // dependency. Do not even probe git metadata: a standalone copied binary
    // must produce clean Markdown on stdout in an empty directory.
    if matches!(command, Cmd::Guide { .. }) {
        return Ok(());
    }

    let report = orch_host::staleness::inspect_repository(root, &compiled_build_stamp());
    match report.verdict {
        StaleVerdict::Fresh => Ok(()),
        StaleVerdict::Unknown => {
            eprintln!(
                "orch: NOTE: binary staleness unknown; command allowed: {}",
                report.detail
            );
            Ok(())
        }
        StaleVerdict::Stale => {
            let build_sha = report.build_sha.as_deref().unwrap_or("?");
            let main_sha = report.main_sha.as_deref().unwrap_or("?");
            if staleness_command_is_read_only(command) {
                eprintln!(
                    "orch: WARNING: stale binary detected but read-only command is allowed; \
                     build_sha={build_sha} main_sha={main_sha}; {}",
                    report.detail
                );
                return Ok(());
            }
            if allow_stale_binary {
                eprintln!(
                    "orch: WARNING: --allow-stale-binary is running a state-changing command \
                     with a stale binary; build_sha={build_sha} main_sha={main_sha}; {}",
                    report.detail
                );
                return Ok(());
            }
            bail!(
                "stale binary refused state-changing command: \
                 build_sha={build_sha} main_sha={main_sha}; {}; \
                 rebuild with `cargo build --workspace --locked --manifest-path orch/Cargo.toml`, \
                 or rerun explicitly with `--allow-stale-binary`",
                report.detail
            )
        }
    }
}

fn command_task(command: &Cmd) -> Option<&str> {
    match command {
        Cmd::RunTask { task, .. }
        | Cmd::Review {
            action:
                ReviewCmd::Reconcile { task, .. }
                | ReviewCmd::Deliver { task, .. }
                | ReviewCmd::Panel {
                    action:
                        ReviewPanelCmd::Select { task, .. }
                        | ReviewPanelCmd::Retry { task, .. }
                        | ReviewPanelCmd::Backfill { task, .. },
                },
        }
        | Cmd::Check { task }
        | Cmd::Verify { task, .. }
        | Cmd::Verdict { task, .. }
        | Cmd::Seal { task, .. }
        | Cmd::Merge { task, .. }
        | Cmd::Record { task, .. }
        | Cmd::Dispatch { task, .. }
        | Cmd::AwaitReport { task, .. }
        | Cmd::RetryDead { task, .. }
        | Cmd::Nudge { task, .. }
        | Cmd::Resume { task, .. }
        | Cmd::Schedule { task }
        | Cmd::Approve { task, .. }
        | Cmd::Round {
            action: RoundCmd::SeedVerified { task, .. },
        } => Some(task),
        Cmd::WakeSupervise
        | Cmd::Init
        | Cmd::Bind
        | Cmd::Doctor
        | Cmd::StallCheck
        | Cmd::Ledger { .. }
        | Cmd::Sites { .. }
        | Cmd::Agent { .. }
        | Cmd::RuntimePolicy { .. }
        | Cmd::Status
        | Cmd::Schema
        | Cmd::Guide { .. }
        | Cmd::Snapshot { .. }
        | Cmd::Bootstrap { .. }
        | Cmd::Round { .. }
        | Cmd::Wake { .. }
        | Cmd::Handshake { .. }
        | Cmd::Cost { .. }
        | Cmd::Plan
        | Cmd::Consult { .. }
        | Cmd::Run
        | Cmd::Step
        | Cmd::Current
        | Cmd::Serve { .. }
        | Cmd::Mcp { .. }
        | Cmd::Session { .. }
        | Cmd::Inbox { .. }
        | Cmd::RunWave { .. } => None,
    }
}

fn validate_full_sha_syntax(value: &str, flag: &str) -> Result<()> {
    if value.len() != 40
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{flag} 必须是完整 40 位小写 hex SHA");
    }
    Ok(())
}

fn preflight_cli_command(root: &std::path::Path, command: &Cmd) -> Result<()> {
    if let Cmd::Inbox {
        action: InboxCmd::Done { file },
    } = command
    {
        orch_host::inbox::validate_filename(file)?;
    }
    if let Cmd::Verdict {
        attempt,
        expected_head,
        expected_main,
        verdict,
        reason,
        ..
    } = command
    {
        let safe_attempt = !attempt.is_empty()
            && attempt
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_alphanumeric())
            && attempt
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
        if !safe_attempt {
            bail!("--attempt 必须是单一安全 component");
        }
        match verdict.as_str() {
            "pass" if reason.is_some() => bail!("PASS verdict 不接受 --reason"),
            "fail" | "blocked" if reason.as_deref().is_none_or(|text| text.trim().is_empty()) => {
                bail!("FAIL/BLOCKED verdict 强制非空 --reason")
            }
            _ => {}
        }
        validate_full_sha_syntax(expected_head, "--expected-head")?;
        validate_full_sha_syntax(expected_main, "--expected-main")?;
    }
    if let Cmd::Seal {
        attempt,
        expected_head,
        ..
    } = command
    {
        let safe_attempt = !attempt.is_empty()
            && attempt
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_alphanumeric())
            && attempt
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
        if !safe_attempt {
            bail!("--attempt 必须是单一安全 component");
        }
        validate_full_sha_syntax(expected_head, "--expected-head")?;
    }
    if let Cmd::Review {
        action: ReviewCmd::Reconcile { task, attempt },
    } = command
    {
        orch_host::wake::validate_review_reconcile_attempt(task, attempt)?;
    }
    if let Cmd::Review {
        action:
            ReviewCmd::Deliver {
                task,
                attempt,
                agent,
                ..
            },
    } = command
    {
        orch_host::wake::validate_review_reconcile_attempt(task, attempt)?;
        if agent.trim().is_empty()
            || !agent
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            bail!("--agent 必须是安全的非空 identity component");
        }
    }
    if let Cmd::Review {
        action: ReviewCmd::Panel { action },
    } = command
    {
        let (task, attempt, seats): (&str, &str, Vec<&str>) = match action {
            ReviewPanelCmd::Select {
                task,
                attempt,
                seats,
            } => (task, attempt, seats.iter().map(String::as_str).collect()),
            ReviewPanelCmd::Retry {
                task,
                attempt,
                seat_id,
            } => (task, attempt, vec![seat_id.as_str()]),
            ReviewPanelCmd::Backfill {
                task,
                attempt,
                seat,
            } => (task, attempt, vec![seat.as_str()]),
        };
        orch_host::wake::validate_review_reconcile_attempt(task, attempt)?;
        if seats.iter().any(|value| {
            value.is_empty()
                || !value.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':')
                })
        }) {
            bail!("review panel seat identity 非安全 component");
        }
    }
    if let Cmd::Dispatch {
        new_attempt,
        reason,
        ..
    } = command
    {
        match (*new_attempt, reason.as_deref()) {
            (true, Some(text)) if !text.trim().is_empty() => {}
            (true, _) => bail!("--new-attempt 强制非空 --reason"),
            (false, Some(_)) => bail!("--reason 仅可与 --new-attempt 同用"),
            (false, None) => {}
        }
    }
    if let Cmd::RuntimePolicy { action } = command {
        let (policy, reason) = match action {
            RuntimePolicyCmd::Activate { policy } => (policy, None),
            RuntimePolicyCmd::Deactivate { policy, reason } => (policy, reason.as_ref()),
        };
        if policy.is_empty()
            || !policy
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            bail!("runtime policy 必须是安全的非空 component");
        }
        if reason.is_some_and(|value| value.trim().is_empty() || value.trim() != value) {
            bail!("runtime-policy deactivate --reason 必须非空且无首尾空白");
        }
    }
    if let Some(task) = command_task(command) {
        orch_host::card::validate_task_id(task)?;
    }
    let active = match command_effect_policy(command) {
        CommandEffectPolicy::ReadOnly
        | CommandEffectPolicy::PlanMutation
        | CommandEffectPolicy::PlanConsultation
        | CommandEffectPolicy::Signoff => None,
        CommandEffectPolicy::BootstrapOnly => {
            if matches!(command, Cmd::Init)
                && root.join("coordination/runtime/CURRENT-ROUND").exists()
            {
                bail!("已有 CURRENT-ROUND 时拒绝 orch init");
            }
            None
        }
        CommandEffectPolicy::RequiresValidated => Some(validated_round_contract(root)?.1),
        CommandEffectPolicy::RequiresActive => Some(active_round_contract(root)?.1),
        CommandEffectPolicy::RecoveryOnly => None,
        CommandEffectPolicy::RootManualForbidden => {
            let (_, validated) = validated_round_contract(root)?;
            if validated.candidate.verification.mode == "root-manual-fixed-head" {
                bail!("root-manual 活跃轮禁止直接修改 session registry；等待 central permit");
            }
            Some(validated)
        }
    };
    if let (Some(task), Some(active)) = (command_task(command), active.as_ref()) {
        if !active
            .candidate
            .tasks
            .iter()
            .any(|candidate| candidate.id == task)
        {
            bail!("task {task} 不在 active ROUND-IR");
        }
    }
    let allowed_agent = |agent: &str, active: &orch_host::plan::ReadonlyIrValidation| {
        active
            .candidate
            .scheduling
            .allowed_agents
            .iter()
            .any(|id| id == agent)
            && active
                .candidate
                .scheduling
                .capacities
                .get(agent)
                .is_some_and(|capacity| capacity.agent > 0 && capacity.quota > 0)
    };
    if let Some(active) = active.as_ref() {
        match command {
            Cmd::Wake { agent, .. } | Cmd::Handshake { agent, .. } => {
                if !allowed_agent(agent, active) {
                    bail!("agent {agent} 不在 active ROUND-IR allowed/capacity 集合");
                }
            }
            Cmd::Nudge { agent, task, .. } => {
                if !allowed_agent(agent, active) {
                    bail!("agent {agent} 不在 active ROUND-IR allowed/capacity 集合");
                }
                let round = orch_host::current_round(root)?;
                let ledger =
                    read_ledger(&root.join(format!("coordination/rounds/{round}/events.jsonl")))?;
                let ctx =
                    orch_host::attempt::resolve_current_dispatch(&ledger.events, task, &round)?;
                if ctx.agent.as_deref() != Some(agent) {
                    bail!("nudge agent 必须等于 current DispatchContext.agent");
                }
            }
            _ => {}
        }
    }
    if matches!(command, Cmd::RunTask { .. })
        && active.as_ref().is_some_and(|active| {
            active.candidate.verification.mode == "root-manual-fixed-head"
                && active.candidate.verification.adapter == "root-manual"
        })
    {
        bail!(
            "legacy run-task 在 root-manual-fixed-head 模式禁用；managed provider 必须经后续 central permit 显式开放"
        );
    }
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let root = match fs::canonicalize(&cli.root) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("orch: 无法解析 --root {}: {e}", cli.root.display());
            return ExitCode::from(2);
        }
    };
    // The detached owner is deliberately independent of the active round and
    // build-tip preflight. Its complete authority is the typed one-shot stdin
    // document; running ordinary preflight here could abandon a live child.
    if matches!(cli.cmd, Cmd::WakeSupervise) {
        return orch_host::wake::run_wake_supervisor_from_stdin(&root)
            .map(|_| ExitCode::SUCCESS)
            .unwrap_or_else(|error| {
                let code = cli_error_exit_code(&error);
                eprintln!("orch: wake supervisor failed: {error:#}");
                ExitCode::from(code)
            });
    }
    // Orphan-control must remain reachable after round close and across an
    // ordinary stale-binary/active-round preflight. Its authority is only the
    // authenticated revision-3 descriptor under the canonical repository.
    if let Cmd::Wake {
        agent,
        wake_id,
        reason,
        mode,
        json,
        message,
        message_file,
        reissue,
        review_for,
        attempt,
        role,
        deadline_secs,
    } = &cli.cmd
    {
        if let Some(action) = WakeControlAction::parse(agent) {
            let result = if reissue.is_some() {
                Err(anyhow::anyhow!(
                    "orch wake control action 禁止普通 wake 的 --reissue"
                ))
            } else {
                match action {
                    WakeControlAction::Status => cmd_wake_status(
                        &root,
                        wake_id.as_deref(),
                        *json,
                        reason.is_some(),
                        mode.is_some(),
                        message.is_some(),
                        message_file.is_some(),
                        review_for.is_some(),
                        attempt.is_some(),
                        role.is_some(),
                        deadline_secs.is_some(),
                    ),
                    WakeControlAction::Attach => cmd_wake_attach(
                        &root,
                        wake_id.as_deref(),
                        mode.as_deref(),
                        reason.as_deref(),
                        *deadline_secs,
                        *json,
                        message.is_some(),
                        message_file.is_some(),
                        review_for.is_some(),
                        attempt.is_some(),
                        role.is_some(),
                    ),
                    WakeControlAction::Cancel => {
                        if mode.is_some() || *json {
                            Err(anyhow::anyhow!("orch wake cancel 禁止 --mode/--json"))
                        } else {
                            cmd_wake_cancel(
                                &root,
                                wake_id.as_deref(),
                                reason.as_deref(),
                                message.is_some(),
                                message_file.is_some(),
                                review_for.is_some(),
                                attempt.is_some(),
                                role.is_some(),
                                deadline_secs.is_some(),
                            )
                        }
                    }
                    WakeControlAction::DeclareDead => cmd_wake_declare_dead(
                        &root,
                        wake_id.as_deref(),
                        reason.is_some(),
                        mode.is_some(),
                        *json,
                        message.is_some(),
                        message_file.is_some(),
                        review_for.is_some(),
                        attempt.is_some(),
                        role.is_some(),
                        deadline_secs.is_some(),
                    ),
                }
            };
            return result.unwrap_or_else(|error| {
                let code = cli_error_exit_code(&error);
                eprintln!("orch: {error:#}");
                ExitCode::from(code)
            });
        }
    }
    if let Err(error) = enforce_binary_staleness(&root, &cli.cmd, cli.allow_stale_binary) {
        let code = cli_error_exit_code(&error);
        eprintln!("orch: {error:#}");
        return ExitCode::from(code);
    }
    if let Err(error) = preflight_cli_command(&root, &cli.cmd) {
        let code = cli_error_exit_code(&error);
        eprintln!("orch: {error:#}");
        return ExitCode::from(code);
    }
    let result = match cli.cmd {
        Cmd::WakeSupervise => {
            unreachable!("hidden wake runtime returned before ordinary dispatch")
        }
        Cmd::Init => cmd_init(&root),
        Cmd::Bind => cmd_bind(&root),
        Cmd::Doctor => cmd_doctor(&root),
        Cmd::StallCheck => cmd_stall_check(&root),
        Cmd::Ledger { action } => cmd_ledger(&root, action),
        Cmd::Review { action } => cmd_review(&root, action),
        Cmd::Sites { action } => cmd_sites(&root, action),
        Cmd::Agent { action } => cmd_agent(&root, action),
        Cmd::RuntimePolicy { action } => cmd_runtime_policy(&root, action),
        Cmd::Status => cmd_status(&root),
        Cmd::Schema => {
            println!("{}", orch_core::event_schema_json());
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Guide { section, check } => cmd_guide(section.as_deref(), check),
        Cmd::RunTask {
            task,
            agent,
            timeout_secs,
        } => cmd_run_task(&root, &task, agent, timeout_secs),
        Cmd::Check { task } => cmd_check(&root, &task),
        Cmd::Verify { task, timeout_secs } => cmd_verify(&root, &task, timeout_secs),
        Cmd::Verdict {
            task,
            attempt,
            expected_head,
            expected_main,
            verdict,
            reason,
            dry_run,
        } => cmd_verdict(
            &root,
            &task,
            &attempt,
            &expected_head,
            &expected_main,
            &verdict,
            reason.as_deref(),
            dry_run,
        ),
        Cmd::Seal {
            task,
            attempt,
            expected_head,
        } => cmd_seal(&root, &task, &attempt, &expected_head),
        Cmd::Merge { task, recover } => {
            if recover {
                orch_host::close::run_merge_recovery(&root, &task).map(|_| ExitCode::SUCCESS)
            } else {
                cmd_merge(&root, &task)
            }
        }
        Cmd::Record { task, at_tip } => cmd_record(&root, &task, at_tip),
        Cmd::Snapshot { json, write } => cmd_snapshot(&root, json, write),
        Cmd::Dispatch {
            task,
            no_wake,
            override_ambiguous_active,
            new_attempt,
            reason,
        } => cmd_dispatch(
            &root,
            &task,
            no_wake,
            override_ambiguous_active,
            new_attempt,
            reason.as_deref(),
        ),
        Cmd::AwaitReport {
            task,
            timeout_secs,
            liveness_grace_secs,
            no_liveness,
        } => cmd_await(&root, &task, timeout_secs, liveness_grace_secs, no_liveness),
        Cmd::RetryDead {
            task,
            max_retries,
            base_secs,
        } => cmd_retry_dead(&root, &task, max_retries, base_secs),
        Cmd::Bootstrap { agent, copy } => cmd_bootstrap(&root, &agent, copy),
        Cmd::Round { action } => cmd_round(&root, action),
        Cmd::Nudge {
            agent,
            task,
            message,
            message_file,
            force,
            no_wake,
        } => cmd_nudge(&root, &agent, &task, message, message_file, force, no_wake),
        Cmd::Wake {
            agent,
            wake_id,
            reason,
            mode,
            json,
            message,
            message_file,
            reissue,
            review_for,
            attempt,
            role,
            deadline_secs,
        } => {
            if mode.is_some() || json {
                Err(anyhow::anyhow!(
                    "普通 orch wake 禁止 action-only 的 --mode/--json"
                ))
            } else {
                cmd_wake(
                    &root,
                    &agent,
                    wake_id,
                    reason,
                    message,
                    message_file,
                    reissue,
                    review_for,
                    attempt,
                    role,
                    deadline_secs,
                )
            }
        }
        Cmd::Handshake {
            agent,
            timeout_secs,
            no_wake,
        } => cmd_handshake(&root, &agent, timeout_secs, no_wake),
        Cmd::Resume { task, copy } => cmd_resume(&root, &task, copy),
        Cmd::Cost { round } => cmd_cost(&root, round),
        Cmd::Schedule { task } => cmd_schedule(&root, &task),
        Cmd::Plan => cmd_plan(&root),
        Cmd::Consult {
            question,
            preset,
            attach,
            judge,
            no_judge,
            member_timeout_secs,
            total_wall_secs,
        } => cmd_consult(
            &root,
            question,
            preset,
            attach,
            judge,
            no_judge,
            member_timeout_secs,
            total_wall_secs,
        ),
        Cmd::Approve {
            task,
            action,
            decision,
            note,
        } => cmd_approve(&root, &task, &action, &decision, note.as_deref()),
        Cmd::Run => cmd_run_loop(&root, false),
        Cmd::Step => cmd_run_loop(&root, true),
        Cmd::Current => cmd_current(&root),
        Cmd::Serve { once, no_monitor } => cmd_serve(&root, once, no_monitor),
        Cmd::Mcp { action } => cmd_mcp(&root, action),
        Cmd::Session { action } => cmd_session(&root, action),
        Cmd::Inbox { action } => cmd_inbox(&root, action),
        Cmd::RunWave { timeout_secs } => cmd_run_wave(&root, timeout_secs),
    };
    result.unwrap_or_else(|e| {
        // Typed command disposition wins over a nested ActionRejection; without
        // either, the only honest default is EffectUnknown(5).
        let code = cli_error_exit_code(&e);
        eprintln!("orch: {e:#}");
        ExitCode::from(code)
    })
}

fn cmd_init(root: &std::path::Path) -> Result<ExitCode> {
    let report = orch_host::scaffold::init(root)?;
    println!("orch init · root={}", root.display());
    println!(
        "  created ({}): {}",
        report.created.len(),
        display_items(&report.created)
    );
    println!(
        "  updated ({}): {}",
        report.updated.len(),
        display_items(&report.updated)
    );
    println!(
        "  already-present ({}): {}",
        report.already_present.len(),
        display_items(&report.already_present)
    );
    Ok(ExitCode::SUCCESS)
}

fn public_leaf_commands() -> Vec<String> {
    fn walk(command: &clap::Command, prefix: &str, output: &mut Vec<String>) {
        for child in command.get_subcommands().filter(|child| {
            child.get_name() != "help"
                && !child.get_name().starts_with("__")
                && !child.is_hide_set()
        }) {
            let path = if prefix.is_empty() {
                child.get_name().to_string()
            } else {
                format!("{prefix} {}", child.get_name())
            };
            let has_public_children = child.get_subcommands().any(|grandchild| {
                grandchild.get_name() != "help"
                    && !grandchild.get_name().starts_with("__")
                    && !grandchild.is_hide_set()
            });
            if has_public_children {
                walk(child, &path, output);
            } else {
                output.push(path);
            }
        }
    }

    let command = Cli::command();
    let mut output = Vec::new();
    walk(&command, "", &mut output);
    output.sort();
    output
}

fn cmd_guide(section: Option<&str>, check: bool) -> Result<ExitCode> {
    if check {
        let wake_actions = WakeControlAction::names();
        let stats = guide::validate(&public_leaf_commands(), &wake_actions)?;
        println!(
            "orch guide: OK · commands={} sections={} invariants={} wakeActions={} dispositions={}",
            stats.commands,
            stats.sections,
            stats.invariants,
            stats.wake_actions,
            stats.dispositions
        );
    } else {
        print!("{}", guide::render(section)?);
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_bind(root: &std::path::Path) -> Result<ExitCode> {
    let proposal = orch_host::scaffold::bind(root)?;
    println!(
        "orch bind · detected={} · proposal only (not written)",
        proposal.ecosystems.join(",")
    );
    print!("{}", proposal.yaml);
    Ok(ExitCode::SUCCESS)
}

fn display_items(items: &[String]) -> String {
    if items.is_empty() {
        "—".to_string()
    } else {
        items.join(", ")
    }
}

fn cmd_doctor(root: &std::path::Path) -> Result<ExitCode> {
    println!("orch doctor · root = {}", root.display());
    let checks = doctor(root);
    let mut failed = false;
    for c in &checks {
        let (mark, is_fail) = match c.status {
            CheckStatus::Pass => ("✅", false),
            CheckStatus::Warn => ("⚠️ ", false),
            CheckStatus::Fail => ("❌", true),
        };
        failed |= is_fail;
        println!("  {mark} {:<18} {}", c.name, c.detail);
    }
    // B151 增项：CURRENT.md 与活轮 + main SHA 一致性（缺失/不一致 → 黄色提示）。
    // orch-core::doctor 冻结，本检查作为 host 层扩展在此追加（不替 orch-core 加项）。
    // CURRENT.md 一致性是黄色提示（Warn），不强制 doctor 退出码失败。
    let (name, status, detail) = current_md_doctor_check(root)?;
    let mark = match status {
        CheckStatus::Pass => "✅",
        CheckStatus::Warn => "⚠️ ",
        CheckStatus::Fail => "❌",
    };
    let is_fail = matches!(status, CheckStatus::Fail);
    failed |= is_fail;
    println!("  {mark} {:<18} {}", name, detail);

    // B169/H35: a stale binary is a yellow rebuild prompt. Doctor itself is
    // read-only and must stay usable even when this check warns or degrades.
    let (name, status, detail) = binary_staleness_doctor_check(root);
    let mark = match status {
        CheckStatus::Pass => "✅",
        CheckStatus::Warn => "⚠️ ",
        CheckStatus::Fail => "❌",
    };
    println!("  {mark} {:<18} {}", name, detail);

    // B152: the untracked WAL survives git-level operations that can silently
    // truncate the tracked event ledger.  Historical rounds without a WAL are
    // compatible and explicitly skipped.
    let (name, status, detail) = wal_doctor_check(root)?;
    let mark = match status {
        CheckStatus::Pass => "✅",
        CheckStatus::Warn => "⚠️ ",
        CheckStatus::Fail => "❌",
    };
    failed |= matches!(status, CheckStatus::Fail);
    println!("  {mark} {:<18} {}", name, detail);

    // B176/H34: verdict-binding artifacts of a closed round must remain byte
    // stable. This is diagnosis only; the operator gets a path-specific manual
    // disposition and the working tree is never changed automatically.
    let (name, status, detail) = closed_round_artifacts_doctor_check(root)?;
    let mark = match status {
        CheckStatus::Pass => "✅",
        CheckStatus::Warn => "⚠️ ",
        CheckStatus::Fail => "❌",
    };
    failed |= matches!(status, CheckStatus::Fail);
    println!("  {mark} {:<18} {}", name, detail);

    // B310: live append, expected-main, archive, and doctor all consume the
    // same typed V1 event-history validator.
    let (status, detail) = match orch_host::verify::audit_runtime_event_contracts(root) {
        Ok((ledgers, events)) => (
            CheckStatus::Pass,
            format!("ledgersChecked={ledgers} runtimeV1Events={events}"),
        ),
        Err(error) => (
            CheckStatus::Fail,
            format!("runtime V1 event audit failed closed: {error:#}"),
        ),
    };
    let mark = match status {
        CheckStatus::Pass => "✅",
        CheckStatus::Warn => "⚠️ ",
        CheckStatus::Fail => "❌",
    };
    failed |= matches!(status, CheckStatus::Fail);
    println!("  {mark} {:<18} {}", "runtime事件合同", detail);

    // B204/H66: verdict-bound review bytes are recomputed from the current
    // main tree. Only the exact disclosed r59/B181 debt is a Warn.
    let (status, detail) = match orch_host::verify::audit_recorded_review_bindings(root) {
        Ok(report) if report.findings.is_empty() => (
            CheckStatus::Pass,
            format!(
                "checked={} legacyTasksSkipped={}",
                report.checked_bindings, report.legacy_tasks_skipped
            ),
        ),
        Ok(report) => {
            let status =
                if report.findings.iter().any(|finding| {
                    finding.level == orch_host::verify::ReviewBindingAuditLevel::Fail
                }) {
                    CheckStatus::Fail
                } else {
                    CheckStatus::Warn
                };
            let detail = report
                .findings
                .iter()
                .map(|finding| {
                    let level = match finding.level {
                        orch_host::verify::ReviewBindingAuditLevel::Warn => "WARN",
                        orch_host::verify::ReviewBindingAuditLevel::Fail => "FAIL",
                    };
                    format!(
                        "level={level} {}/{}/{} path={} boundSha256={} currentSha256={} boundBytes={} currentBytes={}",
                        finding.round,
                        finding.task_id,
                        finding.attempt_id,
                        finding.path,
                        finding.bound_sha256,
                        finding.current_sha256,
                        finding.bound_bytes,
                        finding
                            .current_bytes
                            .map(|bytes| bytes.to_string())
                            .unwrap_or_else(|| "<missing>".to_string()),
                    )
                })
                .collect::<Vec<_>>()
                .join(" | ");
            (status, detail)
        }
        Err(error) => (
            CheckStatus::Fail,
            format!("review binding audit failed closed: {error:#}"),
        ),
    };
    let mark = match status {
        CheckStatus::Pass => "✅",
        CheckStatus::Warn => "⚠️ ",
        CheckStatus::Fail => "❌",
    };
    failed |= matches!(status, CheckStatus::Fail);
    println!("  {mark} {:<18} {}", "裁决审查绑定", detail);
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn closed_round_artifacts_doctor_check(
    root: &std::path::Path,
) -> Result<(&'static str, CheckStatus, String)> {
    let findings = orch_host::closed_round_audit::inspect_repository(root)?;
    if findings.is_empty() {
        return Ok((
            "已收轮产物",
            CheckStatus::Pass,
            "与 HEAD 一致（合法 SUMMARY/dispatch 痕迹已排除）".into(),
        ));
    }

    let details = findings
        .iter()
        .map(|finding| {
            format!(
                "轮 {} 脏改 {}；人工处置：{}",
                finding.round, finding.path, finding.remediation
            )
        })
        .collect::<Vec<_>>()
        .join("；");
    Ok(("已收轮产物", CheckStatus::Fail, details))
}

fn binary_staleness_doctor_check(root: &std::path::Path) -> (&'static str, CheckStatus, String) {
    use orch_host::staleness::StaleVerdict;

    let report = orch_host::staleness::inspect_repository(root, &compiled_build_stamp());
    let build_sha = report.build_sha.as_deref().unwrap_or("?");
    let main_sha = report.main_sha.as_deref().unwrap_or("?");
    match report.verdict {
        StaleVerdict::Fresh => (
            "二进制构建印记",
            CheckStatus::Pass,
            format!(
                "可用（build {build_sha} · main {main_sha}）· {}",
                report.detail
            ),
        ),
        StaleVerdict::Stale => (
            "二进制构建印记",
            CheckStatus::Warn,
            format!(
                "陈旧（build {build_sha} · main {main_sha}）· {} · 请重跑 cargo build",
                report.detail
            ),
        ),
        StaleVerdict::Unknown => (
            "二进制构建印记",
            CheckStatus::Warn,
            format!("无法判定（放行）· {}", report.detail),
        ),
    }
}

fn cmd_ledger(root: &std::path::Path, action: LedgerCmd) -> Result<ExitCode> {
    match action {
        LedgerCmd::Recover { round, apply } => {
            let round = match round {
                Some(round) => round,
                None => orch_host::current_round(root)?,
            };
            orch_host::ledger::run_ledger_recover(root, &round, apply)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn json_event_summary(line: &str) -> String {
    let value = serde_json::from_str::<serde_json::Value>(line).ok();
    let field = |name: &str| {
        value
            .as_ref()
            .and_then(|event| event.get(name))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?")
    };
    format!(
        "type={}/taskId={}/eventId={}",
        field("type"),
        field("taskId"),
        field("eventId")
    )
}

/// Compare the active round's tracked ledger with its untracked WAL mirror.
/// This is diagnostic only: recovery is intentionally left to B153.
fn wal_doctor_check(root: &std::path::Path) -> Result<(&'static str, CheckStatus, String)> {
    let round = match fs::read_to_string(root.join("coordination/runtime/CURRENT-ROUND")) {
        Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => return Ok(("账本 ↔ WAL", CheckStatus::Pass, "—（无活动轮）".into())),
    };
    let wal_path = root.join(format!("coordination/runtime/ledger-wal/{round}.jsonl"));
    if !wal_path.is_file() {
        return Ok((
            "账本 ↔ WAL",
            CheckStatus::Pass,
            format!("本轮 {round} 无 WAL 基线，跳过"),
        ));
    }
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let ledger_src = fs::read_to_string(&ledger_path)
        .with_context(|| format!("读取账本对账输入失败: {}", ledger_path.display()))?;
    let wal_src = fs::read_to_string(&wal_path)
        .with_context(|| format!("读取 WAL 对账输入失败: {}", wal_path.display()))?;
    let ledger_lines: Vec<String> = ledger_src.lines().map(String::from).collect();
    let wal_lines: Vec<String> = wal_src.lines().map(String::from).collect();

    match orch_host::ledger::reconcile_wal(&ledger_lines, &wal_lines) {
        orch_host::ledger::WalVerdict::Consistent => Ok((
            "账本 ↔ WAL",
            CheckStatus::Pass,
            format!("一致（轮 {round} · {} 条事件）", ledger_lines.len()),
        )),
        orch_host::ledger::WalVerdict::LedgerTruncated { missing } => {
            let summaries = wal_lines[ledger_lines.len()..]
                .iter()
                .map(|line| json_event_summary(line))
                .collect::<Vec<_>>()
                .join("；");
            Ok((
                "账本 ↔ WAL",
                CheckStatus::Fail,
                format!(
                    "账本比 WAL 少 {missing} 条事件（疑似 git 级回滚销毁）；缺失：{summaries}；\
                     恢复指引：停止追加并保全 WAL，使用后续 `orch ledger recover` 审核恢复（当前只诊断，不自动写回）"
                ),
            ))
        }
        orch_host::ledger::WalVerdict::LedgerDiverged { at } => Ok((
            "账本 ↔ WAL",
            CheckStatus::Fail,
            format!(
                "第 {} 行起与 WAL 不一致（疑似手工编造/错误合并）；停止追加并人工核对账本与 WAL",
                at + 1
            ),
        )),
    }
}

/// B151：CURRENT.md 与活轮 + main SHA 一致性检查（doctor 增项）。
///
/// - CURRENT.md 不存在 → Warn（提示 `orch current` 生成）。
/// - 存在但 round/main_sha 标记与活轮/当前 main 不一致 → Warn（黄色提示）。
/// - 一致 → Pass。
/// - 无 CURRENT-ROUND（无活动轮）→ 跳过（Pass "—"）。
fn current_md_doctor_check(root: &std::path::Path) -> Result<(&'static str, CheckStatus, String)> {
    let cr = root.join("coordination/runtime/CURRENT-ROUND");
    let round = match fs::read_to_string(&cr) {
        Ok(t) => t.trim().to_string(),
        Err(_) => return Ok(("CURRENT.md", CheckStatus::Pass, "—（无活动轮）".into())),
    };
    let current_md_path = root.join("coordination/CURRENT.md");
    if !current_md_path.is_file() {
        return Ok((
            "CURRENT.md",
            CheckStatus::Warn,
            format!("缺失（轮 {round}）· 跑 `orch current` 生成"),
        ));
    }
    let src = fs::read_to_string(&current_md_path)
        .with_context(|| format!("读取 CURRENT.md 失败: {}", current_md_path.display()))?;
    let main_sha = orch_host::gitx::rev_parse(root, "main")
        .map(|sha| orch_host::gitx::short(&sha).to_string())
        .unwrap_or_else(|_| "?".into());
    if orch_host::binding::current_md_consistent(&src, &round, &main_sha) {
        Ok((
            "CURRENT.md",
            CheckStatus::Pass,
            format!("一致（轮 {round} · main {main_sha}）"),
        ))
    } else {
        Ok((
            "CURRENT.md",
            CheckStatus::Warn,
            format!("不一致（活轮 {round} · main {main_sha}）· 跑 `orch current` 刷新"),
        ))
    }
}

fn cmd_agent(root: &std::path::Path, action: AgentCmd) -> Result<ExitCode> {
    match action {
        AgentCmd::Lint => {
            let definitions = orch_host::registry::load_agent_definitions(root)?;
            println!("orch agent lint · ok · {} entries", definitions.len());
        }
        AgentCmd::List => {
            let definitions = orch_host::registry::load_agent_definitions(root)?;
            println!("agent\ttool\tmodel\teffort\tsource\tquotaDomain\tresponsibility\tinjectable\tsessionId");
            for (id, definition) in definitions {
                let (tool, source) = definition
                    .tool_definition
                    .as_ref()
                    .map(|tool| (tool.tool.as_str(), tool.model_source.as_str()))
                    .unwrap_or(("legacy", "legacy"));
                let model = definition
                    .model
                    .as_deref()
                    .or(definition.expected_model.as_deref())
                    .unwrap_or("—");
                let effort = definition
                    .effort
                    .as_deref()
                    .or(definition.expected_effort.as_deref())
                    .unwrap_or("—");
                println!(
                    "{id}\t{tool}\t{model}\t{effort}\t{source}\t{}\t{}\t{}\t{}",
                    definition.profile.quota_domain,
                    if definition.responsibility.is_empty() {
                        "—"
                    } else {
                        &definition.responsibility
                    },
                    if definition.injectable { "yes" } else { "no" },
                    if definition.session_id.is_empty() {
                        "—"
                    } else {
                        &definition.session_id
                    },
                );
            }
        }
        AgentCmd::SetPin {
            agent,
            provider,
            model,
            effort,
            reason,
        } => {
            let amendment = orch_host::registry::run_agent_pin_amendment(
                root,
                &agent,
                provider.as_deref(),
                model.as_deref(),
                effort.as_deref(),
                &reason,
            )?;
            println!(
                "orch agent set-pin · committed · {}",
                serde_json::to_string(&amendment)?
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_runtime_policy(
    root: &std::path::Path,
    action: RuntimePolicyCmd,
) -> Result<ExitCode> {
    let outcome = match action {
        RuntimePolicyCmd::Activate { policy } => {
            orch_host::plan::activate_runtime_policy(root, &policy)?
        }
        RuntimePolicyCmd::Deactivate { policy, reason } => {
            orch_host::plan::deactivate_runtime_policy(
                root,
                &policy,
                reason.as_deref().unwrap_or("operator-requested"),
            )?
        }
    };
    println!(
        "orch runtime-policy · policy={} state={:?} eventId={} commit={} replayed={}",
        outcome.policy,
        outcome.state,
        outcome.event_id,
        outcome.commit_sha,
        outcome.replayed,
    );
    Ok(ExitCode::SUCCESS)
}

/// B151 `orch current`：只读派生 → 幂等覆写 coordination/CURRENT.md 单文件。
///
/// 从账本 + git 推导活轮、main SHA、任务状态投影、执行者池现状，渲染为人读
/// markdown；原子覆写 coordination/CURRENT.md（运行时投影写，同 BOARD 先例）。
/// 不落账本、无其他副作用。CURRENT.md 的 round 与 main 标记可被 doctor 一致性
/// 检查（`binding::current_md_consistent`）消费。
fn current_active_plan_signed_off(events: &[orch_core::EventRecord], round: &str) -> Result<bool> {
    let Some(validation) = events
        .iter()
        .rev()
        .find(|event| event.kind == "TaskValidated" && event.round.as_deref() == Some(round))
    else {
        return Ok(false);
    };
    let payload = orch_host::plan::decode_runtime_task_validated(validation, round)?;
    Ok(orch_host::plan::matching_user_plan_signoff_position(
        events,
        round,
        payload.ir_revision,
        &payload.validation_digest,
    )?
    .is_some())
}

fn cmd_current(root: &std::path::Path) -> Result<ExitCode> {
    let round = orch_host::current_round(root)?;
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let ledger = read_ledger(&ledger_path)
        .with_context(|| format!("读取账本失败: {}", ledger_path.display()))?;
    if !ledger.bad_lines.is_empty() {
        bail!("orch current 拒绝坏账本（{} 行）", ledger.bad_lines.len());
    }
    let projection = fold(&ledger.events);
    let active_plan_signed_off = current_active_plan_signed_off(&ledger.events, &round)?;
    // 无 git 仓库 / 无 main ref（合成 fixture 根、fresh coordination 目录）时
    // 回退到 "?"，不阻断生成（同 current_md_doctor_check 先例）。orch current 是
    // 只读派生命令，账本可解析即可生成 CURRENT.md；main 缺失只降级标记。
    let main_full = orch_host::gitx::rev_parse(root, "main").unwrap_or_else(|_| "?".to_string());
    let main_short = if main_full == "?" {
        "?".to_string()
    } else {
        orch_host::gitx::short(&main_full).to_string()
    };
    let now = orch_host::ledger::now_rfc3339();

    // ── 任务投影：复用 fold 输出（事件 → state） ──
    let mut task_lines = String::new();
    for (id, t) in &projection.tasks {
        task_lines.push_str(&format!(
            "| {} | {} | {} |{}|\n",
            id,
            t.state.map(|s| s.to_string()).unwrap_or_else(|| "?".into()),
            t.event_count,
            t.merge_sha
                .as_deref()
                .map(|s| format!(" merge `{s}` |"))
                .unwrap_or_else(|| " — |".into()),
        ));
    }
    if task_lines.is_empty() {
        task_lines.push_str("_(无任务事件)_\n");
    }

    // ── 执行者池现状：读 agents.yaml 注册表（只读） ──
    let registry = load_current_registry(root)?;
    let mut agent_lines = String::new();
    for (agent_id, spec) in &registry {
        let model = spec
            .model
            .as_deref()
            .or(spec.expected_model.as_deref())
            .unwrap_or("—");
        let effort = spec
            .effort
            .as_deref()
            .or(spec.expected_effort.as_deref())
            .unwrap_or("—");
        agent_lines.push_str(&format!(
            "| `{}` | `{}` | `{}` | `{}` | injectable={} | sessionId=`{}` |\n",
            agent_id,
            model,
            effort,
            spec.profile.quota_domain,
            if spec.injectable { "yes" } else { "no" },
            if spec.session_id.is_empty() {
                "—"
            } else {
                &spec.session_id
            },
        ));
    }
    if agent_lines.is_empty() {
        agent_lines.push_str("_(无登记执行者)_\n");
    }

    let body = format!(
        "# CURRENT\n\n\
         > 本文件由 `orch current` 只读派生生成（B151）。**唯一现状真值**——\n\
         > AGENTS.md 顶部指向此文件；do not hand-edit（幂等覆写）。\n\
         > 生成时间：{now}\n\n\
         round: {round}\n\
         main: {main_short}\n\
         mainFull: {main_full}\n\
         planSignedOff: {signed}\n\
         roundClosed: {closed}\n\
         events: {events}（坏行 {bad}）\n\n\
         ## 任务投影\n\n\
         | ID | 状态 | 事件 | merge |\n\
         |---|---|---|---|\n\
         {task_lines}\n\
         ## 执行者池\n\n\
         | agent | model | effort | quotaDomain | injectable | sessionId |\n\
         |---|---|---|---|---|---|\n\
         {agent_lines}\n",
        now = now,
        round = round,
        main_short = main_short,
        main_full = main_full,
        signed = if active_plan_signed_off { "yes" } else { "no" },
        closed = if projection.round_closed { "yes" } else { "no" },
        events = projection.total_events,
        bad = ledger.bad_lines.len(),
        task_lines = task_lines,
        agent_lines = agent_lines,
    );

    let current_md_path = root.join("coordination/CURRENT.md");
    // 原子写：先写临时文件再 rename（防止中途崩溃留下半截文件）。
    if let Some(parent) = current_md_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建 CURRENT.md 父目录失败: {}", parent.display()))?;
    }
    let tmp = current_md_path.with_extension("md.tmp");
    fs::write(&tmp, &body)
        .with_context(|| format!("写 CURRENT.md 临时文件失败: {}", tmp.display()))?;
    fs::rename(&tmp, &current_md_path)
        .with_context(|| format!("原子 rename CURRENT.md 失败: {}", current_md_path.display()))?;

    println!("orch current · 已生成 {}", current_md_path.display());
    println!(
        "  round={} · main={} · events={} · tasks={}",
        round,
        main_short,
        projection.total_events,
        projection.tasks.len()
    );
    Ok(ExitCode::SUCCESS)
}

fn load_current_registry(
    root: &std::path::Path,
) -> Result<std::collections::BTreeMap<String, orch_host::registry::AgentDefinition>> {
    let path = root.join("coordination/agents.yaml");
    if !path
        .try_exists()
        .with_context(|| format!("检查 AgentRegistry 失败: {}", path.display()))?
    {
        return Ok(Default::default());
    }
    orch_host::registry::load_agent_definitions(root)
}

#[cfg(test)]
mod current_registry_tests {
    use super::*;

    fn test_root(name: &str) -> PathBuf {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("workspace root")
            .join("target/test-tmp")
            .join(format!("b233-current-{name}-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root).expect("remove stale test root");
        }
        fs::create_dir_all(root.join("coordination")).expect("create test root");
        root
    }

    #[test]
    fn current_tolerates_only_a_missing_registry_file() {
        let root = test_root("registry-errors");
        assert!(load_current_registry(&root).unwrap().is_empty());

        fs::write(root.join("coordination/agents.yaml"), "agents: [broken\n")
            .expect("write malformed registry");
        let error = load_current_registry(&root).expect_err("malformed registry must fail loud");
        assert!(
            error.to_string().contains("解析 AgentRegistry 失败"),
            "unexpected error: {error:#}"
        );

        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn current_surfaces_model_effort_and_quota_domain() {
        let root = test_root("model-effort-domain");
        fs::create_dir_all(root.join("coordination/runtime")).unwrap();
        fs::create_dir_all(root.join("coordination/rounds/r1")).unwrap();
        fs::create_dir_all(root.join("coordination/tools")).unwrap();
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r1\n").unwrap();
        fs::write(root.join("coordination/rounds/r1/events.jsonl"), "").unwrap();
        fs::write(
            root.join("coordination/tools/opencode.yaml"),
            r#"apiVersion: orch/v1alpha1
kind: ToolDefinition
tool: opencode
modelSource: argv
startupSpacingMs: 1200
maxConcurrent: 3
launch:
  argv: ["opencode", "run", "{message}", "--model", "{model}", "--variant", "{effort}"]
observation: {source: opencode-db, policy: strict}
"#,
        )
        .unwrap();
        fs::write(
            root.join("coordination/tools/zcode.yaml"),
            r#"apiVersion: orch/v1alpha1
kind: ToolDefinition
tool: zcode
modelSource: external-config
startupSpacingMs: 0
maxConcurrent: 2
launch:
  argv: ["zcode", "{message}"]
observation: {source: none, policy: advisory}
"#,
        )
        .unwrap();
        fs::write(
            root.join("coordination/agents.yaml"),
            r#"apiVersion: orch/v1alpha1
kind: AgentRegistry
agents:
  executor-opencode:
    tool: opencode
    model: one-dewu-opencode/glm-5.2
    effort: high
    injectable: true
    sessionId: fresh
  executor-zcode:
    tool: zcode
    expectedModel: dcc-dewu-ep/gpt-5-dcc-glm-5-2
    expectedEffort: xhigh
    injectable: true
    sessionId: fresh
"#,
        )
        .unwrap();

        cmd_current(&root).expect("current projection should render");
        let current = fs::read_to_string(root.join("coordination/CURRENT.md")).unwrap();
        assert!(
            current.contains("| agent | model | effort | quotaDomain | injectable | sessionId |")
        );
        assert!(current.contains(
            "| `executor-opencode` | `one-dewu-opencode/glm-5.2` | `high` | `opencode` | injectable=yes | sessionId=`fresh` |"
        ));
        assert!(current.contains(
            "| `executor-zcode` | `dcc-dewu-ep/gpt-5-dcc-glm-5-2` | `xhigh` | `zcode` | injectable=yes | sessionId=`fresh` |"
        ));

        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn current_signoff_is_bound_to_the_latest_validation_tuple() {
        let round = "r-current";
        let digest_v1 = "a".repeat(64);
        let digest_v2 = "b".repeat(64);
        let validation_v1 = orch_host::ledger::event(
            "TaskValidated",
            "runtime:orch",
            None,
            Some(round),
            orch_host::plan::task_validated_payload(1, &digest_v1),
        );
        let signoff_v1 = orch_host::ledger::event(
            "PlanSignedOff",
            "user",
            None,
            Some(round),
            orch_host::plan::plan_signed_off_payload("v1", 1, &digest_v1).unwrap(),
        );
        let validation_v2 = orch_host::ledger::event(
            "TaskValidated",
            "runtime:orch",
            None,
            Some(round),
            orch_host::plan::task_validated_payload(2, &digest_v2),
        );

        assert!(current_active_plan_signed_off(
            &[validation_v1.clone(), signoff_v1.clone()],
            round
        )
        .unwrap());
        assert!(!current_active_plan_signed_off(
            &[validation_v1, signoff_v1, validation_v2],
            round
        )
        .unwrap());
    }
}

fn cmd_plan(root: &std::path::Path) -> Result<ExitCode> {
    let outcome = orch_host::plan::run_plan(root)?;
    println!("orch plan · IR = {}", outcome.ir_path.display());
    println!("  revision: {}", outcome.revision);
    println!("  validationDigest: {}", outcome.digest);
    println!(
        "  IR: {} · TaskValidated: {}",
        if outcome.ir_written {
            "written"
        } else {
            "unchanged"
        },
        if outcome.event_appended {
            "appended"
        } else {
            "unchanged"
        }
    );
    println!(
        "  reverifyTasks: {}",
        display_items(&outcome.reverify_tasks)
    );
    println!("  skipped:");
    for skipped in outcome.skipped {
        println!("    {} · {}", skipped.rule, skipped.reason);
    }
    Ok(ExitCode::SUCCESS)
}

#[allow(clippy::too_many_arguments)]
fn cmd_consult(
    root: &std::path::Path,
    question: PathBuf,
    preset: String,
    attachments: Vec<PathBuf>,
    judge: String,
    no_judge: bool,
    member_timeout_secs: Option<u64>,
    total_wall_secs: Option<u64>,
) -> Result<ExitCode> {
    let judge = if no_judge {
        orch_host::consult::JudgeMode::None
    } else {
        orch_host::consult::JudgeMode::parse(&judge)?
    };
    let outcome = orch_host::consult::run_consultation(
        root,
        &orch_host::consult::ConsultArgs {
            question,
            preset,
            attachments,
            judge,
            member_timeout_secs,
            total_wall_secs,
        },
    )?;
    let members_ok = outcome
        .members
        .iter()
        .filter(|member| member.status == orch_host::consult::MemberStatus::Ok)
        .count();
    println!(
        "orch consult · id={} preset={} fusion={}/{} judge={:?}",
        outcome.id,
        outcome.preset,
        members_ok,
        outcome.members.len(),
        outcome.judge_status
    );
    println!("  artifacts={}", outcome.dir.display());
    Ok(ExitCode::SUCCESS)
}

fn cmd_approve(
    root: &std::path::Path,
    task: &str,
    action: &str,
    decision: &str,
    note: Option<&str>,
) -> Result<ExitCode> {
    use orch_host::approval::HighRiskAction;

    let (round, active) = active_round_contract(root)?;
    if !active.candidate.tasks.iter().any(|item| item.id == task) {
        bail!("approve task {task} 不在 active ROUND-IR");
    }
    orch_host::close::with_protocol_effect(root, "orch approve", || {
        let (fresh_round, fresh_active) = active_round_contract(root)?;
        if fresh_round != round
            || !fresh_active
                .candidate
                .tasks
                .iter()
                .any(|item| item.id == task)
        {
            bail!("approve active ROUND-IR 在 critical point 漂移");
        }
        let action_kind = match action {
            "push" => HighRiskAction::Push,
            "publish" => HighRiskAction::Publish,
            "delete-recursive" => HighRiskAction::DeleteRecursive,
            "network" => HighRiskAction::Network,
            "install" => HighRiskAction::Install,
            _ => anyhow::bail!("未知高风险 action: {action}"),
        };
        let context = note.unwrap_or(task);
        let requested = orch_host::ledger::event(
            "PermissionRequested",
            "runtime:orch",
            Some(task),
            Some(&round),
            orch_host::approval::request_payload(action_kind, context),
        );
        let mut decision_payload = serde_json::json!({
            "action": action_kind.as_str(),
            "risk": "high",
            "decision": decision,
            "by": "user",
        });
        if let Some(note) = note {
            decision_payload["note"] = serde_json::Value::String(note.to_string());
        }
        let decided = orch_host::ledger::event(
            "PermissionDecided",
            "user",
            Some(task),
            Some(&round),
            decision_payload,
        );
        orch_host::ledger::append(root, &round, &[requested, decided])?;
        println!("orch approve {task}: {action} → {decision}（轮 {round}，审批事件 2 条已落账）");
        Ok(ExitCode::SUCCESS)
    })
}

fn cmd_run_loop(root: &std::path::Path, once: bool) -> Result<ExitCode> {
    orch_host::runloop::run_loop(root, once)?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_serve(root: &std::path::Path, once: bool, no_monitor: bool) -> Result<ExitCode> {
    let (_, active) = active_round_contract(root)?;
    if active.candidate.verification.mode == "root-manual-fixed-head"
        && active.candidate.verification.adapter == "root-manual"
    {
        // Root is the only planner/judgement authority in this mode.  The
        // legacy daemon's serve_tick may spawn a configured fresh planner, so
        // root-manual serve is deliberately the mechanical runloop only.
        orch_host::runloop::run_loop(root, once)?;
    } else {
        orch_host::serve::run_daemon_with_monitor(root, once, !no_monitor)?;
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_mcp(root: &std::path::Path, action: McpCmd) -> Result<ExitCode> {
    match action {
        McpCmd::Serve => cmd_mcp_serve(root),
    }
}

fn cmd_mcp_serve(root: &std::path::Path) -> Result<ExitCode> {
    use std::io::{self, BufRead, Write};

    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.context("读取 MCP stdin 失败")?;
        let response = match orch_host::mcp::parse_request(&line) {
            Some(request) => orch_host::mcp::route_request(root, &request),
            None => Some(orch_host::mcp::parse_error_response()),
        };
        if let Some(response) = response {
            writeln!(stdout, "{response}").context("写 MCP stdout 失败")?;
            stdout.flush().context("刷新 MCP stdout 失败")?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_run_task(
    root: &std::path::Path,
    task: &str,
    agent: Option<String>,
    timeout_secs: u64,
) -> Result<ExitCode> {
    println!("orch run-task {task} · root={}", root.display());
    let out = orch_host::run_task(root, task, agent, timeout_secs)?;
    println!("── 完成：{} → ready_for_verification ──", out.task_id);
    println!(
        "  分支 {} @ {} · REPORT {}",
        out.branch, out.base_sha_short, out.report_rel
    );
    for n in &out.mech_notes {
        println!("  {n}");
    }
    for g in &out.gates {
        println!("  门 {} exit={} ({}ms)", g.name, g.exit_code, g.duration_ms);
    }
    if let Some(u) = &out.usage {
        println!("  usage: {}", serde_json::to_string(u).unwrap_or_default());
    }
    println!("  下一步：独立 verifier 复核后由 reviewer 合并（本切片不自动合并）");
    Ok(ExitCode::SUCCESS)
}

fn cmd_dispatch(
    root: &std::path::Path,
    task: &str,
    no_wake: bool,
    override_ambiguous_active: bool,
    new_attempt: bool,
    reason: Option<&str>,
) -> Result<ExitCode> {
    if new_attempt {
        let (agent, base, prepared) = orch_host::attempt::run_approved_reattempt_dispatch(
            root,
            task,
            no_wake,
            override_ambiguous_active,
            reason.context("--new-attempt 强制 --reason")?,
        )?;
        println!(
            "  approved reattempt: {} -> {}（已绑定旧 root PASS 与 reason）",
            prepared.previous_attempt_id, prepared.next_attempt_id
        );
        println!("orch dispatch {task}: GO 已写给 {agent}（基线 {base}）");
        return Ok(ExitCode::SUCCESS);
    }

    let (agent, base) = orch_host::tierf::run_dispatch_with_override(
        root,
        task,
        no_wake,
        override_ambiguous_active,
    )?;
    println!("orch dispatch {task}: GO 已写给 {agent}（基线 {base}）");
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod approved_reattempt_cli_tests {
    use super::*;

    #[test]
    fn clap_requires_explicit_flag_and_reason_pair() {
        assert!(Cli::try_parse_from([
            "orch",
            "dispatch",
            "B204",
            "--new-attempt",
            "--reason",
            "retry after root approval",
        ])
        .is_ok());
        assert!(Cli::try_parse_from(["orch", "dispatch", "B204", "--reason", "orphan"]).is_err());

        let missing_reason = Cli::try_parse_from(["orch", "dispatch", "B204", "--new-attempt"])
            .expect("clap shape is accepted before semantic preflight");
        assert!(
            preflight_cli_command(std::path::Path::new("."), &missing_reason.cmd)
                .unwrap_err()
                .to_string()
                .contains("--reason")
        );
    }

    #[test]
    fn new_attempt_wiring_uses_the_single_atomic_host_orchestrator() {
        let source = include_str!("main.rs");
        let start = source.find("fn cmd_dispatch(").unwrap();
        let end = source[start..]
            .find("\n#[cfg(test)]\nmod approved_reattempt_cli_tests")
            .unwrap()
            + start;
        let body = &source[start..end];
        assert!(body.contains("run_approved_reattempt_dispatch"));
        assert!(!body.contains("prepare_approved_reattempt"));
        assert!(!body.contains("validate_approved_reattempt_successor"));
    }
}

fn cmd_await(
    root: &std::path::Path,
    task: &str,
    timeout_secs: Option<u64>,
    liveness_grace_secs: u64,
    no_liveness: bool,
) -> Result<ExitCode> {
    use orch_host::tierf::AwaitOutcome;
    let (_, active) = active_round_contract(root)?;
    let wall_minutes = active
        .candidate
        .tasks
        .iter()
        .find(|candidate| candidate.id == task)
        .map(|candidate| candidate.wall_minutes)
        .with_context(|| format!("任务 {task} 不在 active ROUND-IR"))?;
    let (timeout_secs, timeout_source) = match timeout_secs {
        Some(explicit) => (explicit, "来源：显式 --timeout-secs".to_string()),
        None => (
            orch_host::tierf::default_await_timeout_secs(wall_minutes),
            format!("来源：任务卡 budgets.wallMinutes={wall_minutes}（active ROUND-IR 绑定）"),
        ),
    };
    let ir_liveness = active.candidate.liveness;
    let live = if no_liveness {
        None
    } else {
        let defaults = orch_host::liveness::LivenessOpts::default();
        Some(orch_host::liveness::LivenessOpts {
            grace: std::time::Duration::from_secs(liveness_grace_secs),
            stall: std::time::Duration::from_secs(
                ir_liveness.working_stall_minutes.saturating_mul(60),
            ),
            probe_every: std::time::Duration::from_secs(ir_liveness.monitor_seconds),
            confirm: u8::try_from(ir_liveness.confirm_samples)
                .context("ROUND-IR liveness.confirmSamples 超出 u8")?,
            ..defaults
        })
    };
    println!(
        "orch await-report {task}（超时 {timeout_secs}s，{timeout_source}，2s stat 轮询·双根{}）",
        if live.is_some() {
            format!(
                "，liveness：grace {liveness_grace_secs}s/stall {}min/probe {}s/confirm {}",
                ir_liveness.working_stall_minutes,
                ir_liveness.monitor_seconds,
                ir_liveness.confirm_samples
            )
        } else {
            "，liveness 关".into()
        }
    );
    match orch_host::tierf::run_await(root, task, timeout_secs, live)? {
        AwaitOutcome::Collected(co) => {
            println!("── {task} → ready_for_verification ──");
            for n in &co.mech_notes {
                println!("  {n}");
            }
            Ok(ExitCode::SUCCESS)
        }
        AwaitOutcome::Blocked { report_rel } => {
            println!("执行者已阻塞：{report_rel}，需 planner 授权/改派");
            Ok(ExitCode::from(6))
        }
        AwaitOutcome::LivenessDead { reason } => {
            println!("── {task} 执行者判死（提前升级，未等满 {timeout_secs}s）──");
            println!("  {reason}");
            println!("  下一步：orch resume {task} --copy（重建现场供新会话）或改派 Tier S（orch run-task {task} --agent ...）");
            Ok(ExitCode::from(3))
        }
        AwaitOutcome::LivenessStalled { reason } => {
            println!("── {task} 执行者判滞 ──");
            println!("  {reason}");
            println!("  下一步：客户端长思考中可直接重跑 await；确认卡死则 orch nudge <agent> --task {task}");
            Ok(ExitCode::from(4))
        }
    }
}

fn retry_task_had_activity(root: &std::path::Path, round: &str, task: &str) -> Result<bool> {
    let card = orch_host::card::load(root, round, task)?;
    let agent = card
        .meta
        .agent
        .as_deref()
        .context("任务卡缺 agent，无法执行 liveness retry guard")?;
    let snapshot = orch_host::liveness::probe(
        root,
        agent,
        task,
        None,
        &card.meta.write_set,
        Instant::now(),
    );
    Ok(snapshot.last_activity_age.is_some())
}

fn cmd_retry_dead(
    root: &std::path::Path,
    task: &str,
    max_retries: u8,
    base_secs: u64,
) -> Result<ExitCode> {
    use orch_host::serve::RetryDecision;

    let (round, initial_active) = active_round_contract(root)?;
    if !initial_active
        .candidate
        .tasks
        .iter()
        .any(|candidate| candidate.id == task)
    {
        bail!("task {task} 不在 active ROUND-IR");
    }
    let permit_revision = initial_active.persisted_revision;
    let permit_digest = initial_active.persisted_digest;
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let ledger = read_ledger(&ledger_path)
        .with_context(|| format!("读取账本失败: {}", ledger_path.display()))?;
    let dead_attempts = orch_host::serve::count_dead_attempts(&ledger.events, task);
    let had_activity = retry_task_had_activity(root, &round, task)?;

    match orch_host::serve::decide_liveness_retry(
        dead_attempts,
        max_retries,
        base_secs,
        had_activity,
    ) {
        RetryDecision::RetryAfter(delay_secs) => {
            let ordinal = dead_attempts.saturating_add(1);
            println!("将于 {delay_secs}s 后重派 {task}（第 {ordinal} 次退避）");
            thread::sleep(Duration::from_secs(delay_secs));
            if retry_task_had_activity(root, &round, task)? {
                println!("不自动重派（已达上限或检测到活动/可能在环）→ 交 planner 改派/BLOCKED");
                return Ok(ExitCode::from(4));
            }
            // Backoff is an authorization gap: the signed IR may have been
            // revised while this process slept.  Revalidate immediately next
            // to redispatch; the startup preflight is intentionally not
            // treated as a durable permit.
            let (fresh_round, fresh_active) = active_round_contract(root)?;
            let same_permit = fresh_round == round
                && fresh_active.persisted_revision == permit_revision
                && fresh_active.persisted_digest == permit_digest
                && fresh_active
                    .candidate
                    .tasks
                    .iter()
                    .any(|candidate| candidate.id == task);
            if !same_permit {
                println!("不自动重派：round/active IR/task membership 在退避期间漂移");
                return Ok(ExitCode::from(4));
            }
            let fresh_ledger = read_ledger(&ledger_path)?;
            if !fresh_ledger.bad_lines.is_empty() {
                bail!("retry-dead 临界前账本出现坏行");
            }
            let second_active =
                orch_host::plan::require_active_round_ir(root, &round, &fresh_ledger.events)?;
            if second_active.persisted_revision != permit_revision
                || second_active.persisted_digest != permit_digest
                || !second_active
                    .candidate
                    .tasks
                    .iter()
                    .any(|candidate| candidate.id == task)
            {
                println!("不自动重派：dispatch 临界前 active permit 漂移");
                return Ok(ExitCode::from(4));
            }
            let fresh_dead_attempts =
                orch_host::serve::count_dead_attempts(&fresh_ledger.events, task);
            let fresh_activity = retry_task_had_activity(root, &round, task)?;
            let fresh_decision = orch_host::serve::decide_liveness_retry(
                fresh_dead_attempts,
                max_retries,
                base_secs,
                fresh_activity,
            );
            if fresh_dead_attempts != dead_attempts
                || fresh_decision != RetryDecision::RetryAfter(delay_secs)
            {
                println!("不自动重派：dead-attempt/activity/retry decision 在退避期间漂移");
                return Ok(ExitCode::from(4));
            }
            let (agent, base) = orch_host::tierf::run_dispatch(root, task, false)?;
            let post = active_round_contract(root);
            if !post.is_ok_and(|(post_round, post_active)| {
                post_round == round
                    && post_active.persisted_revision == permit_revision
                    && post_active.persisted_digest == permit_digest
                    && post_active
                        .candidate
                        .tasks
                        .iter()
                        .any(|candidate| candidate.id == task)
            }) {
                let _ = orch_host::ledger::append(
                    root,
                    &round,
                    &[orch_host::ledger::event(
                        "EscalationRaised",
                        "runtime:orch",
                        Some(task),
                        Some(&round),
                        serde_json::json!({
                            "stage": "retry-dead-post-dispatch",
                            "reason": "active permit drifted across run_dispatch",
                        }),
                    )],
                );
                bail!("重派已发生但 active permit 随后漂移；已升级，停止续跑");
            }
            println!("已重派 {task} 给 {agent}（基线 {base}）");
            Ok(ExitCode::SUCCESS)
        }
        RetryDecision::GiveUp => {
            println!("不自动重派（已达上限或检测到活动/可能在环）→ 交 planner 改派/BLOCKED");
            Ok(ExitCode::from(4))
        }
    }
}

fn copy_to_clipboard(text: &str) -> Result<()> {
    use std::io::Write as _;
    let mut child = std::process::Command::new("pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    child.stdin.as_mut().unwrap().write_all(text.as_bytes())?;
    child.wait()?;
    Ok(())
}

fn cmd_bootstrap(root: &std::path::Path, agent: &str, copy: bool) -> Result<ExitCode> {
    let text = orch_host::tierf::render_bootstrap(root, agent)?;
    println!("{text}");
    if copy {
        copy_to_clipboard(&text)?;
        eprintln!("── 已复制进剪贴板 ──");
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_nudge(
    root: &std::path::Path,
    agent: &str,
    task: &str,
    message: Option<String>,
    message_file: Option<PathBuf>,
    force: bool,
    no_wake: bool,
) -> Result<ExitCode> {
    let default = "请检查当前任务进度：完成则写 REPORT；被阻塞则写 BLOCKED 说明卡点。";
    let resolved = resolve_cli_message(message, message_file, default)?;
    let source = resolved.source;
    let bytes = resolved.bytes;
    let round = orch_host::current_round(root)?;
    let nudge_path = root.join(format!(
        "coordination/rounds/{round}/dispatch/{agent}/NUDGE.md"
    ));
    let before = fs::read(&nudge_path).ok();
    let before_modified = fs::metadata(&nudge_path)
        .ok()
        .and_then(|metadata| metadata.modified().ok());
    let result =
        orch_host::tierf::run_nudge_with_message(root, agent, Some(task), resolved, force, no_wake);
    let o = match result {
        Ok(outcome) => outcome,
        Err(error) => {
            let after = fs::read(&nudge_path).ok();
            let after_modified = fs::metadata(&nudge_path)
                .ok()
                .and_then(|metadata| metadata.modified().ok());
            let written = after.is_some() && (after != before || after_modified != before_modified);
            let rendered_error = format!("{error:#}");
            let fence_refused = rendered_error.contains("continuation conflict")
                || rendered_error.contains("continuation fence");
            match orch_host::wake::nudge_delivery_outcome(written, fence_refused) {
                orch_host::wake::NudgeDelivery::WrittenAwaitingConsumption => {
                    println!(
                        "orch nudge {agent}: 已送达、待消费（看 {}.seen）",
                        nudge_path.display()
                    );
                    println!("  continuation fence 拒绝了额外 provider wake；NUDGE.md 已写盘");
                    return Ok(ExitCode::from(4));
                }
                orch_host::wake::NudgeDelivery::Written => {
                    println!(
                        "orch nudge {agent}: NUDGE.md 已写盘，但后续动作未完成；待消费（看 {}.seen）",
                        nudge_path.display()
                    );
                    eprintln!("orch nudge 后续错误: {rendered_error}");
                    return Ok(ExitCode::from(4));
                }
                orch_host::wake::NudgeDelivery::NotWritten => return Err(error),
            }
        }
    };
    println!(
        "orch nudge {agent}: NUDGE 已写（轮 {}）→ {}",
        o.round,
        o.nudge_path.display()
    );
    println!("  messageSource={source} · messageBytes={bytes}");
    if let Some(seen) = &o.prev_seen {
        println!("  上一条 NUDGE 消费于 {seen}（.seen mtime）");
    }
    println!("  注意：NUDGE 仅在客户端回到 wait 循环时可达；工作中卡死请用 orch resume <task>");
    Ok(ExitCode::SUCCESS)
}

#[allow(clippy::too_many_arguments)]
fn cmd_wake_cancel(
    root: &std::path::Path,
    wake_id: Option<&str>,
    reason: Option<&str>,
    has_message: bool,
    has_message_file: bool,
    has_review_for: bool,
    has_attempt: bool,
    has_role: bool,
    has_deadline_secs: bool,
) -> Result<ExitCode> {
    if has_message
        || has_message_file
        || has_review_for
        || has_attempt
        || has_role
        || has_deadline_secs
    {
        bail!(
            "orch wake cancel 仅接受 <wakeId> --reason；禁止 message/review/attempt/role/deadline 参数"
        );
    }
    let wake_id = wake_id.context("orch wake cancel 要求恰一个 <wakeId>")?;
    let reason = reason.context("orch wake cancel 要求 --reason <nonblank-text>")?;
    let result = orch_host::wake::request_managed_wake_cancel(root, wake_id, reason)?;
    use orch_host::wake::ManagedWakeCancelDisposition;
    match result.disposition {
        ManagedWakeCancelDisposition::Accepted => {
            println!(
                "orch wake cancel {}: accepted requestId={}",
                result.wake_id, result.request_id
            );
            Ok(ExitCode::SUCCESS)
        }
        ManagedWakeCancelDisposition::AlreadyCanceling => {
            println!(
                "orch wake cancel {}: already-canceling requestId={}",
                result.wake_id, result.request_id
            );
            Ok(ExitCode::SUCCESS)
        }
        ManagedWakeCancelDisposition::AlreadyTerminal if result.managed_scope_terminated => {
            println!(
                "orch wake cancel {}: already-terminal requestId={}",
                result.wake_id, result.request_id
            );
            Ok(ExitCode::SUCCESS)
        }
        ManagedWakeCancelDisposition::CleanupPending
        | ManagedWakeCancelDisposition::AlreadyTerminal => {
            bail!(
                "cancel winner 已锁定但 cleanup pending；requestId={}，请用同一命令重查",
                result.request_id
            )
        }
        ManagedWakeCancelDisposition::RequestPending => {
            bail!(
                "cancel request pending；requestId={}，请求已 durable，可用同一命令重查",
                result.request_id
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_wake_status(
    root: &std::path::Path,
    wake_id: Option<&str>,
    json: bool,
    has_reason: bool,
    has_mode: bool,
    has_message: bool,
    has_message_file: bool,
    has_review_for: bool,
    has_attempt: bool,
    has_role: bool,
    has_deadline_secs: bool,
) -> Result<ExitCode> {
    if has_reason
        || has_mode
        || has_message
        || has_message_file
        || has_review_for
        || has_attempt
        || has_role
        || has_deadline_secs
    {
        bail!("orch wake status 仅接受 <wakeId> [--json]");
    }
    let wake_id = wake_id.context("orch wake status 要求恰一个 <wakeId>")?;
    let status = orch_host::wake::managed_wake_status(root, wake_id)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        println!(
            "orch wake status {}: agent={} provider={} terminal={} managedScopeTerminated={} session={}",
            status.wake_id,
            status.agent,
            status.provider_kind,
            status.terminal,
            status.managed_scope_terminated,
            status.session_status,
        );
        if let Some(digest) = status.session_digest.as_deref() {
            println!("  sessionDigest={digest}");
        }
        if let Some(outcome) = status.outcome.as_deref() {
            println!("  outcome={outcome}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[allow(clippy::too_many_arguments)]
fn cmd_wake_attach(
    root: &std::path::Path,
    wake_id: Option<&str>,
    mode: Option<&str>,
    reason: Option<&str>,
    deadline_secs: Option<u64>,
    json: bool,
    has_message: bool,
    has_message_file: bool,
    has_review_for: bool,
    has_attempt: bool,
    has_role: bool,
) -> Result<ExitCode> {
    if json || has_message || has_message_file || has_review_for || has_attempt || has_role {
        bail!(
            "orch wake attach 仅接受 <wakeId> --mode <resume|fork> --reason <text> [--deadline-secs <1..=21600>]"
        );
    }
    let wake_id = wake_id.context("orch wake attach 要求恰一个 <wakeId>")?;
    let mode = match mode.context("orch wake attach 要求 --mode <resume|fork>")? {
        "resume" => orch_host::wake::AttachMode::Resume,
        "fork" => orch_host::wake::AttachMode::Fork,
        _ => unreachable!("clap constrains attach mode"),
    };
    let reason = reason.context("orch wake attach 要求 --reason <nonblank-text>")?;
    let result = orch_host::wake::run_managed_wake_attach(
        root,
        wake_id,
        mode,
        reason,
        deadline_secs.unwrap_or(1800),
    )?;
    println!(
        "orch wake attach {}: phase={} mode={} actionId={} continuationWakeId={}",
        result.source_wake_id,
        result.phase,
        result.mode,
        result.action_id,
        result.continuation_wake_id,
    );
    println!(
        "  sessionPresent=true · sessionDigest={}",
        result.session_digest
    );
    Ok(ExitCode::SUCCESS)
}

#[allow(clippy::too_many_arguments)]
fn cmd_wake_declare_dead(
    root: &std::path::Path,
    wake_id: Option<&str>,
    has_reason: bool,
    has_mode: bool,
    json: bool,
    has_message: bool,
    has_message_file: bool,
    has_review_for: bool,
    has_attempt: bool,
    has_role: bool,
    has_deadline_secs: bool,
) -> Result<ExitCode> {
    if has_reason
        || has_mode
        || json
        || has_message
        || has_message_file
        || has_review_for
        || has_attempt
        || has_role
        || has_deadline_secs
    {
        bail!("orch wake declare-dead 仅接受 <wakeId>");
    }
    let wake_id = wake_id.context("orch wake declare-dead 要求恰一个 <wakeId>")?;
    let declaration =
        orch_host::close::with_protocol_effect(root, "managed session death declaration", || {
            orch_host::wake::record_managed_session_death(root, wake_id)
        })?;
    println!(
        "orch wake declare-dead {}: evidenceSha256={} outcome={} requestDigestExempt=yes",
        declaration.wake_id,
        declaration.evidence.evidence_sha256(),
        declaration.evidence.outcome(),
    );
    Ok(ExitCode::SUCCESS)
}

fn cmd_wake(
    root: &std::path::Path,
    agent: &str,
    wake_id: Option<String>,
    reason: Option<String>,
    message: Option<String>,
    message_file: Option<PathBuf>,
    reissue: Option<String>,
    review_for: Option<String>,
    attempt: Option<String>,
    role: Option<String>,
    deadline_secs: Option<u64>,
) -> Result<ExitCode> {
    if wake_id.is_some() || reason.is_some() {
        bail!("普通 orch wake 禁止 cancel-only 的第二 positional/--reason");
    }
    let default = "orch: 有新信号,请检查你的 dispatch 信道并按协议处理";
    let resolved = resolve_cli_message(message, message_file, default)?;
    let source = resolved.source;
    let bytes = resolved.bytes;
    if let Some(source_wake_id) = reissue {
        if review_for.is_some() || attempt.is_some() || role.is_some() || deadline_secs.is_some() {
            bail!(
                "orch wake --reissue 从 source 推导 review tuple/deadline；禁止 --review-for/--attempt/--role/--deadline-secs"
            );
        }
        let outcome = orch_host::wake::run_review_wake_reissue_authorized(
            root,
            agent,
            &source_wake_id,
            resolved,
        )?;
        println!(
            "orch wake {agent}: reissue {} source={} successor={}",
            if outcome.spawned {
                "spawned"
            } else {
                "idempotent"
            },
            outcome.source_wake_id,
            outcome.successor_wake_id,
        );
        println!("  messageSource={source} · messageBytes={bytes}");
        return Ok(ExitCode::SUCCESS);
    }
    let review = resolve_review_request(root, agent, review_for, attempt, role, deadline_secs)?;
    let outcome = if let Some(review) = review {
        orch_host::wake::run_wake_with_message_authorized_review(root, agent, resolved, review)?
    } else {
        // Keep ordinary wake on its existing wrapper: no review event and no
        // behavioral change when none of the review flags is present.
        orch_host::wake::run_wake_with_message_authorized(root, agent, resolved)?
    };
    match outcome {
        orch_host::wake::WakeRunOutcome::Spawned { .. } => {
            println!("orch wake {agent}: 注入已完成");
        }
        orch_host::wake::WakeRunOutcome::Idempotent { wake_id } => {
            println!("orch wake {agent}: 复用已有 wakeId={wake_id} · 未起新进程");
        }
    }
    println!("  messageSource={source} · messageBytes={bytes}");
    Ok(ExitCode::SUCCESS)
}

fn resolve_review_request(
    root: &std::path::Path,
    agent: &str,
    review_for: Option<String>,
    attempt: Option<String>,
    role: Option<String>,
    deadline_secs: Option<u64>,
) -> Result<Option<orch_host::wake::ReviewRequest>> {
    let supplied = [review_for.is_some(), attempt.is_some(), role.is_some()];
    if supplied.iter().any(|value| *value) && !supplied.iter().all(|value| *value) {
        bail!("--review-for, --attempt 与 --role 必须成组提供；--deadline-secs 仅用于该组");
    }
    if !supplied.iter().any(|value| *value) {
        if deadline_secs.is_some() {
            bail!("--deadline-secs 不能脱离 review 参数组单独使用");
        }
        return Ok(None);
    }
    let task_id = review_for.expect("review group completeness checked");
    let attempt_id = attempt.expect("review group completeness checked");
    let role = role.expect("review group completeness checked");
    // Keep explicit values (including zero) fully authoritative. The derived
    // path is intentionally lazy so an override does not require or reread IR.
    let deadline_secs = match deadline_secs {
        Some(explicit) => explicit,
        None => orch_host::wake::default_review_deadline_secs(root, &task_id, &role)?,
    };
    Ok(Some(orch_host::wake::ReviewRequest {
        task_id,
        attempt_id,
        role,
        agent: agent.to_string(),
        deadline_secs,
    }))
}

/// 把 CLI 的 `--message`/`--message-file` 解析成 [orch_host::wake::ResolvedMessage]。
///
/// 互斥：两者同时给出 → Err（非零退出）。`--message -` 表示从 stdin 读；
/// `--message-file <path>` 从文件读。读文件/stdin 时要求合法 UTF-8（否则非零退出）。
/// 两来源都省略 → 回退 default（明示 stdout 标识 default 来源）。
/// 空文件/空 stdin 是显式空消息（source=file/stdin，text=""），**不回退** default。
///
/// B109 时序契约：互斥冲突必须在**任何 stdin/file 读取副作用之前**拒绝——
/// 冲突时不得先阻塞等 stdin、也不得产生文件读副作用（attempt 1 修复点）。
fn resolve_cli_message(
    message: Option<String>,
    message_file: Option<PathBuf>,
    default: &str,
) -> Result<orch_host::wake::ResolvedMessage> {
    use orch_host::wake::{resolve_message_input, MessageInput};

    // 先验互斥：CLI 层只有 --message（值 `-` 即 stdin 信道）与 --message-file
    // 两个来源开关，同时给出必然是冲突——在任何读取副作用之前直接拒绝。
    if message.is_some() && message_file.is_some() {
        bail!(
            "wake/nudge 消息来源互斥冲突：同时给出 --message 与 --message-file；请仅使用其中之一"
        );
    }

    // `--message -` 走 stdin 信道（source=stdin），不混进 explicit；
    // `--message foo` 走 explicit 信道。两者互斥由 resolve_message_input 判定。
    let (explicit, stdin_contents) = match message {
        Some(value) if value == "-" => (None, Some(read_stdin_to_string()?)),
        Some(value) => (Some(value), None),
        None => (None, None),
    };
    let file_contents = match message_file {
        Some(path) => Some(read_file_to_string(&path)?),
        None => None,
    };
    resolve_message_input(
        MessageInput {
            explicit,
            file_contents,
            stdin_contents,
        },
        default,
    )
    .map_err(anyhow::Error::msg)
}

/// 从 stdin 读全部内容到 String。空 stdin → ""（显式空消息，不回退 default）。
/// 非法 UTF-8 → Err（非零退出）。
fn read_stdin_to_string() -> Result<String> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin()
        .lock()
        .read_to_string(&mut buf)
        .context("读取 stdin 失败")?;
    Ok(buf)
}

/// 从文件读全部内容到 String。空文件 → ""（显式空消息，不回退 default）。
/// 读失败 / 非法 UTF-8 → Err（非零退出）。
fn read_file_to_string(path: &std::path::Path) -> Result<String> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("读取 --message-file 失败: {}", path.display()))?;
    Ok(text)
}

fn cmd_handshake(
    root: &std::path::Path,
    agent: &str,
    timeout_secs: u64,
    no_wake: bool,
) -> Result<ExitCode> {
    let round = orch_host::current_round(root)?;
    let message = "orch handshake：请回一句确认在线";
    let outcome = orch_host::wake::dispatch_wake_authorized(root, agent, &round, message, no_wake)?;

    // B110：POKE 备用道 / --no-wake 无 pid/log —— 显式 Pending，绝不读 legacy
    // 文件冒充探活（spawn 成功也只等于 Delivered，不等于已咬合）。
    let delivery = orch_host::wake::DeliveryState::from_spawn(outcome.injected);
    if delivery != orch_host::wake::DeliveryState::Delivered {
        println!(
            "握手待投递：{agent} 未注入（POKE 备用道或 --no-wake），通道显式 {delivery}；请客户端消费 POKE 后重试"
        );
        return Ok(ExitCode::from(3));
    }

    // B110/A0003：只消费本次 wake 返回的精确 logPath（per-attempt 专属文件）。
    // 每轮严格两段：先 capture immutable {offset,end}，再由 evaluator 只消费
    // 捕获的 end；下一轮才可重新捕获更大的 end。禁止按 mtime 选 legacy 文件，
    // 也禁止在 evaluator 内用读取所得 bytes 长度扩大窗口。
    let log_path = outcome
        .log_path
        .as_ref()
        .context("wake 注入成功但缺 logPath")?;
    let expectation = match outcome.provider_kind {
        Some(_provider_kind) => {
            let wake_id = outcome
                .wake_id
                .as_deref()
                .context("managed wake 注入成功但缺 action wakeId")?;
            Some(orch_host::wake::backend_receipt_expectation_for_wake(
                root, &round, wake_id,
            )?)
        }
        // Test/custom adapters keep the legacy generic probe. Registered
        // Codex/OpenCode/SmartClaw production argv always select the branch
        // above and can no longer be gated by thread/turn substrings.
        None => None,
    };
    let timeout = Duration::from_secs(timeout_secs);
    let started = Instant::now();
    loop {
        let captured =
            orch_host::wake::capture_probe_window(log_path, 0).map_err(anyhow::Error::msg)?;
        let engaged = if let Some(expectation) = expectation.as_ref() {
            orch_host::wake::evaluate_captured_backend_receipt(captured, expectation, false)
                .map_err(anyhow::Error::msg)?
                .is_some()
        } else {
            orch_host::wake::evaluate_captured_probe(captured).map_err(anyhow::Error::msg)?
        };
        if engaged {
            println!("握手成功：{agent} 已咬合");
            return Ok(ExitCode::SUCCESS);
        }

        let elapsed = started.elapsed();
        if elapsed >= timeout {
            println!("握手失败：{agent} 未咬合（通道可能未就绪）");
            return Ok(ExitCode::from(3));
        }
        thread::sleep(Duration::from_secs(2).min(timeout.saturating_sub(elapsed)));
    }
}

fn cmd_resume(root: &std::path::Path, task: &str, copy: bool) -> Result<ExitCode> {
    let (round, prompt) = orch_host::tierf::run_resume(root, task)?;
    println!("{prompt}");
    eprintln!("── RESUME 提示词（轮 {round}，ResumeIssued 已落账）──");
    if copy {
        copy_to_clipboard(&prompt)?;
        eprintln!("── 已复制进剪贴板 ──");
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_review(root: &std::path::Path, action: ReviewCmd) -> Result<ExitCode> {
    match action {
        ReviewCmd::Reconcile { task, attempt } => {
            let delivered =
                orch_host::wake::reconcile_review_attempt_v1(root, &task, &attempt)?;
            println!("orch review reconcile: task={task} attempt={attempt} delivered={delivered}");
            Ok(ExitCode::SUCCESS)
        }
        ReviewCmd::Deliver {
            task,
            attempt,
            role,
            agent,
        } => {
            let outcome = orch_host::wake::deliver_review(root, &task, &attempt, &role, &agent)?;
            println!(
                "orch review deliver: task={task} attempt={attempt} role={role} agent={agent} path={} mode={} appendedEvents={}",
                outcome.path,
                if outcome.post_verdict { "late" } else { "canonical" },
                outcome.appended_events,
            );
            if !outcome.post_verdict {
                println!(
                    "  下一步：commit {} 到 main，再运行 `orch review reconcile {task} --attempt {attempt}`。",
                    outcome.path
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        ReviewCmd::Panel { action } => {
            let outcome = match action {
                ReviewPanelCmd::Select {
                    task,
                    attempt,
                    seats,
                } => orch_host::wake::select_review_panel_v1(root, &task, &attempt, &seats)?,
                ReviewPanelCmd::Retry {
                    task,
                    attempt,
                    seat_id,
                } => {
                    orch_host::wake::retry_review_panel_v1(root, &task, &attempt, &seat_id)?
                }
                ReviewPanelCmd::Backfill {
                    task,
                    attempt,
                    seat,
                } => orch_host::wake::backfill_review_panel_v1(root, &task, &attempt, &seat)?,
            };
            println!(
                "orch review panel: panel={} routes={} spawned={} commit={} replayed={}",
                outcome.panel_id,
                outcome.routes.len(),
                outcome.spawned,
                outcome.commit_sha,
                outcome.replayed,
            );
            for route in outcome.routes {
                println!(
                    "  seat={} generation={} wake={} role={} agent={} deadlineSecs={}",
                    route.seat_id,
                    route.generation,
                    route.wake_id,
                    route.role,
                    route.agent,
                    route.deadline_secs,
                );
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn cmd_sites(root: &std::path::Path, action: SitesCmd) -> Result<ExitCode> {
    match action {
        SitesCmd::Gc { round } => {
            let round = round.unwrap_or(orch_host::current_round(root)?);
            if let Err(error) = orch_host::wake::reconcile_pending_backend_receipts(root, &round) {
                eprintln!(
                    "orch sites gc · backend receipt 对账失败；已降级为诊断并继续回收: {error:#}"
                );
            }
            let outcome = orch_host::sites::reap_released_sites(root, &round)?;
            println!(
                "orch sites gc · round={round} · removed={} quarantined={} refused={} targetFailures={} legacyReported={} freedBytes={}",
                outcome.removed.len(),
                outcome.quarantined.len(),
                outcome.refused.len(),
                outcome.target_failures.len(),
                outcome.legacy_reports.len(),
                outcome.freed_bytes,
            );
            for site in &outcome.removed {
                println!("  removed {site}");
            }
            for site in &outcome.quarantined {
                println!("  quarantined residue from {site}");
            }
            for refusal in &outcome.refused {
                println!("  REFUSED {refusal}");
            }
            for failure in &outcome.target_failures {
                println!("  target cleanup warning: {failure}");
            }
            for legacy in &outcome.legacy_reports {
                println!(
                    "  legacy discovery-only {} · {}",
                    legacy.worktree, legacy.reason
                );
            }
            if let Some(invariant) = &outcome.registry_invariant {
                println!(
                    "  registry invariant: registered={} activeLeases={} preserves={} prunable={} holds={}",
                    invariant.registered,
                    invariant.active_leases,
                    invariant.declared_preserves,
                    invariant.prunable,
                    invariant.holds,
                );
                if !invariant.holds {
                    println!("    missing={:?}", invariant.missing);
                    println!("    unexpected={:?}", invariant.unexpected);
                }
            }
            let clean = outcome.refused.is_empty()
                && outcome.target_failures.is_empty()
                && outcome
                    .registry_invariant
                    .as_ref()
                    .is_none_or(|invariant| invariant.holds);
            Ok(if clean {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        SitesCmd::SweepScratch { ttl_hours } => {
            let ttl_secs = ttl_hours
                .checked_mul(60 * 60)
                .context("--ttl-hours 超出可表示范围")?;
            let scratch_root = root.join("orch/target/test-tmp");
            let report = orch_host::util::sweep_test_scratch_root(
                &scratch_root,
                Duration::from_secs(ttl_secs),
                &[],
            )?;
            println!(
                "orch sites sweep-scratch · root={} · ttlHours={} · removed={} · failed={} · freedBytes={}",
                scratch_root.display(),
                ttl_hours,
                report.removed.len(),
                report.failed.len(),
                report.freed_bytes
            );
            for path in &report.removed {
                println!("  removed {}", path.display());
            }
            for (path, reason) in &report.failed {
                println!("  REFUSED {}: {reason}", path.display());
            }
            Ok(if report.is_total_failure() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            })
        }
        SitesCmd::SweepTrialCache => {
            let report = orch_host::buildcache::sweep_trial_cache(root, 1)?;
            println!(
                "orch sites sweep-trial-cache · keepLatest=1 · removed={} · freedBytes={}",
                report.removed, report.freed_bytes
            );
            Ok(ExitCode::SUCCESS)
        }
        SitesCmd::RotateLogs => {
            let outcome = orch_host::logrotate::rotate_closed_round_logs(root)?;
            let source_bytes = outcome
                .archived
                .iter()
                .chain(outcome.orphaned.iter())
                .map(|mapping| mapping.source_bytes)
                .sum::<u64>();
            let archive_bytes = outcome
                .archived
                .iter()
                .chain(outcome.orphaned.iter())
                .map(|mapping| mapping.archive_bytes)
                .sum::<u64>();
            println!(
                "orch sites rotate-logs · closed={} orphaned={} · bytes {} -> {}",
                outcome.archived.len(),
                outcome.orphaned.len(),
                source_bytes,
                archive_bytes
            );
            for mapping in &outcome.archived {
                println!(
                    "  closed {} · {} -> {}",
                    mapping.round,
                    mapping.source.display(),
                    mapping.archive.display()
                );
            }
            for mapping in &outcome.orphaned {
                println!(
                    "  ORPHAN {} · {} -> {}",
                    mapping.round,
                    mapping.source.display(),
                    mapping.archive.display()
                );
            }
            for round in &outcome.skipped_bad_ledgers {
                println!("  REFUSED bad ledger {round}");
            }
            Ok(if outcome.skipped_bad_ledgers.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        SitesCmd::SweepTargets { ttl_hours } => {
            let ttl_secs = ttl_hours
                .checked_mul(60 * 60)
                .context("--ttl-hours 超出可表示范围")?;
            let report = orch_host::buildcache::sweep_targets_for_round(
                root,
                Duration::from_secs(ttl_secs),
            )?;
            println!(
                "orch sites sweep-targets · ttlHours={} · removed={} refused={} preserved={} failures={} · freedBytes={} debugBytes={}",
                ttl_hours,
                report.removed.len(),
                report.refused.len(),
                report.preserved.len(),
                report.failures.len(),
                report.freed_bytes,
                report.debug_bytes
            );
            for path in &report.removed {
                println!("  removed {path}");
            }
            for path in &report.refused {
                println!("  REFUSED {path}");
            }
            for failure in &report.failures {
                println!("  ERROR {failure}");
            }
            Ok(if report.refused.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
    }
}

fn cmd_round(root: &std::path::Path, action: RoundCmd) -> Result<ExitCode> {
    match action {
        RoundCmd::Open {
            id,
            purpose,
            signed_off,
            sign_note,
            force,
        } => {
            let o = orch_host::round::run_open(
                root,
                &id,
                &purpose,
                signed_off,
                sign_note.as_deref(),
                force,
            )?;
            if o.repaired {
                println!(
                    "orch round open {}: 部分开轮已补齐（仅补写 CURRENT-ROUND）",
                    o.round
                );
            } else {
                println!("orch round open {}: 目录骨架 + RoundOpened{} + CURRENT-ROUND + BOARD 开版行 ✅",
                    o.round, if signed_off { " + PlanSignedOff" } else { "" });
            }
            println!("  下一步：写 tasks/<ID>.md 与 seeds/ → git commit（协议资产先入库再派发）");
            println!("         → orch round seed-verified <ID> --expected-red \"...\"");
            if !signed_off {
                println!("         → orch round sign-off（HITL#1）");
            }
            println!("         → orch dispatch <ID>（Tier F）或 orch run-task <ID>（Tier S）");
            Ok(ExitCode::SUCCESS)
        }
        RoundCmd::SignOff { note } => {
            let round = orch_host::round::run_sign_off(root, note.as_deref())?;
            println!("orch round sign-off: 轮 {round} PlanSignedOff 已落账（actor=user）");
            Ok(ExitCode::SUCCESS)
        }
        RoundCmd::SeedVerified {
            task,
            expected_red,
            record_only,
            allow_file_passed,
        } => {
            let (round, measured) = orch_host::round::run_seed_verified(
                root,
                &task,
                &expected_red,
                record_only,
                allow_file_passed,
            )?;
            match measured {
                Some(m) => {
                    println!("orch round seed-verified {task}: 预验实测通过并落账（轮 {round}）");
                    println!(
                        "  种子文件内 {}f/{}p · 总 {}f/{}p ({}) · O5 判据全过（无空洞位/基线不受扰）",
                        m.file_failed, m.file_passed, m.total_failed, m.total_passed, m.total
                    );
                    for case in m.failed_cases.iter().take(8) {
                        println!("    × {case}");
                    }
                }
                None => println!(
                    "orch round seed-verified {task}: 仅记录（--record-only，未真跑）落账（轮 {round}，expectedRed=\"{expected_red}\"）"
                ),
            }
            Ok(ExitCode::SUCCESS)
        }
        RoundCmd::Close { force, note } => orch_host::close::with_protocol_transition(
            root,
            "orch round close CLI snapshot",
            || {
                // 决策 9：收轮前把本轮聚合面归档进库（`rounds/<r>/SNAPSHOT.json`），
                // 供 r46 历史回放。B102 的 `archive_round` 内部已剥离 activity 原文，
                // 因此入库的是聚合面而非对话内容；失败不阻断收轮（best-effort，明示告知）。
                match snapshot_for_archive(root) {
                Ok((round_id, snap)) => match orch_host::snapshot::archive_round(root, &round_id, &snap) {
                    Ok(()) => println!("· 已归档 coordination/rounds/{round_id}/SNAPSHOT.json（聚合面，不含 activity 原文）"),
                    Err(e) => println!("⚠️ 快照归档失败（不阻断收轮）: {e:#}"),
                },
                Err(e) => println!("⚠️ 快照采样失败（不阻断收轮）: {e:#}"),
            }
                prepare_round_close_cleanup(root);
                let o = orch_host::round::run_close(root, force, note.as_deref())?;
                println!(
                    "orch round close: 轮 {} 已收（main 终态 {}；任务 {}/{} Recorded）",
                    o.round, o.final_main_short, o.recorded, o.total
                );
                if !o.unrecorded.is_empty() {
                    println!("  ⚠️ 未收任务（--force 放行）: {}", o.unrecorded.join(", "));
                }
                println!("  DONE.md 已写（Tier F 客户端 wait 返回 ROUND_COMPLETE 后写 SUMMARY）");
                match orch_host::logrotate::rotate_closed_round_logs(root) {
                    Ok(rotated) => println!(
                        "· 收轮后日志轮转：closed={} orphaned={}（legacy handshake relink 永不轮转）",
                        rotated.archived.len(),
                        rotated.orphaned.len()
                    ),
                    Err(error) => println!(
                        "⚠️ 收轮后日志轮转失败（RoundClosed 已落账，不回滚）: {error:#}"
                    ),
                }
                println!("  建议：git add coordination/BOARD.md coordination/rounds/{}/events.jsonl && git commit -m \"board({}): 收轮\"",
                o.round, o.round);
                Ok(ExitCode::SUCCESS)
            },
        )
        .map_err(bind_round_close_failure),
    }
}

fn cmd_check(root: &std::path::Path, task: &str) -> Result<ExitCode> {
    let round = fs::read_to_string(root.join("coordination/runtime/CURRENT-ROUND"))
        .context("CURRENT-ROUND 缺失")?
        .trim()
        .to_string();
    let c = orch_host::card::load(root, &round, task)?;
    let report_rel = format!("coordination/rounds/{round}/reports/{task}-REPORT.md");
    match orch_host::mech::check(root, &c, &format!("task/{task}"), &report_rel, None) {
        Ok(m) => {
            println!(
                "orch check {task}: 机检通过（merge-base {}）",
                &m.merge_base[..7]
            );
            for n in &m.notes {
                println!("  {n}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            println!("orch check {task}: ❌ 机检 FAIL —— {e:#}");
            Ok(ExitCode::FAILURE)
        }
    }
}

fn cmd_verify(root: &std::path::Path, task: &str, timeout_secs: u64) -> Result<ExitCode> {
    let (_, active) = active_round_contract(root)?;
    let model = active.candidate.verification.model.as_deref().unwrap_or("");
    if !model.is_empty() {
        println!("orch verify {task} · verifier model={model}（signed ROUND-IR）");
    }
    let v = orch_host::run_verify(root, task, model, timeout_secs)?;
    println!(
        "  VERDICT: {} · review {} · {}s{}",
        v.verdict,
        v.review_rel,
        v.duration_secs,
        v.cost_usd
            .map(|c| format!(" · ${c:.4}"))
            .unwrap_or_default()
    );
    Ok(if v.verdict == "PASS" {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn cmd_verdict(
    root: &std::path::Path,
    task: &str,
    attempt: &str,
    expected_head: &str,
    expected_main: &str,
    verdict: &str,
    reason: Option<&str>,
    dry_run: bool,
) -> Result<ExitCode> {
    let verdict = orch_host::verify::RootVerdict::parse(verdict)?;
    let out = orch_host::verify::run_root_verdict(
        root,
        task,
        attempt,
        expected_head,
        expected_main,
        verdict,
        reason,
        dry_run,
    )?;
    for warning in
        orch_host::verify::nongate_attempt_receipt_warnings(root, task, attempt, expected_head)
    {
        eprintln!("{warning}");
    }
    println!(
        "orch verdict {task}: {}{}{} · gates={}",
        out.verdict,
        if out.dry_run { " (dry-run)" } else { "" },
        if !out.dry_run && !out.appended {
            " (idempotent replay)"
        } else {
            ""
        },
        out.gates.len()
    );
    if out.verdict == "PASS" && !out.dry_run {
        eprintln!(
            "⚠️  root PASS fixed authorization 已落账；正常下一步是 `orch seal`。main 写屏障将在 canonical MergeStarted 时开启（分步恢复仅用 `orch merge`）。"
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_seal(
    root: &std::path::Path,
    task: &str,
    attempt: &str,
    expected_head: &str,
) -> Result<ExitCode> {
    let outcome = orch_host::close::run_seal(root, task, attempt, expected_head)?;
    for warning in &outcome.nongate_receipt_warnings {
        eprintln!("{warning}");
    }
    println!(
        "orch seal {task}: complete @{}{} · gates={}",
        outcome.merge_sha_short,
        if outcome.replayed_complete {
            " (durable replay)"
        } else {
            ""
        },
        outcome.gates.len()
    );
    for gate in &outcome.gates {
        println!(
            "  门 {} exit={} ({}ms)",
            gate.name, gate.exit_code, gate.duration_ms
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_merge(root: &std::path::Path, task: &str) -> Result<ExitCode> {
    println!("orch merge {task}");
    let m = orch_host::run_merge(root, task)?;
    println!(
        "  merged @{}（no-ff）· 合后门全绿 · 现场已清理",
        m.merge_sha_short
    );
    for g in &m.gates {
        println!("  门 {} exit={} ({}ms)", g.name, g.exit_code, g.duration_ms);
    }
    Ok(ExitCode::SUCCESS)
}

/// R45S 接线：祖先复核由 `run_record` 自己用 gitx 做（CLI 不得传入未经复核的判断），
/// 门红时 `run_record` 已落升级事实并返回 Err，这里只负责渲染。
fn cmd_record(root: &std::path::Path, task: &str, at_tip: bool) -> Result<ExitCode> {
    println!(
        "orch record {task}{}",
        if at_tip { " --at-tip" } else { "" }
    );
    let r = if at_tip {
        orch_host::close::run_record_at_tip(root, task)?
    } else {
        orch_host::run_record(root, task)?
    };
    if r.already_recorded {
        return Ok(ExitCode::SUCCESS);
    }
    println!("  已补记 TaskRecorded · 复跑门全绿");
    if let Some(proof) = &r.relaxation {
        println!(
            "  RecordGateRelaxed · {}..{} · reason={}",
            proof.merge_sha, proof.tip_sha, proof.reason
        );
        println!("  放行文件（完整清单，{} 项）:", proof.files.len());
        for file in &proof.files {
            println!("    {file}");
        }
    }
    for g in &r.gates {
        println!("  门 {} exit={} ({}ms)", g.name, g.exit_code, g.duration_ms);
    }
    Ok(ExitCode::SUCCESS)
}

/// B102 接线：把仓内真值采样成 `SnapshotInputs` → 纯聚合 → 打印/落盘。
///
/// 分层沿用 `liveness.rs` 的先例：本函数只做 IO 采样与装配，判据全在
/// `snapshot::build_snapshot`（其内部对 `judgement=None` 必经 `liveness::judge`）。
/// **只读**：不落任何账本事件；`--write` 才原子写 runtime/snapshot.json（约定仅 daemon 用）。
/// 收轮归档用：复用 `cmd_snapshot` 的采样链产出一份快照（不打印、不落盘）。
fn snapshot_for_archive(
    root: &std::path::Path,
) -> Result<(String, orch_host::snapshot::OrchSnapshot)> {
    let round = orch_host::current_round(root)?;
    let snap = build_repo_snapshot(root, &round)?;
    Ok((round, snap))
}

/// 采样链（`orch snapshot` 与收轮归档共用）：读账本 → 探针 agents → 折算三维预算
/// → 归一化活动流 → 纯聚合 `OrchSnapshot`，并附加通道健康告警。**只读，不落账本。**
fn build_repo_snapshot(
    root: &std::path::Path,
    round: &str,
) -> Result<orch_host::snapshot::OrchSnapshot> {
    use orch_host::snapshot as snap;
    let round = round.to_string();
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let ledger = read_ledger(&ledger_path)
        .with_context(|| format!("读取账本失败: {}", ledger_path.display()))?;
    if !ledger.bad_lines.is_empty() {
        bail!("snapshot 拒绝坏账本");
    }
    let active = orch_host::plan::require_active_round_ir(root, &round, &ledger.events)?;
    let projection = fold(&ledger.events);
    let now = orch_host::ledger::now_rfc3339();
    let ir_liveness = active.candidate.liveness;

    // ── agent → 在飞任务：取账本里最后一次 DispatchIssued，且该任务尚未终态 ──
    let mut agent_task: std::collections::BTreeMap<String, String> = Default::default();
    for ev in &ledger.events {
        if ev.kind != "DispatchIssued" {
            continue;
        }
        let Some(p) = ev.payload.as_ref() else {
            continue;
        };
        let (Some(agent), Some(task)) = (
            p.get("agent").and_then(|v| v.as_str()),
            p.get("taskId")
                .and_then(|v| v.as_str())
                .or(ev.task_id.as_deref()),
        ) else {
            continue;
        };
        agent_task.insert(agent.to_string(), task.to_string());
    }
    // 注意：`TaskProjection::state` 是 `Option<TaskState>`——早期用
    // `format!("{:?}")` 比字符串会得到 `Some(Recorded)` 而永不命中，
    // 导致已收口的卡仍被算作「在飞」并产生假停滞告警。此处用类型化匹配。
    let terminal = |t: &str| {
        projection
            .tasks
            .get(t)
            .map(|s| matches!(s.state, Some(TaskState::Recorded) | Some(TaskState::Merged)))
            .unwrap_or(false)
    };
    agent_task.retain(|_, t| !terminal(t));

    // ── 探针规格：注册表里的每个 agent 一行；GO 路径与 writeSet 取自卡 ──
    let registry = orch_host::wake::load_registry(root)?;
    let mut specs = Vec::new();
    for agent_id in registry.keys() {
        let current_task = agent_task.get(agent_id).cloned();
        let write_set = current_task
            .as_deref()
            .and_then(|t| orch_host::card::load(root, &round, t).ok())
            .map(|c| c.meta.write_set.clone())
            .unwrap_or_default();
        let go_path = current_task.as_deref().and_then(|t| {
            let dir = root.join(format!("coordination/rounds/{round}/dispatch/{agent_id}"));
            std::fs::read_dir(&dir).ok().and_then(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .map(|n| n.starts_with(&format!("GO-{t}")) && n.ends_with(".md"))
                            .unwrap_or(false)
                    })
                    .max()
            })
        });
        specs.push(snap::AgentProbeSpec {
            agent_id: agent_id.clone(),
            current_task,
            go_path,
            write_set,
        });
    }
    let agents = snap::probe_agents(root, &specs);

    // ── 三维预算：上限取 mode 配置，已花取账本折叠 ──
    let rb = orch_host::budget::load_round_budget(root)?;
    let rc = orch_host::cost::cost_report(root, &round)?;
    let budget = snap::BudgetInput {
        max_usd: rb.as_ref().and_then(|b| b.max_usd),
        max_wall_minutes: rb.as_ref().and_then(|b| b.wall_minutes),
        max_model_wakes: rb.as_ref().and_then(|b| b.max_model_wakes),
        spent_usd: rc.total_usd,
        spent_wall_minutes: orch_host::budget::active_attempt_wall_mins(&ledger.events, &now),
        spent_model_wakes: orch_host::budget::count_model_wakes(&ledger.events),
    };

    // ── 活动流：读 per-attempt wake 日志（B100 落盘），归一化并 redact ──
    let mut activity = Vec::new();
    let log_dir = root.join("coordination/runtime/logs");
    if let Ok(rd) = std::fs::read_dir(&log_dir) {
        let logs: Vec<_> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            // B100 起是 per-attempt `wake-<agent>-<nanos>-<pid>-<seq>.jsonl`；
            // 过渡期仍可能只有旧的截断式 `wake-<agent>.log`，两者都吃，避免面板空窗。
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("wake-"))
                    .unwrap_or(false)
            })
            .collect();
        // 按**修改时间**排序取最近的几个——早期按文件名排序会把两天前的
        // `wake-planner.log`（字母序靠后）当成最新活动，面板会把陈旧错误显示成当前状态。
        let mut logs: Vec<(std::time::SystemTime, PathBuf)> = logs
            .into_iter()
            .filter_map(|p| {
                let mt = std::fs::metadata(&p).ok()?.modified().ok()?;
                Some((mt, p))
            })
            .collect();
        logs.sort_by(|a, b| b.0.cmp(&a.0));
        for (mtime, path) in logs.iter().take(4) {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            // `wake-executor-opencode-<nanos>-<pid>-<seq>.jsonl` 与
            // `wake-executor-opencode.log` 都还原成 `executor-opencode`。
            let agent = name
                .trim_start_matches("wake-")
                .trim_end_matches(".log")
                .trim_end_matches(".jsonl")
                .split('-')
                .take(2)
                .collect::<Vec<_>>()
                .join("-");
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            for ev in orch_host::activity::parse_activity_log(&agent, name, &text)
                .into_iter()
                .rev()
                .take(20)
            {
                activity.push(snap::ActivityLine {
                    // 源行无时间戳时**不得回填 now**（那是凭空造当前时间）；
                    // 退化为该日志文件的 mtime——这是真实观测，不是估算。
                    ts: ev
                        .ts
                        .unwrap_or_else(|| orch_host::ledger::rfc3339_of(*mtime)),
                    agent: ev.agent,
                    kind: format!("{:?}", ev.kind).to_lowercase(),
                    summary: ev.summary,
                });
            }
        }
    }

    let liveness_defaults = orch_host::liveness::LivenessOpts::default();
    let liveness_opts = orch_host::liveness::LivenessOpts {
        stall: std::time::Duration::from_secs(ir_liveness.working_stall_minutes.saturating_mul(60)),
        probe_every: std::time::Duration::from_secs(ir_liveness.monitor_seconds),
        confirm: u8::try_from(ir_liveness.confirm_samples)
            .context("ROUND-IR liveness.confirmSamples 超出 u8")?,
        ..liveness_defaults
    };
    let inputs = snap::SnapshotInputs {
        round_id: round.clone(),
        generated_at: now,
        events: ledger.events,
        agents,
        budget,
        activity,
        liveness_opts,
    };
    let mut s = snap::build_snapshot(&inputs);
    s.source = snap::SnapshotSource::Frontend;

    // 通道健康（planner 接线）：`orch wake` 后台 spawn 后不等待，子进程如何结束无人记录——
    // r45 内四类静默失败（沙箱拒绝 / 会话损坏 / 上下文耗尽 / 上限超时）全靠人肉翻日志才发现。
    // 这里把它变成快照里的一等告警：**只提示不自动处置**（决策 4）。
    let mut wake_pid: std::collections::BTreeMap<String, u32> = Default::default();
    for ev in &inputs.events {
        if ev.kind != "WakeIssued" {
            continue;
        }
        let Some(p) = ev.payload.as_ref() else {
            continue;
        };
        let (Some(agent), Some(pid)) = (
            p.get("agent").and_then(|v| v.as_str()),
            p.get("pid").and_then(|v| v.as_u64()),
        ) else {
            continue;
        };
        wake_pid.insert(agent.to_string(), pid as u32);
    }
    for spec in &specs {
        // 只对**当前有在飞任务**的 agent 报：本判据回答的是「这次注入是不是白发了」，
        // 对空闲通道翻历史故障只会制造噪声（实测会把两天前的 planner session-limit
        // 和一次其实成功的 codex turn 报成 Critical）。
        if spec.current_task.is_none() {
            continue;
        }
        let pid = wake_pid.get(&spec.agent_id).copied();
        let Some(h) = orch_host::chanhealth::probe_wake_health(root, &spec.agent_id, pid) else {
            continue;
        };
        if h.faults.is_empty() {
            continue;
        }
        // 日志必须新鲜（≤6h）：陈旧日志里的失效串不代表当前通道状态。
        let fresh = std::fs::metadata(&h.log_path)
            .and_then(|m| m.modified())
            .map(|mt| {
                mt.elapsed()
                    .map(|d| d.as_secs() <= 6 * 3600)
                    .unwrap_or(true)
            })
            .unwrap_or(false);
        if !fresh {
            continue;
        }
        // 进程已退出 + 有失效证据 = 这一轮注入没干成活（严重）；
        // 进程还在跑但日志已见失效串 = 提醒（可能自行恢复）。
        let severity = if h.ended_without_progress() {
            snap::Severity::Critical
        } else {
            snap::Severity::Warn
        };
        for f in &h.faults {
            s.alerts.push(snap::Alert {
                severity,
                kind: "channel".to_string(),
                subject: spec.agent_id.clone(),
                message: format!(
                    "通道失效 {}（{}）· 日志 {}",
                    f.as_str(),
                    f.hint(),
                    h.log_path.display()
                ),
                suggested_command: spec
                    .current_task
                    .as_deref()
                    .map(|t| format!("orch resume {t}"))
                    .or_else(|| Some(format!("orch handshake {}", spec.agent_id))),
            });
        }
    }

    Ok(s)
}

fn cmd_snapshot(root: &std::path::Path, json: bool, write: bool) -> Result<ExitCode> {
    use orch_host::snapshot as snap;
    let round = orch_host::current_round(root)?;
    let mut s = build_repo_snapshot(root, &round)?;
    if write {
        s.source = snap::SnapshotSource::Daemon;
        snap::write_snapshot(root, &s)?;
        println!("· 已原子写 coordination/runtime/snapshot.json");
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&s)?);
        return Ok(ExitCode::SUCCESS);
    }

    println!(
        "orch snapshot · round={} · {}",
        s.round.round_id, s.generated_at
    );
    println!("  ── agents ──");
    for a in &s.agents {
        println!(
            "  {:<24} {:<14} task={:<8} hb={}s",
            a.agent_id,
            format!("{:?}", a.state).to_lowercase(),
            a.current_task.as_deref().unwrap_or("-"),
            a.heartbeat_age_secs
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into())
        );
    }
    println!("  ── round ──");
    let b = &s.round.budget;
    println!(
        "  usd {:.2}/{} · wall {}/{} min · wakes {}/{}",
        b.usd.spent,
        b.usd
            .max
            .map(|v| format!("{v:.0}"))
            .unwrap_or_else(|| "-".into()),
        b.wall_minutes.spent,
        b.wall_minutes
            .max
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".into()),
        b.model_wakes.spent,
        b.model_wakes
            .max
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".into()),
    );
    if !s.alerts.is_empty() {
        println!("  ── alerts ──");
        for al in &s.alerts {
            println!(
                "  [{:?}] {} · {}{}",
                al.severity,
                al.subject,
                al.message,
                al.suggested_command
                    .as_deref()
                    .map(|c| format!(" → {c}"))
                    .unwrap_or_default()
            );
        }
    }
    println!(
        "  活动流 {} 条 · 任务 {} 个",
        s.activity.len(),
        s.tasks.len()
    );
    Ok(ExitCode::SUCCESS)
}

fn cmd_cost(root: &std::path::Path, round: Option<String>) -> Result<ExitCode> {
    let round = match round {
        Some(r) => r,
        None => fs::read_to_string(root.join("coordination/runtime/CURRENT-ROUND"))
            .context("CURRENT-ROUND 缺失（可 --round 指定）")?
            .trim()
            .to_string(),
    };
    let rc = orch_host::cost::cost_report(root, &round)?;
    print!("{}", orch_host::cost::render_table(&round, &rc));
    Ok(ExitCode::SUCCESS)
}

#[derive(Debug)]
struct StallProcessSnapshot {
    local_orch: Vec<String>,
    agent: Vec<String>,
    probe_available: bool,
}

/// `orch stall-check` is deliberately observational: every input is sampled
/// before classification and no runtime or git write API is reachable here.
fn cmd_stall_check(root: &std::path::Path) -> Result<ExitCode> {
    use orch_host::stall::{classify_attempt, classify_round, RoundInputs, StallInputs};

    let current_round_path = root.join("coordination/runtime/CURRENT-ROUND");
    match fs::read_to_string(&current_round_path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!("orch stall-check · 无活跃轮（CURRENT-ROUND 不存在）");
            return Ok(ExitCode::SUCCESS);
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "stall-check 读取 CURRENT-ROUND 失败: {}",
                    current_round_path.display()
                )
            });
        }
    }
    let round = orch_host::current_round(root)?;
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let ledger = read_ledger(&ledger_path)
        .with_context(|| format!("stall-check 读取账本失败: {}", ledger_path.display()))?;
    if !ledger.bad_lines.is_empty() {
        bail!(
            "stall-check 拒绝坏账本（{} 行；首坏行 #{})",
            ledger.bad_lines.len(),
            ledger.bad_lines[0].0
        );
    }

    let processes = stall_process_snapshot();
    let live_wake_logs = stall_live_wake_logs(root)?;
    let mut by_task: std::collections::BTreeMap<String, Vec<&orch_core::EventRecord>> =
        std::collections::BTreeMap::new();
    for event in &ledger.events {
        if let Some(task) = event.task_id.as_ref() {
            by_task.entry(task.clone()).or_default().push(event);
        }
    }

    println!(
        "orch stall-check · round={round} · events={}",
        ledger.events.len()
    );
    let mut need_action = false;
    let mut rendered_tasks = 0usize;
    for (task, events) in by_task {
        let Some(current_attempt) = orch_host::attempt::current_attempt(&ledger.events, &task)?
        else {
            continue;
        };
        let dispatch_index = events
            .iter()
            .rposition(|event| event.kind == "DispatchIssued")
            .context("current_attempt 存在但 task 事件段缺 DispatchIssued")?;
        let current_events = &events[dispatch_index..];
        let locally_collecting = processes
            .local_orch
            .iter()
            .any(|line| stall_line_has_token(line, &task));
        let Some(projection) = stall_attempt_projection(
            current_events,
            &current_attempt.attempt_id,
            locally_collecting,
        ) else {
            // Verdict/merge is a quiet, mechanically driven transition in the
            // prototype. It is intentionally outside the nine attempt rows.
            continue;
        };
        let attempt_id = current_attempt.attempt_id;
        let agent = current_events
            .iter()
            .rev()
            .find(|event| event.kind == "DispatchIssued")
            .and_then(|event| stall_payload_str(event, "agent"))
            .unwrap_or_default();
        let executor_alive = processes.agent.iter().any(|line| {
            stall_line_has_token(line, &task)
                || (!agent.is_empty() && stall_line_has_token(line, agent))
        });
        let wake_log_growing = (!agent.is_empty())
            && orch_host::snapshot::provider_output_age(
                root,
                &ledger.events,
                &task,
                agent,
                &attempt_id,
            )
            .is_some_and(|age| age <= Duration::from_secs(180));
        let artifact = orch_host::stall::observe_branch_artifact(root, &round, &task, &attempt_id)?;
        let verdict = classify_attempt(&StallInputs {
            task_id: task.clone(),
            attempt_id,
            projection,
            artifact: artifact.artifact,
            artifact_from_current_attempt: artifact.from_current_attempt,
            executor_alive,
            wake_log_growing,
        });
        println!(
            "  task={task} verdict={} needAction={} artifact={} hint={}",
            verdict.verdict,
            verdict.need_action,
            stall_artifact_label(artifact.artifact),
            verdict.hint
        );
        need_action |= verdict.need_action;
        rendered_tasks += 1;
    }
    if rendered_tasks == 0 {
        println!("  tasks=none");
    }

    let ledger_silence_secs = match ledger.events.last() {
        Some(event) => orch_host::liveness::attempt_elapsed_from_dispatch(
            &event.ts,
            &orch_host::ledger::now_rfc3339(),
        )
        .context("stall-check 最后一条账本事件时间无效")?
        .as_secs(),
        None => 0,
    };
    let round_verdict = classify_round(&RoundInputs {
        local_orch_processes: processes.local_orch.len() + usize::from(!processes.probe_available),
        agent_processes: processes.agent.len(),
        live_wake_logs,
        ledger_silence_secs,
    });
    println!(
        "  round=planner-idle plannerIdle={} needAction={} hint={}",
        round_verdict.planner_idle, round_verdict.need_action, round_verdict.hint
    );
    need_action |= round_verdict.need_action;

    Ok(if need_action {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn stall_attempt_projection(
    events: &[&orch_core::EventRecord],
    current_attempt: &str,
    locally_collecting: bool,
) -> Option<orch_host::stall::AttemptProjection> {
    use orch_host::stall::AttemptProjection;

    let last = events.iter().rev().find(|event| {
        let interesting = matches!(
            event.kind.as_str(),
            "DispatchIssued"
                | "ReportCollectClaimed"
                | "ReportCollectExecuting"
                | "ReportCollectExecuted"
                | "ReportCollectCompleted"
                | "ReportCollectReleased"
                | "VerdictIssued"
                | "MergeStarted"
                | "MergeExecuted"
                | "TaskRecorded"
                | "AttemptBlocked"
                | "AttemptFailed"
                | "AttemptTimedOut"
                | "AttemptCrashed"
        );
        interesting
            && stall_payload_str(event, "attemptId")
                .is_none_or(|attempt| attempt == current_attempt)
    })?;

    match last.kind.as_str() {
        "TaskRecorded" => Some(AttemptProjection::Recorded),
        "AttemptBlocked" | "AttemptFailed" | "AttemptTimedOut" | "AttemptCrashed" => {
            Some(AttemptProjection::TerminalFailure)
        }
        "VerdictIssued" | "MergeStarted" | "MergeExecuted" => None,
        _ if locally_collecting => Some(AttemptProjection::CollectingNow),
        "ReportCollectCompleted" => Some(AttemptProjection::ReadyForReview),
        "ReportCollectReleased" => Some(AttemptProjection::CollectRejected),
        "ReportCollectClaimed" | "ReportCollectExecuting" | "ReportCollectExecuted" => {
            Some(AttemptProjection::CollectInflight)
        }
        "DispatchIssued" => Some(AttemptProjection::Dispatched),
        _ => None,
    }
}

fn stall_payload_str<'a>(event: &'a orch_core::EventRecord, key: &str) -> Option<&'a str> {
    event
        .payload
        .as_ref()
        .and_then(|payload| payload.get(key))
        .and_then(serde_json::Value::as_str)
}

fn stall_artifact_label(artifact: orch_host::stall::ArtifactOnBranch) -> &'static str {
    match artifact {
        orch_host::stall::ArtifactOnBranch::None => "none",
        orch_host::stall::ArtifactOnBranch::Report => "REPORT",
        orch_host::stall::ArtifactOnBranch::Blocked => "BLOCKED",
    }
}

fn stall_process_snapshot() -> StallProcessSnapshot {
    let output = match std::process::Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            eprintln!("orch stall-check: WARNING: ps 探针不可用：{error}");
            return StallProcessSnapshot {
                local_orch: Vec::new(),
                agent: Vec::new(),
                probe_available: false,
            };
        }
    };
    if !output.status.success() {
        eprintln!(
            "orch stall-check: WARNING: ps 探针失败（exit={:?}）：{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return StallProcessSnapshot {
            local_orch: Vec::new(),
            agent: Vec::new(),
            probe_available: false,
        };
    }
    let lines = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let local_orch = lines
        .iter()
        .filter(|line| {
            line.contains("orch/target/debug/orch")
                && !line.contains("stall-check")
                && !line.contains("grep")
        })
        .cloned()
        .collect();
    let agent = lines
        .iter()
        .filter(|line| stall_is_agent_process(line))
        .cloned()
        .collect();
    StallProcessSnapshot {
        local_orch,
        agent,
        probe_available: true,
    }
}

fn stall_is_agent_process(line: &str) -> bool {
    !line.contains("grep")
        && (line.contains("codex exec")
            || line.contains("opencode run")
            || line.contains("wake-pi.sh")
            || line.contains("wake-pi-stream.sh")
            || line.contains("wake-zcode")
            || line.contains("wake-multica")
            || line.contains("__wake-supervise")
            || (line.contains(" agy ") && line.contains(" -p")))
}

fn stall_line_has_token(line: &str, expected: &str) -> bool {
    line.split(|character: char| {
        !(character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    })
    .any(|token| token == expected)
}

fn stall_live_wake_logs(root: &std::path::Path) -> Result<usize> {
    let dir = root.join("coordination/runtime/logs");
    if !dir.exists() {
        return Ok(0);
    }
    let now = std::time::SystemTime::now();
    let mut count = 0usize;
    for entry in fs::read_dir(&dir)
        .with_context(|| format!("stall-check 读取 wake 日志目录失败: {}", dir.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("wake-") || !name.ends_with(".jsonl") {
            continue;
        }
        let metadata = entry.metadata()?;
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata.modified().with_context(|| {
            format!(
                "stall-check 读取 wake 日志 mtime 失败: {}",
                entry.path().display()
            )
        })?;
        let age = now.duration_since(modified).with_context(|| {
            format!(
                "stall-check wake 日志 mtime 位于未来: {}",
                entry.path().display()
            )
        })?;
        if age <= Duration::from_secs(180) {
            count += 1;
        }
    }
    Ok(count)
}

fn cmd_status(root: &std::path::Path) -> Result<ExitCode> {
    let coord = root.join("coordination");
    let round = fs::read_to_string(coord.join("runtime/CURRENT-ROUND"))
        .context("runtime/CURRENT-ROUND 缺失（无活动轮；可用 doctor 体检）")?
        .trim()
        .to_string();
    let ledger_path = coord.join(format!("rounds/{round}/events.jsonl"));
    let ledger = read_ledger(&ledger_path)
        .with_context(|| format!("读取账本失败: {}", ledger_path.display()))?;
    let p = fold(&ledger.events);
    let active_plan_signed_off = current_active_plan_signed_off(&ledger.events, &round)?;

    println!("orch status · round={round} · root={}", root.display());
    println!(
        "  事件 {} 条（坏行 {}）· 计划签核: {} · 轮态: {}",
        p.total_events,
        ledger.bad_lines.len(),
        if active_plan_signed_off { "✅" } else { "—" },
        if p.round_closed {
            "已收轮"
        } else {
            "进行中"
        },
    );
    // ts 健康度曝光（B29）：读账本原文 → ts_health → ts_health_line
    if let Ok(raw) = fs::read_to_string(&ledger_path) {
        if let Some(tsl) = orch_host::ledger::ts_health_line(&orch_host::ledger::ts_health(&raw)) {
            println!("  ⚠️ {tsl}");
        }
    }
    if !p.unknown_kinds.is_empty() {
        println!(
            "  ⚠️ 未知事件类型（容错保留）: {}",
            p.unknown_kinds.join(", ")
        );
    }
    println!("  ── 任务投影 ──");
    for (id, t) in &p.tasks {
        println!(
            "  {:<6} {:<24} 事件 {:<3} 最后 {}{}",
            id,
            t.state.map(|s| s.to_string()).unwrap_or_else(|| "?".into()),
            t.event_count,
            t.last_ts,
            t.merge_sha
                .as_deref()
                .map(|s| format!(" · merge {s}"))
                .unwrap_or_default(),
        );
    }
    for (line_no, err) in ledger.bad_lines.iter().take(3) {
        println!("  ⚠️ 坏行 #{line_no}: {err}");
    }
    Ok(ExitCode::SUCCESS)
}

// ───────────────────── B135：orch schedule（只读调度决策面，零副作用零落账） ─────────────────────

/// attempt 终态事件（与 orch_host::attempt 的 TERMINAL_KINDS 对齐；此处只读推导，不改账）。
const SCHEDULE_TERMINAL_KINDS: [&str; 4] = [
    "AttemptBlocked",
    "AttemptTimedOut",
    "AttemptCrashed",
    "AttemptFailed",
];

/// 某 task 单个 attempt 的只读推导视图。
struct ScheduleAttempt {
    attempt_id: String,
    agent: String,
    dispatched_ts: String,
    status: String,
}

/// 从账本派生某 task 的 attempt 序列：DispatchIssued 追加序 = attempt 序（与
/// orch_host::attempt::current_attempt 的推导一致）。每段（dispatch N 与 dispatch N+1 之间）
/// 的最后一条决定性事件给出该 attempt 状态：终态事件 > VerdictIssued > ReportObserved > active。
fn derive_task_attempts(events: &[orch_core::EventRecord], task: &str) -> Vec<ScheduleAttempt> {
    let dispatch_positions: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, ev)| ev.kind == "DispatchIssued" && ev.task_id.as_deref() == Some(task))
        .map(|(pos, _)| pos)
        .collect();
    let mut attempts = Vec::with_capacity(dispatch_positions.len());
    for (index, &pos) in dispatch_positions.iter().enumerate() {
        let dispatch = &events[pos];
        let segment_end = dispatch_positions
            .get(index + 1)
            .copied()
            .unwrap_or(events.len());
        let mut status = "active".to_string();
        for ev in &events[pos + 1..segment_end] {
            if ev.task_id.as_deref() != Some(task) {
                continue;
            }
            if SCHEDULE_TERMINAL_KINDS.contains(&ev.kind.as_str()) {
                status = ev.kind.clone();
            } else if ev.kind == "VerdictIssued" {
                let verdict = ev
                    .payload
                    .as_ref()
                    .and_then(|p| p.get("verdict"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?");
                status = format!("VerdictIssued({verdict})");
            } else if ev.kind == "ReportObserved" && status == "active" {
                status = "ReportObserved".to_string();
            }
        }
        let ordinal = index + 1;
        attempts.push(ScheduleAttempt {
            attempt_id: format!("{task}-A{ordinal:04}"),
            agent: dispatch
                .payload
                .as_ref()
                .and_then(|p| p.get("agent"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?")
                .to_string(),
            dispatched_ts: dispatch.ts.clone(),
            status,
        });
    }
    attempts
}

/// 计振（B135）：该 task 的 AttemptCrashed/AttemptBlocked 各记一振，归因到 attempt 的
/// agent（payload.agent 优先，其次按 attemptId 对齐 dispatch，最后回退事件前最近一次 dispatch）。
/// 两振出局：振数 ≥2 的链上 agent 进 failed 名单。
fn strikes_by_agent(
    events: &[orch_core::EventRecord],
    task: &str,
    attempts: &[ScheduleAttempt],
) -> std::collections::BTreeMap<String, usize> {
    let mut strikes = std::collections::BTreeMap::new();
    let mut last_dispatch_agent: Option<String> = None;
    for ev in events {
        if ev.task_id.as_deref() != Some(task) {
            continue;
        }
        if ev.kind == "DispatchIssued" {
            last_dispatch_agent = ev
                .payload
                .as_ref()
                .and_then(|p| p.get("agent"))
                .and_then(serde_json::Value::as_str)
                .filter(|a| !a.is_empty())
                .map(str::to_string);
            continue;
        }
        if ev.kind != "AttemptCrashed" && ev.kind != "AttemptBlocked" {
            continue;
        }
        let agent = ev
            .payload
            .as_ref()
            .and_then(|p| p.get("agent"))
            .and_then(serde_json::Value::as_str)
            .filter(|a| !a.is_empty())
            .map(str::to_string)
            .or_else(|| {
                ev.payload
                    .as_ref()
                    .and_then(|p| p.get("attemptId"))
                    .and_then(serde_json::Value::as_str)
                    .and_then(|aid| attempts.iter().find(|a| a.attempt_id == aid))
                    .map(|a| a.agent.clone())
            })
            .or_else(|| last_dispatch_agent.clone());
        if let Some(agent) = agent {
            *strikes.entry(agent).or_insert(0) += 1;
        }
    }
    strikes
}

/// 计忙（B135）：当前轮账本里任何 task 的活跃 attempt（最近 dispatch 之后无终态/裁决；
/// ReportObserved 仍占用——审查轮可能打回）都使其 agent 处于 busy。
/// 返回 agent → 占用它的 task 列表。
fn busy_agents(
    events: &[orch_core::EventRecord],
) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut tasks = std::collections::BTreeSet::new();
    for ev in events {
        if let Some(task) = &ev.task_id {
            tasks.insert(task.clone());
        }
    }
    let mut busy: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for task in tasks {
        let attempts = derive_task_attempts(events, &task);
        if let Some(current) = attempts.last() {
            if current.status == "active" || current.status == "ReportObserved" {
                busy.entry(current.agent.clone())
                    .or_default()
                    .push(task.clone());
            }
        }
    }
    busy
}

fn cmd_schedule(root: &std::path::Path, task: &str) -> Result<ExitCode> {
    let round = orch_host::current_round(root)?;
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let ledger = read_ledger(&ledger_path)
        .with_context(|| format!("读取账本失败: {}", ledger_path.display()))?;
    // 决策面不消费坏账本/畸形 attempt 身份：fail-closed（同 dispatch 路径纪律）。
    orch_host::attempt::reject_bad_lines(&ledger)?;
    let _ = orch_host::attempt::current_attempt(&ledger.events, task)?;
    let card = orch_host::card::load(root, &round, task)?;
    let card_agent = card
        .meta
        .agent
        .as_deref()
        .with_context(|| format!("任务卡 {task} 缺 agent 字段"))?;

    let chain = orch_host::scheduler::escalation_chain(card_agent).map_err(anyhow::Error::msg)?;
    let reviews = orch_host::scheduler::review_chain(card_agent).map_err(anyhow::Error::msg)?;

    let attempts = derive_task_attempts(&ledger.events, task);
    let strikes = strikes_by_agent(&ledger.events, task, &attempts);
    let failed: Vec<String> = chain
        .iter()
        .filter(|agent| strikes.get(*agent).copied().unwrap_or(0) >= 2)
        .cloned()
        .collect();
    let busy_map = busy_agents(&ledger.events);
    let busy: Vec<String> = busy_map.keys().cloned().collect();
    let next = orch_host::scheduler::next_candidate(&chain, &failed, &busy);

    println!("orch schedule · round={round} · task={task}");
    println!("card agent: {card_agent}");
    match attempts.last() {
        Some(cur) => println!(
            "current attempt: {} agent={} status={} dispatched={}",
            cur.attempt_id, cur.agent, cur.status, cur.dispatched_ts
        ),
        None => println!("current attempt: none (从未派发)"),
    }
    match attempts.iter().rev().nth(1) {
        Some(prev) => println!(
            "previous attempt: {} agent={} status={}",
            prev.attempt_id, prev.agent, prev.status
        ),
        None => println!("previous attempt: none"),
    }
    println!("escalation chain: {}", chain.join(" -> "));
    if strikes.is_empty() {
        println!("strikes: none");
    } else {
        let text = strikes
            .iter()
            .map(|(agent, count)| format!("{agent}={count}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!("strikes: {text}");
    }
    if failed.is_empty() {
        println!("failed (两振出局): none");
    } else {
        println!("failed (两振出局): {}", failed.join(", "));
    }
    if busy_map.is_empty() {
        println!("busy (round active attempts): none");
    } else {
        let text = busy_map
            .iter()
            .map(|(agent, tasks)| format!("{agent}({})", tasks.join("+")))
            .collect::<Vec<_>>()
            .join(", ");
        println!("busy (round active attempts): {text}");
    }
    match &next {
        Ok(candidate) => println!("next candidate: {candidate}"),
        Err(_) => println!("next candidate: ROOT-TAKEOVER (chain exhausted, never wrap around)"),
    }
    let review_text = reviews
        .iter()
        .map(|(role, agent)| format!("{role}={agent}"))
        .collect::<Vec<_>>()
        .join(", ");
    println!("review chain: {review_text}");
    Ok(ExitCode::SUCCESS)
}

fn cmd_session(root: &std::path::Path, action: SessionCmd) -> Result<ExitCode> {
    match action {
        SessionCmd::Show => {
            let registry = orch_host::wake::load_registry(root)?;
            let round = orch_host::current_round(root)?;
            println!("orch session show · {} agents", registry.len());
            for (agent, spec) in &registry {
                let status = orch_host::wake::session_status(root, &round, agent)?;
                let overlay = if let Some(overlay) = status.overlay {
                    format!(
                        "{}(faultEventId={},generation={})",
                        overlay.fault, overlay.fault_event_id, overlay.generation
                    )
                } else {
                    "none".to_string()
                };
                println!(
                    "  {:<22} configured={} effective={} overlay={} legacy={} injectable={} sessionId={}",
                    agent,
                    status.configured,
                    status.effective,
                    overlay,
                    if status.legacy { "yes" } else { "no" },
                    if spec.injectable { "yes" } else { "no" },
                    if spec.session_id.is_empty() { "（未回填）" } else { &spec.session_id },
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        SessionCmd::Set {
            agent,
            session_id,
            mode,
        } => {
            let registry = orch_host::wake::load_registry(root)?;
            let known: Vec<String> = registry.keys().cloned().collect();
            orch_host::wake::validate_agent(&known, &agent).map_err(|e| anyhow::anyhow!("{e}"))?;
            if session_id.chars().any(|ch| matches!(ch, '\n' | '\r' | '"')) {
                bail!("sessionId 含不安全字符，拒绝改写 registry");
            }
            let configured = match mode {
                Some(value) => value
                    .parse::<orch_host::wake::SessionMode>()
                    .map_err(anyhow::Error::msg)?,
                None => match session_id.as_str() {
                    "fresh-thread-per-wake"
                    | "fresh-session-per-wake"
                    | "multica-sock"
                    | "cwd-scoped--continue"
                    | "console-chat" => orch_host::wake::SessionMode::Fresh,
                    _ => orch_host::wake::SessionMode::Resume,
                },
            };
            // 用户显式配置变更：写 mode + sessionId。不会清 durable overlay；只有后续
            // 新 generation 的 exact Engaged 证据能落 SessionOverlayCleared。
            let yaml_path = root.join("coordination/agents.yaml");
            let text = fs::read_to_string(&yaml_path)
                .with_context(|| format!("读取 agents.yaml 失败: {}", yaml_path.display()))?;
            let mut in_agent_block = false;
            let mut replaced = false;
            let mut mode_replaced = false;
            let new_text: String = text
                .lines()
                .map(|line| {
                    let trimmed = line.trim_start();
                    if !trimmed.starts_with('#')
                        && line.starts_with("  ")
                        && !line.starts_with("    ")
                    {
                        // 顶层 agent 键（2 空格缩进、非 4 空格）
                        in_agent_block = trimmed.starts_with(&format!("{agent}:"));
                    }
                    if in_agent_block && trimmed.starts_with("mode:") {
                        mode_replaced = true;
                        let indent = &line[..line.len() - trimmed.len()];
                        return format!("{indent}mode: {}", configured.as_str());
                    }
                    if in_agent_block && trimmed.starts_with("sessionId:") {
                        in_agent_block = false;
                        replaced = true;
                        let indent = &line[..line.len() - trimmed.len()];
                        let session_line = format!("{indent}sessionId: \"{session_id}\"");
                        if mode_replaced {
                            session_line
                        } else {
                            mode_replaced = true;
                            format!("{indent}mode: {}\n{session_line}", configured.as_str())
                        }
                    } else {
                        line.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !replaced {
                bail!("未找到 {agent} 的 sessionId 行（agents.yaml 结构异常？）");
            }
            fs::write(&yaml_path, format!("{new_text}\n"))?;
            println!(
                "orch session set {agent}: configured={} sessionId={session_id}",
                configured
            );
            // in-flight 检查：账本投影有 Started 未闭合的任务
            let round = fs::read_to_string(root.join("coordination/runtime/CURRENT-ROUND"))
                .context("CURRENT-ROUND 缺失")?
                .trim()
                .to_string();
            let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
            if ledger_path.is_file() {
                let lr = orch_core::read_ledger(&ledger_path)?;
                let p = orch_core::fold(&lr.events);
                let inflight = p
                    .tasks
                    .iter()
                    .any(|(_, t)| t.state == Some(orch_core::TaskState::Dispatched));
                let decision = orch_host::wake::set_session_decision(inflight);
                if let Some(w) = decision.warning {
                    println!("  ⚠️ {w}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn cmd_inbox(root: &std::path::Path, action: InboxCmd) -> Result<ExitCode> {
    use orch_host::inbox::{self, InboxStage};
    use std::time::{SystemTime, UNIX_EPOCH};
    match action {
        InboxCmd::Add { instruction } => {
            active_round_contract(root)?;
            orch_host::close::with_protocol_effect(root, "orch inbox add", || {
                active_round_contract(root)?;
                let ts = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let fname = inbox::add(root, &instruction, ts)?;
                println!("orch inbox add: 已写入 {fname}");
                Ok(ExitCode::SUCCESS)
            })
        }
        InboxCmd::List => {
            let files = inbox::list_pending(root)?;
            if files.is_empty() {
                println!("orch inbox list: 无待处理指令");
            } else {
                println!("orch inbox list: {} 条待处理", files.len());
                for f in &files {
                    println!("  {f}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        InboxCmd::Done { file } => {
            active_round_contract(root)?;
            orch_host::close::with_protocol_effect(root, "orch inbox done", || {
                active_round_contract(root)?;
                inbox::advance(root, &file, InboxStage::Done)?;
                println!("orch inbox done: {file} 已移至 done");
                Ok(ExitCode::SUCCESS)
            })
        }
    }
}

fn cmd_run_wave(root: &std::path::Path, timeout_secs: u64) -> Result<ExitCode> {
    let outcome = orch_host::wave::run_wave(root, timeout_secs)?;
    println!("orch run-wave · merged {} tasks", outcome.wave.merged.len());
    for t in &outcome.wave.merged {
        println!("  ✓ {t}");
    }
    match outcome.wave.blocked_wave {
        Some(w) => println!("  ⛔ blocked at wave {w}"),
        None => println!("  ✅ all waves clean"),
    }
    if !outcome.awaiting_root.is_empty() {
        println!("  awaiting_root: {}", outcome.awaiting_root.join(", "));
    }
    Ok(ExitCode::from(orch_host::wave::root_manual_wave_exit_code(
        outcome.wave.blocked_wave.is_some(),
        outcome.awaiting_root.len(),
    )))
}

#[cfg(test)]
mod wake_control_action_contract_tests {
    use super::*;

    #[test]
    fn every_guide_visible_wake_control_action_round_trips_through_the_router() {
        for name in WakeControlAction::names() {
            assert!(
                WakeControlAction::parse(name).is_some(),
                "wake control action is documented but unroutable: {name}"
            );
        }
        assert!(WakeControlAction::parse("ordinary-agent").is_none());
    }
}

#[cfg(test)]
mod agent_pin_cli_contract_tests {
    use super::*;

    #[test]
    fn set_pin_shape_is_state_changing_and_requires_an_active_round() {
        let cli = Cli::try_parse_from([
            "orch",
            "agent",
            "set-pin",
            "executor-pi",
            "--provider",
            "one-dewu-pi-anthropic",
            "--model",
            "deepseek-v4-next",
            "--effort",
            "max",
            "--reason",
            "audited routing update",
        ])
        .unwrap();
        match &cli.cmd {
            Cmd::Agent {
                action:
                    AgentCmd::SetPin {
                        agent,
                        provider,
                        model,
                        effort,
                        reason,
                    },
            } => {
                assert_eq!(agent, "executor-pi");
                assert_eq!(provider.as_deref(), Some("one-dewu-pi-anthropic"));
                assert_eq!(model.as_deref(), Some("deepseek-v4-next"));
                assert_eq!(effort.as_deref(), Some("max"));
                assert_eq!(reason, "audited routing update");
            }
            _ => panic!("expected agent set-pin command"),
        }
        assert!(matches!(
            command_effect_policy(&cli.cmd),
            CommandEffectPolicy::RequiresActive
        ));
        assert!(!staleness_command_is_read_only(&cli.cmd));
    }

    #[test]
    fn set_pin_requires_reason_while_registry_inspection_stays_read_only() {
        assert!(Cli::try_parse_from([
            "orch",
            "agent",
            "set-pin",
            "executor-pi",
            "--model",
            "deepseek-v4-next",
        ])
        .is_err());
        for leaf in ["list", "lint"] {
            let cli = Cli::try_parse_from(["orch", "agent", leaf]).unwrap();
            assert!(matches!(
                command_effect_policy(&cli.cmd),
                CommandEffectPolicy::ReadOnly
            ));
            assert!(staleness_command_is_read_only(&cli.cmd));
        }
    }
}

#[cfg(test)]
mod action_rejection_exit_tests {
    use super::*;

    #[test]
    fn cli_exit_code_exactly_matches_durable_rejection() {
        for code in [2, 4, 5, 255] {
            let rejection = orch_host::failure::ActionRejection::new(
                "test-operation",
                "test-action",
                "test rejection",
                code,
            )
            .unwrap();
            assert_eq!(cli_error_exit_code(&rejection.into_error()), code as u8);
        }
    }

    #[test]
    fn invalid_or_plain_errors_cannot_claim_a_different_cli_exit() {
        assert!(orch_host::failure::ActionRejection::new("op", "action", "reason", 0).is_err());
        assert!(orch_host::failure::ActionRejection::new("op", "action", "reason", 256).is_err());
        assert_eq!(cli_error_exit_code(&anyhow::anyhow!("plain error")), 5);
    }

    #[test]
    fn round_close_boundary_overrides_a_rejected_leaf_and_is_production_wired() {
        let leaf = orch_host::failure::ActionRejection::new(
            "round-close",
            "close-r1",
            "leaf rejected before the later command boundary",
            2,
        )
        .unwrap()
        .into_error();
        let bound = bind_round_close_failure(leaf);
        assert_eq!(orch_host::failure::rejection_exit_code(&bound), Some(2));
        assert_eq!(cli_error_exit_code(&bound), 5);

        let source = include_str!("main.rs");
        let production = source
            .split("RoundCmd::Close { force, note } =>")
            .nth(1)
            .and_then(|tail| tail.split("\nfn cmd_check").next())
            .expect("main source contains the round-close production arm");
        assert_eq!(
            production
                .matches(".map_err(bind_round_close_failure)")
                .count(),
            1,
            "the tested wrapper must remain attached to the real round-close boundary"
        );
    }
}

#[cfg(test)]
mod review_wake_cli_tests {
    use super::*;

    #[test]
    fn review_flags_form_one_identity_group_and_explicit_deadline_is_lazy() {
        let root = std::path::Path::new("does-not-need-an-active-ir-for-explicit-values");
        assert!(
            resolve_review_request(root, "executor-claw", None, None, None, None)
                .unwrap()
                .is_none()
        );
        assert!(resolve_review_request(
            root,
            "executor-claw",
            Some("B157".to_string()),
            None,
            Some("primary".to_string()),
            None,
        )
        .is_err());
        assert!(
            resolve_review_request(root, "executor-claw", None, None, None, Some(10),).is_err()
        );

        let request = resolve_review_request(
            root,
            "executor-claw",
            Some("B157".to_string()),
            Some("B157-A0001".to_string()),
            Some("primary".to_string()),
            Some(2_345),
        )
        .unwrap()
        .unwrap();
        assert_eq!(request.task_id, "B157");
        assert_eq!(request.attempt_id, "B157-A0001");
        assert_eq!(request.role, "primary");
        assert_eq!(request.agent, "executor-claw");
        assert_eq!(request.deadline_secs, 2_345);
    }

    #[test]
    fn clap_accepts_the_documented_review_wake_shape() {
        let cli = Cli::try_parse_from([
            "orch",
            "wake",
            "executor-opencode",
            "--review-for",
            "B157",
            "--attempt",
            "B157-A0001",
            "--role",
            "secondary",
            "--deadline-secs",
            "0",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Wake {
                agent,
                review_for,
                attempt,
                role,
                deadline_secs,
                ..
            } => {
                assert_eq!(agent, "executor-opencode");
                assert_eq!(review_for.as_deref(), Some("B157"));
                assert_eq!(attempt.as_deref(), Some("B157-A0001"));
                assert_eq!(role.as_deref(), Some("secondary"));
                assert_eq!(deadline_secs, Some(0));
            }
            _ => panic!("expected wake command"),
        }
    }

    #[test]
    fn clap_accepts_the_documented_reissue_wake_shape() {
        let source_wake_id = "019fc246-1111-4222-8333-444455556666";
        assert!(Cli::try_parse_from(["orch", "wake", "executor-opencode", "--reissue",]).is_err());
        let cli = Cli::try_parse_from([
            "orch",
            "wake",
            "executor-opencode",
            "--reissue",
            source_wake_id,
            "--message",
            "repeat the same formal review with fresh context",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Wake {
                agent,
                wake_id,
                reason,
                message,
                message_file,
                reissue,
                review_for,
                attempt,
                role,
                deadline_secs,
                ..
            } => {
                assert_eq!(agent, "executor-opencode");
                assert!(wake_id.is_none());
                assert!(reason.is_none());
                assert_eq!(
                    message.as_deref(),
                    Some("repeat the same formal review with fresh context")
                );
                assert!(message_file.is_none());
                assert_eq!(reissue.as_deref(), Some(source_wake_id));
                assert!(review_for.is_none());
                assert!(attempt.is_none());
                assert!(role.is_none());
                assert!(deadline_secs.is_none());
            }
            _ => panic!("expected reissue wake command"),
        }
    }

    #[test]
    fn reissue_refuses_caller_supplied_review_identity_or_deadline() {
        let source_wake_id = "019fc246-1111-4222-8333-444455556666";
        let conflicts = [
            (Some("B246".to_string()), None, None, None),
            (None, Some("B246-A0001".to_string()), None, None),
            (None, None, Some("primary".to_string()), None),
            (None, None, None, Some(300)),
        ];
        for (review_for, attempt, role, deadline_secs) in conflicts {
            let error = cmd_wake(
                std::path::Path::new("."),
                "executor-opencode",
                None,
                None,
                None,
                None,
                Some(source_wake_id.to_string()),
                review_for,
                attempt,
                role,
                deadline_secs,
            )
            .unwrap_err()
            .to_string();
            assert!(
                error.contains("--reissue 从 source 推导 review tuple/deadline"),
                "unexpected reissue conflict: {error}"
            );
        }
    }

    #[test]
    fn reissue_keeps_positional_reason_and_message_source_conflicts_closed() {
        let source_wake_id = "019fc246-1111-4222-8333-444455556666";
        for (wake_id, reason) in [
            (Some("second-positional".to_string()), None),
            (None, Some("control-only reason".to_string())),
        ] {
            let error = cmd_wake(
                std::path::Path::new("."),
                "executor-opencode",
                wake_id,
                reason,
                None,
                None,
                Some(source_wake_id.to_string()),
                None,
                None,
                None,
                None,
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("第二 positional/--reason"), "{error}");
        }

        let error = cmd_wake(
            std::path::Path::new("."),
            "executor-opencode",
            None,
            None,
            Some("changed review prompt".to_string()),
            Some(PathBuf::from("must-not-be-read.md")),
            Some(source_wake_id.to_string()),
            None,
            None,
            None,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("消息来源互斥冲突"), "{error}");
    }

    #[test]
    fn nongate_wake_is_typed_but_formal_delivery_stays_closed() {
        let cli = Cli::try_parse_from([
            "orch",
            "wake",
            "executor-pi",
            "--review-for",
            "B207",
            "--attempt",
            "B207-A0001",
            "--role",
            "nongate",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Wake { role, .. } => assert_eq!(role.as_deref(), Some("nongate")),
            _ => panic!("expected nongate wake command"),
        }

        assert!(Cli::try_parse_from([
            "orch",
            "review",
            "deliver",
            "B207",
            "B207-A0001",
            "--role",
            "nongate",
            "--agent",
            "executor-pi",
        ])
        .is_err());

        let gc = Cli::try_parse_from(["orch", "sites", "gc", "--round", "r63"]).unwrap();
        match gc.cmd {
            Cmd::Sites {
                action: SitesCmd::Gc { round },
            } => assert_eq!(round.as_deref(), Some("r63")),
            _ => panic!("expected sites gc command"),
        }

        let sweep =
            Cli::try_parse_from(["orch", "sites", "sweep-scratch", "--ttl-hours", "12"]).unwrap();
        match sweep.cmd {
            Cmd::Sites {
                action: SitesCmd::SweepScratch { ttl_hours },
            } => assert_eq!(ttl_hours, 12),
            _ => panic!("expected sites sweep-scratch command"),
        }

        let trial_sweep = Cli::try_parse_from(["orch", "sites", "sweep-trial-cache"]).unwrap();
        assert!(matches!(
            trial_sweep.cmd,
            Cmd::Sites {
                action: SitesCmd::SweepTrialCache
            }
        ));

        let rotate = Cli::try_parse_from(["orch", "sites", "rotate-logs"]).unwrap();
        assert!(matches!(
            rotate.cmd,
            Cmd::Sites {
                action: SitesCmd::RotateLogs
            }
        ));
    }

    #[test]
    fn b191_clap_reserves_cancel_and_keeps_ordinary_wake_shape_strict() {
        let cli = Cli::try_parse_from([
            "orch",
            "wake",
            "cancel",
            "019fc001-1111-4222-8333-444455556666",
            "--reason",
            "operator stop",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Wake {
                agent,
                wake_id,
                reason,
                ..
            } => {
                assert_eq!(agent, "cancel");
                assert_eq!(
                    wake_id.as_deref(),
                    Some("019fc001-1111-4222-8333-444455556666")
                );
                assert_eq!(reason.as_deref(), Some("operator stop"));
            }
            _ => panic!("expected wake cancel command"),
        }

        assert!(cmd_wake_cancel(
            std::path::Path::new("."),
            Some("019fc001-1111-4222-8333-444455556666"),
            Some("operator stop"),
            true,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap_err()
        .to_string()
        .contains("禁止"));
        assert!(cmd_wake(
            std::path::Path::new("."),
            "executor-opencode",
            Some("019fc001-1111-4222-8333-444455556666".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap_err()
        .to_string()
        .contains("cancel-only"));
    }

    #[test]
    fn b192_clap_reserves_status_attach_and_declare_dead_actions() {
        let wake_id = "019fc192-1111-4222-8333-444455556666";
        for argv in [
            vec!["orch", "wake", "status", wake_id, "--json"],
            vec![
                "orch",
                "wake",
                "attach",
                wake_id,
                "--mode",
                "resume",
                "--reason",
                "operator-confirmed session death",
                "--deadline-secs",
                "300",
            ],
            vec!["orch", "wake", "declare-dead", wake_id],
        ] {
            let cli = Cli::try_parse_from(argv).unwrap();
            match cli.cmd {
                Cmd::Wake {
                    agent,
                    wake_id: parsed,
                    ..
                } => {
                    assert!(matches!(
                        agent.as_str(),
                        "status" | "attach" | "declare-dead"
                    ));
                    assert_eq!(parsed.as_deref(), Some(wake_id));
                }
                _ => panic!("expected reserved wake action"),
            }
        }
        assert!(Cli::try_parse_from([
            "orch",
            "wake",
            "attach",
            wake_id,
            "--mode",
            "automatic",
            "--reason",
            "must fail closed",
        ])
        .is_err());
    }

    #[test]
    fn b191_cancel_cli_has_no_process_signal_authority() {
        let source = include_str!("main.rs");
        let start = source.find("fn cmd_wake_cancel(").unwrap();
        let end = source[start..].find("\nfn cmd_wake(").unwrap() + start;
        let cancel_body = &source[start..end];
        assert!(cancel_body.contains("request_managed_wake_cancel"));
        for forbidden in [
            "libc::kill",
            ".kill()",
            "Command::new(\"/bin/kill\")",
            "terminate_revalidated_group",
            "TerminationSignal",
        ] {
            assert!(
                !cancel_body.contains(forbidden),
                "cancel CLI acquired forbidden signal authority: {forbidden}"
            );
        }
    }
}

#[cfg(test)]
mod await_report_cli_tests {
    use super::*;

    #[test]
    fn await_report_distinguishes_omitted_timeout_from_explicit_value() {
        let omitted = Cli::try_parse_from(["orch", "await-report", "B198"]).unwrap();
        match omitted.cmd {
            Cmd::AwaitReport { timeout_secs, .. } => assert_eq!(timeout_secs, None),
            _ => panic!("expected await-report command"),
        }

        let explicit =
            Cli::try_parse_from(["orch", "await-report", "B198", "--timeout-secs", "0"]).unwrap();
        match explicit.cmd {
            Cmd::AwaitReport { timeout_secs, .. } => assert_eq!(timeout_secs, Some(0)),
            _ => panic!("expected await-report command"),
        }
    }
}

#[cfg(test)]
mod record_cli_tests {
    use super::*;

    #[test]
    fn record_at_tip_is_explicit_and_default_stays_fixed() {
        let default = Cli::try_parse_from(["orch", "record", "B158"]).unwrap();
        match default.cmd {
            Cmd::Record { task, at_tip } => {
                assert_eq!(task, "B158");
                assert!(!at_tip, "default record must stay pinned to merge SHA");
            }
            _ => panic!("expected record command"),
        }

        let relaxed = Cli::try_parse_from(["orch", "record", "B158", "--at-tip"]).unwrap();
        match relaxed.cmd {
            Cmd::Record { task, at_tip } => {
                assert_eq!(task, "B158");
                assert!(at_tip, "--at-tip must be an explicit signal");
            }
            _ => panic!("expected record command"),
        }
    }
}

#[cfg(test)]
mod ledger_recover_cli_tests {
    use super::*;

    #[test]
    fn clap_defaults_to_current_round_and_requires_explicit_apply() {
        let dry = Cli::try_parse_from(["orch", "ledger", "recover"]).unwrap();
        match dry.cmd {
            Cmd::Ledger {
                action: LedgerCmd::Recover { round, apply },
            } => {
                assert!(round.is_none());
                assert!(!apply);
            }
            _ => panic!("expected ledger recover command"),
        }

        let apply = Cli::try_parse_from(["orch", "ledger", "recover", "--round", "r55", "--apply"])
            .unwrap();
        match apply.cmd {
            Cmd::Ledger {
                action: LedgerCmd::Recover { round, apply },
            } => {
                assert_eq!(round.as_deref(), Some("r55"));
                assert!(apply);
            }
            _ => panic!("expected ledger recover command"),
        }
    }
}

#[cfg(test)]
mod wal_doctor_tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|path| path.parent())
            .unwrap()
            .join("target/test-tmp")
            .join(format!("b152-wal-doctor-{name}-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root).unwrap();
        }
        fs::create_dir_all(root.join("coordination/runtime/ledger-wal")).unwrap();
        fs::create_dir_all(root.join("coordination/rounds/rWal")).unwrap();
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "rWal\n").unwrap();
        root
    }

    #[test]
    fn historical_round_without_wal_is_skipped() {
        let root = temp_root("missing");
        fs::write(
            root.join("coordination/rounds/rWal/events.jsonl"),
            "{\"eventId\":\"e1\",\"type\":\"RoundOpened\"}\n",
        )
        .unwrap();
        let (_, status, detail) = wal_doctor_check(&root).unwrap();
        assert_eq!(status, CheckStatus::Pass);
        assert!(detail.contains("无 WAL 基线") && detail.contains("跳过"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn truncation_reports_count_event_summary_and_recovery_hint() {
        let root = temp_root("truncated");
        let first = r#"{"eventId":"e1","type":"RoundOpened"}"#;
        let missing = r#"{"eventId":"e2","type":"VerdictIssued","taskId":"B152"}"#;
        fs::write(
            root.join("coordination/rounds/rWal/events.jsonl"),
            format!("{first}\n"),
        )
        .unwrap();
        fs::write(
            root.join("coordination/runtime/ledger-wal/rWal.jsonl"),
            format!("{first}\n{missing}\n"),
        )
        .unwrap();
        let (_, status, detail) = wal_doctor_check(&root).unwrap();
        assert_eq!(status, CheckStatus::Fail);
        assert!(detail.contains("少 1 条事件"), "{detail}");
        assert!(
            detail.contains("type=VerdictIssued/taskId=B152/eventId=e2"),
            "{detail}"
        );
        assert!(
            detail.contains("恢复指引") && detail.contains("不自动写回"),
            "{detail}"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn ledger_only_suffix_is_reported_as_divergence() {
        let root = temp_root("diverged");
        let first = r#"{"eventId":"e1","type":"RoundOpened"}"#;
        let forged = r#"{"eventId":"forged","type":"TaskRecorded"}"#;
        fs::write(
            root.join("coordination/rounds/rWal/events.jsonl"),
            format!("{first}\n{forged}\n"),
        )
        .unwrap();
        fs::write(
            root.join("coordination/runtime/ledger-wal/rWal.jsonl"),
            format!("{first}\n"),
        )
        .unwrap();
        let (_, status, detail) = wal_doctor_check(&root).unwrap();
        assert_eq!(status, CheckStatus::Fail);
        assert!(
            detail.contains("第 2 行起") && detail.contains("手工编造"),
            "{detail}"
        );
        fs::remove_dir_all(root).ok();
    }
}
