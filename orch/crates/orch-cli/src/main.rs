//! orch · 多 Agent 协同运行时 CLI（M1 首切片）
//! 纪律（errata E6/E7）：启动即把 --root 规范化为绝对路径；每个子命令单一职责。

mod guide;

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
#[cfg(feature = "selfhost")]
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
#[cfg(feature = "selfhost")]
use orch_core::{doctor, fold, read_ledger, CheckStatus, TaskState};

#[derive(Parser)]
#[command(
    name = "orch",
    version,
    about = "Harness invocation and explicit consultation",
    disable_help_subcommand = true
)]
struct Cli {
    /// 目标仓库根（将被规范化为绝对路径——errata E6）
    #[arg(long, default_value = ".")]
    root: PathBuf,
    /// 显式逃生舱：允许陈旧二进制执行状态变更命令（会在 stderr 留证）
    #[cfg(feature = "selfhost")]
    #[arg(long, global = true)]
    allow_stale_binary: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
#[command(disable_help_subcommand = true)]
enum Cmd {
    /// Internal one-shot detached provider owner. Launch spec is inherited on stdin.
    #[command(name = "__wake-supervise", hide = true)]
    WakeSupervise,
    /// 检查 Git 与本机 harness 配置；selfhost 项目另核账本与运行环境
    Doctor,
    #[cfg(feature = "selfhost")]
    /// 只读停滞体检：账本 + git + 进程 + wake 日志 → 确定性介入判定
    #[command(name = "stall-check")]
    StallCheck,
    #[cfg(feature = "selfhost")]
    /// WAL 对账恢复：缺省干跑，只有 strict-prefix 账本允许逐字节回灌
    Ledger {
        #[command(subcommand)]
        action: LedgerCmd,
    },
    #[cfg(feature = "selfhost")]
    /// Deliver one exact schema-3 generic review artifact.
    Review {
        #[command(subcommand)]
        action: ReviewCmd,
    },
    #[cfg(feature = "selfhost")]
    /// Lease-gated review-site lifecycle operations.
    Sites {
        #[command(subcommand)]
        action: SitesCmd,
    },
    /// Inspect the git-ignored local harness configuration without model calls.
    Harness {
        #[command(subcommand)]
        action: HarnessCmd,
    },
    #[cfg(feature = "selfhost")]
    /// 折叠当前轮事件账本为任务状态投影（事件是事实，状态是投影）
    Status,
    /// 输出编译进二进制的 AI 机械契约指南（零项目依赖、零写盘）
    Guide {
        /// 只输出一个稳定章节
        #[arg(long, conflicts_with = "check")]
        section: Option<String>,
        /// 将指南标记与当前公开 CLI 命令树做精确覆盖检查
        #[arg(long)]
        check: bool,
    },
    #[cfg(feature = "selfhost")]
    /// 独立机检（不 spawn agent）：域/提交形状/种子 SHA——故障注入验证用
    Check { task: String },
    #[cfg(feature = "selfhost")]
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
        /// Quarantine exact accepted reviews only with BLOCKED; native scopes stay held.
        #[arg(long)]
        quarantine_review: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
    #[cfg(feature = "selfhost")]
    /// One-command normal close: capture main -> root PASS -> merge -> record -> prove postconditions
    Seal {
        task: String,
        #[arg(long)]
        attempt: String,
        #[arg(long)]
        expected_head: String,
    },
    #[cfg(feature = "selfhost")]
    /// 快照（B102 接线）：采样心跳/GO/worktree → 纯聚合 OrchSnapshot；只读，不写账本
    Snapshot {
        /// 输出完整 JSON（默认打人读摘要）
        #[arg(long)]
        json: bool,
        /// 显式将同一只读投影原子保存到 coordination/runtime/snapshot.json
        #[arg(long)]
        write: bool,
    },
    #[cfg(feature = "selfhost")]
    /// schema 3 默认本地派发：建立独立 branch/worktree/GO，不唤醒 provider
    Dispatch {
        task: String,
        /// Execute in a dedicated local planner worktree (schema 3).
        #[arg(long, conflicts_with = "harness")]
        local: bool,
        /// Execute through one configured harness alias.
        #[arg(long, conflicts_with = "local")]
        harness: Option<String>,
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
    #[cfg(feature = "selfhost")]
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
    #[cfg(feature = "selfhost")]
    /// 轮生命周期：open / sign-off / seed-verified / close（消除人肉开轮收轮）
    Round {
        #[command(subcommand)]
        action: RoundCmd,
    },
    /// 按本机配置显式发起一个受管 harness 调用
    Wake {
        #[arg(value_name = "HARNESS")]
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
        /// 将本次真实 wake 记为指定 task 的审查请求
        #[arg(long)]
        review_for: Option<String>,
        /// 被审查的 attempt identity；schema 3 与 --review-for 成组并固定 role=review
        #[arg(long)]
        attempt: Option<String>,
        /// 本次调用总硬期限秒数；显式正值优先于 action 配置，仍受总硬上限约束
        #[arg(long)]
        deadline_secs: Option<u64>,
    },
    #[cfg(feature = "selfhost")]
    /// 成本报表 v1（决策 12）：折叠账本 verifier $/门耗时/agent 时长/tokens/升级次数
    Cost {
        /// 指定轮（默认当前轮）
        #[arg(long)]
        round: Option<String>,
    },
    #[cfg(feature = "selfhost")]
    /// 将 schema 3 binding/cards/seeds 编译为 actorless ROUND-IR，不读取 roster/ModeConfig
    Plan,
    /// 并发咨询显式选定的 harness，并保存各自答卷
    Consult {
        /// 包含咨询问题的 UTF-8 文件
        question: PathBuf,
        /// 当次咨询成员（可重复；保持显式顺序且拒绝重复）
        #[arg(long = "harness", required = true, value_name = "ALIAS")]
        harness: Vec<String>,
        /// 仓内 UTF-8 附件（可重复）
        #[arg(long = "attach")]
        attach: Vec<PathBuf>,
        /// 覆盖 code-owned 单成员超时
        #[arg(long)]
        member_timeout_secs: Option<u64>,
        /// 覆盖 code-owned 整单墙钟上限
        #[arg(long)]
        total_wall_secs: Option<u64>,
    },
    #[cfg(feature = "selfhost")]
    /// B151 只读派生：从账本+git 生成 coordination/CURRENT.md（活轮/main SHA/任务投影；幂等覆写单文件）
    Current,
}
#[cfg(feature = "selfhost")]
#[derive(Subcommand)]
#[command(disable_help_subcommand = true)]
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
#[cfg(feature = "selfhost")]
#[derive(Subcommand)]
#[command(disable_help_subcommand = true)]
#[command(disable_help_subcommand = true)]
enum ReviewCmd {
    /// Install one exact schema-3 generic review artifact before verdict.
    Deliver {
        task: String,
        attempt: String,
        /// Dynamic schema-3 harness alias.
        #[arg(long)]
        harness: String,
        /// Exact wake identity bound to the generic review request.
        #[arg(long = "wake-id")]
        wake_id: String,
    },
}
#[cfg(feature = "selfhost")]
#[derive(Subcommand)]
#[command(disable_help_subcommand = true)]
enum SitesCmd {
    /// Run or inspect exclusively owned local Cargo diagnostic caches.
    Cache {
        #[command(subcommand)]
        action: DiagnosticCmd,
    },
    /// Maintain registered caches and released sites across rounds, including closed rounds.
    Gc {
        /// Optional legacy-round filter; all rounds still protect overlapping ownership.
        #[arg(long)]
        round: Option<String>,
        /// Read-only full eligibility snapshot; create no locks, receipts, or ledger/WAL writes.
        #[arg(long)]
        dry_run: bool,
    },
    /// Inspect legacy scratch ownership; names and age do not authorize deletion.
    SweepScratch {
        /// Entries newer than this many hours are preserved.
        #[arg(long, default_value = "24")]
        ttl_hours: u64,
    },
    /// Inspect legacy shared trial caches without adopting unregistered generations.
    SweepTrialCache,
    /// Remove stale review/consult target directories without touching debug or active leases.
    SweepTargets {
        /// Entries newer than this many hours are preserved.
        #[arg(long, default_value = "24")]
        ttl_hours: u64,
    },
}

#[cfg(feature = "selfhost")]
#[derive(Subcommand)]
#[command(disable_help_subcommand = true)]
enum DiagnosticCmd {
    /// Run a fixed-HEAD Cargo diagnostic with a private disposable target and retained logs.
    Run {
        /// Absolute clean primary or linked worktree root inside this project.
        #[arg(long)]
        cwd: PathBuf,
        /// Nonempty single-line purpose retained with the invocation.
        #[arg(long)]
        purpose: String,
        /// Preserve the cache with an explicit debug marker; other guards still apply.
        #[arg(long)]
        keep_cache: bool,
        /// Exact absolute Cargo executable and arguments after --.
        #[arg(last = true, required = true, num_args = 1..)]
        command: Vec<String>,
    },
    /// Read cache identities, retained evidence and eligibility without writing files.
    Status {
        /// Read the last persisted maintenance report instead of current cache observations.
        #[arg(long)]
        last_maintenance: bool,
    },
    /// Retry quiescent caches with durable child-exit proof, preserving unknown or debug state.
    Sweep,
}

#[derive(Subcommand)]
#[command(disable_help_subcommand = true)]
enum HarnessCmd {
    /// List every alias with a token-free supported/unsupported/unknown status.
    List {
        /// Resolve availability for one action without starting any provider.
        #[arg(long, value_enum)]
        action: Option<HarnessActionArg>,
    },
    /// Validate the exact schema and require every configured alias to be usable.
    Lint,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum HarnessActionArg {
    Execute,
    Review,
    Consult,
}

impl HarnessActionArg {
    fn into_host(self) -> orch_host::harness_config::HarnessAction {
        use orch_host::harness_config::HarnessAction;
        match self {
            Self::Execute => HarnessAction::Execute,
            Self::Review => HarnessAction::Review,
            Self::Consult => HarnessAction::Consult,
        }
    }
}
#[cfg(feature = "selfhost")]
#[derive(Subcommand)]
#[command(disable_help_subcommand = true)]
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
#[cfg(feature = "selfhost")]

/// Round close may have already reconciled receipts, archived a snapshot, or
/// reclaimed storage before a later guard rejects. Bind that command boundary
/// conservatively so a nested zero-effect leaf cannot misdescribe the whole run.
fn bind_round_close_failure(error: anyhow::Error) -> anyhow::Error {
    orch_host::failure::with_cli_disposition(
        error,
        orch_host::failure::CliDisposition::EffectUnknown,
    )
}
#[cfg(feature = "selfhost")]

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
    if fold(&ledger.events).round_closed {
        bail!("round {round} 已关闭；写入口只允许另开 schema 3 新轮");
    }
    let active = orch_host::plan::require_active_round_ir(root, &round, &ledger.events)?;
    Ok((round, active))
}
#[cfg(feature = "selfhost")]

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
    if fold(&ledger.events).round_closed {
        bail!("round {round} 已关闭；写入口只允许另开 schema 3 新轮");
    }
    let validated = orch_host::plan::require_validated_round_ir(root, &round, &ledger.events)?;
    Ok((round, validated))
}
#[cfg(feature = "selfhost")]
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
}
#[cfg(feature = "selfhost")]

fn enforce_actorless_write_generation(root: &std::path::Path, command: &Cmd) -> Result<()> {
    if matches!(
        command_effect_policy(command),
        CommandEffectPolicy::ReadOnly
            | CommandEffectPolicy::BootstrapOnly
            | CommandEffectPolicy::RecoveryOnly
    ) {
        return Ok(());
    }
    let round = orch_host::current_round(root)?;
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let ledger = read_ledger(&ledger_path).with_context(|| {
        format!(
            "写入口代际 preflight 读取账本失败: {}",
            ledger_path.display()
        )
    })?;
    if !ledger.bad_lines.is_empty() {
        bail!("写入口代际 preflight 拒绝坏账本");
    }
    if fold(&ledger.events).round_closed {
        bail!("round {round} 已关闭；写入口只允许另开 schema 3 新轮");
    }
    let generation = orch_host::round::contract_schema_from_events(&ledger.events, &round)?;
    if generation != Some(orch_host::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION) {
        bail!("schema 1/2 active round 只读兼容；拒绝 legacy writer");
    }
    Ok(())
}
#[cfg(feature = "selfhost")]

fn command_effect_policy(command: &Cmd) -> CommandEffectPolicy {
    use CommandEffectPolicy::*;
    match command {
        Cmd::Sites {
            action:
                SitesCmd::Cache {
                    action: DiagnosticCmd::Status { .. },
                },
        } => ReadOnly,
        Cmd::Sites {
            action:
                SitesCmd::Gc { dry_run: true, .. }
                | SitesCmd::SweepScratch { .. }
                | SitesCmd::SweepTrialCache,
        } => ReadOnly,
        Cmd::Sites { .. } => RecoveryOnly,
        Cmd::WakeSupervise
        | Cmd::Doctor
        | Cmd::StallCheck
        | Cmd::Status
        | Cmd::Guide { .. }
        | Cmd::Check { .. }
        | Cmd::Cost { .. }
        | Cmd::Harness {
            action: HarnessCmd::List { .. } | HarnessCmd::Lint,
        }
        | Cmd::Current
        | Cmd::Snapshot { write: false, .. }
        | Cmd::Ledger {
            action: LedgerCmd::Recover { apply: false, .. },
        } => ReadOnly,
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
        Cmd::Ledger {
            action: LedgerCmd::Recover { apply: true, .. },
        } => RecoveryOnly,
        Cmd::Review { .. }
        | Cmd::Verdict { .. }
        | Cmd::Seal { .. }
        | Cmd::Snapshot { .. }
        | Cmd::Dispatch { .. }
        | Cmd::AwaitReport { .. }
        | Cmd::Wake { .. }
        | Cmd::Round {
            action: RoundCmd::Close { .. },
        } => RequiresActive,
    }
}

/// Foreground pre-close maintenance; physical outcomes cannot roll back round/task facts.
#[cfg(feature = "selfhost")]

fn prepare_round_close_cleanup(root: &std::path::Path) -> Vec<String> {
    match orch_host::reclaim::maintain_storage(root, false) {
        Ok(report) => {
            let lines = orch_host::reclaim::maintenance_summary(&report);
            for line in &lines {
                println!("{line}");
            }
            lines
        }
        Err(error) => {
            let line = format!("storage maintenance failed; round facts are unchanged: {error:#}");
            eprintln!("{line}");
            vec![line]
        }
    }
}

#[cfg(feature = "selfhost")]
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
        orch_host::ledger::append(
            &root,
            "rT",
            &[orch_host::ledger::event(
                "RoundOpened",
                "runtime:orch",
                None,
                Some("rT"),
                serde_json::json!({
                    "contractSchemaVersion": orch_host::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION,
                    "purpose": "round-close GC fixture",
                }),
            )],
        )
        .unwrap();
        fs::create_dir_all(root.join(".worktrees")).unwrap();
        fs::create_dir_all(root.join("orch/target")).unwrap();
        let head = orch_host::gitx::rev_parse(&root, "HEAD").unwrap();
        let provision = |task: &str, attempt: &str, agent: &str, wake: &str| {
            orch_host::sites::lease_review_site_with(
                &root,
                "rT",
                task,
                attempt,
                orch_host::sites::SiteRole::Review,
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
            let output = root.join(format!(
                "coordination/runtime/review-inbox/rT/{}.md",
                site.site_id
            ));
            fs::create_dir_all(output.parent().unwrap()).unwrap();
            fs::write(&output, b"preserved native answer").unwrap();
            let terminal = orch_host::ledger::event(
                "ManagedWakeTerminated",
                "runtime:orch",
                Some(&site.task_id),
                Some("rT"),
                serde_json::json!({"agent":site.agent,"wakeId":site.wake_id,"managedScopeTerminated":true,
                    "turnEnded":true,"state":"answered","mechanicalTerminalAbsent":false,"channelBinding":{"fixedHead":head},
                    "outputPath":output,"outputSha256":"9d0c78c187662f65395450f5d4b1827f4b6ce9a919a9e2fd7f7f92f9fe8c4fc9"}),
            );
            let release = orch_host::ledger::event(
                "WorkspaceReleased",
                "runtime:orch",
                Some(&site.task_id),
                Some("rT"),
                serde_json::json!({"siteId":site.site_id,"generation":site.generation,"attemptId":site.attempt_id,
                    "role":site.role.as_str(),"agent":site.agent,"wakeId":site.wake_id,
                    "completionReceipt":orch_host::sites::MANAGED_COMPLETION_RECEIPT,"terminationEventId":terminal.event_id}),
            );
            orch_host::ledger::append(&root, "rT", &[terminal, release]).unwrap();
        }
        fs::write(root.join(&fresh.target).join("fresh-artifact"), b"fresh").unwrap();
        fs::write(root.join(&dirty.worktree).join("tracked.txt"), "dirty\n").unwrap();

        // Exercise the actual CLI close handler. The fixture deliberately has no
        // validated task IR, so closure refuses after its independent maintenance.
        assert!(cmd_round(
            &root,
            RoundCmd::Close {
                force: false,
                note: Some("fixture".into())
            }
        )
        .is_err());
        let report = orch_host::reclaim::latest_maintenance_report(&root)
            .unwrap()
            .unwrap();
        let summary = orch_host::reclaim::maintenance_summary(&report);
        assert!(!root.join(&fresh.worktree).exists());
        assert!(!root.join(&fresh.target).exists());
        assert!(root.join(&dirty.worktree).exists());
        assert!(root.join(&dirty.target).exists());
        assert!(summary.iter().any(|line| line.contains("removed=1")
            && line.contains("held=")
            && line.contains("logicalDeletedBytes=")));
        assert!(summary.iter().any(|line| {
            line.contains(&dirty.site_id)
                && line.contains("held")
                && line.contains("tracked/staged")
        }));

        fs::write(root.join(&dirty.worktree).join("tracked.txt"), "baseline\n").unwrap();
        orch_host::gitx::worktree_remove(&root, &root.join(&dirty.worktree)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
#[cfg(feature = "selfhost")]

fn compiled_build_stamp() -> orch_host::staleness::BuildStamp {
    orch_host::staleness::BuildStamp {
        commit: option_env!("ORCH_BUILD_GIT_SHA")
            .map(str::trim)
            .filter(|sha| !sha.is_empty())
            .map(str::to_owned),
    }
}
#[cfg(feature = "selfhost")]

/// Exact read-only classification for the stale-binary guard.
///
/// This intentionally does not change `CommandEffectPolicy`: preflight groups
/// commands by the round/IR contract they require, while staleness must inspect
/// value-level flags that decide whether a particular invocation writes.
fn staleness_command_is_read_only(command: &Cmd) -> bool {
    match command {
        Cmd::Sites {
            action:
                SitesCmd::Cache {
                    action: DiagnosticCmd::Status { .. },
                },
        } => true,
        Cmd::Sites {
            action: SitesCmd::Gc { dry_run, .. },
        } => *dry_run,
        Cmd::Sites {
            action: SitesCmd::SweepScratch { .. } | SitesCmd::SweepTrialCache,
        } => true,
        Cmd::WakeSupervise => true,
        Cmd::Ledger {
            action: LedgerCmd::Recover { apply, .. },
        } => !apply,
        Cmd::Snapshot { write, .. } => !write,
        Cmd::Doctor
        | Cmd::StallCheck
        | Cmd::Status
        | Cmd::Guide { .. }
        | Cmd::Check { .. }
        | Cmd::Cost { .. }
        | Cmd::Current
        | Cmd::Harness { .. } => true,
        _ => false,
    }
}
#[cfg(feature = "selfhost")]

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

    if !orch_host::has_selfhost_state(root)? {
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
#[cfg(feature = "selfhost")]

fn command_task(command: &Cmd) -> Option<&str> {
    match command {
        Cmd::Review {
            action: ReviewCmd::Deliver { task, .. },
        }
        | Cmd::Check { task }
        | Cmd::Verdict { task, .. }
        | Cmd::Seal { task, .. }
        | Cmd::Dispatch { task, .. }
        | Cmd::AwaitReport { task, .. }
        | Cmd::Round {
            action: RoundCmd::SeedVerified { task, .. },
        } => Some(task),
        Cmd::WakeSupervise
        | Cmd::Doctor
        | Cmd::StallCheck
        | Cmd::Ledger { .. }
        | Cmd::Sites { .. }
        | Cmd::Harness { .. }
        | Cmd::Status
        | Cmd::Guide { .. }
        | Cmd::Snapshot { .. }
        | Cmd::Round { .. }
        | Cmd::Wake { .. }
        | Cmd::Cost { .. }
        | Cmd::Plan
        | Cmd::Consult { .. }
        | Cmd::Current => None,
    }
}
#[cfg(feature = "selfhost")]

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
#[cfg(feature = "selfhost")]

fn preflight_cli_command(root: &std::path::Path, command: &Cmd) -> Result<()> {
    if !orch_host::has_selfhost_state(root)?
        && matches!(command, Cmd::Wake { .. } | Cmd::Consult { .. })
    {
        return validate_standalone_flags(command);
    }
    if let Cmd::Verdict {
        attempt,
        expected_head,
        expected_main,
        verdict,
        reason,
        quarantine_review,
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
        if !quarantine_review.is_empty() {
            if verdict != "blocked" {
                bail!("--quarantine-review requires --verdict blocked");
            }
            let unique = quarantine_review.iter().collect::<std::collections::BTreeSet<_>>();
            if unique.len() != quarantine_review.len()
                || quarantine_review.iter().any(|s| s.is_empty()
                    || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
            { bail!("--quarantine-review requires canonical unique wake ids"); }
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
        action:
            ReviewCmd::Deliver {
                task,
                attempt,
                harness,
                ..
            },
    } = command
    {
        orch_host::wake::validate_review_reconcile_attempt(task, attempt)?;
        if harness.trim().is_empty()
            || !harness
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            bail!("--harness 必须是安全的非空 identity component");
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
    if let Some(task) = command_task(command) {
        orch_host::card::validate_task_id(task)?;
    }
    enforce_actorless_write_generation(root, command)?;
    let active = match command_effect_policy(command) {
        CommandEffectPolicy::ReadOnly
        | CommandEffectPolicy::PlanMutation
        | CommandEffectPolicy::PlanConsultation
        | CommandEffectPolicy::Signoff => None,
        CommandEffectPolicy::BootstrapOnly => None,
        CommandEffectPolicy::RequiresValidated => Some(validated_round_contract(root)?.1),
        CommandEffectPolicy::RequiresActive => Some(active_round_contract(root)?.1),
        CommandEffectPolicy::RecoveryOnly => None,
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
        return orch_host::channel::run_wake_supervisor_from_stdin(
            &root,
            cfg!(feature = "selfhost"),
        )
        .map(|_| ExitCode::SUCCESS)
        .unwrap_or_else(|error| {
            let code = cli_error_exit_code(&error);
            eprintln!("orch: wake supervisor failed: {error:#}");
            ExitCode::from(code)
        });
    }
    if let Err(error) = enforce_build_project_mode(&root, &cli.cmd) {
        eprintln!("orch: {error:#}");
        return ExitCode::from(cli_error_exit_code(&error));
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
        review_for,
        attempt,
        deadline_secs,
    } = &cli.cmd
    {
        if let Some(action) = WakeControlAction::parse(agent) {
            let result = match action {
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
                    deadline_secs.is_some(),
                ),
            };
            return result.unwrap_or_else(|error| {
                let code = cli_error_exit_code(&error);
                eprintln!("orch: {error:#}");
                ExitCode::from(code)
            });
        }
    }
    #[cfg(feature = "selfhost")]
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
        Cmd::Doctor => cmd_doctor(&root),
        #[cfg(feature = "selfhost")]
        Cmd::StallCheck => cmd_stall_check(&root),
        #[cfg(feature = "selfhost")]
        Cmd::Ledger { action } => cmd_ledger(&root, action),
        #[cfg(feature = "selfhost")]
        Cmd::Review { action } => cmd_review(&root, action),
        #[cfg(feature = "selfhost")]
        Cmd::Sites { action } => cmd_sites(&root, action),
        Cmd::Harness { action } => cmd_harness(&root, action),
        #[cfg(feature = "selfhost")]
        Cmd::Status => cmd_status(&root),
        Cmd::Guide { section, check } => cmd_guide(section.as_deref(), check),
        #[cfg(feature = "selfhost")]
        Cmd::Check { task } => cmd_check(&root, &task),
        #[cfg(feature = "selfhost")]
        Cmd::Verdict {
            task,
            attempt,
            expected_head,
            expected_main,
            verdict,
            reason,
            quarantine_review,
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
            &quarantine_review,
        ),
        #[cfg(feature = "selfhost")]
        Cmd::Seal {
            task,
            attempt,
            expected_head,
        } => cmd_seal(&root, &task, &attempt, &expected_head),
        #[cfg(feature = "selfhost")]
        Cmd::Snapshot { json, write } => cmd_snapshot(&root, json, write),
        #[cfg(feature = "selfhost")]
        Cmd::Dispatch {
            task,
            local,
            harness,
            no_wake,
            override_ambiguous_active,
            new_attempt,
            reason,
        } => cmd_dispatch(
            &root,
            &task,
            local,
            harness.as_deref(),
            no_wake,
            override_ambiguous_active,
            new_attempt,
            reason.as_deref(),
        ),
        #[cfg(feature = "selfhost")]
        Cmd::AwaitReport {
            task,
            timeout_secs,
            liveness_grace_secs,
            no_liveness,
        } => cmd_await(&root, &task, timeout_secs, liveness_grace_secs, no_liveness),
        #[cfg(feature = "selfhost")]
        Cmd::Round { action } => cmd_round(&root, action),
        Cmd::Wake {
            agent,
            wake_id,
            reason,
            mode,
            json,
            message,
            message_file,
            review_for,
            attempt,
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
                    review_for,
                    attempt,
                    deadline_secs,
                )
            }
        }
        #[cfg(feature = "selfhost")]
        Cmd::Cost { round } => cmd_cost(&root, round),
        #[cfg(feature = "selfhost")]
        Cmd::Plan => cmd_plan(&root),
        Cmd::Consult {
            question,
            harness,
            attach,
            member_timeout_secs,
            total_wall_secs,
        } => cmd_consult(
            &root,
            question,
            harness,
            attach,
            member_timeout_secs,
            total_wall_secs,
        ),
        #[cfg(feature = "selfhost")]
        Cmd::Current => cmd_current(&root),
    };
    result.unwrap_or_else(|e| {
        // Typed command disposition wins over a nested ActionRejection; without
        // either, the only honest default is EffectUnknown(5).
        let code = cli_error_exit_code(&e);
        eprintln!("orch: {e:#}");
        ExitCode::from(code)
    })
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
#[cfg(feature = "selfhost")]

fn display_items(items: &[String]) -> String {
    if items.is_empty() {
        "—".to_string()
    } else {
        items.join(", ")
    }
}
#[cfg(feature = "selfhost")]

fn cmd_selfhost_doctor(root: &std::path::Path) -> Result<ExitCode> {
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
#[cfg(feature = "selfhost")]

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
#[cfg(feature = "selfhost")]

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
#[cfg(feature = "selfhost")]

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
#[cfg(feature = "selfhost")]

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
#[cfg(feature = "selfhost")]

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
#[cfg(feature = "selfhost")]

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

fn cmd_harness(root: &std::path::Path, action: HarnessCmd) -> Result<ExitCode> {
    let snapshot = orch_host::harness_config::load_harness_config_snapshot(root)?;
    match action {
        HarnessCmd::List { action } => {
            println!(
                "orch harness list · config={} · sha256={}",
                snapshot.source_path().display(),
                snapshot.sha256()
            );
            println!("alias\tdriver\tstatus\treason");
            let rows = match action {
                Some(action) => snapshot.discover_for_action(action.into_host()),
                None => snapshot.discover_without_tokens(),
            };
            for row in rows {
                println!(
                    "{}\t{}\t{}\t{}",
                    row.alias(),
                    row.driver(),
                    row.availability().label(),
                    row.availability().reason().unwrap_or("—")
                );
            }
        }
        HarnessCmd::Lint => {
            snapshot.lint_without_tokens()?;
            println!(
                "orch harness lint · ok · config={} · sha256={} · aliases={}",
                snapshot.source_path().display(),
                snapshot.sha256(),
                snapshot.discover_without_tokens().len()
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}
#[cfg(feature = "selfhost")]

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
#[cfg(feature = "selfhost")]

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

    // Harness availability is local operational state, not a signed roster.
    let harnesses = load_current_harnesses(root)?;
    let mut harness_lines = String::new();
    for row in harnesses {
        harness_lines.push_str(&format!(
            "| `{}` | `{}` | `{}` | {} |\n",
            row.alias, row.driver, row.status, row.reason,
        ));
    }
    if harness_lines.is_empty() {
        harness_lines.push_str("_(未配置本地 harness；任务授权仍以 actorless IR 为准)_\n");
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
         ## Harness 通道（本地动态状态）\n\n\
         | alias | driver | status | reason |\n\
         |---|---|---|---|\n\
         {harness_lines}\n",
        now = now,
        round = round,
        main_short = main_short,
        main_full = main_full,
        signed = if active_plan_signed_off { "yes" } else { "no" },
        closed = if projection.round_closed { "yes" } else { "no" },
        events = projection.total_events,
        bad = ledger.bad_lines.len(),
        task_lines = task_lines,
        harness_lines = harness_lines,
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
#[cfg(feature = "selfhost")]
#[derive(Debug)]
struct CurrentHarnessRow {
    alias: String,
    driver: String,
    status: &'static str,
    reason: String,
}
#[cfg(feature = "selfhost")]

fn load_current_harnesses(root: &std::path::Path) -> Result<Vec<CurrentHarnessRow>> {
    let snapshot = match orch_host::harness_config::load_harness_config_snapshot(root) {
        Ok(snapshot) => snapshot,
        Err(error)
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
            }) =>
        {
            return Ok(Vec::new())
        }
        Err(error) => return Err(error),
    };
    Ok(snapshot
        .discover_without_tokens()
        .into_iter()
        .map(|row| CurrentHarnessRow {
            alias: row.alias().to_string(),
            driver: row.driver().to_string(),
            status: row.availability().label(),
            reason: row.availability().reason().unwrap_or("—").to_string(),
        })
        .collect())
}
#[cfg(feature = "selfhost")]
#[cfg(test)]
mod current_harness_tests {
    use super::*;
    use std::process::Command;

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
        fs::write(root.join(".gitignore"), ".orch/\n").expect("write fixture gitignore");
        let initialized = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["init", "-q"])
            .output()
            .expect("initialize fixture repository");
        assert!(
            initialized.status.success(),
            "git init: {}",
            String::from_utf8_lossy(&initialized.stderr)
        );
        root
    }

    #[test]
    fn current_tolerates_only_a_missing_local_harness_config() {
        let root = test_root("harness-errors");
        assert!(load_current_harnesses(&root).unwrap().is_empty());

        fs::create_dir_all(root.join(".orch")).unwrap();
        fs::write(root.join(".orch/harnesses.yaml"), "harnesses: [broken\n")
            .expect("write malformed harness config");
        let error =
            load_current_harnesses(&root).expect_err("malformed harness config must fail loud");
        assert!(
            format!("{error:#}").contains("harness config"),
            "unexpected error: {error:#}"
        );

        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn current_surfaces_dynamic_alias_driver_and_availability() {
        let root = test_root("dynamic-harnesses");
        fs::create_dir_all(root.join("coordination/runtime")).unwrap();
        fs::create_dir_all(root.join("coordination/rounds/r1")).unwrap();
        fs::create_dir_all(root.join(".orch")).unwrap();
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r1\n").unwrap();
        fs::write(root.join("coordination/rounds/r1/events.jsonl"), "").unwrap();
        fs::write(
            root.join(".orch/harnesses.yaml"),
            "version: 1\nharnesses:\n  alpha:\n    driver: cursor\n    executable: /usr/bin/true\n    enabled: true\n    cwdPolicy: project-root\n  beta:\n    driver: cursor\n    executable: /usr/bin/true\n    enabled: false\n    cwdPolicy: project-root\n",
        )
        .unwrap();

        cmd_current(&root).expect("current projection should render");
        let current = fs::read_to_string(root.join("coordination/CURRENT.md")).unwrap();
        assert!(current.contains("| alias | driver | status | reason |"));
        assert!(current.contains("| `alpha` | `cursor` | `supported` | — |"));
        assert!(
            current.contains("| `beta` | `cursor` | `unsupported` | disabled by local config |")
        );

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
#[cfg(feature = "selfhost")]

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

fn cmd_consult(
    root: &std::path::Path,
    question: PathBuf,
    harnesses: Vec<String>,
    attachments: Vec<PathBuf>,
    member_timeout_secs: Option<u64>,
    total_wall_secs: Option<u64>,
) -> Result<ExitCode> {
    let outcome = orch_host::consult::run_consultation(
        root,
        &orch_host::consult::ConsultArgs {
            question,
            harnesses,
            attachments,
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
        "orch consult · id={} fusion={}/{}",
        outcome.id,
        members_ok,
        outcome.members.len(),
    );
    println!("  artifacts={}", outcome.dir.display());
    println!("  summary={}", outcome.summary_path.display());
    if members_ok == 0 {
        bail!("consult 没有可信 substantive member answer；逐席原始产物已保留");
    }
    Ok(ExitCode::SUCCESS)
}
#[cfg(feature = "selfhost")]

fn cmd_dispatch(
    root: &std::path::Path,
    task: &str,
    local: bool,
    harness: Option<&str>,
    no_wake: bool,
    override_ambiguous_active: bool,
    new_attempt: bool,
    reason: Option<&str>,
) -> Result<ExitCode> {
    let round = orch_host::current_round(root)?;
    let schema = orch_host::round::contract_schema_at_root(root, &round)?;
    if schema == Some(orch_host::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION) {
        let route = orch_host::channel::DispatchRouteV3::from_flags(local, harness)?;
        if no_wake || override_ambiguous_active || new_attempt || reason.is_some() {
            bail!("schema 3 dispatch 不接受 legacy wake/reassignment flags");
        }
        let (outcome, route_label) = match route {
            orch_host::channel::DispatchRouteV3::Local => (
                orch_host::tierf::run_dispatch_local(root, task)?,
                "--local".to_string(),
            ),
            orch_host::channel::DispatchRouteV3::Harness(alias) => (
                orch_host::tierf::run_dispatch_harness(root, task, &alias)?,
                format!("--harness {alias}"),
            ),
        };
        println!(
            "orch dispatch {task} {route_label}: attempt={} base={} worktree={} GO={}{}",
            outcome.attempt_id,
            outcome.base_sha,
            outcome.worktree_rel,
            outcome.go_rel,
            if outcome.replayed { " (replay)" } else { "" },
        );
        return Ok(ExitCode::SUCCESS);
    }
    if local || harness.is_some() {
        bail!("legacy dispatch 不接受 --local/--harness");
    }
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
#[cfg(feature = "selfhost")]
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
#[cfg(feature = "selfhost")]

fn cmd_await(
    root: &std::path::Path,
    task: &str,
    timeout_secs: Option<u64>,
    liveness_grace_secs: u64,
    no_liveness: bool,
) -> Result<ExitCode> {
    use orch_host::tierf::AwaitOutcome;
    let (_, active) = active_round_contract(root)?;
    if active.candidate.schema_version == orch_host::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION {
        let timeout_secs = timeout_secs.unwrap_or(86_400);
        println!(
            "orch await-report {task}（schema 3 local；超时 {timeout_secs}s；provider liveness 不适用）"
        );
        return match orch_host::tierf::run_await(root, task, timeout_secs, None)? {
            AwaitOutcome::Collected(co) => {
                println!("── {task} → ready_for_verification ──");
                for note in &co.mech_notes {
                    println!("  {note}");
                }
                Ok(ExitCode::SUCCESS)
            }
            AwaitOutcome::Blocked { report_rel } => {
                println!("本地实现已阻塞：{report_rel}");
                Ok(ExitCode::from(6))
            }
            AwaitOutcome::LivenessDead { .. } | AwaitOutcome::LivenessStalled { .. } => {
                bail!("schema 3 local await 不得产生 provider liveness 终态")
            }
        };
    }
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
            println!("  下一步：保留终态证据，核实原生作业已结束，再由 planner 决定恢复或重新派发");
            Ok(ExitCode::from(3))
        }
        AwaitOutcome::LivenessStalled { reason } => {
            println!("── {task} 执行者判滞 ──");
            println!("  {reason}");
            println!("  下一步：核实任务是否仍在运行，再选择继续等待或按精确调用身份恢复");
            Ok(ExitCode::from(4))
        }
    }
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
    has_deadline_secs: bool,
) -> Result<ExitCode> {
    if has_message || has_message_file || has_review_for || has_attempt || has_deadline_secs {
        bail!(
            "orch wake cancel 仅接受 <wakeId> --reason；禁止 message/review/attempt/deadline 参数"
        );
    }
    let wake_id = wake_id.context("orch wake cancel 要求恰一个 <wakeId>")?;
    let reason = reason.context("orch wake cancel 要求 --reason <nonblank-text>")?;
    let result = cancel_wake_backend(root, wake_id, reason)?;
    use orch_host::channel::ManagedWakeCancelDisposition;
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
    has_deadline_secs: bool,
) -> Result<ExitCode> {
    if has_reason
        || has_mode
        || has_message
        || has_message_file
        || has_review_for
        || has_attempt
        || has_deadline_secs
    {
        bail!("orch wake status 仅接受 <wakeId> [--json]");
    }
    let wake_id = wake_id.context("orch wake status 要求恰一个 <wakeId>")?;
    let status = status_wake_backend(root, wake_id)?;
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
    _root: &std::path::Path,
    wake_id: Option<&str>,
    mode: Option<&str>,
    reason: Option<&str>,
    deadline_secs: Option<u64>,
    json: bool,
    has_message: bool,
    has_message_file: bool,
    has_review_for: bool,
    has_attempt: bool,
) -> Result<ExitCode> {
    if json || has_message || has_message_file || has_review_for || has_attempt {
        bail!(
            "orch wake attach 仅接受 <wakeId> --mode <resume|fork> --reason <text> [--deadline-secs <1..=21600>]"
        );
    }
    let wake_id = wake_id.context("orch wake attach 要求恰一个 <wakeId>")?;
    let mode = match mode.context("orch wake attach 要求 --mode <resume|fork>")? {
        "resume" => orch_host::channel::AttachMode::Resume,
        "fork" => orch_host::channel::AttachMode::Fork,
        _ => unreachable!("clap constrains attach mode"),
    };
    let reason = reason.context("orch wake attach 要求 --reason <nonblank-text>")?;
    let _ = (wake_id, mode, reason, deadline_secs);
    bail!("unsupported: provider-neutral attach is not implemented; no resume/fork was started");
}

#[allow(clippy::too_many_arguments)]
fn cmd_wake_declare_dead(
    _root: &std::path::Path,
    wake_id: Option<&str>,
    has_reason: bool,
    has_mode: bool,
    json: bool,
    has_message: bool,
    has_message_file: bool,
    has_review_for: bool,
    has_attempt: bool,
    has_deadline_secs: bool,
) -> Result<ExitCode> {
    if has_reason
        || has_mode
        || json
        || has_message
        || has_message_file
        || has_review_for
        || has_attempt
        || has_deadline_secs
    {
        bail!("orch wake declare-dead 仅接受 <wakeId>");
    }
    let wake_id = wake_id.context("orch wake declare-dead 要求恰一个 <wakeId>")?;
    let _ = wake_id;
    bail!(
        "unsupported: review-scoped death declaration is retired; use authenticated status/cancel"
    )
}

fn cmd_wake(
    root: &std::path::Path,
    agent: &str,
    wake_id: Option<String>,
    reason: Option<String>,
    message: Option<String>,
    message_file: Option<PathBuf>,
    review_for: Option<String>,
    attempt: Option<String>,
    deadline_secs: Option<u64>,
) -> Result<ExitCode> {
    if wake_id.is_some() || reason.is_some() {
        bail!("普通 orch wake 禁止 cancel-only 的第二 positional/--reason");
    }
    let default = "orch: 请检查当前项目并汇报进展";
    let resolved = resolve_cli_message(message, message_file, default)?;
    let source = resolved.source;
    let bytes = resolved.bytes;
    let outcome = if !orch_host::has_selfhost_state(root)? {
        if review_for.is_some() || attempt.is_some() {
            bail!("formal review requires a selfhost project");
        }
        orch_host::channel::run_direct_wake(root, agent, resolved, deadline_secs)?
    } else {
        #[cfg(feature = "selfhost")]
        {
            let review = resolve_review_request(root, agent, review_for, attempt, deadline_secs)?;
            if let Some(review) = review {
                orch_host::wake::run_wake_with_message_authorized_review(
                    root, agent, resolved, review,
                )?
            } else {
                orch_host::wake::run_wake_with_message_authorized(root, agent, resolved)?
            }
        }
        #[cfg(not(feature = "selfhost"))]
        {
            bail!("project contains selfhost state; use --features selfhost");
        }
    };
    match outcome {
        orch_host::channel::WakeRunOutcome::Spawned { wake_id } => {
            println!("orch wake {agent}: 注入已完成 · wakeId={wake_id}");
        }
        orch_host::channel::WakeRunOutcome::Idempotent { wake_id } => {
            println!("orch wake {agent}: 复用已有 wakeId={wake_id} · 未起新进程");
        }
    }
    println!("  messageSource={source} · messageBytes={bytes}");
    Ok(ExitCode::SUCCESS)
}
#[cfg(feature = "selfhost")]

fn resolve_review_request(
    root: &std::path::Path,
    agent: &str,
    review_for: Option<String>,
    attempt: Option<String>,
    deadline_secs: Option<u64>,
) -> Result<Option<orch_host::wake::ReviewRequest>> {
    match (review_for, attempt) {
        (None, None) => {
            if deadline_secs.is_some() {
                bail!("--deadline-secs 不能脱离 schema 3 review 参数组");
            }
            Ok(None)
        }
        (Some(task_id), Some(attempt_id)) => {
            let deadline_is_explicit = deadline_secs.is_some();
            let deadline_secs = match deadline_secs {
                Some(explicit) => explicit,
                None => active_round_contract(root)?
                    .1
                    .candidate
                    .tasks
                    .iter()
                    .find(|task| task.id == task_id)
                    .map(|task| {
                        orch_host::wake::review_deadline_secs(
                            "review",
                            task.required_evidence.len(),
                        )
                    })
                    .with_context(|| format!("schema 3 review task {task_id} 不在 active IR"))?,
            };
            Ok(Some(orch_host::wake::ReviewRequest {
                task_id,
                attempt_id,
                role: "review".to_string(),
                agent: agent.to_string(),
                deadline_secs,
                deadline_is_explicit,
            }))
        }
        _ => bail!("schema 3 review 要求 --review-for 与 --attempt 成组提供"),
    }
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
) -> Result<orch_host::channel::ResolvedMessage> {
    use orch_host::channel::{resolve_message_input, MessageInput};

    // 先验互斥：CLI 层只有 --message（值 `-` 即 stdin 信道）与 --message-file
    // 两个来源开关，同时给出必然是冲突——在任何读取副作用之前直接拒绝。
    if message.is_some() && message_file.is_some() {
        bail!("wake 消息来源互斥冲突：同时给出 --message 与 --message-file；请仅使用其中之一");
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
#[cfg(feature = "selfhost")]

fn cmd_review(root: &std::path::Path, action: ReviewCmd) -> Result<ExitCode> {
    match action {
        ReviewCmd::Deliver {
            task,
            attempt,
            harness,
            wake_id,
        } => {
            let outcome = orch_host::generic_review::deliver_generic_review(
                root, &task, &attempt, &harness, &wake_id,
            )?;
            println!(
                "orch review deliver: task={task} attempt={attempt} harness={harness} wakeId={wake_id} path={} commit={}{}",
                outcome.path,
                outcome.commit_sha,
                if outcome.replayed { " (replay)" } else { "" },
            );
            Ok(ExitCode::SUCCESS)
        }
    }
}
#[cfg(feature = "selfhost")]

fn cmd_sites(root: &std::path::Path, action: SitesCmd) -> Result<ExitCode> {
    match action {
        SitesCmd::Cache { action } => match action {
            DiagnosticCmd::Run {
                cwd,
                purpose,
                keep_cache,
                command,
            } => {
                let mut command = command.into_iter();
                let executable =
                    PathBuf::from(command.next().context("diagnostic executable missing")?);
                let request = orch_host::buildcache::DiagnosticRequest {
                    cwd,
                    executable,
                    args: command.collect(),
                    purpose,
                };
                let before = orch_host::storage::observe_filesystem_space(&[
                    root.to_path_buf(),
                    root.join("coordination/runtime/diagnostic-cache"),
                ]);
                let view = orch_host::buildcache::run_managed_diagnostic_with_retention(
                    root, &request, keep_cache,
                )?;
                let success = view.command_exit == Some(0);
                let report =
                    orch_host::reclaim::record_diagnostic_maintenance(root, view.clone(), before);
                let mut output = serde_json::to_value(&view)?;
                output["maintenance"] = serde_json::to_value(report)?;
                println!("{}", serde_json::to_string_pretty(&output)?);
                Ok(if success {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                })
            }
            DiagnosticCmd::Status { last_maintenance } => {
                if last_maintenance {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &orch_host::reclaim::latest_maintenance_report(root)?
                        )?
                    );
                } else {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &orch_host::buildcache::diagnostic_cache_status(root)?
                        )?
                    );
                }
                Ok(ExitCode::SUCCESS)
            }
            DiagnosticCmd::Sweep => {
                let report = orch_host::reclaim::maintain_diagnostic_storage(root)?;
                let failed = report.report_error.is_some()
                    || report.items.iter().any(|item| item.disposition == "failed");
                println!("{}", serde_json::to_string_pretty(&report)?);
                Ok(if failed {
                    ExitCode::from(4)
                } else {
                    ExitCode::SUCCESS
                })
            }
        },
        SitesCmd::Gc { round, dry_run } => {
            // Preserve explicit live receipt recovery without turning historical maintenance
            // into a ledger writer. The host rejects closed/legacy generations before sidecars.
            if !dry_run {
                if let Ok(current) = orch_host::current_round(root) {
                    if round.as_deref().is_none_or(|selected| selected == current) {
                        if let Err(error) =
                            orch_host::wake::reconcile_pending_backend_receipts(root, &current)
                        {
                            eprintln!("[orch] current live receipt recovery failed; independent storage maintenance continues: {error:#}");
                        }
                    }
                }
            }
            let report =
                orch_host::reclaim::maintain_storage_for_round(root, round.as_deref(), dry_run)?;
            let failed = report.report_error.is_some()
                || report.items.iter().any(|item| item.disposition == "failed");
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(if failed {
                ExitCode::from(4)
            } else {
                ExitCode::SUCCESS
            })
        }
        SitesCmd::SweepScratch { ttl_hours } => {
            eprintln!("scratch discovery only: ttlHours={ttl_hours} does not grant ownership");
            let report = orch_host::reclaim::maintain_storage(root, true)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(ExitCode::SUCCESS)
        }
        SitesCmd::SweepTrialCache => {
            let report = orch_host::reclaim::maintain_storage(root, true)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(ExitCode::SUCCESS)
        }
        SitesCmd::SweepTargets { ttl_hours } => {
            let ttl_secs = ttl_hours
                .checked_mul(60 * 60)
                .context("--ttl-hours 超出可表示范围")?;
            let before = orch_host::reclaim::target_maintenance_space(root)?;
            let outcome = orch_host::buildcache::sweep_targets_for_round(
                root,
                Duration::from_secs(ttl_secs),
            )?;
            let report = orch_host::reclaim::record_target_maintenance(root, &outcome, before);
            let failed = report.report_error.is_some()
                || report.items.iter().any(|item| item.disposition == "failed");
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(if failed {
                ExitCode::from(4)
            } else {
                ExitCode::SUCCESS
            })
        }
    }
}
#[cfg(feature = "selfhost")]

fn cmd_round(root: &std::path::Path, action: RoundCmd) -> Result<ExitCode> {
    match action {
        RoundCmd::Open {
            id,
            purpose,
            signed_off,
            sign_note,
            force,
        } => {
            if signed_off || sign_note.is_some() {
                bail!("schema 3 开轮不接受 --signed-off/--sign-note；先 plan 再显式 sign-off");
            }
            let o = orch_host::round::run_open_v3(root, &id, &purpose, force)?;
            if o.repaired {
                println!(
                    "orch round open {}: 部分开轮已补齐（仅补写 CURRENT-ROUND）",
                    o.round
                );
            } else {
                println!("orch round open {}: 目录骨架 + RoundOpened{} + CURRENT-ROUND + BOARD 开版行 ✅",
                    o.round, " (contractSchemaVersion=3)");
            }
            println!("  下一步：写 tasks/<ID>.md 与 seeds/ → git commit（协议资产先入库再派发）");
            println!("         → orch round seed-verified <ID> --expected-red \"...\"");
            println!("         → orch round sign-off（HITL#1）");
            println!("         → orch dispatch <ID> --local（schema 3 root planner worktree）");
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
                println!("  DONE.md 已写（root planner 继续完成 handoff；下一轮显式规划并 dispatch）");
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
#[cfg(feature = "selfhost")]

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
#[cfg(feature = "selfhost")]

fn cmd_verdict(
    root: &std::path::Path,
    task: &str,
    attempt: &str,
    expected_head: &str,
    expected_main: &str,
    verdict: &str,
    reason: Option<&str>,
    dry_run: bool,
    quarantine_review: &[String],
) -> Result<ExitCode> {
    let verdict = orch_host::verify::RootVerdict::parse(verdict)?;
    let out = orch_host::verify::run_root_verdict_with_quarantine(
        root,
        task,
        attempt,
        expected_head,
        expected_main,
        verdict,
        reason,
        dry_run,
        quarantine_review,
    )?;
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
#[cfg(feature = "selfhost")]

fn cmd_seal(
    root: &std::path::Path,
    task: &str,
    attempt: &str,
    expected_head: &str,
) -> Result<ExitCode> {
    let outcome = orch_host::close::run_seal(root, task, attempt, expected_head)?;
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
#[cfg(feature = "selfhost")]

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
#[cfg(feature = "selfhost")]

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
    let schema_version = active.candidate.schema_version;
    let projection = fold(&ledger.events);
    let now = orch_host::ledger::now_rfc3339();
    let ir_liveness = active.candidate.liveness;

    // ── agent → 在飞任务：取账本里最后一次 DispatchIssued，且该任务尚未终态 ──
    let mut agent_task: std::collections::BTreeMap<String, String> = Default::default();
    for ev in &ledger.events {
        if !matches!(
            ev.kind.as_str(),
            "DispatchIssued" | "WakeIssued" | "ReviewRequested"
        ) {
            continue;
        }
        let Some(p) = ev.payload.as_ref() else {
            continue;
        };
        let (Some(agent), Some(task)) = (
            p.get("harness")
                .and_then(|v| v.as_str())
                .or_else(|| p.get("agent").and_then(|v| v.as_str())),
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

    // ── 探针规格：本地 harness alias 一行；不读取 tracked roster ──
    let harnesses = load_current_harnesses(root)?;
    let mut specs = Vec::new();
    for row in harnesses {
        let alias = row.alias;
        let current_task = agent_task.get(&alias).cloned();
        let write_set = current_task
            .as_deref()
            .and_then(|t| orch_host::card::load(root, &round, t).ok())
            .map(|c| c.meta.write_set.clone())
            .unwrap_or_default();
        let go_path = current_task.as_deref().and_then(|t| {
            let dir = root.join(format!("coordination/rounds/{round}/dispatch/{alias}"));
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
            agent_id: alias,
            current_task,
            go_path,
            write_set,
        });
    }
    let agents = snap::probe_agents(root, &specs);

    // ── 三维预算：上限取 mode 配置，已花取账本折叠 ──
    let rb = if schema_version == orch_host::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION {
        None
    } else {
        orch_host::budget::load_round_budget(root)?
    };
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
#[cfg(feature = "selfhost")]

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
#[cfg(feature = "selfhost")]

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
#[cfg(feature = "selfhost")]
#[derive(Debug)]
struct StallProcessSnapshot {
    local_orch: Vec<String>,
    agent: Vec<String>,
    probe_available: bool,
}
#[cfg(feature = "selfhost")]

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
#[cfg(feature = "selfhost")]

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
#[cfg(feature = "selfhost")]

fn stall_payload_str<'a>(event: &'a orch_core::EventRecord, key: &str) -> Option<&'a str> {
    event
        .payload
        .as_ref()
        .and_then(|payload| payload.get(key))
        .and_then(serde_json::Value::as_str)
}
#[cfg(feature = "selfhost")]

fn stall_artifact_label(artifact: orch_host::stall::ArtifactOnBranch) -> &'static str {
    match artifact {
        orch_host::stall::ArtifactOnBranch::None => "none",
        orch_host::stall::ArtifactOnBranch::Report => "REPORT",
        orch_host::stall::ArtifactOnBranch::Blocked => "BLOCKED",
    }
}
#[cfg(feature = "selfhost")]

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
#[cfg(feature = "selfhost")]

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
#[cfg(feature = "selfhost")]

fn stall_line_has_token(line: &str, expected: &str) -> bool {
    line.split(|character: char| {
        !(character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    })
    .any(|token| token == expected)
}
#[cfg(feature = "selfhost")]

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
#[cfg(feature = "selfhost")]

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
#[cfg(feature = "selfhost")]
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
#[cfg(feature = "selfhost")]
#[cfg(test)]
mod review_wake_cli_tests {
    use super::*;

    #[test]
    fn schema3_review_pair_is_role_fixed_and_explicit_deadline_is_lazy() {
        let root = std::path::Path::new("explicit-deadline-needs-no-active-ir");
        assert!(resolve_review_request(root, "alpha", None, None, None)
            .unwrap()
            .is_none());
        assert!(
            resolve_review_request(root, "alpha", Some("B323".to_string()), None, None,).is_err()
        );
        assert!(resolve_review_request(root, "alpha", None, None, Some(10)).is_err());

        let request = resolve_review_request(
            root,
            "alpha",
            Some("B323".to_string()),
            Some("B323-A0001".to_string()),
            Some(2_345),
        )
        .unwrap()
        .unwrap();
        assert_eq!(request.task_id, "B323");
        assert_eq!(request.attempt_id, "B323-A0001");
        assert_eq!(request.role, "review");
        assert_eq!(request.agent, "alpha");
        assert_eq!(request.deadline_secs, 2_345);
        assert!(
            request.deadline_is_explicit,
            "CLI intent must survive workload defaulting"
        );
    }

    #[test]
    fn clap_exposes_only_generic_review_and_rejects_retired_shapes() {
        let wake = Cli::try_parse_from([
            "orch",
            "wake",
            "alpha",
            "--review-for",
            "B323",
            "--attempt",
            "B323-A0001",
            "--deadline-secs",
            "7200",
        ])
        .unwrap();
        match wake.cmd {
            Cmd::Wake {
                agent,
                review_for,
                attempt,
                deadline_secs,
                ..
            } => {
                assert_eq!(agent, "alpha");
                assert_eq!(review_for.as_deref(), Some("B323"));
                assert_eq!(attempt.as_deref(), Some("B323-A0001"));
                assert_eq!(deadline_secs, Some(7_200));
            }
            _ => panic!("expected generic review wake"),
        }

        let deliver = Cli::try_parse_from([
            "orch",
            "review",
            "deliver",
            "B323",
            "B323-A0001",
            "--harness",
            "alpha",
            "--wake-id",
            "wake-alpha",
        ])
        .unwrap();
        match deliver.cmd {
            Cmd::Review {
                action:
                    ReviewCmd::Deliver {
                        task,
                        attempt,
                        harness,
                        wake_id,
                    },
            } => {
                assert_eq!(task, "B323");
                assert_eq!(attempt, "B323-A0001");
                assert_eq!(harness, "alpha");
                assert_eq!(wake_id, "wake-alpha");
            }
            _ => panic!("expected generic review delivery"),
        }

        for retired in [
            vec!["orch", "runtime-policy", "activate", "review-pool-v1"],
            vec![
                "orch",
                "review",
                "reconcile",
                "B323",
                "--attempt",
                "B323-A0001",
            ],
            vec![
                "orch",
                "review",
                "panel",
                "select",
                "B323",
                "--attempt",
                "B323-A0001",
            ],
            vec![
                "orch",
                "review",
                "deliver",
                "B323",
                "B323-A0001",
                "--role",
                "primary",
                "--agent",
                "legacy",
            ],
            vec!["orch", "wake", "alpha", "--role", "primary"],
            vec!["orch", "wake", "alpha", "--reissue", "old-wake"],
        ] {
            assert!(Cli::try_parse_from(retired).is_err());
        }
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
        )
        .unwrap_err()
        .to_string()
        .contains("禁止"));
        assert!(cmd_wake(
            std::path::Path::new("."),
            "alpha",
            Some("019fc001-1111-4222-8333-444455556666".to_string()),
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
        assert!(cancel_body.contains("cancel_wake_backend"));
        let backend_start = source.find("\nfn cancel_wake_backend(").unwrap() + 1;
        let backend_end = source[backend_start..].find("\n}").unwrap() + backend_start + 2;
        let backend = &source[backend_start..backend_end];
        assert!(
            backend.contains("request_managed_wake_cancel")
                && backend.contains("cancel_direct_wake")
        );
        let cancel_body = format!("{cancel_body}\n{backend}");
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

    #[test]
    fn sites_maintenance_shapes_remain_available() {
        let gc = Cli::try_parse_from(["orch", "sites", "gc", "--round", "r63"]).unwrap();
        match gc.cmd {
            Cmd::Sites {
                action: SitesCmd::Gc { round, dry_run },
            } => {
                assert_eq!(round.as_deref(), Some("r63"));
                assert!(!dry_run);
            }
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

        assert!(Cli::try_parse_from(["orch", "sites", "rotate-logs"]).is_err());
    }
}
#[cfg(feature = "selfhost")]
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
#[cfg(feature = "selfhost")]
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
#[cfg(feature = "selfhost")]
#[cfg(test)]
mod v3_round_boundary_tests {
    use super::*;

    fn fixture_root(label: &str) -> PathBuf {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap()
            .join("orch/target/test-tmp")
            .join(format!("v3-round-boundary-{label}-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root).unwrap();
        }
        fs::create_dir_all(root.join("coordination/runtime")).unwrap();
        fs::create_dir_all(root.join("coordination/rounds/r83")).unwrap();
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r83\n").unwrap();
        let live_manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let live_root = live_manifest.ancestors().nth(3).unwrap().to_path_buf();
        fs::copy(
            live_root.join("coordination/PROJECT-BINDING.yaml"),
            root.join("coordination/PROJECT-BINDING.yaml"),
        )
        .unwrap();
        root
    }

    fn write_events(root: &std::path::Path, events: &[orch_core::EventRecord]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for event in events {
            serde_json::to_writer(&mut bytes, event).unwrap();
            bytes.push(b'\n');
        }
        fs::write(root.join("coordination/rounds/r83/events.jsonl"), &bytes).unwrap();
        bytes
    }

    #[test]
    fn legacy_or_malformed_binding_never_unlocks_retired_round_writers() {
        let root = fixture_root("legacy");
        let binding_path = root.join("coordination/PROJECT-BINDING.yaml");
        let v3_binding = fs::read(&binding_path).unwrap();
        let legacy_open = orch_host::ledger::event(
            "RoundOpened",
            "runtime:orch",
            None,
            Some("r83"),
            serde_json::json!({"purpose": "legacy"}),
        );
        let before = write_events(&root, std::slice::from_ref(&legacy_open));
        assert!(enforce_actorless_write_generation(&root, &Cmd::Plan).is_err());
        assert_eq!(
            fs::read(root.join("coordination/rounds/r83/events.jsonl")).unwrap(),
            before
        );
        fs::write(
            &binding_path,
            b"project: {ecosystems: [rust]}\ncommands: {}\nscope: {protectedPaths: []}\ngit: {pushPolicy: forbidden}\n",
        )
        .unwrap();
        assert!(enforce_actorless_write_generation(&root, &Cmd::Plan).is_err());
        let mut malformed_v3 = v3_binding.clone();
        malformed_v3.extend_from_slice(b"unknownMigrationEscape: null\n");
        fs::write(&binding_path, malformed_v3).unwrap();
        assert!(enforce_actorless_write_generation(&root, &Cmd::Plan).is_err());
        fs::remove_file(&binding_path).unwrap();
        assert!(enforce_actorless_write_generation(&root, &Cmd::Plan).is_err());
        assert!(orch_host::plan::run_plan(&root).is_err());
        assert_eq!(
            fs::read(root.join("coordination/rounds/r83/events.jsonl")).unwrap(),
            before
        );
        fs::write(&binding_path, v3_binding).unwrap();

        let marked_open = orch_host::ledger::event(
            "RoundOpened",
            "runtime:orch",
            None,
            Some("r83"),
            serde_json::json!({"purpose": "v3", "contractSchemaVersion": 3}),
        );
        let closed = orch_host::ledger::event(
            "RoundClosed",
            "runtime:orch",
            None,
            Some("r83"),
            serde_json::json!({"forced": false}),
        );
        let before = write_events(&root, &[marked_open, closed]);
        assert!(enforce_actorless_write_generation(&root, &Cmd::Plan).is_err());
        assert!(enforce_actorless_write_generation(
            &root,
            &Cmd::Round {
                action: RoundCmd::Open {
                    id: "r84".to_string(),
                    purpose: String::new(),
                    signed_off: false,
                    sign_note: None,
                    force: false,
                },
            },
        )
        .is_ok());
        assert_eq!(
            fs::read(root.join("coordination/rounds/r83/events.jsonl")).unwrap(),
            before
        );
        fs::remove_dir_all(root).ok();
    }
}
#[cfg(feature = "selfhost")]
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

#[cfg(test)]
mod retired_writer_surface_tests {
    use super::*;

    #[test]
    fn only_managed_supervisor_is_hidden_and_no_command_aliases_exist() {
        fn walk(command: &clap::Command, prefix: &str, hidden: &mut Vec<String>) {
            for child in command.get_subcommands() {
                let path = if prefix.is_empty() {
                    child.get_name().to_string()
                } else {
                    format!("{prefix} {}", child.get_name())
                };
                assert_eq!(
                    child.get_all_aliases().count(),
                    0,
                    "unreviewed command alias: {path}"
                );
                if child.is_hide_set() {
                    hidden.push(path.clone());
                }
                walk(child, &path, hidden);
            }
        }
        let mut hidden = Vec::new();
        walk(&Cli::command(), "", &mut hidden);
        assert_eq!(
            hidden,
            ["__wake-supervise"],
            "unexpected hidden runtime entry"
        );
    }
}

fn enforce_build_project_mode(root: &std::path::Path, command: &Cmd) -> Result<()> {
    if !cfg!(feature = "selfhost")
        && matches!(
            command,
            Cmd::Wake { .. } | Cmd::Consult { .. } | Cmd::Doctor
        )
        && orch_host::has_selfhost_state(root)?
    {
        bail!("project contains selfhost state; use an orch build with --features selfhost");
    }
    Ok(())
}

fn validate_standalone_flags(command: &Cmd) -> Result<()> {
    if let Cmd::Wake {
        review_for,
        attempt,
        ..
    } = command
    {
        if review_for.is_some() || attempt.is_some() {
            bail!("review flags require a selfhost project");
        }
    }
    Ok(())
}

#[cfg(not(feature = "selfhost"))]
fn preflight_cli_command(root: &std::path::Path, command: &Cmd) -> Result<()> {
    enforce_build_project_mode(root, command)?;
    validate_standalone_flags(command)
}

fn cmd_doctor(root: &std::path::Path) -> Result<ExitCode> {
    #[cfg(feature = "selfhost")]
    if orch_host::has_selfhost_state(root)? {
        return cmd_selfhost_doctor(root);
    }
    let head = orch_host::gitx::rev_parse(root, "HEAD^{commit}")?;
    let status = cmd_harness(root, HarnessCmd::Lint)?;
    println!("orch doctor: standalone Git head={head}; local harness configuration checked");
    Ok(status)
}

fn status_wake_backend(
    root: &std::path::Path,
    wake_id: &str,
) -> Result<orch_host::channel::ManagedWakeStatusView> {
    #[cfg(feature = "selfhost")]
    if !orch_host::channel::is_direct_wake(root, wake_id)? {
        return orch_host::wake::managed_wake_status(root, wake_id);
    }
    orch_host::channel::direct_wake_status(root, wake_id)
}

fn cancel_wake_backend(
    root: &std::path::Path,
    wake_id: &str,
    reason: &str,
) -> Result<orch_host::channel::ManagedWakeCancelResult> {
    #[cfg(feature = "selfhost")]
    if !orch_host::channel::is_direct_wake(root, wake_id)? {
        return orch_host::wake::request_managed_wake_cancel(root, wake_id, reason);
    }
    orch_host::channel::cancel_direct_wake(root, wake_id, reason)
}
